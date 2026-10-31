use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

const MAX_BODY_BYTES: usize = 4 << 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_MAX_ENTRIES: usize = 2000;
const MAX_FILTERED_LIST_COUNT: u64 = 50;

#[derive(Debug)]
pub enum UpstreamError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueListItem {
    pub id: u64,
    #[serde(default)]
    pub series: IssueSeries,
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub cover_date: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub upc: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IssueSeries {
    pub id: u64,
    pub name: String,
    pub volume: Option<u32>,
    pub year_began: Option<u16>,
    pub genres: Vec<NamedItem>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueDetail {
    pub id: u64,
    #[serde(default)]
    pub publisher: Option<NamedItem>,
    #[serde(default)]
    pub series: IssueSeries,
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub name: Vec<String>,
    #[serde(default)]
    pub cover_date: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
    #[serde(default)]
    pub upc: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub average_rating: Option<f64>,
    #[serde(default)]
    pub rating_count: Option<u64>,
    #[serde(default)]
    pub credits: Vec<Credit>,
    #[serde(default)]
    pub resource_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NamedItem {
    pub id: Option<u64>,
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Credit {
    pub id: Option<u64>,
    pub creator: String,
    pub role: Vec<NamedItem>,
}

#[derive(Deserialize)]
struct ListResponse<T> {
    #[serde(default)]
    count: u64,
    results: Vec<T>,
}

struct CacheEntry {
    body: Vec<u8>,
    inserted_at: Instant,
}

pub struct Client {
    base: String,
    auth_header: String,
    http: reqwest::Client,
    pacer: Pacer,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl Client {
    pub fn new(base: &str, api_key: &str, user_agent: &str, min_interval: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(user_agent)
            .build()
            .expect("could not build the HTTP client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            auth_header: format!("Bearer {}", api_key.trim()),
            http,
            pacer: Pacer::new(min_interval),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn search_issues(
        &self,
        series_name: &str,
        number: &str,
        year_began: Option<u16>,
    ) -> Result<Vec<IssueListItem>, UpstreamError> {
        let mut query = vec![("series_name", series_name), ("number", number)];
        let year_str;
        if let Some(year) = year_began {
            year_str = year.to_string();
            query.push(("series_year_began", &year_str));
        }
        self.get_filtered_list("/issue/", &query).await
    }

    pub async fn issues_by_upc(&self, upc: &str) -> Result<Vec<IssueListItem>, UpstreamError> {
        self.get_filtered_list("/issue/", &[("upc", upc)]).await
    }

    pub async fn issue(&self, id: u64) -> Result<IssueDetail, UpstreamError> {
        self.get(&format!("/issue/{id}/"), &[]).await
    }

    pub fn cached_issue(&self, id: u64) -> Option<IssueDetail> {
        let url = reqwest::Url::parse(&format!("{}/issue/{id}/", self.base)).ok()?;
        let cached = self.get_cached(url.as_str())?;
        serde_json::from_slice(&cached).ok()
    }

    async fn get_filtered_list<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>, UpstreamError> {
        let body: ListResponse<T> = self.get(path, query).await?;
        if body.count > MAX_FILTERED_LIST_COUNT {
            eprintln!("[provider.metron] [warn] filter ignored by upstream");
            return Ok(Vec::new());
        }
        Ok(body.results)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, UpstreamError> {
        let mut url = reqwest::Url::parse(&format!("{}{path}", self.base))
            .map_err(|_| UpstreamError::Failed("invalid upstream URL".into()))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let cache_key = url.as_str().to_string();

        if let Some(cached) = self.get_cached(&cache_key) {
            return serde_json::from_slice(&cached).map_err(|error| {
                UpstreamError::Failed(format!("metron answer unreadable: {error}"))
            });
        }

        // While blocked by a Metron 429, every uncached request answers 429 immediately without calling Metron.
        self.pacer.check_blocked()?;

        let _guard = self.pacer.acquire().await?;

        // Re-check cache after waiting on the pacer in case an earlier request populated it.
        if let Some(cached) = self.get_cached(&cache_key) {
            return serde_json::from_slice(&cached).map_err(|error| {
                UpstreamError::Failed(format!("metron answer unreadable: {error}"))
            });
        }

        let mut res = self
            .http
            .get(url)
            .header("Accept", "application/json")
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .map_err(|error| UpstreamError::Failed(error.without_url().to_string()))?;

        match res.status().as_u16() {
            404 => return Err(UpstreamError::NotFound),
            401 | 403 => {
                let status = res.status().as_u16();
                eprintln!(
                    "[provider.metron] [auth] status={status} - Metron rejected METRON_API_KEY"
                );
                return Err(UpstreamError::Failed(format!(
                    "Metron rejected METRON_API_KEY (status {status})"
                )));
            }
            429 => {
                let retry_after = res
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .filter(|seconds| *seconds > 0);
                let backoff = match retry_after {
                    Some(secs) => Duration::from_secs(secs + 1),
                    None => Duration::from_secs(60),
                };
                let retry_after_seconds = self.pacer.block_for(backoff);
                eprintln!(
                    "[provider.metron] [ratelimit] status=429 retry_after={retry_after_seconds}"
                );
                return Err(UpstreamError::RateLimited {
                    retry_after_seconds,
                });
            }
            200..=299 => {}
            status => {
                return Err(UpstreamError::Failed(format!("metron answered {status}")));
            }
        }

        let mut body = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|error| UpstreamError::Failed(error.without_url().to_string()))?
        {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(UpstreamError::Failed("metron answer too large".into()));
            }
            body.extend_from_slice(&chunk);
        }

        let parsed: T = serde_json::from_slice(&body)
            .map_err(|error| UpstreamError::Failed(format!("metron answer unreadable: {error}")))?;
        self.put_cached(cache_key, body);
        Ok(parsed)
    }

    fn get_cached(&self, key: &str) -> Option<Vec<u8>> {
        let mut cache = self.cache.lock().expect("cache lock poisoned");
        let entry = cache.get(key)?;
        if Instant::now().duration_since(entry.inserted_at) < CACHE_TTL {
            Some(entry.body.clone())
        } else {
            cache.remove(key);
            None
        }
    }

    fn put_cached(&self, key: String, body: Vec<u8>) {
        let mut cache = self.cache.lock().expect("cache lock poisoned");
        let now = Instant::now();
        if cache.len() >= CACHE_MAX_ENTRIES && !cache.contains_key(&key) {
            cache.retain(|_, v| now.duration_since(v.inserted_at) < CACHE_TTL);
            if cache.len() >= CACHE_MAX_ENTRIES {
                if let Some(oldest_key) = cache
                    .iter()
                    .min_by_key(|(_, v)| v.inserted_at)
                    .map(|(k, _)| k.clone())
                {
                    cache.remove(&oldest_key);
                }
            }
        }
        cache.insert(
            key,
            CacheEntry {
                body,
                inserted_at: now,
            },
        );
    }
}

/// Process-wide pacer: spaces Metron requests at least `interval` apart, ensures only one request
/// is in flight at a time, and immediately rejects callers while a 429 backoff is active.
pub struct Pacer {
    interval: Duration,
    blocked_until: Mutex<Option<Instant>>,
    next_start: AsyncMutex<Instant>,
}

pub struct PacerGuard<'a> {
    _guard: AsyncMutexGuard<'a, Instant>,
}

impl Pacer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            blocked_until: Mutex::new(None),
            next_start: AsyncMutex::new(Instant::now()),
        }
    }

    pub fn check_blocked(&self) -> Result<(), UpstreamError> {
        let mut blocked = self.blocked_until.lock().expect("pacer lock poisoned");
        if let Some(until) = *blocked {
            let now = Instant::now();
            if until > now {
                let remaining = (until - now).as_secs_f64().ceil() as u64;
                return Err(UpstreamError::RateLimited {
                    retry_after_seconds: remaining.max(1),
                });
            }
            *blocked = None;
        }
        Ok(())
    }

    pub async fn acquire(&self) -> Result<PacerGuard<'_>, UpstreamError> {
        self.check_blocked()?;
        let mut next_start = self.next_start.lock().await;
        self.check_blocked()?;
        if !self.interval.is_zero() {
            let now = Instant::now();
            if *next_start > now {
                tokio::time::sleep_until(tokio::time::Instant::from_std(*next_start)).await;
            }
            self.check_blocked()?;
            *next_start = Instant::now() + self.interval;
        }
        Ok(PacerGuard { _guard: next_start })
    }

    pub fn block_for(&self, duration: Duration) -> u64 {
        let until = Instant::now() + duration;
        *self.blocked_until.lock().expect("pacer lock poisoned") = Some(until);
        duration.as_secs_f64().ceil().max(1.0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pacer_spaces_consecutive_calls_by_at_least_interval() {
        let interval = Duration::from_millis(60);
        let pacer = Pacer::new(interval);

        let first_start = Instant::now();
        {
            let _g1 = pacer.acquire().await.expect("first call succeeds");
        }
        let second_start = {
            let _g2 = pacer.acquire().await.expect("second call succeeds");
            Instant::now()
        };
        assert!(
            second_start.duration_since(first_start) >= interval,
            "expected >= {:?}, got {:?}",
            interval,
            second_start.duration_since(first_start)
        );
    }

    #[tokio::test]
    async fn pacer_blocks_callers_immediately_during_429_backoff() {
        let pacer = Pacer::new(Duration::ZERO);
        let retry = pacer.block_for(Duration::from_secs(31));
        assert_eq!(retry, 31);
        match pacer.acquire().await {
            Err(UpstreamError::RateLimited {
                retry_after_seconds,
            }) => assert!((1..=31).contains(&retry_after_seconds)),
            other => panic!("expected RateLimited, got {:?}", other.err()),
        };
    }
}
