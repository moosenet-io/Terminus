//! MUSEL-LIVE (MUSE #111 phase 1) — **live** Plex session data.
//!
//! Everything else the media domain reads from Plex is historical: library
//! sections, watch history, on-deck, recently-added. `/status/sessions` — what
//! is playing RIGHT NOW — was used nowhere in this tree. This module is that
//! missing read, and the `media_now_playing` tool that exposes it.
//!
//! Phase 2 renders this in the MUSE web GUI, so the STRUCTURED payload emitted
//! here is a contract, not an implementation detail. See
//! [`build_response`] for the field-by-field shape.
//!
//! ## Three outcomes that must never render identically
//!
//! "I could not reach Plex", "Plex refused my token" and "nobody is watching"
//! mean opposite things to somebody reading a dashboard, and collapsing them is
//! the ambiguity this item exists to remove. They are kept apart at the type
//! level from the transport up: [`PlexSessionsError`] in the client, and the
//! `status` discriminant here. `idle` is an `ok` answer with an empty list;
//! `unreachable` and `unauthorized` are not answers at all and carry no
//! session data — not even a count.
//!
//! ## Never cached
//!
//! Live session data is worthless stale, so `media_now_playing` is named in
//! [`crate::tool_cache::NEVER_CACHED_TOOLS`]. That is a DELIBERATE extension of
//! the never-cache rule rather than a naming trick: the pre-existing rule keyed
//! on `alert`/`severe`/`warning`, which no honest name for this tool contains.
//!
//! ## Entitlement — the sensitive part
//!
//! Now-playing reveals which household member is home and what they are
//! watching, in real time. It is gated on the caller via the TRTR-05
//! [`CallerContext`] mechanism: an unentitled caller (guest, unknown, absent
//! principal, or the un-threaded `execute()` path) gets `status: "forbidden"`
//! and NOTHING else — no titles, no usernames, no device names, and no session
//! count, since a count alone discloses occupancy. The gate runs BEFORE the
//! client is touched, so an unentitled call issues no Plex request at all.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::instrument;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{CallerContext, RustTool, ToolOutput};

use super::clients::plex::{PlexClient, PlexSessionsError};

/// The tool name. Also the key the never-cache rule matches on.
pub const TOOL_NAME: &str = "media_now_playing";

// ── entitlement ─────────────────────────────────────────────────────────────

/// May this caller see who is watching what, right now?
///
/// **Why this reuses the two existing TRTR-05 signals instead of adding a
/// third flag.** `CallerContext`'s flags are minted by
/// `GatewayFramework::caller_context` from PROBE TOOLS — "may a tool fold in a
/// source of operator context on this caller's behalf" is answered by asking
/// whether the caller could already read that source directly. Both existing
/// probes (`google_calendar_today`, `commute_estimate`) are operator-tier and
/// neither is in `GUEST_BASELINE_ALLOW`, which is a CEILING a guest grant is
/// clamped to — so requiring BOTH is exactly the predicate "this principal is
/// operator-tier, not a guest and not an unknown". Requiring both rather than
/// either is the fail-closed choice: a partial grant yields nothing.
///
/// The cleaner long-term shape is a dedicated `may_see_household_activity`
/// flag minted from a `NOW_PLAYING_CONTEXT_PROBE`, which would make the gate
/// self-describing instead of inferred. That is a `gateway_framework` change
/// and is deliberately NOT made here (see the report accompanying this item):
/// this module is additive, and reusing the existing signal is strictly no
/// weaker than a new flag probed off an operator-tier tool. Recorded here so
/// the next reader knows it was a decision, not an oversight.
pub fn may_see_now_playing(caller: CallerContext) -> bool {
    caller.may_infer_from_calendar() && caller.may_infer_from_routine()
}

// ── the source seam (so "no request was issued" is testable) ────────────────

/// The live reads `media_now_playing` needs, behind a trait so a test can
/// COUNT calls and prove the entitlement gate short-circuits before any I/O.
#[async_trait]
pub trait SessionSource: Send + Sync {
    async fn sessions(&self) -> Result<Value, PlexSessionsError>;
    /// Best-effort server header. An `Err` here must never change the session
    /// outcome — it only means the GUI header is unavailable this tick.
    async fn identity(&self) -> Result<Value, PlexSessionsError>;
}

#[async_trait]
impl SessionSource for PlexClient {
    async fn sessions(&self) -> Result<Value, PlexSessionsError> {
        PlexClient::sessions(self).await
    }
    async fn identity(&self) -> Result<Value, PlexSessionsError> {
        PlexClient::identity(self).await
    }
}

// ── parsing: model what the server actually returns ─────────────────────────

/// How Plex is delivering one session's media.
///
/// Plex does NOT hand this over as a single field — it has to be DERIVED, and
/// the derivation was checked against a live server rather than the docs:
/// - no `TranscodeSession` at all           => direct play
/// - `TranscodeSession` with video AND audio `decision == "copy"`  => direct
///   stream (a container remux; nothing is re-encoded)
/// - anything else                          => transcode
///
/// The live session observed while building this had `videoDecision: "copy"`
/// with `audioDecision: "transcode"`, which is a TRANSCODE — treating any
/// `copy` as direct stream would have mislabelled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackDecision {
    DirectPlay,
    DirectStream,
    Transcode,
}

impl PlaybackDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectPlay => "direct_play",
            Self::DirectStream => "direct_stream",
            Self::Transcode => "transcode",
        }
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Plex is inconsistent about numeric-vs-string JSON (`sessionKey` is a
/// string, `duration` an int, `Genre[].count` a string). Accept both.
fn num_at(v: &Value, key: &str) -> Option<i64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn f64_at(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn bool_at(v: &Value, key: &str) -> Option<bool> {
    match v.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0),
        Some(Value::String(s)) => match s.trim() {
            "1" | "true" => Some(true),
            "0" | "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `MediaContainer.Metadata[]`, tolerating an absent key (Plex omits
/// `Metadata` entirely when `size` is 0 — verified live).
fn session_items(raw: &Value) -> Vec<&Value> {
    raw.get("MediaContainer")
        .and_then(|mc| mc.get("Metadata"))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// `Media[0].Part[0].decision` — where Plex states the per-part decision
/// explicitly (`directplay` / `copy` / `transcode`).
fn part_decision(item: &Value) -> Option<&str> {
    let part = item
        .get("Media")?
        .as_array()?
        .first()?
        .get("Part")?
        .as_array()?
        .first()?;
    str_at(part, "decision")
}

fn decide(item: &Value) -> PlaybackDecision {
    let Some(ts) = item.get("TranscodeSession") else {
        // No transcode session at all. Normally that is a direct play, but
        // Plex states the decision on the Part too, so cross-check it rather
        // than assuming: a payload that says `transcode` without a
        // TranscodeSession must not be reported as a direct play.
        return match part_decision(item) {
            Some("transcode") => PlaybackDecision::Transcode,
            Some("copy") => PlaybackDecision::DirectStream,
            _ => PlaybackDecision::DirectPlay,
        };
    };
    let video = str_at(ts, "videoDecision").unwrap_or("copy");
    let audio = str_at(ts, "audioDecision").unwrap_or("copy");
    if video == "copy" && audio == "copy" {
        PlaybackDecision::DirectStream
    } else {
        PlaybackDecision::Transcode
    }
}

/// Plex's own reason string when it gives one, else a derived one.
///
/// `TranscodeSession.transcodeReason` is documented but was **absent** on the
/// live server checked for this item (Plex Media Server 1.42.x), so a
/// reason-or-nothing implementation would have shown nothing in practice.
/// When it is missing we say which streams are being re-encoded and from what
/// — the operationally useful part of the answer.
fn transcode_reason(item: &Value, decision: PlaybackDecision) -> Option<String> {
    if decision == PlaybackDecision::DirectPlay {
        return None;
    }
    let ts = item.get("TranscodeSession")?;
    if let Some(reason) = str_at(ts, "transcodeReason") {
        return Some(reason.to_string());
    }

    let mut parts: Vec<String> = Vec::new();
    for (label, decision_key, src_key, dst_key) in [
        ("video", "videoDecision", "sourceVideoCodec", "videoCodec"),
        ("audio", "audioDecision", "sourceAudioCodec", "audioCodec"),
    ] {
        if str_at(ts, decision_key) == Some("transcode") {
            match (str_at(ts, src_key), str_at(ts, dst_key)) {
                (Some(src), Some(dst)) => parts.push(format!("{label} {src} -> {dst}")),
                _ => parts.push(format!("{label} re-encoded")),
            }
        }
    }
    if str_at(ts, "subtitleDecision") == Some("transcode") {
        parts.push("subtitles burned in or converted".to_string());
    }
    if parts.is_empty() {
        // Direct stream: nothing re-encoded, the container is being remuxed.
        let container = str_at(ts, "protocol").unwrap_or("a different container");
        parts.push(format!("remuxed to {container}"));
    }
    Some(parts.join("; "))
}

/// A display title that reads correctly for BOTH shapes Plex nests
/// differently (verified against live payloads):
/// - movie:   `title` + `year`
/// - episode: `grandparentTitle` (show) + `parentIndex` (season) +
///            `index` (episode) + `title` (episode title)
fn full_title(item: &Value) -> String {
    let title = str_at(item, "title").unwrap_or("Unknown");
    if str_at(item, "type") == Some("episode") {
        let show = str_at(item, "grandparentTitle").unwrap_or("Unknown show");
        return match (num_at(item, "parentIndex"), num_at(item, "index")) {
            (Some(s), Some(e)) => format!("{show} - S{s:02}E{e:02} - {title}"),
            _ => format!("{show} - {title}"),
        };
    }
    match num_at(item, "year") {
        Some(y) => format!("{title} ({y})"),
        None => title.to_string(),
    }
}

/// One session, flattened into the shape the GUI consumes.
///
/// Deliberately NOT included: `Player.address` and `Player.remotePublicAddress`
/// (household IP addresses, of no use to the dashboard) and the whole
/// `Media`/`Part`/`Stream` metadata tail (cast, genres, artwork, summaries).
/// A live-activity view needs who/what/where/how, not the catalogue record.
fn session_json(item: &Value) -> Value {
    let decision = decide(item);
    let user = item.get("User").cloned().unwrap_or(Value::Null);
    let player = item.get("Player").cloned().unwrap_or(Value::Null);
    let session = item.get("Session").cloned().unwrap_or(Value::Null);
    let ts = item.get("TranscodeSession").cloned().unwrap_or(Value::Null);
    let media = item
        .get("Media")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);

    let duration_ms = num_at(item, "duration");
    let progress_ms = num_at(item, "viewOffset").unwrap_or(0);
    let progress_percent = match duration_ms {
        Some(d) if d > 0 => Some(((progress_ms as f64 / d as f64) * 1000.0).round() / 10.0),
        _ => None,
    };

    json!({
        "session_key": str_at(item, "sessionKey"),
        "media_type": str_at(item, "type"),
        "title": str_at(item, "title"),
        "show_title": str_at(item, "grandparentTitle"),
        "season": num_at(item, "parentIndex"),
        "episode": num_at(item, "index"),
        "year": num_at(item, "year"),
        "full_title": full_title(item),
        "library_section": str_at(item, "librarySectionTitle"),
        "user": str_at(&user, "title"),
        "user_id": user.get("id").map(|v| v.to_string().trim_matches('"').to_string()),
        "player": str_at(&player, "title"),
        "player_product": str_at(&player, "product"),
        "player_platform": str_at(&player, "platform"),
        "player_state": str_at(&player, "state"),
        "player_local": bool_at(&player, "local"),
        "progress_ms": progress_ms,
        "duration_ms": duration_ms,
        "progress_percent": progress_percent,
        "decision": decision.as_str(),
        "video_decision": str_at(&ts, "videoDecision"),
        "audio_decision": str_at(&ts, "audioDecision"),
        "subtitle_decision": str_at(&ts, "subtitleDecision"),
        "transcode_reason": transcode_reason(item, decision),
        "transcode_progress_percent": f64_at(&ts, "progress"),
        "transcode_throttled": bool_at(&ts, "throttled"),
        "transcode_hw": bool_at(&ts, "transcodeHwRequested"),
        "bandwidth_kbps": num_at(&session, "bandwidth"),
        "stream_location": str_at(&session, "location"),
        "container": str_at(&media, "container"),
        "video_resolution": str_at(&media, "videoResolution"),
    })
}

/// `MediaContainer` from `/identity`, reduced to the GUI-useful fields.
fn server_json(raw: &Value) -> Value {
    let mc = raw.get("MediaContainer").cloned().unwrap_or(Value::Null);
    json!({
        "version": str_at(&mc, "version"),
        "api_version": str_at(&mc, "apiVersion"),
        "machine_identifier": str_at(&mc, "machineIdentifier"),
    })
}

// ── the response contract (Phase 2 consumes this verbatim) ──────────────────

/// Build the structured payload.
///
/// `status` is the ONLY discriminant a consumer needs, and it is exhaustive:
///
/// | `status`        | `ok`  | has `sessions`/`session_count` | means |
/// |-----------------|-------|-------------------------------|-------|
/// | `playing`       | true  | yes (non-empty)               | somebody is watching |
/// | `idle`          | true  | yes (empty, count 0)          | Plex answered: nobody is watching |
/// | `unreachable`   | false | **no**                        | no answer from Plex — we do not know |
/// | `unauthorized`  | false | **no**                        | Plex refused the token |
/// | `malformed`     | false | **no**                        | Plex answered something we cannot read |
/// | `not_configured`| false | **no**                        | `PLEX_URL`/`PLEX_TOKEN` unset |
/// | `forbidden`     | false | **no**                        | caller is not entitled; nothing was fetched |
///
/// The absence of `session_count` on every non-`ok` status is deliberate and
/// load-bearing in two directions: a failed read must not be mistakable for an
/// empty house, and an unentitled caller must not learn occupancy from a count.
fn build_response(status: &str, message: &str, extra: Value) -> Value {
    let mut out = json!({
        "status": status,
        "ok": matches!(status, "playing" | "idle"),
        "message": message,
    });
    if let (Some(map), Some(more)) = (out.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            map.insert(k.clone(), v.clone());
        }
    }
    out
}

/// The refusal. Constructed WITHOUT consulting Plex or any argument, so there
/// is nothing in it to leak.
fn forbidden_response() -> Value {
    build_response(
        "forbidden",
        "Live viewing activity is private to the household operator; this caller is not entitled to it.",
        json!({}),
    )
}

fn ok_response(sessions: Vec<Value>, server: Option<Value>) -> Value {
    let total_bandwidth: Option<i64> = {
        let sum: i64 = sessions.iter().filter_map(|s| s["bandwidth_kbps"].as_i64()).sum();
        let any = sessions.iter().any(|s| s["bandwidth_kbps"].is_i64());
        any.then_some(sum)
    };
    let count = sessions.len();
    let (status, message) = if count == 0 {
        ("idle", "Plex answered: nothing is playing right now.".to_string())
    } else {
        ("playing", format!("{count} stream(s) playing right now."))
    };
    build_response(
        status,
        &message,
        json!({
            "session_count": count,
            "total_bandwidth_kbps": total_bandwidth,
            "sessions": sessions,
            "server": server,
        }),
    )
}

fn error_response(err: &PlexSessionsError) -> Value {
    let message = match err {
        PlexSessionsError::Unreachable(_) => {
            "Could not reach Plex, so what is playing is UNKNOWN - this is not the same as nothing playing."
        }
        PlexSessionsError::TokenRejected(_) => {
            "Plex is up but rejected the configured token, so live activity could not be read."
        }
        PlexSessionsError::Malformed(_) => {
            "Plex answered with something this client could not read, so live activity is unknown."
        }
    };
    build_response(err.kind(), message, json!({ "detail": err.detail() }))
}

// ── the tool ────────────────────────────────────────────────────────────────

pub struct MediaNowPlaying {
    source: Option<Arc<dyn SessionSource>>,
}

impl MediaNowPlaying {
    pub fn from_env() -> Self {
        Self {
            source: PlexClient::from_env().ok().map(|c| Arc::new(c) as Arc<dyn SessionSource>),
        }
    }

    pub fn with_source(source: Arc<dyn SessionSource>) -> Self {
        Self { source: Some(source) }
    }

    /// The whole decision, in one place.
    ///
    /// Order matters: the entitlement gate is FIRST, before `self.source` is
    /// even inspected, so an unentitled call cannot issue a Plex request and
    /// cannot distinguish "not entitled" from "not configured" either.
    async fn run(&self, caller: CallerContext) -> Value {
        if !may_see_now_playing(caller) {
            return forbidden_response();
        }
        let Some(source) = self.source.as_ref() else {
            return build_response(
                "not_configured",
                "Plex is not configured here (PLEX_URL / PLEX_TOKEN are unset).",
                json!({}),
            );
        };

        match source.sessions().await {
            Ok(raw) => {
                let sessions: Vec<Value> = session_items(&raw).into_iter().map(session_json).collect();
                // Best-effort only: the server header must never turn a good
                // session read into a failure.
                let server = source.identity().await.ok().map(|v| server_json(&v));
                ok_response(sessions, server)
            }
            Err(e) => error_response(&e),
        }
    }
}

#[async_trait]
impl RustTool for MediaNowPlaying {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Show what is playing on Plex RIGHT NOW: title, who is watching, on which player, how far in, and whether it is direct play, direct stream or a transcode (with the reason). Live and never cached. Private: only an entitled operator-tier caller receives any of it." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    /// The UN-THREADED path. It gets [`CallerContext::untrusted`], which is
    /// unentitled, so it returns the refusal — never household activity.
    #[instrument(skip(self, _args), fields(tool = "media_now_playing"))]
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Ok(self.run(CallerContext::untrusted()).await.to_string())
    }

    /// The authorized path: the gateway derived this caller's entitlement from
    /// the same server-verified principal it authorized the call with.
    async fn execute_with_caller(
        &self,
        _args: Value,
        caller: CallerContext,
    ) -> Result<ToolOutput, ToolError> {
        let payload = self.run(caller).await;
        let text = payload["message"].as_str().unwrap_or_default().to_string();
        Ok(ToolOutput::with_structured(text, payload))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(MediaNowPlaying::from_env()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── fixtures ───────────────────────────────────────────────────────────
    //
    // Every title, username and device name below is INVENTED. Nothing here
    // came off the live library that was probed to learn the payload shape.

    /// A transcoding EPISODE, shaped exactly as a live server returns it:
    /// `grandparentTitle`/`parentIndex`/`index` nesting, a `TranscodeSession`
    /// with per-stream decisions, `Session.bandwidth`, `User`, `Player`.
    fn transcoding_episode() -> Value {
        json!({
            "type": "episode",
            "sessionKey": "101",
            "title": "The Placeholder Episode", // pii-test-fixture: invented title, not from the live library
            "grandparentTitle": "Placeholder Show", // pii-test-fixture: invented title, not from the live library
            "parentIndex": 4,
            "index": 12,
            "year": 1996,
            "duration": 2_400_000,
            "viewOffset": 600_000,
            "librarySectionTitle": "TV Shows",
            "Media": [{ "container": "mkv", "videoResolution": "1080p",
                        "Part": [{ "decision": "transcode" }] }],
            "User": { "id": "42", "title": "household-member-a" }, // pii-test-fixture: invented user, not a real household account
            "Player": { "title": "Placeholder Player", "product": "Plex for Placeholder", // pii-test-fixture: invented device name
                        "platform": "Android", "state": "playing", "local": false },
            "Session": { "id": "sess-a", "bandwidth": 8906, "location": "wan" },
            "TranscodeSession": {
                "throttled": true, "complete": false, "progress": 36.5,
                "videoDecision": "copy", "audioDecision": "transcode",
                "subtitleDecision": "transcode",
                "sourceVideoCodec": "h264", "sourceAudioCodec": "eac3",
                "videoCodec": "h264", "audioCodec": "opus",
                "protocol": "hls", "transcodeHwRequested": true
            }
        })
    }

    /// A direct-play MOVIE: no `TranscodeSession` at all, `Part.decision` is
    /// `directplay`, and the title/year live at the top level.
    fn direct_play_movie() -> Value {
        json!({
            "type": "movie",
            "sessionKey": "102",
            "title": "A Placeholder Motion Picture", // pii-test-fixture: invented title, not from the live library
            "year": 2025,
            "duration": 6_000_000,
            "viewOffset": 3_000_000,
            "librarySectionTitle": "Movies",
            "Media": [{ "container": "mkv", "videoResolution": "2160p",
                        "Part": [{ "decision": "directplay" }] }],
            "User": { "id": 7, "title": "household-member-b" }, // pii-test-fixture: invented user, not a real household account
            "Player": { "title": "Placeholder TV", "product": "Plex for Placeholder TV", // pii-test-fixture: invented device name
                        "platform": "tvOS", "state": "paused", "local": true },
            "Session": { "id": "sess-b", "bandwidth": 40_000, "location": "lan" }
        })
    }

    /// Direct STREAM: a `TranscodeSession` exists but nothing is re-encoded.
    fn direct_stream_movie() -> Value {
        json!({
            "type": "movie",
            "sessionKey": "103",
            "title": "Another Placeholder Picture", // pii-test-fixture: invented title, not from the live library
            "duration": 5_000_000,
            "viewOffset": 0,
            "User": { "id": "9", "title": "household-member-c" }, // pii-test-fixture: invented user, not a real household account
            "Player": { "title": "Placeholder Browser", "state": "buffering" }, // pii-test-fixture: invented device name
            "TranscodeSession": {
                "videoDecision": "copy", "audioDecision": "copy",
                "protocol": "dash", "progress": 4.0
            }
        })
    }

    fn container(items: Vec<Value>) -> Value {
        json!({ "MediaContainer": { "size": items.len(), "Metadata": items } })
    }

    fn identity_payload() -> Value {
        json!({ "MediaContainer": {
            "machineIdentifier": "MACHINE_ID_PLACEHOLDER",
            "version": "1.42.0.0", "apiVersion": "1.1.1" } })
    }

    /// A source that COUNTS how many times it was asked. The entitlement test
    /// asserts this stays at zero — a gate that returned an empty result after
    /// fetching would still have leaked timing/load, and would pass a weaker
    /// assertion.
    struct CountingSource {
        sessions: AtomicUsize,
        identity: AtomicUsize,
        payload: Result<Value, PlexSessionsError>,
    }

    impl CountingSource {
        fn ok(payload: Value) -> Arc<Self> {
            Arc::new(Self {
                sessions: AtomicUsize::new(0),
                identity: AtomicUsize::new(0),
                payload: Ok(payload),
            })
        }
        fn failing(err: PlexSessionsError) -> Arc<Self> {
            Arc::new(Self {
                sessions: AtomicUsize::new(0),
                identity: AtomicUsize::new(0),
                payload: Err(err),
            })
        }
        fn calls(&self) -> usize {
            self.sessions.load(Ordering::SeqCst) + self.identity.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SessionSource for CountingSource {
        async fn sessions(&self) -> Result<Value, PlexSessionsError> {
            self.sessions.fetch_add(1, Ordering::SeqCst);
            self.payload.clone()
        }
        async fn identity(&self) -> Result<Value, PlexSessionsError> {
            self.identity.fetch_add(1, Ordering::SeqCst);
            Ok(identity_payload())
        }
    }

    /// An entitled caller. Uses the `cfg(test)`-only constructor rather than
    /// standing up a gateway; the PRODUCTION boundary (only the gateway can
    /// mint one) is proven by the `compile_fail` doctest on `CallerContext`.
    fn entitled() -> CallerContext {
        CallerContext::entitled_for_test_only(true, true)
    }

    // ── decision parsing ───────────────────────────────────────────────────

    #[test]
    fn transcode_and_direct_play_are_both_parsed_from_one_container() {
        let raw = container(vec![transcoding_episode(), direct_play_movie()]);
        let parsed: Vec<Value> = session_items(&raw).into_iter().map(session_json).collect();
        assert_eq!(parsed.len(), 2);

        assert_eq!(parsed[0]["decision"], "transcode");
        assert_eq!(parsed[0]["video_decision"], "copy");
        assert_eq!(parsed[0]["audio_decision"], "transcode");
        let reason = parsed[0]["transcode_reason"].as_str().expect("a transcode has a reason");
        assert!(reason.contains("audio eac3 -> opus"), "reason was {reason}");
        assert!(reason.contains("subtitles"), "reason was {reason}");

        assert_eq!(parsed[1]["decision"], "direct_play");
        assert_eq!(parsed[1]["transcode_reason"], Value::Null);
    }

    #[test]
    fn plexs_own_transcode_reason_wins_over_the_derived_one() {
        let mut item = transcoding_episode();
        item["TranscodeSession"]["transcodeReason"] =
            json!("Conversion of the source audio is required");
        let parsed = session_json(&item);
        assert_eq!(parsed["transcode_reason"], "Conversion of the source audio is required");
    }

    #[test]
    fn a_part_decision_without_a_transcode_session_is_still_believed() {
        // Defensive: absence of TranscodeSession normally means direct play,
        // but Plex states the decision on the Part too. If the two ever
        // disagree, an explicit "transcode" must not be flattened to
        // "direct play" — the optimistic answer is the misleading one.
        let mut item = direct_play_movie();
        item["Media"][0]["Part"][0]["decision"] = json!("transcode");
        assert_eq!(session_json(&item)["decision"], "transcode");

        item["Media"][0]["Part"][0]["decision"] = json!("copy");
        assert_eq!(session_json(&item)["decision"], "direct_stream");

        item["Media"][0]["Part"][0]["decision"] = json!("directplay");
        assert_eq!(session_json(&item)["decision"], "direct_play");
    }

    #[test]
    fn copy_on_both_streams_is_direct_stream_not_transcode() {
        let parsed = session_json(&direct_stream_movie());
        assert_eq!(parsed["decision"], "direct_stream");
        // The live server's session had videoDecision=copy WITH
        // audioDecision=transcode; that must stay a transcode.
        assert_eq!(session_json(&transcoding_episode())["decision"], "transcode");
    }

    // ── episode vs movie shapes ────────────────────────────────────────────

    #[test]
    fn episode_and_movie_titles_both_render_from_their_own_nesting() {
        let ep = session_json(&transcoding_episode());
        assert_eq!(ep["media_type"], "episode");
        assert_eq!(ep["show_title"], "Placeholder Show");
        assert_eq!(ep["season"], 4);
        assert_eq!(ep["episode"], 12);
        assert_eq!(ep["full_title"], "Placeholder Show - S04E12 - The Placeholder Episode");

        let mv = session_json(&direct_play_movie());
        assert_eq!(mv["media_type"], "movie");
        assert_eq!(mv["show_title"], Value::Null);
        assert_eq!(mv["season"], Value::Null);
        assert_eq!(mv["full_title"], "A Placeholder Motion Picture (2025)");
    }

    #[test]
    fn progress_user_and_player_are_carried_through() {
        let ep = session_json(&transcoding_episode());
        assert_eq!(ep["progress_ms"], 600_000);
        assert_eq!(ep["duration_ms"], 2_400_000);
        assert_eq!(ep["progress_percent"], 25.0);
        assert_eq!(ep["user"], "household-member-a");
        assert_eq!(ep["user_id"], "42");
        assert_eq!(ep["player"], "Placeholder Player");
        assert_eq!(ep["player_state"], "playing");
        assert_eq!(ep["bandwidth_kbps"], 8906);
        assert_eq!(ep["stream_location"], "wan");

        // Plex sends User.id as a bare number in some payloads and a string in
        // others; both must land as the same contract type.
        assert_eq!(session_json(&direct_play_movie())["user_id"], "7");
    }

    #[test]
    fn a_household_ip_address_is_never_carried_into_the_payload() {
        let mut item = transcoding_episode();
        item["Player"]["address"] = json!("<internal-ip>"); // pii-test-fixture: a synthetic RFC1918 address, present so the test can prove it is DROPPED
        item["Player"]["remotePublicAddress"] = json!("203.0.113.9");
        let rendered = session_json(&item).to_string();
        assert!(!rendered.contains("<internal-ip>")); // pii-test-fixture: same synthetic address, asserted absent from the payload
        assert!(!rendered.contains("203.0.113.9"));
    }

    // ── the three outcomes, end to end through the tool ────────────────────

    #[tokio::test]
    async fn entitled_caller_gets_full_session_detail() {
        // POSITIVE CONTROL. A version that silently returns nothing for
        // everybody cannot pass this.
        let src = CountingSource::ok(container(vec![transcoding_episode()]));
        let tool = MediaNowPlaying::with_source(src.clone());

        let out = tool.execute_with_caller(json!({}), entitled()).await.unwrap();
        let p = out.structured.expect("entitled callers get structured detail");

        assert_eq!(p["status"], "playing");
        assert_eq!(p["ok"], true);
        assert_eq!(p["session_count"], 1);
        assert_eq!(p["total_bandwidth_kbps"], 8906);
        assert_eq!(p["sessions"][0]["full_title"], "Placeholder Show - S04E12 - The Placeholder Episode");
        assert_eq!(p["sessions"][0]["user"], "household-member-a");
        assert_eq!(p["sessions"][0]["player"], "Placeholder Player");
        assert_eq!(p["sessions"][0]["decision"], "transcode");
        assert!(p["sessions"][0]["transcode_reason"].is_string());
        assert_eq!(p["server"]["version"], "1.42.0.0");
        assert!(!out.text.is_empty());
    }

    #[tokio::test]
    async fn zero_sessions_is_idle_and_is_not_a_failure() {
        let src = CountingSource::ok(json!({ "MediaContainer": { "size": 0 } }));
        let tool = MediaNowPlaying::with_source(src);

        let p = tool
            .execute_with_caller(json!({}), entitled())
            .await
            .unwrap()
            .structured
            .unwrap();

        assert_eq!(p["status"], "idle");
        assert_eq!(p["ok"], true);
        assert_eq!(p["session_count"], 0);
        assert_eq!(p["sessions"], json!([]));
        assert_eq!(p["total_bandwidth_kbps"], Value::Null);
    }

    #[tokio::test]
    async fn unreachable_token_rejected_and_idle_never_render_identically() {
        let idle = MediaNowPlaying::with_source(CountingSource::ok(json!({
            "MediaContainer": { "size": 0 }
        })))
        .run(entitled())
        .await;
        let down = MediaNowPlaying::with_source(CountingSource::failing(
            PlexSessionsError::Unreachable("could not connect to Plex".into()),
        ))
        .run(entitled())
        .await;
        let refused = MediaNowPlaying::with_source(CountingSource::failing(
            PlexSessionsError::TokenRejected("Plex rejected the configured token (HTTP 401)".into()),
        ))
        .run(entitled())
        .await;

        assert_eq!(idle["status"], "idle");
        assert_eq!(down["status"], "unreachable");
        assert_eq!(refused["status"], "unauthorized");

        // Distinct on every axis a consumer could key on.
        for (a, b) in [(&idle, &down), (&idle, &refused), (&down, &refused)] {
            assert_ne!(a["status"], b["status"]);
            assert_ne!(a["message"], b["message"]);
            assert_ne!(a.to_string(), b.to_string());
        }

        // A failed read must NEVER be mistakable for an empty house.
        assert_eq!(down["ok"], false);
        assert_eq!(refused["ok"], false);
        assert!(down.get("session_count").is_none());
        assert!(refused.get("session_count").is_none());
        assert!(down.get("sessions").is_none());
        assert!(refused.get("sessions").is_none());
    }

    #[tokio::test]
    async fn a_failed_identity_lookup_never_spoils_a_good_session_read() {
        struct IdentityFails(Value);
        #[async_trait]
        impl SessionSource for IdentityFails {
            async fn sessions(&self) -> Result<Value, PlexSessionsError> {
                Ok(self.0.clone())
            }
            async fn identity(&self) -> Result<Value, PlexSessionsError> {
                Err(PlexSessionsError::Unreachable("no answer from Plex".into()))
            }
        }

        let tool = MediaNowPlaying::with_source(Arc::new(IdentityFails(container(vec![
            direct_play_movie(),
        ]))));
        let p = tool.run(entitled()).await;
        assert_eq!(p["status"], "playing");
        assert_eq!(p["server"], Value::Null);
    }

    #[tokio::test]
    async fn not_configured_is_its_own_outcome() {
        let tool = MediaNowPlaying { source: None };
        let p = tool.run(entitled()).await;
        assert_eq!(p["status"], "not_configured");
        assert_eq!(p["ok"], false);
        assert!(p.get("session_count").is_none());
    }

    // ── entitlement ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn unentitled_caller_gets_nothing_and_no_plex_request_is_issued() {
        let cases = [
            ("untrusted", CallerContext::untrusted()),
            ("default", CallerContext::default()),
            ("calendar only", CallerContext::entitled_for_test_only(true, false)),
            ("routine only", CallerContext::entitled_for_test_only(false, true)),
        ];

        for (label, caller) in cases {
            let src = CountingSource::ok(container(vec![transcoding_episode()]));
            let tool = MediaNowPlaying::with_source(src.clone());

            let out = tool.execute_with_caller(json!({}), caller).await.unwrap();
            let p = out.structured.expect("even a refusal is structured");

            assert_eq!(p["status"], "forbidden", "{label}");
            assert_eq!(p["ok"], false, "{label}");
            assert_eq!(src.calls(), 0, "{label}: an unentitled call must not touch Plex");

            // Nothing about the household leaks — not a title, a username, a
            // device name, and not even a count, which alone reveals occupancy.
            let rendered = format!("{}{}", out.text, p);
            for secret in [
                "Placeholder Show",
                "The Placeholder Episode",
                "household-member-a",
                "Placeholder Player",
            ] {
                assert!(!rendered.contains(secret), "{label}: leaked {secret}");
            }
            assert!(p.get("session_count").is_none(), "{label}: leaked a count");
            assert!(p.get("sessions").is_none(), "{label}: leaked sessions");
            assert!(p.get("server").is_none(), "{label}: leaked the server header");
        }
    }

    #[tokio::test]
    async fn the_unthreaded_execute_path_is_unentitled() {
        let src = CountingSource::ok(container(vec![transcoding_episode()]));
        let tool = MediaNowPlaying::with_source(src.clone());

        let text = tool.execute(json!({})).await.unwrap();
        let p: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(p["status"], "forbidden");
        assert_eq!(src.calls(), 0);
    }

    #[test]
    fn the_gate_predicate_requires_both_operator_signals() {
        assert!(may_see_now_playing(CallerContext::entitled_for_test_only(true, true)));
        assert!(!may_see_now_playing(CallerContext::entitled_for_test_only(true, false)));
        assert!(!may_see_now_playing(CallerContext::entitled_for_test_only(false, true)));
        assert!(!may_see_now_playing(CallerContext::untrusted()));
    }

    // ── caching ────────────────────────────────────────────────────────────

    #[test]
    fn the_live_tool_is_never_served_from_cache() {
        assert!(crate::tool_cache::policy_for(TOOL_NAME).is_none());
        assert!(
            crate::tool_cache::is_never_cached(TOOL_NAME),
            "must be caught by the RULE, not merely by having no matching prefix"
        );
    }

    // ── registration ───────────────────────────────────────────────────────

    #[test]
    fn tool_metadata_and_registration() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains(TOOL_NAME));

        let tool = MediaNowPlaying::from_env();
        assert_eq!(tool.name(), TOOL_NAME);
        assert!(!tool.description().is_empty());
        assert_eq!(tool.parameters()["type"], "object");
    }
}
