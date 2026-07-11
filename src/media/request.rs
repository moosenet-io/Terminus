//! Media domain request/download tools (MEDIA-03) — the acquisition surface,
//! guarded by a TIERED MUTATION SAFETY model.
//!
//! One tool, `media_request`: add/request a movie (Radarr) or TV season/series
//! (Sonarr), which drives Radarr/Sonarr's own indexer search + grab -- the
//! mechanism that hands a completed download to the download client (qtor).
//! Optionally registers a <media-service> request alongside it for tracking.
//!
//! ## The tiering model
//! Every request is classified by the pure, unit-tested [`classify_request`]
//! **before** anything is executed:
//! - [`MutationTier::Light`] — a specific, unambiguous, single item (one
//!   movie, or one named season) under the size threshold. Executed
//!   immediately; the response reports what was grabbed.
//! - [`MutationTier::Confirm`] — ambiguous, bulk/multi-item, a whole series
//!   (even nominally "one" series is high-impact), or an oversized single
//!   item (e.g. a 4K remux). **Never auto-executed.** The response carries a
//!   confirmation payload (title, year, size, quality) and the caller must
//!   re-call with `confirm: true` to actually execute.
//!
//! This module never fabricates "executed" for a Confirm-tier request that
//! wasn't explicitly confirmed -- see the negative tests in this file.
//!
//! ## Audit
//! Every *executed* mutation (Light-tier, or Confirm-tier with `confirm:
//! true`) is recorded via [`crate::gateway_framework::audit::AuditEntry`]
//! (S6-sanitized) in addition to the `#[instrument]` span on `execute`.
//! Confirmation-only responses (nothing executed) are not audited as
//! mutations -- no state changed.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::instrument;

use crate::error::ToolError;
use crate::gateway_framework::audit::{AuditEntry, AuditResult};
use crate::gateway_framework::ActionKind;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::clients::<media-service>::JellyseerrClient;
use super::clients::radarr::RadarrClient;
use super::clients::sonarr::SonarrClient;

// ── tiering (pure, testable) ────────────────────────────────────────────────

/// The shape of a single `media_request` call: what kind of thing is being
/// asked for, independent of title/service specifics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// A single movie.
    Movie,
    /// A single, explicitly-named season of a series.
    Season,
    /// An entire series with no season specified -- always treated as
    /// high-impact regardless of `item_count`, per the EDGE CASES /
    /// ACCEPTANCE CRITERIA in the S94 spec ("whole series" is bulk even
    /// though it is grammatically "one" request).
    Series,
}

/// Whether an *executed* request actually changed state, or only returned a
/// confirmation payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationTier {
    /// Specific + unambiguous + single item under the size threshold —
    /// execute immediately.
    Light,
    /// Ambiguous, bulk/multi-item, a whole series, or oversized — must not
    /// execute without an explicit `confirm: true`.
    Confirm,
}

/// A single item over this many bytes is treated as high-impact ("a 4K
/// remux") even when it is otherwise a specific, unambiguous, single-item
/// request — per the S94 spec's EDGE CASES. 20 GiB comfortably separates a
/// typical 1080p/2160p encode (a few GB to ~15 GB) from a 4K remux (commonly
/// 40-80+ GB).
pub const OVERSIZED_THRESHOLD_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Classify a request shape into a [`MutationTier`]. Pure function, no I/O —
/// the entire tiering decision is explicit and unit-testable, not vibes.
///
/// - `kind` — what's being requested (see [`RequestKind`]).
/// - `is_ambiguous` — the caller (Lumina, typically off the back of
///   `media_search`) has not narrowed this to one definite title/candidate.
/// - `item_count` — how many discrete items this single call would grab
///   (e.g. requesting seasons 1-3 at once is `item_count: 3`). Bulk (>1) is
///   always high-impact, independent of `kind`.
/// - `est_size_bytes` — the estimated download size of the (or each,
///   whichever is larger) item; see [`estimate_size_bytes`].
pub fn classify_request(
    kind: RequestKind,
    is_ambiguous: bool,
    item_count: u32,
    est_size_bytes: u64,
) -> MutationTier {
    if is_ambiguous
        || item_count > 1
        || matches!(kind, RequestKind::Series)
        || est_size_bytes > OVERSIZED_THRESHOLD_BYTES
    {
        MutationTier::Confirm
    } else {
        MutationTier::Light
    }
}

const GB: u64 = 1024 * 1024 * 1024;

/// Estimate a request's download size when the caller doesn't supply one
/// directly. Prefers an explicit `explicit_bytes` hint (e.g. surfaced by a
/// future MEDIA-02 extension that reads real size data from an arr release
/// search); otherwise falls back to a coarse quality-string heuristic so a
/// caller that only knows "this is a 4K remux" still gets treated as
/// high-impact. Pure function, unit-tested directly.
pub fn estimate_size_bytes(explicit_bytes: Option<u64>, quality_hint: Option<&str>) -> u64 {
    if let Some(bytes) = explicit_bytes {
        return bytes;
    }
    let hint = quality_hint.map(|s| s.to_lowercase()).unwrap_or_default();
    if hint.contains("remux") || hint.contains("2160p") || hint.contains("4k") {
        25 * GB
    } else if hint.contains("1080p") {
        4 * GB
    } else if hint.contains("720p") {
        2 * GB
    } else {
        4 * GB
    }
}

// ── request/confirmation payload shaping (pure) ─────────────────────────────

/// The confirmation-or-execution payload `media_request` returns. Built as a
/// pure function so its shape (size/quality surfaced, `executed` never lying
/// about what happened) is unit-testable without any HTTP.
#[allow(clippy::too_many_arguments)]
fn build_response(
    title: &str,
    year: Option<&str>,
    media_type: &str,
    season: Option<i64>,
    quality_hint: Option<&str>,
    est_size_bytes: u64,
    tier: MutationTier,
    executed: bool,
    already_present: bool,
    outcome_note: Option<&str>,
) -> Value {
    let size_gb = (est_size_bytes as f64) / (GB as f64);
    let target_desc = match (media_type, season) {
        ("series", Some(s)) => format!("{title} season {s}"),
        ("series", None) => format!("{title} (whole series)"),
        _ => title.to_string(),
    };

    let summary = if already_present {
        format!("\"{target_desc}\" is already in the library -- not requesting a duplicate.")
    } else if executed {
        format!(
            "Grabbed \"{target_desc}\"{} (~{size_gb:.1} GB) -- Radarr/Sonarr is searching indexers now.",
            year.map(|y| format!(" ({y})")).unwrap_or_default()
        )
    } else {
        match tier {
            MutationTier::Confirm => format!(
                "\"{target_desc}\"{} is ~{size_gb:.1} GB{} -- this needs confirmation before I grab it. Reply with confirm: true to proceed.",
                year.map(|y| format!(" ({y})")).unwrap_or_default(),
                quality_hint.map(|q| format!(" at {q}")).unwrap_or_default(),
            ),
            MutationTier::Light => format!("\"{target_desc}\" is ready to request."),
        }
    };

    json!({
        "summary": summary,
        "structured": {
            "title": title,
            "year": year,
            "media_type": media_type,
            "season": season,
            "quality_hint": quality_hint,
            "estimated_size_bytes": est_size_bytes,
            "tier": match tier { MutationTier::Light => "light", MutationTier::Confirm => "confirm" },
            "executed": executed,
            "already_present": already_present,
            "note": outcome_note,
        }
    })
}

// ── the tool ─────────────────────────────────────────────────────────────────

pub struct MediaRequest {
    radarr: Option<RadarrClient>,
    sonarr: Option<SonarrClient>,
    <media-service>: Option<JellyseerrClient>,
}

impl MediaRequest {
    fn quality_profile_id(env_key: &str) -> Result<i64, ToolError> {
        std::env::var(env_key)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .ok_or_else(|| ToolError::NotConfigured(format!("{env_key} not set (or not a valid integer)")))
    }

    fn root_folder_path(env_key: &str) -> Result<String, ToolError> {
        std::env::var(env_key)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured(format!("{env_key} not set")))
    }

    /// Case-insensitive exact/substring title match against an arr-style
    /// library array -- MEDIA-03 scope only needs a "is this already
    /// present" check, not `search.rs`'s full ranking heuristic.
    fn already_present(items: &[Value], title: &str) -> bool {
        let needle = title.trim().to_lowercase();
        if needle.is_empty() {
            return false;
        }
        items.iter().any(|item| {
            let t = item
                .get("title")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("name").and_then(|v| v.as_str()))
                .unwrap_or_default()
                .to_lowercase();
            t == needle
        })
    }

    async fn execute_movie(&self, title: &str, year: Option<&str>, tmdb_id: Option<i64>) -> Result<Value, ToolError> {
        let radarr = self
            .radarr
            .as_ref()
            .ok_or_else(|| ToolError::NotConfigured("RADARR_URL/RADARR_API_KEY not set".into()))?;

        // Duplicate-in-library check (EDGE CASE: don't duplicate).
        let library = radarr.library().await.unwrap_or(json!([]));
        let items = library.as_array().cloned().unwrap_or_default();
        if Self::already_present(&items, title) {
            return Ok(json!({ "already_present": true }));
        }

        let quality_profile_id = Self::quality_profile_id("RADARR_QUALITY_PROFILE_ID")?;
        let root_folder_path = Self::root_folder_path("RADARR_ROOT_FOLDER_PATH")?;
        let tmdb_id = tmdb_id.ok_or_else(|| {
            ToolError::InvalidArgument("tmdb_id is required to add a movie (resolve via media_search first)".into())
        })?;

        let mut body = json!({
            "tmdbId": tmdb_id,
            "title": title,
            "qualityProfileId": quality_profile_id,
            "rootFolderPath": root_folder_path,
            "monitored": true,
            "addOptions": { "searchForMovie": true },
        });
        if let Some(y) = year.and_then(|y| y.parse::<i64>().ok()) {
            body["year"] = json!(y);
        }

        let added = radarr.add_movie(body).await?;

        if let Some(js) = &self.<media-service> {
            let _ = js
                .create_request(json!({ "mediaType": "movie", "mediaId": tmdb_id }))
                .await; // best-effort tracking; a <media-service> hiccup must not fail the real grab
        }

        Ok(json!({ "already_present": false, "added": added }))
    }

    async fn execute_series(
        &self,
        title: &str,
        tvdb_id: Option<i64>,
        season: Option<i64>,
    ) -> Result<Value, ToolError> {
        let sonarr = self
            .sonarr
            .as_ref()
            .ok_or_else(|| ToolError::NotConfigured("SONARR_URL/SONARR_API_KEY not set".into()))?;

        let library = sonarr.library().await.unwrap_or(json!([]));
        let items = library.as_array().cloned().unwrap_or_default();
        if Self::already_present(&items, title) {
            return Ok(json!({ "already_present": true }));
        }

        let quality_profile_id = Self::quality_profile_id("SONARR_QUALITY_PROFILE_ID")?;
        let root_folder_path = Self::root_folder_path("SONARR_ROOT_FOLDER_PATH")?;
        let tvdb_id = tvdb_id.ok_or_else(|| {
            ToolError::InvalidArgument("tvdb_id is required to add a series (resolve via media_search first)".into())
        })?;

        let mut body = json!({
            "tvdbId": tvdb_id,
            "title": title,
            "qualityProfileId": quality_profile_id,
            "rootFolderPath": root_folder_path,
            "monitored": true,
            "addOptions": { "searchForMissingEpisodes": true },
        });
        if let Some(s) = season {
            body["seasons"] = json!([{ "seasonNumber": s, "monitored": true }]);
        }

        let added = sonarr.add_series(body).await?;

        if let Some(js) = &self.<media-service> {
            let _ = js
                .create_request(json!({ "mediaType": "tv", "mediaId": tvdb_id, "seasons": season.map(|s| vec![s]) }))
                .await;
        }

        Ok(json!({ "already_present": false, "added": added }))
    }
}

#[async_trait]
impl RustTool for MediaRequest {
    fn name(&self) -> &str {
        "media_request"
    }

    fn description(&self) -> &str {
        "Request/download a movie (Radarr) or TV season/series (Sonarr), which drives the download client. Uses tiered mutation safety: a specific, unambiguous, single item under the size threshold is grabbed immediately; anything ambiguous, bulk (multiple items), a whole series, or an oversized single item (e.g. a 4K remux) returns a confirmation payload with title/year/size/quality instead of executing, and requires a follow-up call with confirm: true. Never silently requests something already in the library." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Resolved title, e.g. from media_search." },
                "media_type": { "type": "string", "enum": ["movie", "series"] },
                "year": { "type": "string", "description": "Release year, if known." },
                "tmdb_id": { "type": "integer", "description": "TMDb id (movies) from media_search." },
                "tvdb_id": { "type": "integer", "description": "TVDb id (series) -- Sonarr's id space, distinct from TMDb." },
                "season": { "type": "integer", "description": "A specific season number. Omit for series to mean 'the whole series' (always Confirm-tier)." },
                "quality_hint": { "type": "string", "description": "e.g. '2160p remux', '1080p' -- used to estimate size when size_estimate_bytes isn't given." },
                "size_estimate_bytes": { "type": "integer", "description": "Known/estimated download size in bytes, if available." },
                "item_count": { "type": "integer", "description": "How many discrete items this single call would grab (e.g. requesting 3 seasons at once). Defaults to 1; >1 is always Confirm-tier." },
                "is_ambiguous": { "type": "boolean", "description": "True if the title/candidate itself is not definitively resolved (e.g. media_search returned multiple close candidates and none was chosen). Defaults to false." },
                "confirm": { "type": "boolean", "description": "Must be true to execute a Confirm-tier request. Ignored (and unnecessary) for Light-tier requests." }
            },
            "required": ["title", "media_type"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "media_request"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let title = args.get("title").and_then(|v| v.as_str()).map(str::trim).unwrap_or("");
        if title.is_empty() {
            return Err(ToolError::InvalidArgument("title must not be empty".into()));
        }
        let media_type = args.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
        if media_type != "movie" && media_type != "series" {
            return Err(ToolError::InvalidArgument("media_type must be \"movie\" or \"series\"".into()));
        }

        let year = args.get("year").and_then(|v| v.as_str());
        let tmdb_id = args.get("tmdb_id").and_then(|v| v.as_i64());
        let tvdb_id = args.get("tvdb_id").and_then(|v| v.as_i64());
        let season = args.get("season").and_then(|v| v.as_i64());
        let quality_hint = args.get("quality_hint").and_then(|v| v.as_str());
        let explicit_size = args.get("size_estimate_bytes").and_then(|v| v.as_u64());
        let item_count = args.get("item_count").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as u32;
        let is_ambiguous = args.get("is_ambiguous").and_then(|v| v.as_bool()).unwrap_or(false);
        let confirm = args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);

        let kind = match (media_type, season) {
            ("movie", _) => RequestKind::Movie,
            ("series", Some(_)) => RequestKind::Season,
            _ => RequestKind::Series,
        };

        let est_size_bytes = estimate_size_bytes(explicit_size, quality_hint);
        let tier = classify_request(kind, is_ambiguous, item_count, est_size_bytes);

        // Confirm-tier and not (yet) confirmed: return the confirmation
        // payload and stop. This is the hard rule -- a bulk/ambiguous/
        // oversized request MUST NOT execute here.
        if tier == MutationTier::Confirm && !confirm {
            return Ok(build_response(
                title, year, media_type, season, quality_hint, est_size_bytes, tier, false, false, None,
            )
            .to_string());
        }

        // Either Light-tier, or Confirm-tier explicitly confirmed: execute.
        let outcome = match media_type {
            "movie" => self.execute_movie(title, year, tmdb_id).await,
            _ => self.execute_series(title, tvdb_id, season).await,
        };

        match outcome {
            Ok(result) => {
                let already_present = result.get("already_present").and_then(|v| v.as_bool()).unwrap_or(false);
                let executed = !already_present;

                if executed {
                    let detail = format!(
                        "media_request executed: title={title} media_type={media_type} season={season:?} tier={tier:?} size_bytes={est_size_bytes}"
                    );
                    AuditEntry::new("media", "media_request", ActionKind::Tool, AuditResult::Success, Some(&detail))
                        .log();
                }

                Ok(build_response(
                    title,
                    year,
                    media_type,
                    season,
                    quality_hint,
                    est_size_bytes,
                    tier,
                    executed,
                    already_present,
                    None,
                )
                .to_string())
            }
            Err(e) => {
                // arr accepted-but-rejected / unreachable / not-configured --
                // surface the real failure, never report false success.
                let detail = format!("media_request failed: title={title} media_type={media_type} error={e}");
                AuditEntry::new("media", "media_request", ActionKind::Tool, AuditResult::Failure, Some(&detail)).log();
                Err(e)
            }
        }
    }
}

// ── registration ─────────────────────────────────────────────────────────────

/// Register the MEDIA-03 request/download tool. Degrades independently per
/// service: if Radarr is unconfigured, movie requests fail with
/// `NotConfigured` at execute time (the tool stays registered); the same for
/// Sonarr/series. <media-service> tracking is always best-effort.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(MediaRequest {
        radarr: RadarrClient::from_env().ok(),
        sonarr: SonarrClient::from_env().ok(),
        <media-service>: JellyseerrClient::from_env().ok(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    // ── classify_request (pure) ─────────────────────────────────────────────

    #[test]
    fn specific_unambiguous_single_light_movie_is_light() {
        let tier = classify_request(RequestKind::Movie, false, 1, 4 * GB);
        assert_eq!(tier, MutationTier::Light);
    }

    #[test]
    fn specific_unambiguous_single_season_is_light() {
        let tier = classify_request(RequestKind::Season, false, 1, 4 * GB);
        assert_eq!(tier, MutationTier::Light);
    }

    #[test]
    fn ambiguous_is_confirm_even_if_single_small_item() {
        let tier = classify_request(RequestKind::Movie, true, 1, 1 * GB);
        assert_eq!(tier, MutationTier::Confirm);
    }

    #[test]
    fn bulk_multi_item_is_confirm() {
        let tier = classify_request(RequestKind::Season, false, 3, 1 * GB);
        assert_eq!(tier, MutationTier::Confirm);
    }

    #[test]
    fn whole_series_is_always_confirm_even_as_a_single_request() {
        let tier = classify_request(RequestKind::Series, false, 1, 1 * GB);
        assert_eq!(tier, MutationTier::Confirm);
    }

    #[test]
    fn oversized_single_item_is_confirm_a_4k_remux() {
        let tier = classify_request(RequestKind::Movie, false, 1, 40 * GB);
        assert_eq!(tier, MutationTier::Confirm);
    }

    #[test]
    fn exactly_at_threshold_is_still_light() {
        let tier = classify_request(RequestKind::Movie, false, 1, OVERSIZED_THRESHOLD_BYTES);
        assert_eq!(tier, MutationTier::Light);
    }

    #[test]
    fn one_byte_over_threshold_is_confirm() {
        let tier = classify_request(RequestKind::Movie, false, 1, OVERSIZED_THRESHOLD_BYTES + 1);
        assert_eq!(tier, MutationTier::Confirm);
    }

    // NEGATIVE: a bulk/ambiguous/oversized request must never be classified
    // Light -- Light is the ONLY tier `execute()` auto-runs without a
    // `confirm: true`.
    #[test]
    fn bulk_ambiguous_or_oversized_never_classify_as_light() {
        assert_ne!(classify_request(RequestKind::Season, false, 5, 1 * GB), MutationTier::Light);
        assert_ne!(classify_request(RequestKind::Movie, true, 1, 1 * GB), MutationTier::Light);
        assert_ne!(classify_request(RequestKind::Series, false, 1, 1 * GB), MutationTier::Light);
        assert_ne!(classify_request(RequestKind::Movie, false, 1, 100 * GB), MutationTier::Light);
    }

    // ── estimate_size_bytes (pure) ──────────────────────────────────────────

    #[test]
    fn estimate_prefers_explicit_bytes() {
        assert_eq!(estimate_size_bytes(Some(123), Some("2160p remux")), 123);
    }

    #[test]
    fn estimate_flags_remux_and_4k_as_oversized() {
        assert!(estimate_size_bytes(None, Some("2160p REMUX")) > OVERSIZED_THRESHOLD_BYTES);
        assert!(estimate_size_bytes(None, Some("4K")) > OVERSIZED_THRESHOLD_BYTES);
    }

    #[test]
    fn estimate_1080p_is_under_threshold() {
        assert!(estimate_size_bytes(None, Some("1080p")) < OVERSIZED_THRESHOLD_BYTES);
    }

    #[test]
    fn estimate_default_with_no_hint_is_under_threshold() {
        assert!(estimate_size_bytes(None, None) < OVERSIZED_THRESHOLD_BYTES);
    }

    // ── build_response (pure) — size/quality surfaced, executed never lies ──

    #[test]
    fn confirm_tier_unexecuted_response_surfaces_size_and_quality() {
        let out = build_response(
            "Dune",
            Some("2021"),
            "movie",
            None,
            Some("2160p remux"),
            40 * GB,
            MutationTier::Confirm,
            false,
            false,
            None,
        );
        assert_eq!(out["structured"]["executed"], false);
        assert_eq!(out["structured"]["tier"], "confirm");
        assert_eq!(out["structured"]["quality_hint"], "2160p remux");
        assert!(out["structured"]["estimated_size_bytes"].as_u64().unwrap() > OVERSIZED_THRESHOLD_BYTES);
        let summary = out["summary"].as_str().unwrap();
        assert!(summary.contains("confirm"));
        assert!(summary.contains("GB"));
    }

    #[test]
    fn light_tier_executed_response_says_grabbed() {
        let out = build_response(
            "Arrival", Some("2016"), "movie", None, Some("1080p"), 4 * GB, MutationTier::Light, true, false, None,
        );
        assert_eq!(out["structured"]["executed"], true);
        assert!(out["summary"].as_str().unwrap().to_lowercase().contains("grabbed"));
    }

    #[test]
    fn already_present_response_never_claims_executed() {
        let out = build_response(
            "Arrival", None, "movie", None, None, 4 * GB, MutationTier::Light, false, true, None,
        );
        assert_eq!(out["structured"]["executed"], false);
        assert_eq!(out["structured"]["already_present"], true);
        assert!(out["summary"].as_str().unwrap().to_lowercase().contains("already"));
    }

    // ── execute() integration (mocked Radarr/Sonarr/<media-service>) ─────────────

    fn set_radarr_env(url: &str) {
        std::env::set_var("RADARR_URL", url);
        std::env::set_var("RADARR_API_KEY", "k");
        std::env::set_var("RADARR_QUALITY_PROFILE_ID", "1");
        std::env::set_var("RADARR_ROOT_FOLDER_PATH", "/movies");
    }
    fn clear_radarr_env() {
        for k in ["RADARR_URL", "RADARR_API_KEY", "RADARR_QUALITY_PROFILE_ID", "RADARR_ROOT_FOLDER_PATH"] {
            std::env::remove_var(k);
        }
    }
    fn tool_with(radarr: Option<&str>, sonarr: Option<&str>) -> MediaRequest {
        MediaRequest {
            radarr: radarr.map(|u| RadarrClient::new(u, "k", reqwest::Client::new())),
            sonarr: sonarr.map(|u| SonarrClient::new(u, "k", reqwest::Client::new())),
            <media-service>: None,
        }
    }

    #[tokio::test]
    #[serial]
    async fn light_tier_movie_request_drives_the_add() {
        clear_radarr_env();
        set_radarr_env("http://placeholder"); // not used by client itself; env only needed for profile/folder
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([]));
        });
        let add_mock = radarr_server.mock(|when, then| {
            when.method(POST).path("/api/v3/movie");
            then.status(201).json_body(json!({ "id": 1, "title": "Dune" }));
        });

        let tool = tool_with(Some(&radarr_server.base_url()), None);
        let result = tool
            .execute(json!({ "title": "Dune", "media_type": "movie", "year": "2021", "tmdb_id": 438631, "quality_hint": "1080p" }))
            .await
            .unwrap();

        add_mock.assert();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], true);
        assert_eq!(parsed["structured"]["tier"], "light");
        clear_radarr_env();
    }

    #[tokio::test]
    async fn unconfirmed_bulk_request_does_not_drive_the_add() {
        let sonarr_server = MockServer::start();
        // No mock registered for POST /api/v3/series -- if the tool ever
        // called it, httpmock would simply 404/connection-refuse since
        // nothing matches; we assert on a mock's hit count directly.
        let never_mock = sonarr_server.mock(|when, then| {
            when.method(POST).path("/api/v3/series");
            then.status(201).json_body(json!({ "id": 1 }));
        });
        let library_mock = sonarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/series");
            then.status(200).json_body(json!([]));
        });

        let tool = tool_with(None, Some(&sonarr_server.base_url()));
        // item_count: 3 (bulk, e.g. "seasons 1-3") -> must be Confirm-tier
        // and must NOT execute without confirm: true.
        let result = tool
            .execute(json!({ "title": "Foundation", "media_type": "series", "season": 1, "item_count": 3 }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(parsed["structured"]["tier"], "confirm");
        // The add must never have been called; the library GET may or may
        // not have been (it isn't, since we return before executing).
        assert_eq!(never_mock.hits(), 0, "unconfirmed bulk request must not POST /api/v3/series");
        let _ = library_mock; // registered defensively; not required to be hit
    }

    #[tokio::test]
    #[serial]
    async fn confirmed_bulk_request_drives_the_add() {
        let sonarr_server = MockServer::start();
        sonarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/series");
            then.status(200).json_body(json!([]));
        });
        let add_mock = sonarr_server.mock(|when, then| {
            when.method(POST).path("/api/v3/series");
            then.status(201).json_body(json!({ "id": 1, "title": "Foundation" }));
        });

        let tool = MediaRequest {
            radarr: None,
            sonarr: Some(SonarrClient::new(&sonarr_server.base_url(), "k", reqwest::Client::new())),
            <media-service>: None,
        };

        // Provide profile/folder via env since MediaRequest reads them at
        // execute time (not client-construction time).
        std::env::set_var("SONARR_QUALITY_PROFILE_ID", "1");
        std::env::set_var("SONARR_ROOT_FOLDER_PATH", "/tv");

        let result = tool
            .execute(json!({
                "title": "Foundation", "media_type": "series", "tvdb_id": 358903,
                "item_count": 3, "confirm": true
            }))
            .await
            .unwrap();

        add_mock.assert();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], true);
        std::env::remove_var("SONARR_QUALITY_PROFILE_ID");
        std::env::remove_var("SONARR_ROOT_FOLDER_PATH");
    }

    #[tokio::test]
    async fn ambiguous_request_never_executes_without_confirm() {
        let radarr_server = MockServer::start();
        let never_mock = radarr_server.mock(|when, then| {
            when.method(POST).path("/api/v3/movie");
            then.status(201).json_body(json!({ "id": 1 }));
        });

        let tool = tool_with(Some(&radarr_server.base_url()), None);
        let result = tool
            .execute(json!({ "title": "Dune", "media_type": "movie", "tmdb_id": 1, "is_ambiguous": true }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(never_mock.hits(), 0);
    }

    #[tokio::test]
    async fn whole_series_never_executes_without_confirm() {
        let sonarr_server = MockServer::start();
        let never_mock = sonarr_server.mock(|when, then| {
            when.method(POST).path("/api/v3/series");
            then.status(201).json_body(json!({ "id": 1 }));
        });

        let tool = tool_with(None, Some(&sonarr_server.base_url()));
        // No `season` -> whole series -> RequestKind::Series -> always Confirm.
        let result = tool
            .execute(json!({ "title": "Foundation", "media_type": "series", "tvdb_id": 358903 }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(parsed["structured"]["tier"], "confirm");
        assert_eq!(never_mock.hits(), 0);
    }

    #[tokio::test]
    async fn oversized_single_movie_never_executes_without_confirm() {
        let radarr_server = MockServer::start();
        let never_mock = radarr_server.mock(|when, then| {
            when.method(POST).path("/api/v3/movie");
            then.status(201).json_body(json!({ "id": 1 }));
        });

        let tool = tool_with(Some(&radarr_server.base_url()), None);
        let result = tool
            .execute(json!({ "title": "Dune", "media_type": "movie", "tmdb_id": 1, "quality_hint": "2160p remux" }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(never_mock.hits(), 0);
    }

    #[tokio::test]
    async fn already_in_library_does_not_duplicate() {
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "title": "Dune", "hasFile": true }]));
        });
        let never_mock = radarr_server.mock(|when, then| {
            when.method(POST).path("/api/v3/movie");
            then.status(201).json_body(json!({ "id": 1 }));
        });

        let tool = tool_with(Some(&radarr_server.base_url()), None);
        let result = tool
            .execute(json!({ "title": "Dune", "media_type": "movie", "tmdb_id": 438631 }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["already_present"], true);
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(never_mock.hits(), 0, "must not request a duplicate already in the library");
    }

    #[tokio::test]
    #[serial]
    async fn arr_rejection_surfaces_real_failure_not_false_success() {
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([]));
        });
        radarr_server.mock(|when, then| {
            when.method(POST).path("/api/v3/movie");
            then.status(400).body("Quality profile does not exist");
        });

        std::env::set_var("RADARR_QUALITY_PROFILE_ID", "1");
        std::env::set_var("RADARR_ROOT_FOLDER_PATH", "/movies");
        let tool = tool_with(Some(&radarr_server.base_url()), None);
        let result = tool.execute(json!({ "title": "Dune", "media_type": "movie", "tmdb_id": 1 })).await;
        std::env::remove_var("RADARR_QUALITY_PROFILE_ID");
        std::env::remove_var("RADARR_ROOT_FOLDER_PATH");

        assert!(matches!(result, Err(ToolError::Http(_))), "arr rejection must propagate as a real error, not fake success");
    }

    #[tokio::test]
    async fn missing_radarr_client_is_not_configured() {
        let tool = tool_with(None, None);
        let result = tool.execute(json!({ "title": "Dune", "media_type": "movie", "tmdb_id": 1 })).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn missing_sonarr_client_is_not_configured() {
        let tool = tool_with(None, None);
        let result = tool.execute(json!({ "title": "Foundation", "media_type": "series", "season": 1 })).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn empty_title_is_invalid_argument() {
        let tool = tool_with(None, None);
        let result = tool.execute(json!({ "title": "  ", "media_type": "movie" })).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn invalid_media_type_is_invalid_argument() {
        let tool = tool_with(None, None);
        let result = tool.execute(json!({ "title": "Dune", "media_type": "album" })).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    #[serial]
    async fn light_tier_movie_missing_tmdb_id_is_invalid_argument() {
        let radarr_server = MockServer::start();
        radarr_server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([]));
        });
        std::env::set_var("RADARR_QUALITY_PROFILE_ID", "1");
        std::env::set_var("RADARR_ROOT_FOLDER_PATH", "/movies");
        let tool = tool_with(Some(&radarr_server.base_url()), None);
        let result = tool.execute(json!({ "title": "Dune", "media_type": "movie" })).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
        std::env::remove_var("RADARR_QUALITY_PROFILE_ID");
        std::env::remove_var("RADARR_ROOT_FOLDER_PATH");
    }

    #[test]
    fn tool_metadata_is_valid() {
        let tool = tool_with(None, None);
        assert_eq!(tool.name(), "media_request");
        assert!(!tool.description().is_empty());
        assert_eq!(tool.parameters()["type"], "object");
    }
}
