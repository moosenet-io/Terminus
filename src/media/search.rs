//! Media domain search/status tools (MEDIA-02) — read-only, conversation-first.
//!
//! Two tools:
//! - `media_search(query)` — resolve a fuzzy natural-language title via TMDb
//!   ([`crate::media::clients::tmdb::TmdbClient`]) to candidate title(s)+IDs.
//!   Ambiguous matches (e.g. a remake and its original sharing a title) come
//!   back as ranked options with disambiguating detail, never a single wrong
//!   guess; no matches come back as a friendly "couldn't find that" message,
//!   never a hard error.
//! - `media_status(id_or_title)` — aggregate presence/availability/quality
//!   across Radarr, Sonarr, and Plex ([`crate::media::clients::{radarr,
//!   sonarr, plex}`]), degrading gracefully (per-service note, not a panic
//!   or a failed call) when any one of those services is unconfigured or
//!   unreachable.
//!
//! Both tools return **narration-shaped** JSON: a short natural-language
//! `summary` string a personality agent (Lumina) can say out loud, plus a
//! `structured` field with the underlying data for a caller that wants to
//! act on it programmatically. Neither tool mutates anything — no
//! confirmation gates live here (those are MEDIA-03/04).

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::instrument;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::clients::plex::PlexClient;
use super::clients::radarr::RadarrClient;
use super::clients::sonarr::SonarrClient;
use super::clients::tmdb::TmdbClient;

// ── media_search ─────────────────────────────────────────────────────────────

/// A single TMDb search result, ranked against the query.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    title: String,
    tmdb_id: i64,
    media_type: String,
    year: Option<String>,
    popularity: f64,
    score: f64,
}

/// Above this similarity score a candidate is considered a "strong" match
/// (not just a loose word-overlap hit).
const STRONG_MATCH: f64 = 0.75;
/// When the top two candidates' scores are both strong and within this delta
/// of each other, the query is ambiguous (e.g. a 1984 and a 2021 "Dune").
const CLOSE_DELTA: f64 = 0.15;
/// Candidates scoring below this are dropped entirely — not relevant enough
/// to surface, even as a weak option.
const RELEVANCE_FLOOR: f64 = 0.2;

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Pure string-similarity heuristic between a user query and a candidate
/// title: exact match (case-insensitive) scores 1.0; a substring match
/// scores in the 0.6-0.9 range weighted by how much of the longer string the
/// shorter one covers; otherwise falls back to Jaccard word overlap, capped
/// below the substring band so partial-word matches never outrank a real
/// substring hit. Deliberately dependency-free (no fuzzy-match crate) so the
/// ranking is easy to unit-test and reason about.
fn title_similarity(query: &str, title: &str) -> f64 {
    let q = normalize(query);
    let t = normalize(title);
    if q.is_empty() || t.is_empty() {
        return 0.0;
    }
    if q == t {
        return 1.0;
    }
    if t.contains(&q) || q.contains(&t) {
        let longer = q.len().max(t.len()) as f64;
        let shorter = q.len().min(t.len()) as f64;
        return 0.6 + 0.3 * (shorter / longer);
    }

    let qw: std::collections::HashSet<&str> = q.split_whitespace().collect();
    let tw: std::collections::HashSet<&str> = t.split_whitespace().collect();
    if qw.is_empty() || tw.is_empty() {
        return 0.0;
    }
    let overlap = qw.intersection(&tw).count() as f64;
    let union = qw.union(&tw).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        (overlap / union) * 0.55
    }
}

/// Extract + rank candidates from a raw TMDb `/search/multi` payload. Pure
/// function (no I/O) so it's unit-testable without a mock server. Filters
/// out `media_type: "person"` results (TMDb's combined search also returns
/// cast/crew) and anything below [`RELEVANCE_FLOOR`], then sorts descending
/// by score (popularity is a tie-breaker only, never overrides title match).
fn rank_candidates(results: &[Value], query: &str) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = results
        .iter()
        .filter_map(|r| {
            let media_type = r.get("media_type").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if media_type != "movie" && media_type != "tv" {
                return None;
            }
            let title = r
                .get("title")
                .and_then(|v| v.as_str())
                .or_else(|| r.get("name").and_then(|v| v.as_str()))?
                .to_string();
            let tmdb_id = r.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let year = r
                .get("release_date")
                .and_then(|v| v.as_str())
                .or_else(|| r.get("first_air_date").and_then(|v| v.as_str()))
                .filter(|s| s.len() >= 4)
                .map(|s| s[0..4].to_string());
            let popularity = r.get("popularity").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let score = title_similarity(query, &title);
            Some(Candidate { title, tmdb_id, media_type, year, popularity, score })
        })
        .filter(|c| c.score >= RELEVANCE_FLOOR)
        .collect();

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.popularity.partial_cmp(&a.popularity).unwrap_or(std::cmp::Ordering::Equal))
    });
    candidates
}

/// Whether the top candidates are close enough that guessing would likely be
/// wrong — the caller should ask, not pick. Requires the top TWO candidates
/// to both be strong matches within [`CLOSE_DELTA`] of each other; a single
/// dominant strong match, or a strong match well ahead of a weak second
/// place, is not ambiguous.
fn is_ambiguous(candidates: &[Candidate]) -> bool {
    if candidates.len() < 2 {
        return false;
    }
    let top = candidates[0].score;
    let second = candidates[1].score;
    top >= STRONG_MATCH && second >= STRONG_MATCH && (top - second) <= CLOSE_DELTA
}

/// Build the narration-shaped `media_search` response from ranked candidates.
/// Pure function, unit-tested directly.
fn build_search_response(query: &str, candidates: &[Candidate]) -> Value {
    let ambiguous = is_ambiguous(candidates);
    let top_n: Vec<&Candidate> = candidates.iter().take(5).collect();

    let summary = if candidates.is_empty() {
        format!(
            "I couldn't find anything matching \"{query}\". Try a more specific title, or add the year if you know it."
        )
    } else if ambiguous {
        let opts: Vec<String> = top_n
            .iter()
            .take(3)
            .map(|c| {
                let year = c.year.clone().unwrap_or_else(|| "year unknown".to_string());
                format!("{} ({}, {})", c.title, year, c.media_type)
            })
            .collect();
        format!(
            "I found a few close matches for \"{query}\": {}. Which one did you mean?",
            opts.join("; ")
        )
    } else {
        let top = top_n[0];
        let year = top.year.clone().unwrap_or_else(|| "year unknown".to_string());
        format!("Found \"{}\" ({}) — {}.", top.title, year, top.media_type)
    };

    json!({
        "summary": summary,
        "structured": {
            "query": query,
            "ambiguous": ambiguous,
            "candidates": top_n.iter().map(|c| json!({
                "title": c.title,
                "year": c.year,
                "tmdb_id": c.tmdb_id,
                "media_type": c.media_type,
                "score": (c.score * 1000.0).round() / 1000.0,
            })).collect::<Vec<_>>(),
        }
    })
}

pub struct MediaSearch {
    client: Option<TmdbClient>,
}

impl MediaSearch {
    fn require_client(&self) -> Result<&TmdbClient, ToolError> {
        self.client
            .as_ref()
            .ok_or_else(|| ToolError::NotConfigured("TMDB_API_KEY not set".into()))
    }
}

#[async_trait]
impl RustTool for MediaSearch {
    fn name(&self) -> &str {
        "media_search"
    }

    fn description(&self) -> &str {
        "Resolve a fuzzy natural-language movie/show title (e.g. \"that dark sci-fi thing with the AI\" or a partial title) to real TMDb candidates. Read-only -- does not request or download anything. Returns ranked candidates when the match is ambiguous (e.g. a remake sharing a title with the original) instead of guessing, and a friendly not-found message with refinement suggestions when nothing resolves." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language or partial movie/show title to resolve."
                }
            },
            "required": ["query"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "media_search"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args.get("query").and_then(|v| v.as_str()).map(str::trim).unwrap_or("");
        if query.is_empty() {
            return Err(ToolError::InvalidArgument("query must not be empty".into()));
        }

        let client = self.require_client()?;
        let raw = client.search_multi(query).await?;
        let results = raw.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let candidates = rank_candidates(&results, query);
        Ok(build_search_response(query, &candidates).to_string())
    }
}

// ── media_status ─────────────────────────────────────────────────────────────

/// The outcome of checking one backing service for a title. Deliberately
/// distinguishes "not configured" from "configured but errored" from
/// "configured, reachable, title absent" so `media_status` can report each
/// independently and never let one service's trouble mask another's answer.
enum ServiceCheck {
    NotConfigured,
    Error(String),
    NotFoundInLibrary,
    Found(Value),
}

fn service_to_json(check: &ServiceCheck) -> Value {
    match check {
        ServiceCheck::NotConfigured => json!({ "configured": false }),
        ServiceCheck::Error(msg) => json!({ "configured": true, "reachable": false, "error": msg }),
        ServiceCheck::NotFoundInLibrary => json!({ "configured": true, "reachable": true, "present": false }),
        ServiceCheck::Found(detail) => json!({ "configured": true, "reachable": true, "present": true, "detail": detail }),
    }
}

/// Find the item in an arr-style library array (movies or series) whose
/// title most closely matches `query`, above [`RELEVANCE_FLOOR`]. Pure
/// function, unit-tested directly.
fn find_by_title<'a>(items: &'a [Value], query: &str) -> Option<&'a Value> {
    items
        .iter()
        .filter_map(|item| {
            let title = item.get("title").and_then(|v| v.as_str()).or_else(|| item.get("name").and_then(|v| v.as_str()))?;
            let score = title_similarity(query, title);
            (score >= RELEVANCE_FLOOR).then_some((score, item))
        })
        .max_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, item)| item)
}

async fn check_radarr(client: &Option<RadarrClient>, query: &str) -> ServiceCheck {
    match client {
        None => ServiceCheck::NotConfigured,
        Some(c) => match c.library().await {
            Ok(v) => {
                let items = v.as_array().cloned().unwrap_or_default();
                match find_by_title(&items, query) {
                    Some(item) => ServiceCheck::Found(item.clone()),
                    None => ServiceCheck::NotFoundInLibrary,
                }
            }
            Err(e) => ServiceCheck::Error(e.to_string()),
        },
    }
}

async fn check_sonarr(client: &Option<SonarrClient>, query: &str) -> ServiceCheck {
    match client {
        None => ServiceCheck::NotConfigured,
        Some(c) => match c.library().await {
            Ok(v) => {
                let items = v.as_array().cloned().unwrap_or_default();
                match find_by_title(&items, query) {
                    Some(item) => ServiceCheck::Found(item.clone()),
                    None => ServiceCheck::NotFoundInLibrary,
                }
            }
            Err(e) => ServiceCheck::Error(e.to_string()),
        },
    }
}

/// Plex has no per-title lookup in the MEDIA-01 client, so presence here is
/// checked against recent watch history (`PlexClient::history`) -- a
/// reasonable "available to watch / already seen" proxy from the client
/// surface this item is scoped to reuse. A dedicated library-content lookup
/// can replace this in a later item without changing `media_status`'s shape.
async fn check_plex(client: &Option<PlexClient>, query: &str) -> ServiceCheck {
    match client {
        None => ServiceCheck::NotConfigured,
        Some(c) => match c.history().await {
            Ok(v) => {
                let items = v
                    .get("MediaContainer")
                    .and_then(|m| m.get("Metadata"))
                    .and_then(|m| m.as_array())
                    .cloned()
                    .or_else(|| v.as_array().cloned())
                    .unwrap_or_default();
                match find_by_title(&items, query) {
                    Some(item) => ServiceCheck::Found(item.clone()),
                    None => ServiceCheck::NotFoundInLibrary,
                }
            }
            Err(e) => ServiceCheck::Error(e.to_string()),
        },
    }
}

/// Build the narration-shaped `media_status` response. Pure function over
/// the three already-resolved [`ServiceCheck`]s, unit-tested directly.
fn build_status_response(query: &str, radarr: ServiceCheck, sonarr: ServiceCheck, plex: ServiceCheck) -> Value {
    let phrase = |name: &str, check: &ServiceCheck| -> String {
        match check {
            ServiceCheck::NotConfigured => format!("{name} isn't configured"),
            ServiceCheck::Error(_) => format!("{name} is unreachable right now"),
            ServiceCheck::NotFoundInLibrary => format!("not in {name}"),
            ServiceCheck::Found(detail) => {
                let quality = detail
                    .get("hasFile")
                    .and_then(|v| v.as_bool())
                    .map(|has_file| if has_file { "downloaded" } else { "monitored, not downloaded yet" })
                    .or_else(|| {
                        detail
                            .get("monitored")
                            .and_then(|v| v.as_bool())
                            .map(|m| if m { "monitored" } else { "known" })
                    })
                    .unwrap_or("present");
                format!("in {name} ({quality})")
            }
        }
    };

    let parts = vec![phrase("Radarr", &radarr), phrase("Sonarr", &sonarr), phrase("Plex", &plex)];
    let summary = format!("\"{query}\": {}.", parts.join("; "));

    json!({
        "summary": summary,
        "structured": {
            "query": query,
            "radarr": service_to_json(&radarr),
            "sonarr": service_to_json(&sonarr),
            "plex": service_to_json(&plex),
        }
    })
}

pub struct MediaStatus {
    radarr: Option<RadarrClient>,
    sonarr: Option<SonarrClient>,
    plex: Option<PlexClient>,
}

#[async_trait]
impl RustTool for MediaStatus {
    fn name(&self) -> &str {
        "media_status"
    }

    fn description(&self) -> &str {
        "Check whether a movie or show (by title, or the title returned from media_search) is already in the library -- present in Radarr/Sonarr, available to watch in Plex, and at what quality. Read-only -- does not request or download anything. Degrades gracefully per-service: an unconfigured or unreachable service is reported as such without failing the whole check." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id_or_title": {
                    "type": "string",
                    "description": "Title (or TMDb id as a string) to check status for."
                }
            },
            "required": ["id_or_title"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "media_status"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args.get("id_or_title").and_then(|v| v.as_str()).map(str::trim).unwrap_or("");
        if query.is_empty() {
            return Err(ToolError::InvalidArgument("id_or_title must not be empty".into()));
        }

        let radarr = check_radarr(&self.radarr, query).await;
        let sonarr = check_sonarr(&self.sonarr, query).await;
        let plex = check_plex(&self.plex, query).await;

        Ok(build_status_response(query, radarr, sonarr, plex).to_string())
    }
}

// ── registration ─────────────────────────────────────────────────────────────

/// Register the MEDIA-02 read/search tools. Each service client is built
/// independently from its own env config; a missing/unreachable one degrades
/// only that service (see [`MediaSearch::require_client`] and the
/// `ServiceCheck::NotConfigured` path in `media_status`), never the whole
/// domain.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(MediaSearch { client: TmdbClient::from_env().ok() }));
    registry.register_or_replace(Box::new(MediaStatus {
        radarr: RadarrClient::from_env().ok(),
        sonarr: SonarrClient::from_env().ok(),
        plex: PlexClient::from_env().ok(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn tmdb_client(base_url: &str) -> TmdbClient {
        TmdbClient::new(base_url, "testkey", reqwest::Client::new())
    }

    // ── pure-logic tests (no HTTP) ──────────────────────────────────────────

    #[test]
    fn title_similarity_exact_match_scores_one() {
        assert_eq!(title_similarity("Dune", "dune"), 1.0);
    }

    #[test]
    fn title_similarity_unrelated_scores_low() {
        assert!(title_similarity("Dune", "The Great British Bake Off") < RELEVANCE_FLOOR);
    }

    #[test]
    fn rank_candidates_filters_out_persons_and_irrelevant() {
        let results = json!([
            { "media_type": "person", "name": "Denis Villeneuve", "id": 1 },
            { "media_type": "movie", "title": "Dune", "id": 438631, "release_date": "2021-10-01", "popularity": 500.0 },
            { "media_type": "tv", "name": "The Great British Bake Off", "id": 99, "popularity": 10.0 },
        ]);
        let candidates = rank_candidates(results.as_array().unwrap(), "dune");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].title, "Dune");
    }

    #[test]
    fn is_ambiguous_true_for_two_close_strong_matches() {
        let candidates = vec![
            Candidate { title: "Dune".into(), tmdb_id: 1, media_type: "movie".into(), year: Some("2021".into()), popularity: 500.0, score: 1.0 },
            Candidate { title: "Dune".into(), tmdb_id: 2, media_type: "movie".into(), year: Some("1984".into()), popularity: 50.0, score: 1.0 },
        ];
        assert!(is_ambiguous(&candidates));
    }

    #[test]
    fn is_ambiguous_false_for_dominant_top_match() {
        let candidates = vec![
            Candidate { title: "Dune".into(), tmdb_id: 1, media_type: "movie".into(), year: Some("2021".into()), popularity: 500.0, score: 1.0 },
            Candidate { title: "Dune Messiah".into(), tmdb_id: 2, media_type: "movie".into(), year: None, popularity: 5.0, score: 0.3 },
        ];
        assert!(!is_ambiguous(&candidates));
    }

    #[test]
    fn is_ambiguous_false_for_single_candidate() {
        let candidates = vec![Candidate {
            title: "Dune".into(),
            tmdb_id: 1,
            media_type: "movie".into(),
            year: Some("2021".into()),
            popularity: 500.0,
            score: 1.0,
        }];
        assert!(!is_ambiguous(&candidates));
    }

    #[test]
    fn build_search_response_is_narration_shaped_on_no_results() {
        let out = build_search_response("nonexistent xyz", &[]);
        assert!(out.get("summary").is_some());
        assert!(out.get("structured").is_some());
        let summary = out["summary"].as_str().unwrap();
        assert!(summary.to_lowercase().contains("couldn't find"));
        assert_eq!(out["structured"]["candidates"].as_array().unwrap().len(), 0);
    }

    // ── media_search execute() tests (mocked TMDb) ──────────────────────────

    #[tokio::test]
    async fn media_search_exact_query_resolves_single_candidate() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search/multi").query_param("query", "dune");
            then.status(200).json_body(json!({
                "results": [
                    { "media_type": "movie", "title": "Dune", "id": 438631, "release_date": "2021-10-01", "popularity": 500.0 }
                ]
            }));
        });

        let tool = MediaSearch { client: Some(tmdb_client(&server.base_url())) };
        let result = tool.execute(json!({ "query": "dune" })).await.unwrap();
        mock.assert();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["summary"].as_str().unwrap().contains("Dune"));
        assert_eq!(parsed["structured"]["ambiguous"], false);
        assert_eq!(parsed["structured"]["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["structured"]["candidates"][0]["tmdb_id"], 438631);
    }

    #[tokio::test]
    async fn media_search_fuzzy_query_returns_ranked_candidates() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search/multi").query_param("query", "spy thriller");
            then.status(200).json_body(json!({
                "results": [
                    { "media_type": "movie", "title": "The Spy Who Loved Thrillers", "id": 1, "popularity": 20.0, "release_date": "2010-01-01" },
                    { "media_type": "movie", "title": "Gardening for Beginners", "id": 2, "popularity": 900.0, "release_date": "2015-01-01" },
                    { "media_type": "tv", "name": "Spy Thriller Nights", "id": 3, "popularity": 15.0, "first_air_date": "2019-01-01" }
                ]
            }));
        });

        let tool = MediaSearch { client: Some(tmdb_client(&server.base_url())) };
        let result = tool.execute(json!({ "query": "spy thriller" })).await.unwrap();
        mock.assert();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let candidates = parsed["structured"]["candidates"].as_array().unwrap();
        // The irrelevant high-popularity "Gardening for Beginners" must not
        // outrank a genuine (if partial) title match on popularity alone.
        assert!(candidates.iter().all(|c| c["title"] != "Gardening for Beginners"));
        assert_eq!(candidates[0]["title"], "Spy Thriller Nights");
    }

    #[tokio::test]
    async fn media_search_ambiguous_query_returns_options_not_a_guess() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search/multi").query_param("query", "dune");
            then.status(200).json_body(json!({
                "results": [
                    { "media_type": "movie", "title": "Dune", "id": 438631, "release_date": "2021-10-01", "popularity": 500.0 },
                    { "media_type": "movie", "title": "Dune", "id": 890, "release_date": "1984-12-14", "popularity": 60.0 }
                ]
            }));
        });

        let tool = MediaSearch { client: Some(tmdb_client(&server.base_url())) };
        let result = tool.execute(json!({ "query": "dune" })).await.unwrap();
        mock.assert();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["ambiguous"], true);
        let candidates = parsed["structured"]["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        // The summary must not commit to a single answer -- it should carry
        // disambiguating detail (both years) and pose a question.
        let summary = parsed["summary"].as_str().unwrap();
        assert!(summary.contains("2021"));
        assert!(summary.contains("1984"));
        assert!(summary.contains('?'));
    }

    #[tokio::test]
    async fn media_search_no_matches_returns_friendly_message_not_error() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search/multi").query_param("query", "asdkfjhaslkdjfh");
            then.status(200).json_body(json!({ "results": [] }));
        });

        let tool = MediaSearch { client: Some(tmdb_client(&server.base_url())) };
        let result = tool.execute(json!({ "query": "asdkfjhaslkdjfh" })).await;
        mock.assert();

        assert!(result.is_ok(), "no-match must be a friendly response, not an Err");
        let parsed: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(parsed["summary"].as_str().unwrap().to_lowercase().contains("couldn't find"));
        assert_eq!(parsed["structured"]["candidates"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn media_search_empty_query_is_invalid_argument() {
        let tool = MediaSearch { client: Some(tmdb_client("http://127.0.0.1:1")) };
        let result = tool.execute(json!({ "query": "   " })).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn media_search_not_configured_without_client() {
        let tool = MediaSearch { client: None };
        let result = tool.execute(json!({ "query": "dune" })).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    // ── media_status execute() tests (mocked Radarr/Sonarr/Plex) ────────────

    fn radarr_client(base_url: &str) -> RadarrClient {
        RadarrClient::new(base_url, "k", reqwest::Client::new())
    }
    fn sonarr_client(base_url: &str) -> SonarrClient {
        SonarrClient::new(base_url, "k", reqwest::Client::new())
    }
    fn plex_client(base_url: &str) -> PlexClient {
        PlexClient::new(base_url, "t", reqwest::Client::new())
    }

    #[tokio::test]
    async fn media_status_aggregates_all_three_services() {
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "title": "Dune", "hasFile": true }]));
        });
        let sonarr_server = MockServer::start();
        sonarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/series");
            then.status(200).json_body(json!([{ "title": "Foundation", "monitored": true }]));
        });
        let plex_server = MockServer::start();
        plex_server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(200).json_body(json!({
                "MediaContainer": { "Metadata": [{ "title": "Dune" }] }
            }));
        });

        let tool = MediaStatus {
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: Some(sonarr_client(&sonarr_server.base_url())),
            plex: Some(plex_client(&plex_server.base_url())),
        };
        let result = tool.execute(json!({ "id_or_title": "Dune" })).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert!(parsed.get("summary").is_some());
        assert!(parsed.get("structured").is_some());
        assert_eq!(parsed["structured"]["radarr"]["present"], true);
        assert_eq!(parsed["structured"]["radarr"]["detail"]["hasFile"], true);
        assert_eq!(parsed["structured"]["plex"]["present"], true);
        // "Foundation" in Sonarr's library must not register as a match for
        // the "Dune" query.
        assert_eq!(parsed["structured"]["sonarr"]["present"], false);
    }

    #[tokio::test]
    async fn media_status_degrades_when_a_service_errors() {
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "title": "Dune", "hasFile": true }]));
        });
        let plex_server = MockServer::start();
        plex_server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(500);
        });

        let tool = MediaStatus {
            radarr: Some(radarr_client(&radarr_server.base_url())),
            sonarr: None,
            plex: Some(plex_client(&plex_server.base_url())),
        };
        // Must not panic despite Plex erroring and Sonarr being unconfigured.
        let result = tool.execute(json!({ "id_or_title": "Dune" })).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["structured"]["radarr"]["present"], true);
        assert_eq!(parsed["structured"]["sonarr"]["configured"], false);
        assert_eq!(parsed["structured"]["plex"]["reachable"], false);
        assert!(parsed["structured"]["plex"]["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn media_status_all_unconfigured_still_returns_ok() {
        let tool = MediaStatus { radarr: None, sonarr: None, plex: None };
        let result = tool.execute(json!({ "id_or_title": "Dune" })).await;
        assert!(result.is_ok());
        let parsed: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["structured"]["radarr"]["configured"], false);
        assert_eq!(parsed["structured"]["sonarr"]["configured"], false);
        assert_eq!(parsed["structured"]["plex"]["configured"], false);
    }

    #[tokio::test]
    async fn media_status_empty_id_or_title_is_invalid_argument() {
        let tool = MediaStatus { radarr: None, sonarr: None, plex: None };
        let result = tool.execute(json!({ "id_or_title": "" })).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    fn tool_metadata_is_valid() {
        let search = MediaSearch { client: None };
        assert_eq!(search.name(), "media_search");
        assert!(!search.description().is_empty());
        assert_eq!(search.parameters()["type"], "object");

        let status = MediaStatus { radarr: None, sonarr: None, plex: None };
        assert_eq!(status.name(), "media_status");
        assert!(!status.description().is_empty());
        assert_eq!(status.parameters()["type"], "object");
    }
}
