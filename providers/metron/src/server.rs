use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use regex::Regex;
use serde_json::json;

use crate::mapping::{
    clean_code, extract_upc, map_detail, map_list_item, parse_query, parse_series_and_index,
    rank_issues, story_title_matches, BookMetadata,
};
use crate::metron::{Client, IssueDetail, UpstreamError};
use crate::{Config, VERSION};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 10;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ID_LENGTH: usize = 12;
const MAX_UPC_DETAIL_CHECKS: usize = 2;
// TODO: pick Metron's final brand color.
const COLOR: &str = "#ffc107";

static BOOK_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]{1,12}$").expect("valid id pattern"));

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    metron: Arc<Client>,
}

pub fn routes(cfg: Arc<Config>, metron: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{id}", get(lookup))
        .with_state(AppState { cfg, metron })
}

async fn info(State(state): State<AppState>) -> Response {
    // TODO: set avatarUrl once a raster icon is chosen.
    Json(json!({
        "name": "Metron",
        "key": "metron",
        "version": VERSION,
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        "color": COLOR,
        "mediaTypes": ["comic"],
        "lookup": true,
        "searchParams": ["isbn", "limit", "series", "seriesIndex"],
        "supportedFields": [
            "title", "description", "cover", "authors", "publisher", "isbn",
            "publishedYear", "language", "pageCount", "communityRating", "seriesName",
            "seriesIndex", "genres"
        ],
        "bookUrlTemplate": "https://metron.cloud/issue/{id}/",
        "idMaxLength": MAX_ID_LENGTH,
        "rateLimit": { "maxConcurrency": 1, "minIntervalMs": 7000 },
        "coverShape": "portrait",
    }))
    .into_response()
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let query = q.get("query").map(|v| v.trim()).unwrap_or_default();
    let isbn = q.get("isbn").map(|v| v.trim()).unwrap_or_default();
    let series = q.get("series").map(|v| v.trim()).unwrap_or_default();
    let series_index = q.get("seriesIndex").map(|v| v.trim()).unwrap_or_default();
    if query.is_empty() && isbn.is_empty() && (series.is_empty() || series_index.is_empty()) {
        return error(StatusCode::BAD_REQUEST, "query is required");
    }
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(DEFAULT_LIMIT, |n| n.clamp(1, MAX_LIMIT));

    match tokio::time::timeout(
        REQUEST_TIMEOUT,
        run_search(&state.metron, query, isbn, series, series_index, limit),
    )
    .await
    {
        Ok(Ok(matches)) => Json(json!({ "matches": matches })).into_response(),
        Ok(Err(err)) => upstream_error("search", err),
        Err(_) => upstream_error("search", UpstreamError::Failed("timed out".into())),
    }
}

async fn run_search(
    metron: &Client,
    query: &str,
    isbn_param: &str,
    series_param: &str,
    series_index_param: &str,
    limit: usize,
) -> Result<Vec<BookMetadata>, UpstreamError> {
    if let Some(upc) = extract_upc(isbn_param) {
        if let Some(matched) = search_by_upc(metron, &upc).await? {
            return Ok(vec![map_detail(&matched)]);
        }
    }

    let parsed = if !series_param.is_empty() && !series_index_param.is_empty() {
        parse_series_and_index(series_param, series_index_param)
    } else {
        parse_query(query)
    };
    let Some(parsed) = parsed else {
        return Ok(Vec::new());
    };

    let list = metron
        .search_issues(&parsed.series, &parsed.number, parsed.year)
        .await?;
    let candidates: Vec<_> = rank_issues(list, &parsed).into_iter().take(limit).collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let first_detail = match metron.issue(candidates[0].id).await {
        Ok(detail) => Some(detail),
        Err(err) => {
            eprintln!(
                "[provider.search] [warn] id={} error={err:?} - detail fetch failed, falling back to list item",
                candidates[0].id
            );
            None
        }
    };

    let mut preferred_idx = 0usize;
    let mut preferred_detail = first_detail;
    if !query.is_empty()
        && !preferred_detail
            .as_ref()
            .is_some_and(|d| story_title_matches(d, query))
    {
        for (idx, item) in candidates.iter().enumerate().skip(1) {
            if let Some(cached) = metron.cached_issue(item.id) {
                if story_title_matches(&cached, query) {
                    preferred_idx = idx;
                    preferred_detail = Some(cached);
                    break;
                }
            }
        }
    }

    let mut matches = Vec::with_capacity(candidates.len());
    if let Some(detail) = preferred_detail {
        matches.push(map_detail(&detail));
    } else {
        matches.push(map_list_item(&candidates[preferred_idx]));
    }
    for (idx, item) in candidates.iter().enumerate() {
        if idx != preferred_idx {
            matches.push(map_list_item(item));
        }
    }

    Ok(matches)
}

async fn search_by_upc(
    metron: &Client,
    wanted: &str,
) -> Result<Option<IssueDetail>, UpstreamError> {
    let items = metron.issues_by_upc(wanted).await?;

    for item in items.into_iter().take(MAX_UPC_DETAIL_CHECKS) {
        if item.upc.as_deref().is_some_and(|v| clean_code(v) == wanted) {
            if let Ok(detail) = metron.issue(item.id).await {
                if detail_upc_matches(&detail, wanted) {
                    return Ok(Some(detail));
                }
            }
            continue;
        }
        if item.upc.is_none() {
            match metron.issue(item.id).await {
                Ok(detail) if detail_upc_matches(&detail, wanted) => {
                    return Ok(Some(detail));
                }
                Ok(_) | Err(UpstreamError::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
    }

    Ok(None)
}

fn detail_upc_matches(detail: &IssueDetail, wanted: &str) -> bool {
    detail
        .upc
        .as_deref()
        .is_some_and(|v| clean_code(v) == wanted)
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    if !BOOK_ID.is_match(&raw) {
        return error(StatusCode::NOT_FOUND, "Not found");
    }
    let Ok(id) = raw.parse::<u64>() else {
        return error(StatusCode::NOT_FOUND, "Not found");
    };
    if id == 0 {
        return error(StatusCode::NOT_FOUND, "Not found");
    }

    match tokio::time::timeout(REQUEST_TIMEOUT, state.metron.issue(id)).await {
        Ok(Ok(detail)) => Json(map_detail(&detail)).into_response(),
        Ok(Err(UpstreamError::NotFound)) => error(StatusCode::NOT_FOUND, "Not found"),
        Ok(Err(err)) => upstream_error("lookup", err),
        Err(_) => upstream_error("lookup", UpstreamError::Failed("timed out".into())),
    }
}

fn upstream_error(op: &str, err: UpstreamError) -> Response {
    match err {
        UpstreamError::RateLimited {
            retry_after_seconds,
        } => {
            let mut res = error(StatusCode::TOO_MANY_REQUESTS, "rate limited");
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            res
        }
        UpstreamError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        UpstreamError::Failed(message) => {
            eprintln!("[provider.{op}] [fail] error={message:?} - metron request failed");
            error(StatusCode::BAD_GATEWAY, "metron unavailable")
        }
    }
}

fn deny_unauthorized(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let token = state.cfg.auth_token.as_ref()?;
    let given = headers
        .get(header::AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    (!constant_time_eq(given, token.as_bytes()))
        .then(|| error(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    struct FakeMetron {
        status: AtomicU16,
        hits: AtomicUsize,
        last_issue_query: Mutex<Option<HashMap<String, String>>>,
    }

    impl FakeMetron {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                status: AtomicU16::new(0),
                hits: AtomicUsize::new(0),
                last_issue_query: Mutex::new(None),
            })
        }

        fn last_query(&self) -> HashMap<String, String> {
            self.last_issue_query
                .lock()
                .expect("lock")
                .clone()
                .unwrap_or_default()
        }
    }

    async fn start_fake_metron(fake: Arc<FakeMetron>) -> String {
        let app = Router::new()
            .route(
                "/api/issue/",
                get({
                    let fake = fake.clone();
                    move |Query(q): Query<HashMap<String, String>>| {
                        let fake = fake.clone();
                        async move {
                            fake.hits.fetch_add(1, Ordering::SeqCst);
                            *fake.last_issue_query.lock().expect("lock") = Some(q.clone());
                            if let Some(res) = forced_status(&fake) {
                                return res;
                            }
                            if q.get("upc").map(String::as_str) == Some("70985301166800111") {
                                (
                                    StatusCode::OK,
                                    include_str!("../testdata/issue_by_upc.json"),
                                )
                                    .into_response()
                            } else if q.get("upc").map(String::as_str) == Some("70985300000000000")
                            {
                                let body = include_str!("../testdata/issue_series_name.json")
                                    .replacen(r#""count":32"#, r#""count":186997"#, 1);
                                (StatusCode::OK, body).into_response()
                            } else {
                                // Returns the 32-issue Saga fixture even for unknown filters to test
                                // client-side verification against Metron ignoring unknown filters.
                                (
                                    StatusCode::OK,
                                    include_str!("../testdata/issue_series_name.json"),
                                )
                                    .into_response()
                            }
                        }
                    }
                }),
            )
            .route(
                "/api/issue/{id}/",
                get({
                    let fake = fake.clone();
                    move |Path(id): Path<String>| {
                        let fake = fake.clone();
                        async move {
                            fake.hits.fetch_add(1, Ordering::SeqCst);
                            if let Some(res) = forced_status(&fake) {
                                return res;
                            }
                            match id.as_str() {
                                "7997" => (
                                    StatusCode::OK,
                                    include_str!("../testdata/issue_detail_saga1.json"),
                                )
                                    .into_response(),
                                _ => (StatusCode::NOT_FOUND, r#"{"detail":"Not found."}"#)
                                    .into_response(),
                            }
                        }
                    }
                }),
            );
        format!("{}/api/", serve(app).await)
    }

    fn forced_status(fake: &FakeMetron) -> Option<Response> {
        match fake.status.load(Ordering::SeqCst) {
            0 => None,
            code => Some(
                (
                    StatusCode::from_u16(code).unwrap(),
                    [(header::RETRY_AFTER, "30")],
                    r#"{"detail":"throttled"}"#,
                )
                    .into_response(),
            ),
        }
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn provider(auth_token: Option<&str>) -> (String, Arc<FakeMetron>) {
        let fake = FakeMetron::new();
        let upstream = start_fake_metron(fake.clone()).await;
        let cfg = Config {
            auth_token: auth_token.map(str::to_string),
        };
        let client = Client::new(&upstream, "test-api-key", "test-agent", Duration::ZERO);
        (serve(routes(Arc::new(cfg), Arc::new(client))).await, fake)
    }

    async fn get_json(url: &str, auth: Option<&str>) -> (u16, serde_json::Value, HeaderMap) {
        let mut req = reqwest::Client::new().get(url);
        if let Some(auth) = auth {
            req = req.header("Authorization", auth);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let body =
            serde_json::from_slice(&res.bytes().await.unwrap()).unwrap_or(serde_json::Value::Null);
        (status, body, headers)
    }

    #[tokio::test]
    async fn info_declares_the_contract() {
        let (base, _) = provider(None).await;
        let (code, body, _) = get_json(&format!("{base}/provider/info"), None).await;
        assert_eq!(code, 200);
        assert_eq!(body["name"], "Metron");
        assert_eq!(body["key"], "metron");
        assert_eq!(body["version"], "0.1.0");
        assert_eq!(body["mediaTypes"], json!(["comic"]));
        assert_eq!(body["lookup"], true);
        assert_eq!(
            body["searchParams"],
            json!(["isbn", "limit", "series", "seriesIndex"])
        );
        let supported = body["supportedFields"].as_array().unwrap();
        assert!(supported.contains(&json!("title")));
        assert!(!supported.contains(&json!("subtitle")));
        assert_eq!(body["bookUrlTemplate"], "https://metron.cloud/issue/{id}/");
        assert_eq!(body["idMaxLength"], 12);
        assert_eq!(body["rateLimit"]["maxConcurrency"], 1);
        assert_eq!(body["rateLimit"]["minIntervalMs"], 7000);
        assert_eq!(body["coverShape"], "portrait");
        assert!(body.get("avatarUrl").is_none());
    }

    #[tokio::test]
    async fn search_by_query_ranks_saga_first_and_fetches_first_detail_only() {
        let (base, fake) = provider(None).await;
        let (code, body, _) = get_json(
            &format!("{base}/search?query=Saga+%231&mediaType=audiobook"),
            None,
        )
        .await;
        assert_eq!(code, 200);
        let matches = body["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 5);
        assert_eq!(matches[0]["providerId"], "7997");
        assert_eq!(matches[0]["title"], "Chapter One");
        assert!(matches[0].get("subtitle").is_none());
        assert_eq!(matches[0]["authors"], json!(["Brian K. Vaughan"]));
        assert_eq!(
            matches[0]["series"],
            json!([{ "series": "Saga (2012)", "sequence": "1" }])
        );
        // Subsequent results are mapped from list items without extra detail requests.
        assert_eq!(matches[1]["title"], "Saga #1");
        assert!(matches[1].get("authors").is_none());
        assert_eq!(fake.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn search_by_series_and_series_index_with_and_without_year_and_precedence() {
        let (base, fake) = provider(None).await;

        // 1. With run year in `series`: sends `series_year_began` and takes precedence over a
        // story-title `query` ("Chapter One") that has no issue number.
        let (code, body, _) = get_json(
            &format!(
                "{base}/search?query=Chapter+One&series=Saga+(2012)+(Digital)&seriesIndex=001"
            ),
            None,
        )
        .await;
        assert_eq!(code, 200);
        let saw_with_year = fake.last_query();
        assert_eq!(
            saw_with_year.get("series_name").map(String::as_str),
            Some("Saga")
        );
        assert_eq!(saw_with_year.get("number").map(String::as_str), Some("1"));
        assert_eq!(
            saw_with_year.get("series_year_began").map(String::as_str),
            Some("2012")
        );
        let matches = body["matches"].as_array().unwrap();
        assert_eq!(matches[0]["providerId"], "7997");
        assert_eq!(matches[0]["title"], "Chapter One");
        assert!(matches[0].get("subtitle").is_none());
        assert_eq!(fake.hits.load(Ordering::SeqCst), 2);

        // 2. Without year in `series`: omits `series_year_began` and still takes precedence over a
        // conflicting `query` ("Watchmen #99").
        let (code, body, _) = get_json(
            &format!("{base}/search?query=Watchmen+%2399&series=Saga&seriesIndex=1"),
            None,
        )
        .await;
        assert_eq!(code, 200);
        let saw_no_year = fake.last_query();
        assert_eq!(
            saw_no_year.get("series_name").map(String::as_str),
            Some("Saga")
        );
        assert_eq!(saw_no_year.get("number").map(String::as_str), Some("1"));
        assert!(!saw_no_year.contains_key("series_year_began"));
        let matches = body["matches"].as_array().unwrap();
        assert_eq!(matches[0]["providerId"], "7997");
        // Detail for 7997 was already cached, so only the new list URL hit upstream (+1 hit).
        assert_eq!(fake.hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn search_by_upc_returns_verified_issue_and_ignores_isbn() {
        let (base, fake) = provider(None).await;

        // ISBN-13 and ISBN-10 values are ignored for lookup without calling Metron.
        let (code, body, _) = get_json(&format!("{base}/search?isbn=9781607066019"), None).await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );
        assert_eq!(fake.hits.load(Ordering::SeqCst), 0);

        // When both an ISBN-13 and a title query are given, the ISBN is ignored and search falls
        // through to the title query.
        let (code, body, _) = get_json(
            &format!("{base}/search?query=Saga+%231&isbn=978-1-60706-601-9"),
            None,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(body["matches"][0]["providerId"], "7997");
        assert_eq!(fake.hits.load(Ordering::SeqCst), 2);

        // 12+ digit non-ISBN-13 code is looked up via ?upc=.
        let (code, body, _) =
            get_json(&format!("{base}/search?isbn=70985301166800111"), None).await;
        assert_eq!(code, 200);
        let matches = body["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["providerId"], "7997");
        assert_eq!(matches[0]["authors"], json!(["Brian K. Vaughan"]));
    }

    #[tokio::test]
    async fn ignored_filter_safeguard_rejects_large_count_without_detail_calls() {
        let (base, fake) = provider(None).await;
        let (code, body, _) =
            get_json(&format!("{base}/search?isbn=70985300000000000"), None).await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );
        assert_eq!(
            fake.hits.load(Ordering::SeqCst),
            1,
            "count > 50 must abort before any detail calls"
        );
    }

    #[tokio::test]
    async fn lookup_by_id_and_rejects_non_numeric_without_upstream_call() {
        let (base, fake) = provider(None).await;

        for bad_id in ["abc", "..", "12a", "0", &"9".repeat(13)] {
            let (code, _, _) = get_json(&format!("{base}/book/{bad_id}"), None).await;
            assert_eq!(code, 404, "bad id {bad_id}");
        }
        assert_eq!(
            fake.hits.load(Ordering::SeqCst),
            0,
            "non-numeric ids must not call Metron"
        );

        let (code, body, _) = get_json(&format!("{base}/book/7997"), None).await;
        assert_eq!(code, 200);
        assert_eq!(body["providerId"], "7997");
        assert_eq!(body["title"], "Chapter One");
        assert!(body.get("subtitle").is_none());
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);

        // Second lookup of the same id is served from the in-memory cache.
        let (code2, _, _) = get_json(&format!("{base}/book/7997"), None).await;
        assert_eq!(code2, 200);
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);

        let (code_missing, _, _) = get_json(&format!("{base}/book/999999"), None).await;
        assert_eq!(code_missing, 404);
    }

    #[tokio::test]
    async fn unknown_filter_safety_drops_unrelated_upstream_results() {
        let (base, fake) = provider(None).await;

        // Query without an issue number returns empty without calling Metron.
        let (code, body, _) = get_json(&format!("{base}/search?query=Saga"), None).await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );
        assert_eq!(fake.hits.load(Ordering::SeqCst), 0);

        // Fake Metron ignores unknown filters and returns the 32 Saga #1 issues even for
        // "Watchmen #5" or an unrelated UPC; client-side verification drops all of them.
        let (code, body, _) = get_json(&format!("{base}/search?query=Watchmen+%231"), None).await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );

        let (code, body, _) = get_json(&format!("{base}/search?query=Saga+%2399"), None).await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );

        // Unrelated UPC whose list items have count <= 50 still verifies detail UPC and drops mismatches.
        let (code, body, _) =
            get_json(&format!("{base}/search?isbn=70985399999999999"), None).await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );
    }

    #[tokio::test]
    async fn rate_limit_429_blocks_uncached_requests_and_serves_cached_ones() {
        let (base, fake) = provider(None).await;

        // Prime the cache for /book/7997.
        let (prime_code, prime_body, _) = get_json(&format!("{base}/book/7997"), None).await;
        assert_eq!(prime_code, 200);
        assert_eq!(prime_body["providerId"], "7997");
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);

        // Trigger a 429 on another URL.
        fake.status.store(429, Ordering::SeqCst);
        let (code, _, headers) = get_json(&format!("{base}/search?query=Saga+%231"), None).await;
        assert_eq!(code, 429);
        let retry_after = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .expect("Retry-After header present");
        assert!(
            (30..=31).contains(&retry_after),
            "retry_after={retry_after}"
        );
        assert_eq!(fake.hits.load(Ordering::SeqCst), 2);

        // Even if upstream recovers, an uncached request is answered 429 immediately without
        // calling Metron while the backoff window is active.
        fake.status.store(0, Ordering::SeqCst);
        let (code_uncached, _, headers2) = get_json(&format!("{base}/book/1234"), None).await;
        assert_eq!(code_uncached, 429);
        assert!(headers2.get("retry-after").is_some());
        assert_eq!(
            fake.hits.load(Ordering::SeqCst),
            2,
            "blocked pacer must not hit upstream for uncached URL"
        );

        // The cached URL still answers 200 with no upstream hit during the backoff.
        let (code_cached, cached_body, _) = get_json(&format!("{base}/book/7997"), None).await;
        assert_eq!(code_cached, 200);
        assert_eq!(cached_body["providerId"], "7997");
        assert_eq!(
            fake.hits.load(Ordering::SeqCst),
            2,
            "cached URL must be served during backoff without hitting upstream"
        );
    }

    #[tokio::test]
    async fn upstream_401_403_and_5xx_map_to_502() {
        for upstream_code in [401, 403, 500, 503] {
            let (base, fake) = provider(None).await;
            fake.status.store(upstream_code, Ordering::SeqCst);
            let (code, _, _) = get_json(&format!("{base}/book/7997"), None).await;
            assert_eq!(code, 502, "upstream {upstream_code} must map to 502");
        }
    }

    #[tokio::test]
    async fn auth_token_guards_search_and_lookup() {
        let (base, _) = provider(Some("Bearer s3cret")).await;
        assert_eq!(
            get_json(&format!("{base}/search?query=Saga+%231"), None)
                .await
                .0,
            401
        );
        assert_eq!(
            get_json(
                &format!("{base}/search?query=Saga+%231"),
                Some("Bearer s3cret")
            )
            .await
            .0,
            200
        );
        assert_eq!(
            get_json(&format!("{base}/provider/info"), None).await.0,
            200
        );
    }
}
