use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::metron::{Credit, IssueDetail, IssueListItem};

/// BookOrbit metadata record for a comic issue.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub provider_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub media_type: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub published_date: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub language_code: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isbn: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<SeriesEntry>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_count: Option<u64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeriesEntry {
    pub series: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sequence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    pub series: String,
    pub number: String,
    pub year: Option<u16>,
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

static FILE_EXT: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)\.(?:cbz|cbr|cb7|cbt|pdf)$"));
static PAREN_OR_BRACKET: LazyLock<Regex> = LazyLock::new(|| regex(r"\(([^()]*)\)|\[([^\[\]]*)\]"));
static TRAILING_TAGS: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?:\s*(?:\([^()]*\)|\[[^\[\]]*\]))+$"));
static YEAR_ONLY: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?:19\d{2}|20\d{2})$"));
static VOLUME_MARKER: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)(?:^|\s)(?:vol(?:ume)?\.?\s*\d+|v\d+)(?:\s|$)"));
static HASH_ISSUE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^(?P<series>.+?)\s*#\s*(?P<number>-?\d+(?:\.\d+|/\d+)?|½)(?:\s*[-–—:]\s*.*|\s+.*)?$")
});
static BARE_ISSUE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^(?P<series>.+?)\s+(?P<number>-?\d+(?:\.\d+|/\d+)?|½)(?:\s+[-–—]\s+.*)?$")
});
static TRAILING_PUNCT: LazyLock<Regex> = LazyLock::new(|| regex(r"[\s,.:;_\-\x{2013}\x{2014}]+$"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| regex(r"\s+"));

static HTML_PARAGRAPH: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)<\s*/?\s*p(?:\s[^>]*)?>"));
static HTML_BREAK: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)<\s*br\s*/?\s*>|<\s*/\s*(?:div|li)\s*>"));
static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"<[^>]+>"));
static NUMERIC_ENTITY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"&#(?:([0-9]{1,7})|[xX]([0-9a-fA-F]{1,6}));"));

/// Extracts a lookup-eligible UPC from the `isbn` search parameter: 12+ ASCII digits after
/// stripping hyphens and whitespace, excluding 13-digit ISBN-13 codes (starting with `978` or `979`).
pub fn extract_upc(raw: &str) -> Option<String> {
    let cleaned = clean_code(raw);
    if cleaned.len() < 12 || !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if cleaned.len() == 13 && (cleaned.starts_with("978") || cleaned.starts_with("979")) {
        return None;
    }
    Some(cleaned)
}

pub fn clean_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .collect()
}

/// Normalises an issue number: strips leading zeros (keeping a lone `0`) and maps `½` to `1/2`.
pub fn normalize_number(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix('#').map_or(s, str::trim);
    if s == "½" {
        return "1/2".to_string();
    }
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_len == 0 {
        return s.to_lowercase();
    }
    let digits = &rest[..digit_len];
    let tail = &rest[digit_len..];
    let trimmed = digits.trim_start_matches('0');
    let norm_digits = if trimmed.is_empty() { "0" } else { trimmed };
    format!("{sign}{norm_digits}{}", tail.to_lowercase())
}

/// Parses explicit `series` and `seriesIndex` search parameters into `(series, number, optional year)`.
/// Strips trailing bracket/paren tags from `series` (extracting a 4-digit run year if present) and
/// normalises `seriesIndex`.
pub fn parse_series_and_index(series_raw: &str, index_raw: &str) -> Option<ParsedQuery> {
    let series_trimmed = series_raw.trim();
    let number = normalize_number(index_raw);
    if series_trimmed.is_empty() || number.is_empty() {
        return None;
    }

    let mut year: Option<u16> = None;
    let without_tags = if let Some(m) = TRAILING_TAGS.find(series_trimmed) {
        for caps in PAREN_OR_BRACKET.captures_iter(m.as_str()) {
            let inner = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map(|c| c.as_str().trim())
                .unwrap_or_default();
            if year.is_none() && YEAR_ONLY.is_match(inner) {
                year = inner.parse::<u16>().ok();
            }
        }
        &series_trimmed[..m.start()]
    } else {
        series_trimmed
    };

    let cleaned = TRAILING_PUNCT.replace_all(without_tags.trim(), "");
    let series = SPACES.replace_all(cleaned.trim(), " ").into_owned();
    if series.is_empty() {
        return None;
    }

    Some(ParsedQuery {
        series,
        number,
        year,
    })
}

/// Parses a comic query into `(series, number, optional year)`.
/// Returns `None` if no issue number is present.
pub fn parse_query(raw: &str) -> Option<ParsedQuery> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_ext = FILE_EXT.replace(trimmed, "");
    let separated = replace_separators(&without_ext);

    let mut year: Option<u16> = None;
    let without_groups = PAREN_OR_BRACKET
        .replace_all(&separated, |caps: &regex::Captures<'_>| {
            let inner = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            if year.is_none() && YEAR_ONLY.is_match(inner) {
                year = inner.parse::<u16>().ok();
            }
            " "
        })
        .into_owned();

    // Remove volume markers (`v1`, `Vol. 2`, `Volume 2`); loop in case two are adjacent.
    let mut cleaned = without_groups;
    while VOLUME_MARKER.is_match(&cleaned) {
        cleaned = VOLUME_MARKER.replace_all(&cleaned, " ").into_owned();
    }
    let cleaned = SPACES.replace_all(&cleaned, " ").trim().to_string();
    if cleaned.is_empty() {
        return None;
    }

    let (raw_series, raw_number) = if let Some(caps) = HASH_ISSUE.captures(&cleaned) {
        (caps["series"].to_string(), caps["number"].to_string())
    } else if let Some(caps) = BARE_ISSUE.captures(&cleaned) {
        (caps["series"].to_string(), caps["number"].to_string())
    } else {
        return None;
    };

    let series = TRAILING_PUNCT
        .replace_all(raw_series.trim(), "")
        .trim()
        .to_string();
    let number = normalize_number(&raw_number);
    if series.is_empty() || number.is_empty() {
        return None;
    }

    Some(ParsedQuery {
        series,
        number,
        year,
    })
}

/// Replaces underscores and word-separating dots with spaces while preserving decimal numbers
/// (`#1.5`, `2.0`) and abbreviations followed by whitespace (`Vol. `, `vs. `).
fn replace_separators(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '_' {
            out.push(' ');
            continue;
        }
        if ch == '.' {
            let prev_is_digit = i > 0 && chars[i - 1].is_ascii_digit();
            let next_is_space_or_end = i + 1 >= chars.len() || chars[i + 1].is_whitespace();
            let mut digit_run = 0;
            while i + 1 + digit_run < chars.len() && chars[i + 1 + digit_run].is_ascii_digit() {
                digit_run += 1;
            }
            let is_decimal = prev_is_digit && (1..=2).contains(&digit_run);
            if !next_is_space_or_end && !is_decimal {
                out.push(' ');
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// Normalises a series name for comparison: lowercase, `&` -> `and`, punctuation stripped,
/// and leading `"the "` removed.
pub fn normalize_series_name(name: &str) -> String {
    let expanded = name.to_lowercase().replace('&', " and ");
    let words: Vec<&str> = expanded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    joined
        .strip_prefix("the ")
        .map(str::to_string)
        .unwrap_or(joined)
}

/// Verifies and ranks Metron issue list results against a parsed query.
pub fn rank_issues(items: Vec<IssueListItem>, parsed: &ParsedQuery) -> Vec<IssueListItem> {
    let want_number = normalize_number(&parsed.number);
    let want_series = normalize_series_name(&parsed.series);
    let want_series_raw = parsed.series.to_lowercase();

    let mut verified: Vec<IssueListItem> = items
        .into_iter()
        .filter(|item| normalize_number(&item.number) == want_number)
        .filter(|item| {
            let norm = normalize_series_name(&item.series.name);
            (!want_series.is_empty() && norm.contains(&want_series))
                || item.series.name.to_lowercase().contains(&want_series_raw)
        })
        .collect();

    verified.sort_by(|a, b| {
        let a_exact = normalize_series_name(&a.series.name) == want_series;
        let b_exact = normalize_series_name(&b.series.name) == want_series;

        let a_year = parsed.year.is_some_and(|y| a.series.year_began == Some(y));
        let b_year = parsed.year.is_some_and(|y| b.series.year_began == Some(y));

        let a_vol = a.series.volume.unwrap_or(u32::MAX);
        let b_vol = b.series.volume.unwrap_or(u32::MAX);

        let a_date = a
            .cover_date
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or("~");
        let b_date = b
            .cover_date
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or("~");

        (!a_exact, !a_year, a_vol, a_date).cmp(&(!b_exact, !b_year, b_vol, b_date))
    });

    verified
}

pub fn story_title_matches(detail: &IssueDetail, query: &str) -> bool {
    let q = query.trim();
    !q.is_empty()
        && detail
            .name
            .iter()
            .map(|s| s.trim())
            .any(|s| !s.is_empty() && s.eq_ignore_ascii_case(q))
}

pub fn map_list_item(item: &IssueListItem) -> BookMetadata {
    let published_date = item.cover_date.clone().unwrap_or_default();
    let published_year = extract_year(&published_date);
    BookMetadata {
        provider_id: item.id.to_string(),
        media_type: "comic".into(),
        title: format_issue_title(&item.series.name, &item.number),
        series: build_series_entries(&item.series.name, item.series.year_began, &item.number),
        published_date,
        published_year,
        cover: item.image.clone().unwrap_or_default(),
        ..BookMetadata::default()
    }
}

pub fn map_detail(detail: &IssueDetail) -> BookMetadata {
    let published_date = detail.cover_date.clone().unwrap_or_default();
    let published_year = extract_year(&published_date);
    let title = detail
        .name
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format_issue_title(&detail.series.name, &detail.number));
    let (rating, rating_count) = match (detail.average_rating, detail.rating_count) {
        (Some(avg), Some(count)) if count > 0 => (Some(avg), Some(count)),
        _ => (None, None),
    };

    BookMetadata {
        provider_id: detail.id.to_string(),
        media_type: "comic".into(),
        title,
        authors: writer_credits(&detail.credits),
        description: detail
            .desc
            .as_deref()
            .map(clean_description)
            .unwrap_or_default(),
        publisher: detail
            .publisher
            .as_ref()
            .map(|p| p.name.trim().to_string())
            .unwrap_or_default(),
        published_year,
        published_date,
        language_code: detail
            .series
            .language
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        isbn: detail
            .isbn
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        pages: detail.page.filter(|&p| p > 0),
        genres: unique_strings(detail.series.genres.iter().map(|g| g.name.as_str())),
        series: build_series_entries(
            &detail.series.name,
            detail.series.year_began,
            &detail.number,
        ),
        cover: detail.image.clone().unwrap_or_default(),
        rating,
        rating_count,
        url: detail.resource_url.clone().unwrap_or_default(),
    }
}

fn format_issue_title(series_name: &str, number: &str) -> String {
    let series = series_name.trim();
    let num = number.trim();
    if num.is_empty() {
        series.to_string()
    } else {
        format!("{series} #{num}")
    }
}

fn build_series_entries(
    series_name: &str,
    year_began: Option<u16>,
    number: &str,
) -> Vec<SeriesEntry> {
    let name = series_name.trim();
    if name.is_empty() {
        return Vec::new();
    }
    let formatted = match year_began.filter(|&y| y > 0) {
        Some(year) => format!("{name} ({year})"),
        None => name.to_string(),
    };
    vec![SeriesEntry {
        series: formatted,
        sequence: number.trim().to_string(),
    }]
}

fn writer_credits(credits: &[Credit]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for credit in credits {
        let is_writer = credit.role.iter().any(|r| is_writer_role(r.name.trim()));
        if !is_writer {
            continue;
        }
        let name = credit.creator.trim();
        if !name.is_empty() && seen.insert(name.to_lowercase()) {
            out.push(name.to_string());
        }
    }
    out
}

fn is_writer_role(role: &str) -> bool {
    matches!(
        role.to_lowercase().as_str(),
        "writer" | "script" | "story" | "plot" | "writer (original story)"
    )
}

fn unique_strings<'a>(iter: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in iter {
        let trimmed = item.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_lowercase()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn extract_year(date: &str) -> String {
    let year = date.get(..4).unwrap_or_default();
    if year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()) {
        year.to_string()
    } else {
        String::new()
    }
}

/// Strips HTML tags and entities, collapses whitespace within paragraphs, and keeps paragraph breaks.
pub fn clean_description(raw: &str) -> String {
    let s = raw.replace("\r\n", "\n").replace('\r', "\n");
    let s = HTML_PARAGRAPH.replace_all(&s, "\n\n");
    let s = HTML_BREAK.replace_all(&s, "\n");
    let s = HTML_TAG.replace_all(&s, "");
    let s = decode_html_entities(&s);

    let mut paragraphs = Vec::new();
    let mut current_lines: Vec<String> = Vec::new();
    for line in s.split('\n') {
        let collapsed = SPACES.replace_all(line, " ").trim().to_string();
        if collapsed.is_empty() {
            if !current_lines.is_empty() {
                paragraphs.push(current_lines.join(" "));
                current_lines.clear();
            }
        } else {
            current_lines.push(collapsed);
        }
    }
    if !current_lines.is_empty() {
        paragraphs.push(current_lines.join(" "));
    }
    paragraphs.join("\n\n")
}

fn decode_html_entities(input: &str) -> String {
    let named = [
        ("&nbsp;", " "),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&ndash;", "–"),
        ("&mdash;", "—"),
        ("&lsquo;", "‘"),
        ("&rsquo;", "’"),
        ("&ldquo;", "“"),
        ("&rdquo;", "”"),
        ("&hellip;", "…"),
        ("&copy;", "©"),
        ("&reg;", "®"),
        ("&trade;", "™"),
    ];
    let mut s = input.to_string();
    for (entity, replacement) in named {
        if s.contains(entity) {
            s = s.replace(entity, replacement);
        }
    }
    if s.contains("&#") {
        s = NUMERIC_ENTITY
            .replace_all(&s, |caps: &regex::Captures<'_>| {
                let code = caps
                    .get(1)
                    .and_then(|m| m.as_str().parse::<u32>().ok())
                    .or_else(|| {
                        caps.get(2)
                            .and_then(|m| u32::from_str_radix(m.as_str(), 16).ok())
                    });
                code.and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            })
            .into_owned();
    }
    // Decode `&amp;` last so `&amp;lt;` becomes `&lt;`, not `<`.
    if s.contains("&amp;") {
        s = s.replace("&amp;", "&");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUE_SERIES_NAME: &str = include_str!("../testdata/issue_series_name.json");
    const ISSUE_DETAIL_SAGA1: &str = include_str!("../testdata/issue_detail_saga1.json");

    fn load_issue_list(json: &str) -> Vec<IssueListItem> {
        #[derive(serde::Deserialize)]
        struct Body {
            results: Vec<IssueListItem>,
        }
        serde_json::from_str::<Body>(json)
            .expect("fixture parses")
            .results
    }

    #[test]
    fn parses_every_query_shape() {
        type Expected = (&'static str, &'static str, Option<u16>);
        let cases: &[(&str, Option<Expected>)] = &[
            ("Saga #1", Some(("Saga", "1", None))),
            ("Saga #001", Some(("Saga", "1", None))),
            ("Saga 001", Some(("Saga", "1", None))),
            ("Saga 1", Some(("Saga", "1", None))),
            ("Saga (2012) #1", Some(("Saga", "1", Some(2012)))),
            ("Saga (2012) 001", Some(("Saga", "1", Some(2012)))),
            ("Saga v1 #1", Some(("Saga", "1", None))),
            ("Saga Vol. 2 #3", Some(("Saga", "3", None))),
            ("Saga #1 - Chapter One", Some(("Saga", "1", None))),
            ("Saga 001 (2012)", Some(("Saga", "1", Some(2012)))),
            ("Saga 001 (of 6) (2012)", Some(("Saga", "1", Some(2012)))),
            ("Saga #1.5", Some(("Saga", "1.5", None))),
            ("Saga #-1", Some(("Saga", "-1", None))),
            ("Saga #½", Some(("Saga", "1/2", None))),
            (
                "Saga 001 (2012) (Digital) (Zone-Empire)",
                Some(("Saga", "1", Some(2012))),
            ),
            (
                "Saga_001_(2012)_(Digital)_(Zone-Empire)",
                Some(("Saga", "1", Some(2012))),
            ),
            ("Saga.001.(2012)", Some(("Saga", "1", Some(2012)))),
            ("Saga #0", Some(("Saga", "0", None))),
            ("Saga #00", Some(("Saga", "0", None))),
            ("Saga", None),
            ("Saga (2012)", None),
            ("Saga Vol. 2", None),
        ];

        for &(query, expected) in cases {
            let expected = expected.map(|(series, number, year)| ParsedQuery {
                series: series.to_string(),
                number: number.to_string(),
                year,
            });
            assert_eq!(parse_query(query), expected, "query: {query}");
        }
    }

    #[test]
    fn ranks_saga_1_and_annihilation_saga_1_first() {
        let items = load_issue_list(ISSUE_SERIES_NAME);

        let saga_query = parse_query("Saga #1").expect("parses Saga #1");
        let ranked_saga = rank_issues(items.clone(), &saga_query);
        assert!(!ranked_saga.is_empty());
        assert_eq!(ranked_saga[0].id, 7997);

        let ann_query = parse_query("Annihilation Saga #1").expect("parses Annihilation Saga #1");
        let ranked_ann = rank_issues(items.clone(), &ann_query);
        assert!(!ranked_ann.is_empty());
        assert_eq!(ranked_ann[0].id, 131474);

        let unrelated = parse_query("Batman #1").expect("parses Batman #1");
        let ranked_batman = rank_issues(items, &unrelated);
        // Only the two Batman "... Saga" series contain "Batman"; non-Batman series are dropped.
        assert!(ranked_batman
            .iter()
            .all(|i| i.series.name.to_lowercase().contains("batman")));
    }

    #[test]
    fn parses_series_and_series_index() {
        type Expected = (&'static str, &'static str, Option<u16>);
        let cases: &[(&str, &str, Option<Expected>)] = &[
            (
                "The Uncanny X-Men (1981)",
                "141",
                Some(("The Uncanny X-Men", "141", Some(1981))),
            ),
            (
                "The Boys (2006)",
                "001",
                Some(("The Boys", "1", Some(2006))),
            ),
            ("Gospel (2022)", "001", Some(("Gospel", "1", Some(2022)))),
            (
                "The Flash (2016) (Digital) [Empire]",
                "1.5",
                Some(("The Flash", "1.5", Some(2016))),
            ),
            ("Saga", "1", Some(("Saga", "1", None))),
            ("Saga", "-1", Some(("Saga", "-1", None))),
            ("Saga", "½", Some(("Saga", "1/2", None))),
            ("Saga", "#001", Some(("Saga", "1", None))),
            ("Saga", "00", Some(("Saga", "0", None))),
            ("", "1", None),
            ("Saga", "", None),
            ("(2012)", "1", None),
        ];

        for &(series, index, expected) in cases {
            let expected = expected.map(|(s, n, year)| ParsedQuery {
                series: s.to_string(),
                number: n.to_string(),
                year,
            });
            assert_eq!(
                parse_series_and_index(series, index),
                expected,
                "series={series:?} index={index:?}"
            );
        }
    }

    #[test]
    fn maps_issue_detail_saga1() {
        let detail: IssueDetail =
            serde_json::from_str(ISSUE_DETAIL_SAGA1).expect("detail fixture parses");
        let book = map_detail(&detail);

        assert_eq!(book.provider_id, "7997");
        assert_eq!(book.media_type, "comic");
        assert_eq!(book.title, "Chapter One");
        assert_eq!(book.authors, ["Brian K. Vaughan"]);
        assert_eq!(
            book.series,
            [SeriesEntry {
                series: "Saga (2012)".into(),
                sequence: "1".into(),
            }]
        );
        assert_eq!(book.publisher, "Image Comics");
        assert_eq!(book.published_date, "2012-03-01");
        assert_eq!(book.published_year, "2012");
        assert_eq!(book.pages, Some(52));
        assert_eq!(book.genres, ["Science Fiction"]);
        assert_eq!(book.language_code, "en");
        assert_eq!(
            book.cover,
            "https://static.metron.cloud/media/issue/2019/12/10/saga-1.jpg"
        );
        assert_eq!(book.url, "https://metron.cloud/issue/saga-1/");
        assert!(book.isbn.is_empty());
        assert_eq!(book.rating, None);
        assert_eq!(book.rating_count, None);
        assert!(book
            .description
            .starts_with("THE LAST MAN writer BRIAN K. VAUGHAN"));
        let serialized = serde_json::to_value(&book).expect("serializes");
        assert!(serialized.get("subtitle").is_none());

        // When story titles (`name[]`) are empty, falls back to "<series.name> #<number>".
        let mut without_story_titles = detail.clone();
        without_story_titles.name = vec![String::new(), "   ".into()];
        let fallback_book = map_detail(&without_story_titles);
        assert_eq!(fallback_book.title, "Saga #1");
    }

    #[test]
    fn cleans_html_and_preserves_paragraphs() {
        let raw = "<p>First   paragraph &amp; <b>bold</b>&#33;</p>\r\n<p>Second<br>line &mdash; here.</p>";
        assert_eq!(
            clean_description(raw),
            "First paragraph & bold!\n\nSecond line — here."
        );
    }

    #[test]
    fn extracts_upc_and_ignores_isbn10_and_isbn13() {
        assert_eq!(
            extract_upc("70985301166800111"),
            Some("70985301166800111".into())
        );
        assert_eq!(
            extract_upc("709853-011668-00111"),
            Some("70985301166800111".into())
        );
        assert_eq!(extract_upc("709853011668"), Some("709853011668".into()));
        assert_eq!(extract_upc("7098530116680"), Some("7098530116680".into()));
        assert_eq!(extract_upc("9781607066019"), None);
        assert_eq!(extract_upc("978-1-60706-601-9"), None);
        assert_eq!(extract_upc("9791032700000"), None);
        assert_eq!(extract_upc("1607066017"), None);
        assert_eq!(extract_upc("030640615X"), None);
        assert_eq!(extract_upc("12345"), None);
    }
}
