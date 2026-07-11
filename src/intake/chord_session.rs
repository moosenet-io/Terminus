//! RESIL-03: sweep registers, checkpoints to, and auto-resumes from Chord's
//! session cache. Terminus TERM/Plane CHRD #51.
//!
//! ## Why this exists
//! The coder and assistant sweeps already resume locally via
//! [`super::checkpoint::FileCheckpoint`] — a JSON-lines file on the reliable
//! NAS staging dir. That stays the fast, primary local resume path. This
//! module adds a SECOND, independent durability signal: Chord's session
//! cache (`POST /api/sweep/session`, `GET /api/sweep/session/:id`,
//! `POST /api/sweep/session/:id/advance`), so a restart can resume even if
//! the local staging dir was lost, and vice versa — the file checkpoint
//! covers a Chord outage or a fresh `CHORD_CONTROL_URL`.
//!
//! Neither source is authoritative over the other: a caller reconciles BOTH
//! and skips a unit of work if EITHER marks it done (see the module doc on
//! reconciliation in `mod.rs`'s sweep wiring). This module only speaks to
//! Chord; it has no opinion on the file checkpoint.
//!
//! ## Endpoint shape (Chord `src/control.rs`, read-only reference — this repo
//! does not modify Chord), JWT-gated the SAME way as everywhere else Chord is
//! called from this repo (see [`super::gpu_authority`]'s `chord_call` and
//! [`super::chord_pull`], the two precedents this module matches):
//! - `POST {CHORD_CONTROL_URL}/api/sweep/session` body
//!   `{"session_id":"<str>","queue":["<key>",...]}` → 200
//!   `{session_id,total,done_count,remaining:[...]}`. Idempotent: the SAME
//!   queue is a no-op that preserves progress; a DIFFERENT queue replaces it
//!   and resets progress.
//! - `GET {CHORD_CONTROL_URL}/api/sweep/session/:id` → 200 (same shape) or
//!   `404` (unknown session id).
//! - `POST {CHORD_CONTROL_URL}/api/sweep/session/:id/advance` body
//!   `{"keys":["<key>",...]}` → 200 (same shape) or `404`. Append-only,
//!   idempotent (a key marked twice is a no-op); keys not in the registered
//!   queue are ignored.
//!
//! ## Convention reused, not reinvented
//! - Base URL: [`crate::config::chord_control_url`] — the SAME env var
//!   (`CHORD_CONTROL_URL`) [`super::chord_pull::fetch_model`] and
//!   `serving_tools::ServingProfileRefresh` already use. `None` ⇒
//!   [`NotConfigured`], never a guessed host, never an internet call — the
//!   ONLY remote calls this module makes are to `CHORD_CONTROL_URL` itself.
//! - JWT sourcing: `CHORD_JWT`, read the exact same way as
//!   `gpu_authority::chord_auth_token` / `chord_pull::chord_auth_token`
//!   (trimmed, empty ⇒ no token).
//! - Every public entry point is soft-fail: an unconfigured or unreachable
//!   Chord NEVER turns into a panic or a propagated hard failure a sweep
//!   can't continue past. Callers are expected to log once and fall back to
//!   file-checkpoint-only durability (see `mod.rs`).

use std::time::Duration;

use crate::config;

/// A single unit-of-work identifier as registered in / reported back by a
/// Chord sweep session. Kept as a plain `String` (not a richer struct) so
/// this module stays agnostic to whether the caller is the coder or the
/// assistant sweep — callers derive the exact same stable string their file
/// checkpoint already keys on (see [`action_key`]) so Chord's `done` and the
/// file checkpoint's `done` describe the identical units.
pub type ActionKey = String;

/// `CHORD_CONTROL_URL` is unset — a session call cannot even be attempted.
/// Mirrors [`super::chord_pull::NotConfigured`] exactly (same shape, same
/// "caller misconfiguration, not something Chord reported" meaning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotConfigured(pub String);

impl std::fmt::Display for NotConfigured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Every non-success outcome a session call can resolve to. Unlike
/// [`super::chord_pull::PullOutcome`] (which has caller-actionable distinct
/// variants like `InsufficientDiskSpace`), a session-cache call has exactly
/// one caller-visible behavior for every failure mode: soft-fail and fall
/// back to the file checkpoint. Kept as a small enum (rather than a single
/// stringly-typed error) purely so log lines and tests can distinguish WHY,
/// without giving callers a reason to branch differently on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// `CHORD_CONTROL_URL` unset.
    NotConfigured(String),
    /// Transport-level failure (connection refused/timeout/DNS) — Chord's
    /// control API is not reachable at all.
    Unreachable(String),
    /// Chord responded but with an unexpected status/body (not the documented
    /// 200/404 shapes).
    Failed(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::NotConfigured(d) => write!(f, "not configured: {d}"),
            SessionError::Unreachable(d) => write!(f, "unreachable: {d}"),
            SessionError::Failed(d) => write!(f, "failed: {d}"),
        }
    }
}

impl From<NotConfigured> for SessionError {
    fn from(e: NotConfigured) -> Self {
        SessionError::NotConfigured(e.0)
    }
}

/// The `{session_id,total,done_count,remaining}` summary Chord returns from
/// every one of the three endpoints.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub total: usize,
    pub done_count: usize,
    pub remaining: Vec<ActionKey>,
}

/// Bearer token for Chord's JWT auth (`CHORD_JWT`). Sourced identically to
/// `chord_pull::chord_auth_token` / `gpu_authority::chord_auth_token` — same
/// env var, same trim-then-empty-is-none rule.
fn chord_auth_token() -> Option<String> {
    std::env::var("CHORD_JWT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Request timeout for session calls. These are cheap metadata round trips
/// (unlike `chord_pull`'s multi-GB model pull), so a short default is
/// appropriate — deliberately much shorter than `chord_pull::fetch_timeout`'s
/// 600s default. From `MINT_SWEEP_SESSION_TIMEOUT_SECS`, default 10.
fn session_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("MINT_SWEEP_SESSION_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(10),
    )
}

fn base_url() -> Result<String, NotConfigured> {
    config::chord_control_url().ok_or_else(|| {
        NotConfigured(
            "CHORD_CONTROL_URL not set — sweep session cache requires Chord's control endpoint"
                .into(),
        )
    })
}

fn client() -> Result<reqwest::Client, SessionError> {
    reqwest::Client::builder()
        .timeout(session_timeout())
        .build()
        .map_err(|e| SessionError::Failed(format!("http client build failed: {e}")))
}

fn attach_auth(mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    if let Some(t) = chord_auth_token() {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    req
}

/// Map a completed HTTP response to a `Result<Option<SessionSummary>, SessionError>`.
/// `Ok(None)` is reserved for a `404` on the two id-scoped endpoints (unknown
/// session — see [`remaining`]); `register` never expects a 404 (it creates
/// the session) so it discards the `None` case at the call site.
async fn interpret(resp: reqwest::Response) -> Result<Option<SessionSummary>, SessionError> {
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    interpret_status_body(status, &body_text)
}

/// Pure: map an HTTP status + raw body text to a session outcome. Split from
/// the network call (mirrors `chord_pull::interpret_response`'s split) so the
/// status/body → outcome mapping is unit-testable without a live Chord
/// instance or a mock HTTP server.
fn interpret_status_body(status: u16, body_text: &str) -> Result<Option<SessionSummary>, SessionError> {
    if status == 404 {
        return Ok(None);
    }
    if !(200..=299).contains(&status) {
        return Err(SessionError::Failed(format!(
            "HTTP {status}: {}",
            body_text.chars().take(200).collect::<String>()
        )));
    }
    match serde_json::from_str::<SessionSummary>(body_text) {
        Ok(s) => Ok(Some(s)),
        Err(e) => Err(SessionError::Failed(format!("unparseable response body: {e}"))),
    }
}

fn map_transport_err(e: reqwest::Error) -> SessionError {
    SessionError::Unreachable(format!("chord control endpoint unreachable: {e}"))
}

/// `POST {CHORD_CONTROL_URL}/api/sweep/session` — register (or idempotently
/// re-register) `session_id`'s planned `queue`. Same queue = no-op preserving
/// progress; different queue = replace + reset (Chord-side behavior, not
/// something this client can or needs to special-case).
///
/// Soft-fail contract: `Err` here is ALWAYS one of "not configured" or
/// "unreachable/failed" — never a panic. Callers (see `mod.rs`) log this once
/// and fall back to file-checkpoint-only durability; a Chord outage must
/// never stop a sweep from starting.
pub async fn register(session_id: &str, queue: &[ActionKey]) -> Result<SessionSummary, SessionError> {
    let base = base_url()?;
    let url = format!("{}/api/sweep/session", base.trim_end_matches('/'));
    let c = client()?;
    let body = serde_json::json!({ "session_id": session_id, "queue": queue });
    let req = attach_auth(c.post(&url).json(&body));
    let resp = req.send().await.map_err(map_transport_err)?;
    match interpret(resp).await? {
        Some(s) => Ok(s),
        None => Err(SessionError::Failed(
            "unexpected 404 registering a sweep session".into(),
        )),
    }
}

/// `GET {CHORD_CONTROL_URL}/api/sweep/session/:id` — the current remaining
/// queue for `session_id`. `Ok(None)` means "unknown to Chord" (404 — e.g.
/// never registered, or Chord's cache was reset): callers must NOT treat this
/// as an error, just as "no Chord-side resume signal available", and fall
/// back to whatever the file checkpoint says.
pub async fn remaining(session_id: &str) -> Result<Option<Vec<ActionKey>>, SessionError> {
    let base = base_url()?;
    let url = format!(
        "{}/api/sweep/session/{}",
        base.trim_end_matches('/'),
        percent_encode_path_segment(session_id)
    );
    let c = client()?;
    let req = attach_auth(c.get(&url));
    let resp = req.send().await.map_err(map_transport_err)?;
    Ok(interpret(resp).await?.map(|s| s.remaining))
}

/// `POST {CHORD_CONTROL_URL}/api/sweep/session/:id/advance` — append-only,
/// idempotent: `keys` already marked done, or not present in the registered
/// queue, are ignored by Chord. `Ok(None)` on a 404 (unknown session — e.g.
/// Chord's cache was reset mid-run after `register` succeeded); callers treat
/// this identically to any other soft failure — best-effort, log, continue.
pub async fn advance(
    session_id: &str,
    keys: &[ActionKey],
) -> Result<Option<SessionSummary>, SessionError> {
    let base = base_url()?;
    let url = format!(
        "{}/api/sweep/session/{}/advance",
        base.trim_end_matches('/'),
        percent_encode_path_segment(session_id)
    );
    let c = client()?;
    let body = serde_json::json!({ "keys": keys });
    let req = attach_auth(c.post(&url).json(&body));
    let resp = req.send().await.map_err(map_transport_err)?;
    interpret(resp).await
}

/// Percent-encode `s` for safe use as exactly ONE path segment. Mirrors
/// `chord_pull::percent_encode_path_segment` verbatim (same RFC 3986
/// unreserved set) — a `session_id` derived from a hash (see
/// [`derive_session_id`]) is always URL-safe in practice, but this keeps the
/// client correct even if that ever changes, and matches this repo's
/// established convention for one-path-segment Chord calls.
fn percent_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ===========================================================================
// Stable identity derivation — session_id + ActionKey
// ===========================================================================

/// Derive a STABLE Chord session id from a sweep's identity: the harness
/// epoch, the run kind (`"coder"` / `"assistant"`), and a stable hash of the
/// planned queue's contents. Stable across a restart with the SAME planned
/// queue (a fresh process re-derives the identical id, so it re-attaches to
/// the SAME Chord-side session rather than fragmenting into a new one every
/// run) while still changing when the actual planned work changes (a
/// materially different queue — e.g. the fleet or `--only-stale` selection
/// changed — naturally gets a fresh session, matching Chord's own "different
/// queue = replace + reset" semantics rather than fighting it).
///
/// Uses SHA-1 (via `sha1_smol`, already this repo's convention for a stable,
/// build-independent digest — see `plane::mod::redis_cache_key`'s doc for why
/// `std`'s `DefaultHasher` is explicitly wrong here: it is NOT stable across
/// Rust versions/builds, which would silently fragment resume identity across
/// a routine binary rebuild).
pub fn derive_session_id(epoch: &str, run_kind: &str, queue: &[ActionKey]) -> String {
    let mut h = sha1_smol::Sha1::new();
    // Queue order is part of the sweep's own contract (both sweeps build it
    // deterministically from a sorted grid), so hashing in-order is stable
    // and also naturally distinguishes a same-membership-different-order
    // queue — which would represent evolved intake logic, not merely a
    // process restart.
    for key in queue {
        h.update(key.as_bytes());
        h.update(b"\0");
    }
    format!("mint-{run_kind}-{epoch}-{}", h.digest())
}

/// Derive the stable [`ActionKey`] string for one unit of work — the SAME
/// shape a caller's file checkpoint key already serializes to conceptually
/// (`"<run_kind>|<model>|<backend>|<case>"`), so Chord's `done` set and the
/// file checkpoint's `done` set describe the identical units and either can
/// be used to skip the other's re-run. `case` is optional (the coder sweep's
/// checkpoint unit is a whole `(model, backend)` pass, with no per-case
/// granularity — see `coder_sweep::CodeCheckpointKey`); omitted when `None`
/// rather than serialized as an empty segment, so the coder key
/// (`"coder|model|gpu"`) doesn't collide with a hypothetical
/// `"coder|model|gpu|"` from a caller that always passes `Some("")`.
pub fn action_key(run_kind: &str, model: &str, backend: &str, case: Option<&str>) -> ActionKey {
    match case {
        Some(c) => format!("{run_kind}|{model}|{backend}|{c}"),
        None => format!("{run_kind}|{model}|{backend}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- percent_encode_path_segment (mirrors chord_pull's exact behavior) ----

    #[test]
    fn percent_encode_leaves_session_id_shape_untouched() {
        // derive_session_id output is exactly this alphabet — confirms the
        // common case never gets escaped.
        let id = derive_session_id("v3", "coder", &["coder|m:8b|gpu".to_string()]);
        assert_eq!(percent_encode_path_segment(&id), id);
    }

    #[test]
    fn percent_encode_escapes_unsafe_characters() {
        assert_eq!(percent_encode_path_segment("a/b?c"), "a%2Fb%3Fc");
    }

    // ---- derive_session_id: stability + sensitivity ----

    #[test]
    fn session_id_is_stable_across_a_simulated_restart() {
        let queue = vec![
            "coder|m1:8b|gpu".to_string(),
            "coder|m2:8b|cpu".to_string(),
        ];
        let a = derive_session_id("v3", "coder", &queue);
        // Simulate a fresh process: re-derive from the SAME planned queue.
        let b = derive_session_id("v3", "coder", &queue);
        assert_eq!(a, b, "same epoch+run_kind+queue must re-derive the identical session id");
        assert!(a.starts_with("mint-coder-v3-"));
    }

    #[test]
    fn session_id_differs_across_run_kind_and_epoch_and_queue_contents() {
        let queue = vec!["coder|m1:8b|gpu".to_string()];
        let base = derive_session_id("v3", "coder", &queue);
        assert_ne!(base, derive_session_id("v4", "coder", &queue), "epoch bump must yield a new session");
        assert_ne!(base, derive_session_id("v3", "assistant", &queue), "run_kind must be part of the identity");
        assert_ne!(
            base,
            derive_session_id("v3", "coder", &["coder|m2:8b|gpu".to_string()]),
            "a materially different queue must yield a new session"
        );
    }

    #[test]
    fn session_id_is_order_sensitive() {
        let a = derive_session_id(
            "v3",
            "coder",
            &["coder|m1:8b|gpu".to_string(), "coder|m2:8b|gpu".to_string()],
        );
        let b = derive_session_id(
            "v3",
            "coder",
            &["coder|m2:8b|gpu".to_string(), "coder|m1:8b|gpu".to_string()],
        );
        assert_ne!(a, b);
    }

    // ---- action_key: stable string shape ----

    #[test]
    fn action_key_shape_matches_checkpoint_convention() {
        assert_eq!(action_key("coder", "qwen3:8b", "gpu", None), "coder|qwen3:8b|gpu");
        assert_eq!(
            action_key("assistant", "qwen3:8b", "cpu", Some("recall")),
            "assistant|qwen3:8b|cpu|recall"
        );
    }

    #[test]
    fn action_key_none_case_never_collides_with_empty_string_case() {
        assert_ne!(action_key("coder", "m", "gpu", None), action_key("coder", "m", "gpu", Some("")));
    }

    // ---- chord_auth_token: mirrors chord_pull's exact rules ----

    #[test]
    #[serial_test::serial]
    fn chord_auth_token_trims_and_treats_blank_as_none() {
        std::env::set_var("CHORD_JWT", "  tok  ");
        assert_eq!(chord_auth_token(), Some("tok".to_string()));
        std::env::set_var("CHORD_JWT", "   ");
        assert_eq!(chord_auth_token(), None);
        std::env::remove_var("CHORD_JWT");
        assert_eq!(chord_auth_token(), None);
    }

    // ---- session_timeout: default + override + non-numeric/zero rejection ----

    #[test]
    #[serial_test::serial]
    fn session_timeout_defaults_and_overrides() {
        std::env::remove_var("MINT_SWEEP_SESSION_TIMEOUT_SECS");
        assert_eq!(session_timeout(), Duration::from_secs(10));
        std::env::set_var("MINT_SWEEP_SESSION_TIMEOUT_SECS", "0");
        assert_eq!(session_timeout(), Duration::from_secs(10));
        std::env::set_var("MINT_SWEEP_SESSION_TIMEOUT_SECS", "bogus");
        assert_eq!(session_timeout(), Duration::from_secs(10));
        std::env::set_var("MINT_SWEEP_SESSION_TIMEOUT_SECS", "3");
        assert_eq!(session_timeout(), Duration::from_secs(3));
        std::env::remove_var("MINT_SWEEP_SESSION_TIMEOUT_SECS");
    }

    // ---- register/remaining/advance: NotConfigured path (no network needed) ----

    #[tokio::test]
    #[serial_test::serial]
    async fn register_not_configured_when_control_url_unset() {
        std::env::remove_var("CHORD_CONTROL_URL");
        let err = register("sid", &["k1".to_string()]).await.unwrap_err();
        assert!(matches!(err, SessionError::NotConfigured(_)));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn remaining_not_configured_when_control_url_unset() {
        std::env::remove_var("CHORD_CONTROL_URL");
        let err = remaining("sid").await.unwrap_err();
        assert!(matches!(err, SessionError::NotConfigured(_)));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn advance_not_configured_when_control_url_unset() {
        std::env::remove_var("CHORD_CONTROL_URL");
        let err = advance("sid", &["k1".to_string()]).await.unwrap_err();
        assert!(matches!(err, SessionError::NotConfigured(_)));
    }

    // ---- unreachable path: never hangs (bounded by session_timeout), never panics ----

    #[tokio::test]
    #[serial_test::serial]
    async fn register_unreachable_when_control_url_points_nowhere() {
        std::env::set_var("CHORD_CONTROL_URL", "http://127.0.0.1:1");
        std::env::set_var("MINT_SWEEP_SESSION_TIMEOUT_SECS", "2");
        let err = register("sid", &["k1".to_string()]).await.unwrap_err();
        std::env::remove_var("CHORD_CONTROL_URL");
        std::env::remove_var("MINT_SWEEP_SESSION_TIMEOUT_SECS");
        assert!(matches!(err, SessionError::Unreachable(_)), "got {err:?}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn advance_unreachable_is_soft_failure_never_fatal() {
        // NEGATIVE test: an unreachable Chord must resolve to a plain `Err`
        // value the caller can log-and-continue on — never a panic, never a
        // hang past the bounded timeout.
        std::env::set_var("CHORD_CONTROL_URL", "http://127.0.0.1:1");
        std::env::set_var("MINT_SWEEP_SESSION_TIMEOUT_SECS", "2");
        let result = advance("sid", &["k1".to_string()]).await;
        std::env::remove_var("CHORD_CONTROL_URL");
        std::env::remove_var("MINT_SWEEP_SESSION_TIMEOUT_SECS");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SessionError::Unreachable(_)));
    }

    // ---- interpret_status_body: pure status/body -> outcome mapping ----

    #[test]
    fn interpret_404_is_ok_none() {
        let out = interpret_status_body(404, "not found").unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn interpret_200_parses_summary() {
        let body = json!({
            "session_id": "sid",
            "total": 3,
            "done_count": 1,
            "remaining": ["a", "b"]
        })
        .to_string();
        let out = interpret_status_body(200, &body).unwrap().unwrap();
        assert_eq!(out.session_id, "sid");
        assert_eq!(out.total, 3);
        assert_eq!(out.done_count, 1);
        assert_eq!(out.remaining, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn interpret_500_is_failed() {
        let err = interpret_status_body(500, "boom").unwrap_err();
        assert!(matches!(err, SessionError::Failed(_)));
    }

    #[test]
    fn interpret_200_with_unparseable_body_is_failed_not_panic() {
        let err = interpret_status_body(200, "not json").unwrap_err();
        assert!(matches!(err, SessionError::Failed(_)));
    }

    // ---- reconciliation semantics used by mod.rs's sweep wiring ----
    // (pure helper mirrored here as a documentation-by-test of the contract;
    //  the actual reconciliation call sites live in coder_sweep.rs / mod.rs.)

    #[test]
    fn resume_prefers_the_more_complete_signal() {
        use std::collections::BTreeSet;

        let file_done: BTreeSet<ActionKey> =
            ["coder|m1:8b|gpu".to_string()].into_iter().collect();
        let chord_remaining: Vec<ActionKey> =
            vec!["coder|m1:8b|gpu".to_string(), "coder|m2:8b|gpu".to_string()];
        let planned: Vec<ActionKey> = vec![
            "coder|m1:8b|gpu".to_string(),
            "coder|m2:8b|gpu".to_string(),
            "coder|m3:8b|gpu".to_string(),
        ];

        // A unit is skippable iff EITHER source marks it done: file_done
        // contains it, OR it is planned but absent from chord's remaining
        // list (i.e. chord considers it already advanced).
        let chord_remaining_set: BTreeSet<&ActionKey> = chord_remaining.iter().collect();
        let skip = |k: &ActionKey| {
            file_done.contains(k) || !chord_remaining_set.contains(k)
        };

        assert!(skip(&"coder|m1:8b|gpu".to_string()), "file checkpoint marks it done");
        assert!(!skip(&"coder|m2:8b|gpu".to_string()), "neither source marks it done");
        assert!(skip(&"coder|m3:8b|gpu".to_string()), "chord's remaining omits it -> chord considers it done");
    }
}
