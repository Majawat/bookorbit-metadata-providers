# Metron metadata provider

BookOrbit metadata for comic issues from [Metron](https://metron.cloud), an open, English-centric
comic book database. It runs beside BookOrbit as a small container.

Rust: axum, tokio, reqwest with rustls, serde and regex. The image is a static musl binary on
`scratch`; rustls carries its own root certificates, so there is nothing else in it.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  metron:
    image: ghcr.io/bookorbit/metron-provider:0.1.0
    restart: unless-stopped
    environment:
      METRON_API_KEY: your-metron-api-key
```

`METRON_API_KEY` is required (the provider refuses to start without it). Get an API key from your
[metron.cloud](https://metron.cloud) account page; each user should use their own key because
Metron's rate limits apply per account.

Then add `http://metron:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. To build it yourself: `docker build -t metron-provider .`, or with a Rust toolchain,
`cargo run --release`.

### BookOrbit settings

A single search makes up to 2–3 Metron API calls spaced `METRON_MIN_INTERVAL_MS` apart (default
3.5 s), so set the provider's **timeout to 30 seconds** in BookOrbit (**Settings > Metadata >
Providers > External > edit**).

ISBN lookups (ISBN-10 and ISBN-13) are not supported because Metron has no `isbn` filter on its
issue endpoint (it silently ignores `isbn=` and returns all issues). UPC lookups (12+ digit codes
that are not 13-digit `978`/`979` ISBNs, sent by BookOrbit in the `isbn` search parameter) are
supported via `GET /api/issue/?upc=`. When an ISBN is passed alongside a title `query`, the ISBN is
ignored for lookup and the search falls through to the title query.

| Variable | Default | |
|---|---|---|
| `METRON_API_KEY` | *(required)* | Bearer token from your metron.cloud account page |
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `METRON_BASE_URL` | `https://metron.cloud/api/` | Upstream API base URL |
| `METRON_MIN_INTERVAL_MS` | `3500` | Minimum gap between requests sent to Metron |
| `USER_AGENT` | `bookorbit-metron-provider/0.1.0` | `User-Agent` header sent to Metron |

## Rate limiting and caching

Metron enforces a burst limit of 20 requests per minute and 5,000 requests per day. A single
process-wide pacer ensures that requests to Metron start at least `METRON_MIN_INTERVAL_MS`
(3.5 s by default) apart and never overlap. Because a search may make 2–3 Metron calls
(e.g. issue list + top match detail), `/provider/info` advertises `minIntervalMs: 7000` and
`maxConcurrency: 1` to BookOrbit.

Successful Metron `GET` responses are cached in memory by full URL for 24 hours (up to 2,000
entries). If Metron returns `429 Too Many Requests`, the pacer backs off for `Retry-After` (+1 s,
or 60 s if the header is absent). While blocked, cached responses are still served, and any
uncached BookOrbit request is answered `429` with `Retry-After` immediately without calling Metron.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `metron`, media `comic`, lookup, `isbn`, `limit`, `series`, and `seriesIndex` search params, 7-second `rateLimit`, portrait covers |
| `GET /search?query=&series=&seriesIndex=&isbn=&limit=` | `limit` defaults to 5, at most 10 |
| `GET /book/{id}` | Numeric Metron issue id (`1`–`12` digits) |

Status codes: `400` no `query`, `isbn`, or `series`+`seriesIndex` params, `401` bad `AUTH_TOKEN`,
`404` unknown or non-numeric id, `429` with `Retry-After` when rate-limited, `502` when Metron fails
or rejects `METRON_API_KEY`.

## What it sends to Metron and how it maps

- **What is sent to Metron:** the series name, issue number, and optional series start year
  (`GET /api/issue/?series_name=<series>&number=<number>` plus `&series_year_began=<year>` when a
  4-digit year is known), a UPC when provided in the `isbn` parameter (`GET /api/issue/?upc=…`),
  and the numeric issue ID (`GET /api/issue/<id>/`) for the top search match, UPC verification, or
  `/book/{id}` lookup. Raw query strings are never logged.
- **`series` and `seriesIndex` parameters:** when BookOrbit sends both `series` and `seriesIndex`
  (non-empty), they take precedence over parsing `query`. A run year in `series` (e.g.
  `The Uncanny X-Men (1981)`) is extracted as `series_year_began` (`1981`) and any other trailing
  parenthetical or bracketed tags are stripped; `seriesIndex` (`1`, `001`, `1.5`, `-1`, `½`) is
  normalised as the issue number. If `query` is also present, it is used only as a tie-breaker when
  a fetched or cached issue detail's story title (`name[]`) equals `query` (no extra upstream calls
  are made).
- **Fallback `query` parsing:** when only `query` is present, the parser extracts series name,
  issue number, and optional start year from titles that include an issue number (`Gospel 001`,
  `Saga #1`, `Saga 001 (2012)`, `Saga Vol. 2 #3`, `Saga #1 - Chapter One`, scene-style tags like
  `Saga 001 (2012) (Digital) (Zone-Empire)`, and dot/underscore-separated filenames).
- **Ignored-filter safeguard and client-side verification:** Metron silently ignores unknown filter
  parameters and returns all issues. Any filtered list response with `count > 50` logs a warning
  (`[provider.metron] [warn] filter ignored by upstream`) and is treated as empty without making
  detail calls. Every remaining list or detail item is verified locally against the requested UPC,
  issue number (normalising leading zeros and `½` → `1/2`), and series name before being returned.
- **Ranking:** exact normalised series name match (case, punctuation, leading `The`, and `&`/`and`
  insensitive) ranks ahead of substring matches, followed by matching `year_began`, lower volume
  number, and earlier `cover_date`.
- **Detail vs. list mapping:** `/search` fetches the full issue detail only for the first ranked
  result (falling back to the list item if the detail call fails) and maps remaining results from
  the list response.
- **Fields:** like BookOrbit's built-in ComicVine provider, full-detail `title` is the first
  non-empty story title from `name[]`, falling back to `<series.name> #<number>` when `name[]` is
  empty; list-item mapping uses `<series.name> #<number>` (story titles are not in list items), and
  `subtitle` is not used. `series` is `<series.name> (<series.year_began>)` with `sequence`
  `<number>`, `authors` includes credits with writer roles (`Writer`, `Script`, `Story`, `Plot`,
  `Writer (Original Story)`), `description` has HTML tags and entities stripped while preserving
  paragraphs, `isbn` is populated from the issue's own `isbn` field when present, and `rating` is
  included only when `rating_count > 0`.

## Known gaps

- **Story-title-only books require `series` and `seriesIndex`:** Metron has no story-title filter on
  `/api/issue/`. When BookOrbit sends `series` and `seriesIndex` search parameters, books titled
  with their story title (such as `The Dead Have No Rights` in `The Uncanny X-Men (1981)`) are
  matched via `series_name`, `number`, and `series_year_began`. Until BookOrbit sends `series` and
  `seriesIndex`, only titles that contain an issue number (such as `Gospel 001` or `Saga #1`) can be
  matched from `query`, and story-title-only books cannot.
- **No ISBN lookups:** Metron has no `isbn` filter on `/api/issue/`, so ISBN-10 and ISBN-13 values
  cannot be looked up directly (UPC lookups via `GET /api/issue/?upc=` are supported).
- **Queries without an issue number return nothing:** if `series`+`seriesIndex` are not provided,
  `query` has no issue number, and no matching UPC is given, `/search` returns an empty `matches`
  list without guessing an issue.
- **Comic-specific credits and metadata:** BookOrbit's provider contract has no fields for
  pencillers/inkers/colorists/letterers/cover artists, characters, teams, universes, or story arcs,
  so only writer roles are mapped to `authors` and those extra fields are omitted.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The fixtures in `testdata/` are real Metron API responses captured on 2026-10-31. All tests run
offline against an in-process fake Metron server.
