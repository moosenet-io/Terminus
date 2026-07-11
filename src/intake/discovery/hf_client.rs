//! DISC-04: public HuggingFace Hub models-listing client.
//!
//! Terminus TERM #254. Replaces ASMT-08's manual leaderboard-browsing research
//! pass (`src/intake/assistant/docs/ASMT-08-discovery.md`) with a programmatic
//! signal feed: [`HfHubClient::list_models`] queries HF Hub's PUBLIC
//! models-listing API (`GET {HF_API_BASE_URL}/api/models`), filtered/sorted per
//! fleet category, and returns typed [`HfModelSummary`] rows for a later
//! DISC-05 (classification) and DISC-06 (daily refresh orchestration) to
//! consume. This module does NOT fetch/download a model — that is DISC-08's
//! job, over an entirely different (authenticated, write-adjacent) code path.
//!
//! ## Public listing vs. DISC-08's authenticated fetch — read this before
//! reusing anything here for DISC-08
//! HF Hub's models-listing endpoint is PUBLIC: no bearer token, no `HF_TOKEN`,
//! required or read anywhere in this module. `HF_TOKEN` (vault-only, per
//! DISC-07) is a DISC-08 concern for pulling gated/private repos onto cold
//! storage — a fundamentally different operation (an authenticated download,
//! not an anonymous catalog read). Do not add auth here "for consistency";
//! the two clients have deliberately different trust boundaries, and mixing
//! them would mean this read-only discovery pass suddenly needs a secret it
//! has no reason to hold.
//!
//! ## Typed-outcome convention (mirrors [`crate::intake::chord_pull::PullOutcome`])
//! Every non-2xx HTTP status and every transport-level failure (DNS,
//! connection refused, timeout) resolves to a distinct [`HfListError`]
//! variant, never a bare `String` and never a panic — matching the pattern
//! `chord_pull.rs`'s `PullOutcome` established for the exact same reason:
//! callers (DISC-06's daily refresh) need to match on *why* one category's
//! listing failed without parsing free text, and must be able to continue to
//! the next category rather than aborting the whole run.
//!
//! A single MALFORMED model record within an otherwise-successful response
//! (e.g. missing the `id` field) is a different failure class: it is skipped
//! with a `tracing::warn!`, not fatal to the call — see
//! [`parse_model_summary`].
//!
//! ## Category → HF filter mapping (data, not scattered `if`s)
//! [`CATEGORY_MAPPINGS`] is the single source of truth DISC-05/DISC-06 iterate
//! generically over all seven [`HfCategory`] values via [`HfCategory::all`].

use std::time::Duration;

use serde::Deserialize;

use crate::config;

/// One of the fleet's seven model-discovery target categories. Mirrors the
/// naming DISC-01's `FleetCategory` schema enum is expected to use
/// (`tool_router`/`writer_slm`/`assistant`/`coder`/`embedding`/`visual`/
/// `voice`) — kept as its own small enum here rather than depending on
/// DISC-01's `intake::discovery::schema` module, since that item may not have
/// landed yet in the tree this builds against (see the module root doc).
/// DISC-05/06 are expected to reconcile the two once both are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HfCategory {
    ToolRouter,
    WriterSlm,
    Assistant,
    Coder,
    Embedding,
    Visual,
    Voice,
}

impl HfCategory {
    /// Stable snake_case identifier, matching the convention
    /// `catalog.rs::CoverageStatus::as_str()` established.
    pub fn as_str(self) -> &'static str {
        match self {
            HfCategory::ToolRouter => "tool_router",
            HfCategory::WriterSlm => "writer_slm",
            HfCategory::Assistant => "assistant",
            HfCategory::Coder => "coder",
            HfCategory::Embedding => "embedding",
            HfCategory::Visual => "visual",
            HfCategory::Voice => "voice",
        }
    }

    /// All seven categories, in a stable order — what DISC-06's daily refresh
    /// iterates to cover the whole fleet catalog surface.
    pub fn all() -> [HfCategory; 7] {
        [
            HfCategory::ToolRouter,
            HfCategory::WriterSlm,
            HfCategory::Assistant,
            HfCategory::Coder,
            HfCategory::Embedding,
            HfCategory::Visual,
            HfCategory::Voice,
        ]
    }
}

/// One row of the category → HF-Hub-filter mapping table. `pipeline_tag` is
/// HF Hub's own `pipeline_tag` query filter; `search_terms` are additional
/// free-text hints folded into the `search=` query param (space-joined) to
/// narrow an otherwise-broad `pipeline_tag` (e.g. `text-generation` alone
/// covers far more than "small instruction-tuned router candidates").
#[derive(Debug, Clone, Copy)]
pub struct HfCategoryMapping {
    pub category: HfCategory,
    pub pipeline_tag: &'static str,
    pub search_terms: &'static [&'static str],
}

/// The category → HF-Hub-filter mapping table (DISC-04's ## APPROACH item 2):
/// a small const table, not scattered `if`/`match` branches spread through
/// the client, so DISC-05/DISC-06 can iterate categories generically via
/// [`HfCategory::all`] + [`category_mapping`].
pub const CATEGORY_MAPPINGS: &[HfCategoryMapping] = &[
    HfCategoryMapping {
        category: HfCategory::ToolRouter,
        pipeline_tag: "text-generation",
        search_terms: &["instruct", "router"],
    },
    HfCategoryMapping {
        category: HfCategory::WriterSlm,
        pipeline_tag: "text-generation",
        search_terms: &["instruct", "small"],
    },
    HfCategoryMapping {
        category: HfCategory::Assistant,
        pipeline_tag: "text-generation",
        search_terms: &["chat"],
    },
    HfCategoryMapping {
        category: HfCategory::Coder,
        pipeline_tag: "text-generation",
        search_terms: &["code"],
    },
    HfCategoryMapping {
        category: HfCategory::Embedding,
        pipeline_tag: "feature-extraction",
        search_terms: &[],
    },
    HfCategoryMapping {
        category: HfCategory::Visual,
        pipeline_tag: "image-text-to-text",
        search_terms: &[],
    },
    HfCategoryMapping {
        category: HfCategory::Voice,
        pipeline_tag: "automatic-speech-recognition",
        search_terms: &[],
    },
];

/// Look up the mapping row for `category`. `CATEGORY_MAPPINGS` always covers
/// every `HfCategory` variant (locked by a unit test below), so this never
/// returns `None` in practice — kept fallible rather than panicking so a
/// future incomplete edit to the table fails a test instead of a runtime
/// unwrap.
pub fn category_mapping(category: HfCategory) -> Option<&'static HfCategoryMapping> {
    CATEGORY_MAPPINGS.iter().find(|m| m.category == category)
}

/// A single candidate model summary from HF Hub's public listing API. Only
/// the fields DISC-05's classifier needs are kept; everything else in HF's
/// (much larger) response payload is discarded at parse time.
#[derive(Debug, Clone, PartialEq)]
pub struct HfModelSummary {
    /// HF repo id, e.g. `"Qwen/Qwen3-8B-Instruct"`.
    pub hf_repo: String,
    pub pipeline_tag: Option<String>,
    pub downloads: u64,
    pub likes: u64,
    pub trending_score: f64,
    pub tags: Vec<String>,
}

/// Typed failure outcome for one [`HfHubClient::list_models`] call. Never a
/// bare `String` — see the module doc's "typed-outcome convention" section.
#[derive(Debug, Clone, PartialEq)]
pub enum HfListError {
    /// A non-2xx HTTP response from HF Hub. `detail` is already genericized
    /// (no host/path beyond what HF itself returned).
    Failed { status: u16, detail: String },
    /// Transport-level failure: DNS, connection refused, or the request
    /// exceeded [`request_timeout`] — HF Hub was not reachable at all.
    Unreachable { detail: String },
}

impl std::fmt::Display for HfListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HfListError::Failed { status, detail } => {
                write!(f, "HF Hub listing failed (HTTP {status}): {detail}")
            }
            HfListError::Unreachable { detail } => {
                write!(f, "HF Hub unreachable: {detail}")
            }
        }
    }
}

/// Per-request wall-clock timeout. Bounds the "unreachable host must resolve
/// within a configured bound, never hang" edge case from DISC-04's TEST PLAN.
/// From `HF_DISCOVERY_TIMEOUT_SECS`, default 10 — generous for a JSON listing
/// call, far short of DISC-08's multi-GB fetch timeout (a different client
/// entirely).
fn request_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("HF_DISCOVERY_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(10),
    )
}

/// Result-per-page cap sent to HF Hub (the `limit` query param) and the
/// maximum number of pages [`HfHubClient::list_models`] will follow via the
/// `Link: rel="next"` header before stopping. Bounded so a malformed or
/// unexpectedly-endless pagination sequence can never loop forever (DISC-04's
/// EDGE CASES: "HF's pagination cursor is malformed/missing on a later page —
/// stop paginating cleanly rather than looping forever").
const PAGE_SIZE: u32 = 20;
const MAX_PAGES: u32 = 5;

/// A minimal client for HF Hub's public models-listing API
/// (`GET {base_url}/api/models`). Holds no credentials — see the module doc.
pub struct HfHubClient {
    client: reqwest::Client,
    base_url: String,
    rate_limit_per_min: u32,
    last_call: tokio::sync::Mutex<Option<std::time::Instant>>,
}

impl HfHubClient {
    /// Build a client from `config::hf_api_base_url()` /
    /// `config::hf_discovery_rate_limit_per_min()` — never a literal host.
    pub fn new() -> Self {
        Self::with_base_url(config::hf_api_base_url())
    }

    /// Build a client against an explicit `base_url` (used by tests to point
    /// at an `httpmock` server; production callers should use [`Self::new`]).
    pub fn with_base_url(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(request_timeout())
            .build()
            // A client-build failure here is a process-environment problem
            // (e.g. no TLS backend available), not a config or network issue
            // — reqwest::Client::builder() failing is exceptionally rare and
            // has no recoverable typed outcome to return from a constructor,
            // so this is the one place a genuine environment misconfiguration
            // surfaces loudly rather than being silently swallowed into a
            // per-call typed error.
            .expect("failed to build HTTP client for HfHubClient");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            rate_limit_per_min: config::hf_discovery_rate_limit_per_min(),
            last_call: tokio::sync::Mutex::new(None),
        }
    }

    /// Self-throttle to `rate_limit_per_min` requests/minute by sleeping, if
    /// needed, since the previous call this client instance made. A simple
    /// sleep-between-calls throttle (DISC-04's ## APPROACH explicitly allows
    /// "a simple token-bucket or sleep-between-calls ... the simplest correct
    /// implementation" absent an existing rate-limit convention elsewhere in
    /// this crate — none was found for outbound HTTP clients).
    async fn throttle(&self) {
        if self.rate_limit_per_min == 0 {
            return;
        }
        let min_interval = Duration::from_secs_f64(60.0 / self.rate_limit_per_min as f64);
        let mut last_call = self.last_call.lock().await;
        if let Some(prev) = *last_call {
            let elapsed = prev.elapsed();
            if elapsed < min_interval {
                tokio::time::sleep(min_interval - elapsed).await;
            }
        }
        *last_call = Some(std::time::Instant::now());
    }

    /// Query HF Hub for candidate models in `category`, per the
    /// [`CATEGORY_MAPPINGS`] filter row, sorted by trending score descending.
    /// Paginates via the `Link: rel="next"` header up to [`MAX_PAGES`] pages
    /// of [`PAGE_SIZE`] results each.
    ///
    /// - Zero results for a narrow filter is `Ok(vec![])`, not an error.
    /// - A malformed individual record (missing `id`) is skipped with a
    ///   warning, not fatal to the call.
    /// - Any non-2xx response or transport failure returns `Err(HfListError)`
    ///   — the caller (DISC-06) is expected to log it and continue to the
    ///   next category, never abort the whole refresh over one category.
    pub async fn list_models(&self, category: HfCategory) -> Result<Vec<HfModelSummary>, HfListError> {
        let mapping = category_mapping(category).unwrap_or(&CATEGORY_MAPPINGS[0]);
        let mut results = Vec::new();
        let mut url = self.first_page_url(mapping);

        for _page in 0..MAX_PAGES {
            self.throttle().await;

            let resp = match self.client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    return Err(HfListError::Unreachable {
                        detail: format!("request to HF Hub failed: {e}"),
                    })
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let body_text = resp.text().await.unwrap_or_default();
                return Err(HfListError::Failed {
                    status: status.as_u16(),
                    detail: if body_text.is_empty() {
                        format!("HTTP {status}")
                    } else {
                        // Bound how much of an unexpected error body we carry
                        // — this is a listing-API error path, not an audit
                        // log, but there is no reason to hold an unbounded
                        // string from an untrusted response either.
                        body_text.chars().take(300).collect()
                    },
                });
            }

            // Grab the next-page Link BEFORE consuming the body (reqwest
            // headers are available regardless of body consumption order,
            // but reading them first keeps the control flow linear).
            let next_link = extract_next_link(&resp);

            let raw: Vec<serde_json::Value> = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    return Err(HfListError::Failed {
                        status: status.as_u16(),
                        detail: format!("could not parse HF Hub response as a JSON array: {e}"),
                    })
                }
            };

            if raw.is_empty() {
                break;
            }

            for entry in &raw {
                match parse_model_summary(entry) {
                    Some(summary) => results.push(summary),
                    None => {
                        tracing::warn!(
                            category = category.as_str(),
                            "skipping malformed HF model record (missing expected field): {entry}"
                        );
                    }
                }
            }

            match next_link {
                // A malformed/unparseable `Link` header is treated identically
                // to "no next page" — stop paginating cleanly rather than
                // looping forever (DISC-04 EDGE CASES).
                Some(next) if is_plausible_url(&next) => url = next,
                _ => break,
            }
        }

        Ok(results)
    }

    fn first_page_url(&self, mapping: &HfCategoryMapping) -> String {
        let mut url = format!(
            "{}/api/models?pipeline_tag={}&sort=trendingScore&direction=-1&limit={}",
            self.base_url, mapping.pipeline_tag, PAGE_SIZE
        );
        if !mapping.search_terms.is_empty() {
            let search = mapping.search_terms.join(" ");
            url.push_str("&search=");
            url.push_str(&urlencode(&search));
        }
        url
    }
}

impl Default for HfHubClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal query-value percent-encoding for the handful of characters that
/// can appear in a `search_terms` join (spaces, primarily). Written inline
/// rather than pulling in a new crate dependency, matching
/// `chord_pull.rs::percent_encode_path_segment`'s stated rationale for doing
/// the same.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A local field-projection of HF Hub's per-model listing JSON shape. Only
/// used to drive `serde`'s deserialization inside [`parse_model_summary`];
/// never constructed or returned directly outside this module.
#[derive(Debug, Deserialize)]
struct RawHfModel {
    id: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    #[serde(rename = "pipeline_tag")]
    pipeline_tag: Option<String>,
    downloads: Option<u64>,
    likes: Option<u64>,
    #[serde(rename = "trendingScore")]
    trending_score: Option<f64>,
    #[serde(default)]
    tags: Vec<String>,
}

/// Parse one raw JSON model entry into an [`HfModelSummary`]. `None` when the
/// entry is missing the one field with no safe default — a repo identifier
/// (`id` or, on some HF API surfaces, `modelId`) — per DISC-04's EDGE CASES:
/// "a model summary missing an expected field ... skip that one summary ...
/// never abort the whole category's listing over one malformed record."
/// Every other field has a safe zero/default value rather than causing a skip
/// (downloads/likes/trending_score default to 0/0.0, pipeline_tag to `None`,
/// tags to empty) — those are legitimately-absent-sometimes fields on HF's
/// own API, not signals of a corrupt record.
fn parse_model_summary(raw: &serde_json::Value) -> Option<HfModelSummary> {
    let parsed: RawHfModel = serde_json::from_value(raw.clone()).ok()?;
    let hf_repo = parsed.id.or(parsed.model_id)?;
    if hf_repo.trim().is_empty() {
        return None;
    }
    Some(HfModelSummary {
        hf_repo,
        pipeline_tag: parsed.pipeline_tag,
        downloads: parsed.downloads.unwrap_or(0),
        likes: parsed.likes.unwrap_or(0),
        trending_score: parsed.trending_score.unwrap_or(0.0),
        tags: parsed.tags,
    })
}

/// Extract a `rel="next"` URL from the response's `Link` header, if present
/// and well-formed (GitHub-style `Link: <url>; rel="next", <url>; rel="last"`
/// pagination, which HF Hub's listing API also uses). Returns `None` for a
/// missing header, an unparseable header, or a header with no `rel="next"`
/// entry — all three collapse to the same "stop paginating" outcome in
/// [`HfHubClient::list_models`].
fn extract_next_link(resp: &reqwest::Response) -> Option<String> {
    let header = resp.headers().get(reqwest::header::LINK)?.to_str().ok()?;
    for part in header.split(',') {
        let part = part.trim();
        if !part.contains("rel=\"next\"") {
            continue;
        }
        let start = part.find('<')?;
        let end = part.find('>')?;
        if end > start {
            return Some(part[start + 1..end].to_string());
        }
    }
    None
}

/// Cheap sanity check on a candidate next-page URL before following it —
/// rejects an empty string or one missing a scheme, which is enough to catch
/// the "malformed cursor" edge case without pulling in a full URL-parsing
/// crate for this one call site.
fn is_plausible_url(url: &str) -> bool {
    !url.is_empty() && (url.starts_with("http://") || url.starts_with("https://"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    // ---- category mapping table --------------------------------------

    #[test]
    fn every_category_has_a_mapping_row() {
        for category in HfCategory::all() {
            let mapping = category_mapping(category);
            assert!(mapping.is_some(), "missing mapping for {:?}", category);
            assert_eq!(mapping.unwrap().category, category);
        }
    }

    #[test]
    fn category_as_str_is_snake_case_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for category in HfCategory::all() {
            assert!(seen.insert(category.as_str()), "duplicate as_str for {:?}", category);
            assert_eq!(category.as_str(), category.as_str().to_lowercase());
        }
    }

    // ---- parse_model_summary -------------------------------------------

    #[test]
    fn parse_model_summary_happy_path() {
        let raw = json!({
            "id": "Qwen/Qwen3-8B-Instruct",
            "pipeline_tag": "text-generation",
            "downloads": 12345,
            "likes": 678,
            "trendingScore": 9.5,
            "tags": ["instruct", "text-generation"]
        });
        let summary = parse_model_summary(&raw).expect("should parse");
        assert_eq!(summary.hf_repo, "Qwen/Qwen3-8B-Instruct");
        assert_eq!(summary.pipeline_tag.as_deref(), Some("text-generation"));
        assert_eq!(summary.downloads, 12345);
        assert_eq!(summary.likes, 678);
        assert_eq!(summary.trending_score, 9.5);
        assert_eq!(summary.tags, vec!["instruct", "text-generation"]);
    }

    #[test]
    fn parse_model_summary_falls_back_to_model_id_field() {
        let raw = json!({ "modelId": "org/model" });
        let summary = parse_model_summary(&raw).expect("should parse via modelId");
        assert_eq!(summary.hf_repo, "org/model");
    }

    #[test]
    fn parse_model_summary_missing_id_is_skipped() {
        let raw = json!({ "downloads": 5, "likes": 1 });
        assert!(parse_model_summary(&raw).is_none());
    }

    #[test]
    fn parse_model_summary_blank_id_is_skipped() {
        let raw = json!({ "id": "   " });
        assert!(parse_model_summary(&raw).is_none());
    }

    #[test]
    fn parse_model_summary_missing_optional_fields_default_cleanly() {
        let raw = json!({ "id": "org/bare-model" });
        let summary = parse_model_summary(&raw).expect("should parse");
        assert_eq!(summary.downloads, 0);
        assert_eq!(summary.likes, 0);
        assert_eq!(summary.trending_score, 0.0);
        assert!(summary.pipeline_tag.is_none());
        assert!(summary.tags.is_empty());
    }

    // ---- extract_next_link / is_plausible_url --------------------------

    #[test]
    fn is_plausible_url_rejects_empty_and_schemeless() {
        assert!(!is_plausible_url(""));
        assert!(!is_plausible_url("not-a-url"));
        assert!(is_plausible_url("https://huggingface.co/api/models?cursor=abc"));
    }

    // ---- list_models: happy path per category --------------------------

    #[tokio::test]
    async fn list_models_happy_path_for_every_category() {
        for category in HfCategory::all() {
            let server = MockServer::start();
            let mapping = category_mapping(category).unwrap();
            let mock = server.mock(|when, then| {
                when.method(GET)
                    .path("/api/models")
                    .query_param("pipeline_tag", mapping.pipeline_tag);
                then.status(200).json_body(json!([
                    { "id": "org/model-a", "downloads": 100, "likes": 10, "trendingScore": 1.0, "tags": [] },
                    { "id": "org/model-b", "downloads": 200, "likes": 20, "trendingScore": 2.0, "tags": [] },
                ]));
            });

            let client = HfHubClient::with_base_url(server.base_url());
            let result = client.list_models(category).await.expect("should succeed");
            mock.assert();
            assert_eq!(result.len(), 2, "category {:?}", category);
            assert_eq!(result[0].hf_repo, "org/model-a");
        }
    }

    #[tokio::test]
    async fn list_models_zero_results_is_clean_empty_list() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/models");
            then.status(200).json_body(json!([]));
        });

        let client = HfHubClient::with_base_url(server.base_url());
        let result = client.list_models(HfCategory::Coder).await.expect("empty is not an error");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn list_models_skips_malformed_record_but_keeps_the_rest() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/models");
            then.status(200).json_body(json!([
                { "id": "org/good-model", "downloads": 1, "likes": 1, "trendingScore": 1.0 },
                { "downloads": 999 },
                { "id": "" },
            ]));
        });

        let client = HfHubClient::with_base_url(server.base_url());
        let result = client.list_models(HfCategory::Assistant).await.expect("should succeed");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].hf_repo, "org/good-model");
    }

    // ---- list_models: negative tests -----------------------------------

    #[tokio::test]
    async fn list_models_non_2xx_is_typed_failure_not_panic() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/models");
            then.status(503).body("upstream overloaded");
        });

        let client = HfHubClient::with_base_url(server.base_url());
        let err = client.list_models(HfCategory::Visual).await.unwrap_err();
        match err {
            HfListError::Failed { status, detail } => {
                assert_eq!(status, 503);
                assert!(detail.contains("upstream overloaded"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_models_does_not_retry_indefinitely_on_failure() {
        // A single non-2xx response returns immediately (no internal retry
        // loop) — the mock only needs to be hit once for the call to
        // resolve, proving there is no unbounded retry.
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/models");
            then.status(500);
        });

        let client = HfHubClient::with_base_url(server.base_url());
        let _ = client.list_models(HfCategory::Embedding).await;
        mock.assert_hits(1);
    }

    #[tokio::test]
    async fn list_models_unreachable_host_times_out_never_hangs() {
        std::env::set_var("HF_DISCOVERY_TIMEOUT_SECS", "1");
        // A non-routable TEST-NET-1 address (RFC 5737) — guaranteed not to
        // accept a connection, so the client's own timeout (not the OS) is
        // what bounds this call.
        let client = HfHubClient::with_base_url("http://192.0.2.1:65535".to_string());
        let started = std::time::Instant::now();
        let err = client.list_models(HfCategory::ToolRouter).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "must not hang past the configured timeout");
        assert!(matches!(err, HfListError::Unreachable { .. }));
        std::env::remove_var("HF_DISCOVERY_TIMEOUT_SECS");
    }

    #[tokio::test]
    async fn list_models_malformed_response_body_is_typed_failure() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/models");
            then.status(200).body("not json at all");
        });

        let client = HfHubClient::with_base_url(server.base_url());
        let err = client.list_models(HfCategory::Coder).await.unwrap_err();
        assert!(matches!(err, HfListError::Failed { .. }));
    }

    // ---- pagination ------------------------------------------------------

    #[tokio::test]
    async fn list_models_stops_cleanly_when_no_next_link() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/models");
            then.status(200).json_body(json!([
                { "id": "org/only-page", "downloads": 1, "likes": 1, "trendingScore": 1.0 }
            ]));
            // No Link header at all.
        });

        let client = HfHubClient::with_base_url(server.base_url());
        let result = client.list_models(HfCategory::WriterSlm).await.expect("should succeed");
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn list_models_follows_a_well_formed_next_link_once() {
        let server = MockServer::start();
        let next_url = format!("{}/api/models?page=2", server.base_url());
        let page1 = server.mock(|when, then| {
            when.method(GET).path("/api/models").query_param_exists("pipeline_tag");
            then.status(200)
                .header("Link", format!("<{next_url}>; rel=\"next\""))
                .json_body(json!([
                    { "id": "org/page1-model", "downloads": 1, "likes": 1, "trendingScore": 1.0 }
                ]));
        });
        let page2 = server.mock(|when, then| {
            when.method(GET).path("/api/models").query_param("page", "2");
            then.status(200).json_body(json!([
                { "id": "org/page2-model", "downloads": 2, "likes": 2, "trendingScore": 2.0 }
            ]));
        });

        let client = HfHubClient::with_base_url(server.base_url());
        let result = client.list_models(HfCategory::Coder).await.expect("should succeed");
        // page1's mock matched the first request; page2's the followed one.
        assert!(page1.hits() >= 1);
        page2.assert();
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|m| m.hf_repo == "org/page1-model"));
        assert!(result.iter().any(|m| m.hf_repo == "org/page2-model"));
    }

    #[tokio::test]
    async fn list_models_malformed_next_link_stops_paginating_cleanly() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/models");
            then.status(200)
                .header("Link", "not-a-valid-link-header")
                .json_body(json!([
                    { "id": "org/only-model", "downloads": 1, "likes": 1, "trendingScore": 1.0 }
                ]));
        });

        let client = HfHubClient::with_base_url(server.base_url());
        let result = client.list_models(HfCategory::Voice).await.expect("should succeed");
        mock.assert_hits(1);
        assert_eq!(result.len(), 1);
    }

    // ---- rate limiting -----------------------------------------------------

    #[tokio::test]
    async fn throttle_sleeps_between_consecutive_calls_per_configured_rate() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/models");
            then.status(200).json_body(json!([]));
        });

        std::env::set_var("HF_DISCOVERY_RATE_LIMIT_PER_MIN", "600"); // 100ms min interval
        let client = HfHubClient::with_base_url(server.base_url());
        std::env::remove_var("HF_DISCOVERY_RATE_LIMIT_PER_MIN");

        let started = std::time::Instant::now();
        let _ = client.list_models(HfCategory::Coder).await;
        let _ = client.list_models(HfCategory::Coder).await;
        // Two calls at 600/min (100ms min interval) must take at least ~100ms
        // total — proves the throttle actually sleeps between calls rather
        // than firing back-to-back.
        assert!(started.elapsed() >= Duration::from_millis(90));
    }
}
