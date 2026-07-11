//! Media domain toggleable taste-memory module (MEDIA-06).
//!
//! Everything memory-shaped for the media domain lives in THIS file, never
//! in `crate::media::recommend` (MEDIA-05) -- that module's
//! `stateless_module_makes_no_memory_calls` test scans its own source for
//! memory-shaped identifiers and must keep passing unmodified. This module
//! is a pure ADD-ON: a feature flag, an assumed REST facade client, and a
//! decorator tool that wraps MEDIA-05's stateless `MediaRecommend` to
//! optionally enrich its output.
//!
//! ## Feature flag
//! `MEDIA_TASTE_MEMORY_ENABLED` (default OFF; three-state
//! `1/true/on/yes` -> on, `0/false/off/no`/unset/unrecognized -> off) gates
//! the ENTIRE module. OFF: `crate::media::register` registers MEDIA-05's
//! plain `MediaRecommend` and nothing else changes -- the flag-OFF path is
//! byte-identical to the pre-MEDIA-06 behavior, and this module makes zero
//! facade calls. ON: `register` below REPLACES the `media_recommend`
//! registration with [`TasteAwareMediaRecommend`], a decorator that calls
//! MEDIA-05's stateless tool for the base recommendation set and then
//! blends in taste signals; it also registers the optional write-back tool
//! `media_taste_feedback`.
//!
//! ## Engram / facade (SPEC-TO-REALITY correction #3)
//! There is no in-repo Engram client. Like `vitals`/`odyssey`, this module
//! assumes a REST facade behind `MEDIA_TASTE_API_URL` with the endpoints
//! documented on [`TasteMemoryClient`] below. **These endpoint paths and
//! payload shapes are this item's design, not verified against any live
//! service** -- human audit should confirm/replace them before this is
//! wired to production, exactly like the odyssey/vitals precedent.
//!
//! ## Never hard-depends on the facade
//! Flag ON but the facade is unset, unreachable, or errors: recommendations
//! degrade to MEDIA-05's stateless result with a logged note in the
//! response's `structured.taste_memory` field -- `execute` still returns
//! `Ok`. Write-back failures are logged, never surfaced as a tool error.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{instrument, warn};

use crate::error::ToolError;
use crate::gateway_framework::audit::{AuditEntry, AuditResult};
use crate::gateway_framework::ActionKind;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::recommend::MediaRecommend;

// ── feature flag (pure, unit-tested) ────────────────────────────────────────

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Pure token -> three-state mapping. `1/true/on/yes` -> `Some(true)`,
/// `0/false/off/no` -> `Some(false)`, anything else (including unset/blank)
/// -> `None`. Mirrors `crate::intake::code_v2::parse_three_state_bool`
/// (private to that module, so this module carries its own copy per the
/// blueprint's flag idiom -- see BLUEPRINT.md §7).
pub fn parse_three_state_bool(raw: Option<&str>) -> Option<bool> {
    match raw.map(|s| s.trim().to_lowercase()).as_deref() {
        Some("1" | "true" | "on" | "yes") => Some(true),
        Some("0" | "false" | "off" | "no") => Some(false),
        _ => None,
    }
}

/// Whether the taste-memory module is enabled. Default OFF on any unset or
/// unrecognized value -- an unrecognized value is treated as "not
/// explicitly turned on", never as an error.
pub fn media_taste_memory_enabled() -> bool {
    parse_three_state_bool(env_nonempty("MEDIA_TASTE_MEMORY_ENABLED").as_deref()).unwrap_or(false)
}

// ── assumed REST facade client ──────────────────────────────────────────────

/// Thin client for the assumed taste-memory facade behind
/// `MEDIA_TASTE_API_URL`. Wire shape NOT verified against a live service --
/// see the module doc above.
///
/// Assumed endpoints:
///   GET  {base}/media/taste/signals?account_id=      -- liked/disliked
///     genres+directors, free-text curation notes, recency-weighted
///   POST {base}/media/taste/engagement                -- record a single
///     engagement signal ({title, media_type, signal, account_id, note})
#[derive(Clone)]
pub struct TasteMemoryClient {
    base_url: String,
    http: reqwest::Client,
}

impl TasteMemoryClient {
    /// Build a client from `MEDIA_TASTE_API_URL`. Never panics; missing or
    /// empty config maps to `NotConfigured`, which callers treat the same
    /// as "facade absent" -- a cold-start/degrade condition, not an error.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("MEDIA_TASTE_API_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("MEDIA_TASTE_API_URL not set".into()))?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self { base_url, http })
    }

    pub fn new(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self { base_url: base_url.into().trim_end_matches('/').to_string(), http }
    }

    /// `GET /media/taste/signals` -- the user's stored taste/curation
    /// signals (liked/disliked genres+directors, curation notes).
    pub async fn get_signals(&self, account_id: Option<&str>) -> Result<Value, ToolError> {
        let url = format!("{}/media/taste/signals", self.base_url);
        let mut req = self.http.get(&url).header("Accept", "application/json");
        if let Some(id) = account_id {
            req = req.query(&[("account_id", id)]);
        }
        let resp = req.send().await.map_err(|e| ToolError::Http(format!("taste memory facade unavailable: {e}")))?;
        map_response(resp).await
    }

    /// `POST /media/taste/engagement` -- record a single engagement signal
    /// (requested/watched/dismissed). No PII beyond what the caller
    /// supplies in `title`/`account_id`/`note`; callers are responsible for
    /// not passing anything more sensitive than a media title.
    pub async fn record_engagement(&self, body: &Value) -> Result<(), ToolError> {
        let url = format!("{}/media/taste/engagement", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Accept", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("taste memory facade unavailable: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!(
                "taste memory facade rejected engagement write (HTTP {status}): {}",
                text.chars().take(200).collect::<String>()
            )));
        }
        Ok(())
    }
}

async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!("taste memory facade HTTP {status}: {}", body.chars().take(200).collect::<String>())));
    }
    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from taste memory facade: {e}")))
}

// ── blending (pure, testable) ───────────────────────────────────────────────

/// Extract a lowercase liked-genre set from the facade's assumed signals
/// payload (`{"liked_genres": [...], "disliked_genres": [...], "notes": [...]}`).
fn extract_string_list(signals: &Value, key: &str) -> Vec<String> {
    signals.get(key).and_then(|v| v.as_array()).map(|arr| arr.iter().filter_map(|v| v.as_str()).map(str::to_string).collect()).unwrap_or_default()
}

/// Blend taste-memory signals into a base `media_recommend` response
/// (already-JSON `Value`, as produced by `MediaRecommend::execute`). Pure
/// function, unit-tested directly without HTTP. Never fails -- a signals
/// payload with no recognizable fields is a no-op blend (cold start).
fn apply_taste_signals(mut base: Value, signals: &Value) -> Value {
    let liked = extract_string_list(signals, "liked_genres");
    let disliked = extract_string_list(signals, "disliked_genres");
    let notes = extract_string_list(signals, "notes");

    // Recency-weighted, not a hard flip: a genre both liked and disliked
    // (conflicting signals over time) is treated as liked-with-lower-
    // confidence rather than cancelled out, since the facade is assumed to
    // already return its own recency-collapsed view -- this blend only
    // avoids double-punishing a genre that appears in both lists.
    let liked_set: std::collections::HashSet<&str> = liked.iter().map(String::as_str).collect();
    let disliked_set: std::collections::HashSet<&str> =
        disliked.iter().map(String::as_str).filter(|g| !liked_set.contains(g)).collect();

    if let Some(recs) = base.get_mut("structured").and_then(|s| s.get_mut("recommendations")).and_then(|r| r.as_array_mut()) {
        for rec in recs.iter_mut() {
            let matched: Vec<String> = rec.get("matched_genres").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let taste_hit = matched.iter().any(|g| liked_set.contains(g.as_str()));
            let taste_miss = matched.iter().any(|g| disliked_set.contains(g.as_str()));
            if let Some(obj) = rec.as_object_mut() {
                if taste_hit {
                    if let Some(score) = obj.get("score").and_then(|v| v.as_f64()) {
                        obj.insert("score".into(), json!(score * 1.25));
                    }
                    if let Some(rationale) = obj.get("rationale").and_then(|v| v.as_str()).map(str::to_string) {
                        obj.insert("rationale".into(), json!(format!("{rationale} -- and you told me you're into that")));
                    }
                } else if taste_miss {
                    if let Some(score) = obj.get("score").and_then(|v| v.as_f64()) {
                        obj.insert("score".into(), json!(score * 0.5));
                    }
                }
            }
        }
        recs.sort_by(|a, b| {
            let sa = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sb = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    if let Some(summary) = base.get("summary").and_then(|v| v.as_str()).map(str::to_string) {
        if let Some(note) = notes.first() {
            base["summary"] = json!(format!("{summary} (you told me: \"{note}\")"));
        } else if !liked.is_empty() {
            base["summary"] = json!(format!("{summary} (you're into {})", liked.join("/")));
        }
    }

    if let Some(obj) = base.as_object_mut() {
        if let Some(structured) = obj.get_mut("structured").and_then(|s| s.as_object_mut()) {
            structured.insert(
                "taste_memory".into(),
                json!({ "enabled": true, "applied": true, "liked_genres": liked, "disliked_genres": disliked, "note": null }),
            );
        }
    }
    base
}

/// Mark a base response as taste-memory-enabled but degraded (facade
/// missing/unreachable/errored) -- the recommendations are returned
/// unmodified, with a logged note in `structured.taste_memory`.
fn mark_taste_degraded(mut base: Value, note: &str) -> Value {
    if let Some(structured) = base.get_mut("structured").and_then(|s| s.as_object_mut()) {
        structured.insert("taste_memory".into(), json!({ "enabled": true, "applied": false, "note": note }));
    }
    base
}

// ── media_recommend decorator ───────────────────────────────────────────────

/// Decorator over MEDIA-05's stateless `MediaRecommend`: computes the base
/// recommendation set exactly as MEDIA-05 would, then optionally blends in
/// taste-memory signals from the facade. Registered under the SAME tool
/// name (`media_recommend`) via `register_or_replace`, so it fully replaces
/// the stateless tool when the flag is on -- callers see one tool either
/// way.
pub struct TasteAwareMediaRecommend {
    inner: MediaRecommend,
    client: Option<TasteMemoryClient>,
}

#[async_trait]
impl RustTool for TasteAwareMediaRecommend {
    fn name(&self) -> &str {
        "media_recommend"
    }

    fn description(&self) -> &str {
        "Suggest movies/shows already in the library that haven't been watched yet, ranked by a taste profile built from recent Plex watch history AND (when configured) longer-term taste memory of liked/disliked genres and curation notes -- rationale reflects both, e.g. \"because you watched Dune (sci-fi) -- and you told me you're into that\". Degrades to the plain watch-history-only ranking if taste memory is unset or unreachable; never fails because of it." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    #[instrument(skip(self, args), fields(tool = "media_recommend", taste_memory = true))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let base_str = self.inner.execute(args.clone()).await?;
        let base: Value = serde_json::from_str(&base_str).map_err(|e| ToolError::Execution(format!("internal: base recommendation was not valid JSON: {e}")))?;

        let Some(client) = &self.client else {
            return Ok(mark_taste_degraded(base, "taste memory not configured (MEDIA_TASTE_API_URL unset)").to_string());
        };

        let account_id = args.get("account_id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
        match client.get_signals(account_id).await {
            Ok(signals) => Ok(apply_taste_signals(base, &signals).to_string()),
            Err(e) => {
                warn!("media taste-memory facade unreachable, degrading to stateless recommendations: {e}");
                Ok(mark_taste_degraded(base, "taste memory unreachable, showing watch-history-only recommendations").to_string())
            }
        }
    }
}

// ── media_taste_feedback (optional write-back, flag-gated at registration) ──

const VALID_SIGNALS: [&str; 3] = ["requested", "watched", "dismissed"];

/// Optional write-back tool: capture an engagement signal (requested/
/// watched/dismissed) into taste memory so curation improves over time.
/// Only ever registered when the flag is on (see [`register`]) -- its mere
/// presence in the registry is what MEDIA-06's "write-back only when flag
/// on" guarantee rests on.
pub struct MediaTasteFeedback {
    client: Option<TasteMemoryClient>,
}

#[async_trait]
impl RustTool for MediaTasteFeedback {
    fn name(&self) -> &str {
        "media_taste_feedback"
    }

    fn description(&self) -> &str {
        "Record a taste-memory engagement signal for a title -- requested, watched, or dismissed -- so future media_recommend calls learn from it. Only available when taste memory is enabled; a no-op (NotConfigured) if the taste memory facade isn't set up." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Title of the movie/show the signal is about." },
                "media_type": { "type": "string", "enum": ["movie", "tv"], "description": "Movie or TV series. Optional, defaults to \"movie\"." },
                "signal": { "type": "string", "enum": ["requested", "watched", "dismissed"], "description": "The engagement signal to record." },
                "account_id": { "type": "string", "description": "Optional Plex account/user id, for multi-user servers." },
                "note": { "type": "string", "description": "Optional free-text curation note, e.g. \"loved the slow pacing\"." }
            },
            "required": ["title", "signal"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "media_taste_feedback"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let title = args.get("title").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| ToolError::InvalidArgument("title is required".into()))?;
        let media_type = args.get("media_type").and_then(|v| v.as_str()).unwrap_or("movie");
        let signal = args.get("signal").and_then(|v| v.as_str()).map(str::trim).ok_or_else(|| ToolError::InvalidArgument("signal is required".into()))?;
        if !VALID_SIGNALS.contains(&signal) {
            return Err(ToolError::InvalidArgument(format!("signal must be one of {VALID_SIGNALS:?}")));
        }
        let account_id = args.get("account_id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
        let note = args.get("note").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());

        let Some(client) = &self.client else {
            return Err(ToolError::NotConfigured("MEDIA_TASTE_API_URL is not set -- taste memory write-back is unavailable".into()));
        };

        let body = json!({
            "title": title,
            "media_type": media_type,
            "signal": signal,
            "account_id": account_id,
            "note": note,
        });

        match client.record_engagement(&body).await {
            Ok(()) => {
                let detail = format!("media_taste_feedback recorded: media_type={media_type} signal={signal}");
                AuditEntry::new("media", "media_taste_feedback", ActionKind::Tool, AuditResult::Success, Some(&detail)).log();
                Ok(json!({ "summary": "Got it, noted.", "structured": { "recorded": true } }).to_string())
            }
            Err(e) => {
                // A failed write-back is logged, not surfaced as a tool
                // error -- recording a signal is never in the critical path
                // of the conversation it's attached to.
                warn!("media taste-memory write-back failed, not surfaced to caller: {e}");
                let detail = format!("media_taste_feedback failed: media_type={media_type} signal={signal} error={e}");
                AuditEntry::new("media", "media_taste_feedback", ActionKind::Tool, AuditResult::Failure, Some(&detail)).log();
                Ok(json!({ "summary": "Noted, though I couldn't save it to long-term memory right now.", "structured": { "recorded": false } }).to_string())
            }
        }
    }
}

// ── registration ─────────────────────────────────────────────────────────────

/// Flag-gated registration. OFF: no-op -- `crate::media::recommend::register`
/// (called just before this in `crate::media::register`) already installed
/// the plain stateless `media_recommend`, and this function makes zero
/// facade calls and touches nothing. ON: replaces `media_recommend` with
/// [`TasteAwareMediaRecommend`] and additionally registers
/// `media_taste_feedback`.
pub fn register(registry: &mut ToolRegistry) {
    if !media_taste_memory_enabled() {
        return;
    }
    let client = TasteMemoryClient::from_env().ok();
    registry.register_or_replace(Box::new(TasteAwareMediaRecommend { inner: MediaRecommend::from_env(), client: client.clone() }));
    registry.register_or_replace(Box::new(MediaTasteFeedback { client }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    fn clear_flag_env() {
        std::env::remove_var("MEDIA_TASTE_MEMORY_ENABLED");
        std::env::remove_var("MEDIA_TASTE_API_URL");
    }

    // ── flag parser (pure) ──────────────────────────────────────────────────

    #[test]
    fn flag_parser_recognizes_true_tokens() {
        for tok in ["1", "true", "TRUE", "on", "On", "yes"] {
            assert_eq!(parse_three_state_bool(Some(tok)), Some(true), "{tok} should parse true");
        }
    }

    #[test]
    fn flag_parser_recognizes_false_tokens() {
        for tok in ["0", "false", "FALSE", "off", "no"] {
            assert_eq!(parse_three_state_bool(Some(tok)), Some(false), "{tok} should parse false");
        }
    }

    #[test]
    fn flag_parser_unset_and_garbage_are_none() {
        assert_eq!(parse_three_state_bool(None), None);
        assert_eq!(parse_three_state_bool(Some("")), None);
        assert_eq!(parse_three_state_bool(Some("maybe")), None);
        assert_eq!(parse_three_state_bool(Some("2")), None);
    }

    #[test]
    #[serial]
    fn media_taste_memory_enabled_defaults_off() {
        clear_flag_env();
        assert!(!media_taste_memory_enabled());
        std::env::set_var("MEDIA_TASTE_MEMORY_ENABLED", "garbage");
        assert!(!media_taste_memory_enabled());
        clear_flag_env();
    }

    #[test]
    #[serial]
    fn media_taste_memory_enabled_reads_true_tokens() {
        clear_flag_env();
        std::env::set_var("MEDIA_TASTE_MEMORY_ENABLED", "true");
        assert!(media_taste_memory_enabled());
        clear_flag_env();
    }

    // ── registration: flag OFF ──────────────────────────────────────────────

    #[test]
    #[serial]
    fn flag_off_register_does_not_replace_or_add_tools() {
        clear_flag_env();
        let mut reg = ToolRegistry::new();
        super::super::recommend::register(&mut reg);
        assert!(reg.contains("media_recommend"));
        assert!(!reg.contains("media_taste_feedback"));
        register(&mut reg);
        assert!(reg.contains("media_recommend"));
        assert!(!reg.contains("media_taste_feedback"), "write-back tool must not be registered when the flag is off");
    }

    #[tokio::test]
    #[serial]
    async fn flag_off_media_recommend_makes_no_facade_calls() {
        clear_flag_env();
        let taste_server = MockServer::start();
        let mock = taste_server.mock(|when, then| {
            when.method(GET).path("/media/taste/signals");
            then.status(200).json_body(json!({ "liked_genres": ["Science Fiction"] }));
        });
        // Flag OFF even though MEDIA_TASTE_API_URL happens to be set --
        // register() must short-circuit before ever building a client.
        std::env::set_var("MEDIA_TASTE_API_URL", taste_server.base_url());

        let mut reg = ToolRegistry::new();
        super::super::recommend::register(&mut reg);
        register(&mut reg); // no-op: flag is off

        let tool = MediaRecommend::from_env();
        let result = tool.execute(json!({})).await;
        assert!(result.is_ok());
        assert_eq!(mock.hits(), 0, "flag OFF must never call the taste memory facade");
        clear_flag_env();
    }

    // ── registration: flag ON ───────────────────────────────────────────────

    #[test]
    #[serial]
    fn flag_on_register_replaces_recommend_and_adds_feedback_tool() {
        clear_flag_env();
        std::env::set_var("MEDIA_TASTE_MEMORY_ENABLED", "on");
        let mut reg = ToolRegistry::new();
        super::super::recommend::register(&mut reg);
        register(&mut reg);
        assert!(reg.contains("media_recommend"));
        assert!(reg.contains("media_taste_feedback"), "write-back tool must be registered when the flag is on");
        clear_flag_env();
    }

    // ── decorator: enrichment ────────────────────────────────────────────────

    #[tokio::test]
    async fn flag_on_recommendations_incorporate_taste_memory_in_rationale() {
        let taste_server = MockServer::start();
        let mock = taste_server.mock(|when, then| {
            when.method(GET).path("/media/taste/signals");
            then.status(200).json_body(json!({ "liked_genres": ["Science Fiction"], "notes": ["into slow-burn sci-fi"] }));
        });

        let base = json!({
            "summary": "You might like \"Dune\" -- because you watched Arrival (Science Fiction).",
            "structured": {
                "thin_signal": false,
                "degraded": null,
                "recommendations": [
                    { "title": "Dune", "media_type": "movie", "score": 1.0, "matched_genres": ["Science Fiction"], "rationale": "because you watched Arrival (Science Fiction)" }
                ]
            }
        });
        let signals = taste_server_get_signals(&taste_server).await;
        mock.assert();
        let enriched = apply_taste_signals(base, &signals);

        assert!(enriched["structured"]["taste_memory"]["applied"].as_bool().unwrap());
        assert!(enriched["summary"].as_str().unwrap().contains("slow-burn sci-fi"));
        assert!(enriched["structured"]["recommendations"][0]["rationale"].as_str().unwrap().contains("you told me"));
    }

    async fn taste_server_get_signals(server: &MockServer) -> Value {
        let client = TasteMemoryClient::new(server.base_url(), reqwest::Client::new());
        client.get_signals(None).await.unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn decorator_end_to_end_blends_taste_into_media_recommend() {
        let plex_server = MockServer::start();
        plex_server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(200).json_body(json!({
                "MediaContainer": { "Metadata": [
                    { "title": "Arrival", "Genre": [{"tag": "Science Fiction"}], "viewedAt": 1000 }
                ] }
            }));
        });
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([
                { "title": "Dune", "genres": ["Science Fiction"] },
                { "title": "Cooking Show", "genres": ["Food"] }
            ]));
        });
        let taste_server = MockServer::start();
        let taste_mock = taste_server.mock(|when, then| {
            when.method(GET).path("/media/taste/signals");
            then.status(200).json_body(json!({ "liked_genres": ["Science Fiction"] }));
        });

        let inner = build_test_media_recommend(&plex_server, &radarr_server);
        let decorator = TasteAwareMediaRecommend { inner, client: Some(TasteMemoryClient::new(taste_server.base_url(), reqwest::Client::new())) };
        let result = decorator.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        taste_mock.assert();
        assert!(parsed["structured"]["taste_memory"]["applied"].as_bool().unwrap());
        assert_eq!(parsed["structured"]["recommendations"][0]["title"], "Dune");
    }

    fn build_test_media_recommend(plex_server: &MockServer, radarr_server: &MockServer) -> MediaRecommend {
        // MediaRecommend's fields are private to `recommend`; the test-only
        // constructors it exposes for its own tests aren't `pub`, so this
        // decorator test goes through `from_env()` plus env vars instead.
        std::env::set_var("PLEX_URL", plex_server.base_url());
        std::env::set_var("PLEX_TOKEN", "t");
        std::env::set_var("RADARR_URL", radarr_server.base_url());
        std::env::set_var("RADARR_API_KEY", "k");
        std::env::remove_var("SONARR_URL");
        std::env::remove_var("SONARR_API_KEY");
        let tool = MediaRecommend::from_env();
        std::env::remove_var("PLEX_URL");
        std::env::remove_var("PLEX_TOKEN");
        std::env::remove_var("RADARR_URL");
        std::env::remove_var("RADARR_API_KEY");
        tool
    }

    // ── decorator: graceful degrade ─────────────────────────────────────────

    #[tokio::test]
    async fn decorator_no_client_degrades_without_facade_call() {
        let decorator = TasteAwareMediaRecommend { inner: MediaRecommend::from_env(), client: None };
        let result = decorator.execute(json!({})).await;
        assert!(result.is_ok());
        let parsed: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["structured"]["taste_memory"]["applied"], false);
        assert!(parsed["structured"]["taste_memory"]["note"].as_str().unwrap().contains("not configured"));
    }

    #[tokio::test]
    async fn decorator_facade_unreachable_degrades_to_stateless_not_error() {
        let taste_server = MockServer::start();
        taste_server.mock(|when, then| {
            when.method(GET).path("/media/taste/signals");
            then.status(500);
        });
        let decorator = TasteAwareMediaRecommend {
            inner: MediaRecommend::from_env(),
            client: Some(TasteMemoryClient::new(taste_server.base_url(), reqwest::Client::new())),
        };
        let result = decorator.execute(json!({})).await;
        assert!(result.is_ok(), "a facade error must degrade, never fail the tool");
        let parsed: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["structured"]["taste_memory"]["applied"], false);
        assert!(parsed["structured"]["taste_memory"]["note"].as_str().unwrap().to_lowercase().contains("unreachable"));
    }

    // Cold start: facade returns an empty signal set. Taste memory is active
    // (applied=true) but no signal moves the ranking -- behaves ~stateless and
    // starts learning. Regression guard for the empty-payload path.
    #[test]
    fn cold_start_empty_signals_applies_without_changing_ranking() {
        let base = json!({
            "summary": "You might like \"Dune\".",
            "structured": { "recommendations": [
                { "title": "Dune", "media_type": "movie", "score": 1.0, "matched_genres": ["Science Fiction"], "rationale": "because you watched Arrival" }
            ] }
        });
        let before_score = base["structured"]["recommendations"][0]["score"].clone();
        let enriched = apply_taste_signals(base, &json!({}));
        assert_eq!(enriched["structured"]["taste_memory"]["applied"], true, "module active even on cold start");
        assert_eq!(enriched["structured"]["recommendations"][0]["score"], before_score, "no signal must not move the ranking");
        assert_eq!(enriched["structured"]["recommendations"][0]["title"], "Dune");
    }

    // Conflicting signals: the same genre both liked AND disliked must resolve
    // to liked (the disliked set excludes anything also liked), i.e. recency/
    // like wins -- never a hard flip to the pure-dislike penalty.
    #[test]
    fn conflicting_like_and_dislike_same_genre_favors_like_not_hard_flip() {
        let base = json!({
            "summary": "s",
            "structured": { "recommendations": [
                { "title": "Dune", "media_type": "movie", "score": 1.0, "matched_genres": ["Science Fiction"], "rationale": "r" }
            ] }
        });
        let signals = json!({ "liked_genres": ["Science Fiction"], "disliked_genres": ["Science Fiction"] });
        let enriched = apply_taste_signals(base, &signals);
        let score = enriched["structured"]["recommendations"][0]["score"].as_f64().unwrap();
        assert!(score > 1.0, "a genre both liked and disliked must resolve to liked (boosted), got {score}");
    }

    // Flag ON but the facade returns an unparseable body → degrade to the
    // stateless result, never fail the tool.
    #[tokio::test]
    async fn decorator_facade_invalid_json_degrades_to_stateless() {
        let taste_server = MockServer::start();
        taste_server.mock(|when, then| {
            when.method(GET).path("/media/taste/signals");
            then.status(200).body("not valid json");
        });
        let decorator = TasteAwareMediaRecommend {
            inner: MediaRecommend::from_env(),
            client: Some(TasteMemoryClient::new(taste_server.base_url(), reqwest::Client::new())),
        };
        let result = decorator.execute(json!({})).await;
        assert!(result.is_ok(), "an unparseable facade response must degrade, never fail the tool");
        let parsed: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["structured"]["taste_memory"]["applied"], false);
    }

    // ── write-back ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_back_posts_engagement_signal_when_configured() {
        let taste_server = MockServer::start();
        let mock = taste_server.mock(|when, then| {
            when.method(POST).path("/media/taste/engagement");
            then.status(200).json_body(json!({ "ok": true }));
        });
        let tool = MediaTasteFeedback { client: Some(TasteMemoryClient::new(taste_server.base_url(), reqwest::Client::new())) };
        let result = tool.execute(json!({"title": "Dune", "media_type": "movie", "signal": "watched"})).await.unwrap();
        mock.assert();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["recorded"], true);
    }

    #[tokio::test]
    async fn write_back_not_configured_without_client() {
        let tool = MediaTasteFeedback { client: None };
        let result = tool.execute(json!({"title": "Dune", "media_type": "movie", "signal": "watched"})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn write_back_facade_error_does_not_fail_the_tool() {
        let taste_server = MockServer::start();
        taste_server.mock(|when, then| {
            when.method(POST).path("/media/taste/engagement");
            then.status(500);
        });
        let tool = MediaTasteFeedback { client: Some(TasteMemoryClient::new(taste_server.base_url(), reqwest::Client::new())) };
        let result = tool.execute(json!({"title": "Dune", "media_type": "movie", "signal": "watched"})).await;
        assert!(result.is_ok(), "a failed write-back must not surface as a tool error");
        let parsed: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["structured"]["recorded"], false);
    }

    #[tokio::test]
    async fn write_back_rejects_invalid_signal() {
        let tool = MediaTasteFeedback { client: None };
        let result = tool.execute(json!({"title": "Dune", "media_type": "movie", "signal": "bogus"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    fn tool_metadata_is_valid() {
        let tool = TasteAwareMediaRecommend { inner: MediaRecommend::from_env(), client: None };
        assert_eq!(tool.name(), "media_recommend");
        assert!(!tool.description().is_empty());

        let feedback = MediaTasteFeedback { client: None };
        assert_eq!(feedback.name(), "media_taste_feedback");
        assert!(!feedback.description().is_empty());
        assert_eq!(feedback.parameters()["type"], "object");
    }
}
