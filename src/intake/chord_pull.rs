//! MINT Phase 5: delegate model re-pull/re-quantize to Chord's existing
//! `PullCoordinator` over its authenticated control-API endpoint
//! (`POST /api/models/:name/pull`).
//!
//! ## Why this exists
//! `mint fetch-model --model=X` is the operator/CLI front door; [`breakfix`]
//! (MINT Phase 4) wires the SAME call into its bounded diagnostic toolkit —
//! today breakfix can only try an alternate config or drop/escalate, with no
//! way to say "maybe the model itself is missing or corrupt on this host, try
//! re-pulling it from the archive." Neither caller talks to Chord directly;
//! both go through [`fetch_model`].
//!
//! ## Endpoint shape (Chord `src/control.rs::pull_model`, read-only reference —
//! this repo does not modify Chord)
//! `POST {CHORD_CONTROL_URL}/api/models/{name}/pull`, Bearer-JWT gated (same
//! `CHORD_JWT_SECRET`/`sub:"lumina"` scheme as everywhere else Chord is
//! called from this repo — see [`super::gpu_authority`]'s `chord_call` for the
//! precedent this module matches). On success: `200` with
//! `{"status":"warm","model":"<name>"}`. Chord's `ensure_local` internally
//! dedups concurrent pulls of the SAME model behind a per-model lock — a
//! caller racing another pull of the same model simply blocks (server-side)
//! until the first completes, then also sees `200`; there is no distinct
//! "already warm" vs. "freshly pulled" signal in the response, and no `409`
//! for a concurrent same-model pull (unlike the `/v1/gpu-exclusive` API's
//! conflict semantics) — so [`PullOutcome::Warmed`] intentionally covers BOTH.
//! Errors: `404` (unknown model in the registry, or known but missing from
//! the archive), `507` (insufficient local disk space), `401` (bad/missing
//! JWT — Chord's control router never returns `403` for this, only `401`),
//! anything else is a generic failure.
//!
//! ## Convention reused, not reinvented
//! - Base URL: [`crate::config::chord_control_url`] — the SAME env var
//!   (`CHORD_CONTROL_URL`) `serving_tools::ServingProfileRefresh` already uses
//!   for Chord's control API (a DIFFERENT port from the `/v1/gpu-exclusive`
//!   proxy-port endpoints `gpu_authority` calls). `None` ⇒ `NotConfigured`,
//!   never a guessed host.
//! - JWT sourcing: `CHORD_JWT`, read the exact same way as
//!   `gpu_authority::chord_auth_token` (trimmed, empty ⇒ no token — matches
//!   Chord's own auth_check, which disables auth entirely when its configured
//!   secret is empty).

use std::time::Duration;

use crate::config;

/// Outcome of one `fetch_model` call. Distinct variants (not stringly-typed)
/// so callers (the CLI and breakfix) can match on the exact failure mode
/// rather than parsing an error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullOutcome {
    /// Chord reports the model is now warm (locally present) — covers BOTH a
    /// freshly completed pull and a model that was already warm/hot (see
    /// module doc: Chord's response does not distinguish the two).
    Warmed { model: String },
    /// `404` — the model is not in Chord's registry at all, or is registered
    /// but has no archive entry to pull from. Chord's own error message
    /// (already genericized, no host/path) is preserved for context.
    NotFound { detail: String },
    /// `507` — not enough free local disk space to hold the model.
    InsufficientDiskSpace { detail: String },
    /// `401` — the bearer JWT was missing/invalid for a Chord instance with
    /// auth enabled.
    Unauthorized,
    /// Transport-level failure (connection refused/timeout/DNS) — Chord's
    /// control API is not reachable at all.
    Unreachable { detail: String },
    /// Any other non-2xx status or unexpected/unparseable response body.
    Failed { detail: String },
}

/// `CHORD_CONTROL_URL` is unset — `fetch_model` cannot even attempt a call.
/// Kept distinct from [`PullOutcome`] because this is a caller misconfiguration
/// (no HTTP was attempted), not something Chord itself reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotConfigured(pub String);

impl std::fmt::Display for NotConfigured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Bearer token for Chord's JWT auth (`CHORD_JWT`). Sourced identically to
/// `gpu_authority::chord_auth_token` — same env var, same trim-then-empty-is-
/// none rule — so a host configured for one Chord-calling path is
/// automatically configured for this one too.
fn chord_auth_token() -> Option<String> {
    std::env::var("CHORD_JWT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Request timeout. A pull can legitimately take a while (archive copy of a
/// multi-GB model) — deliberately more generous than `gpu_authority`'s 10s
/// gpu-exclusive-lock timeout, which only ever does a cheap acquire/release
/// round trip. From `MINT_FETCH_MODEL_TIMEOUT_SECS`, default 600 (10 minutes),
/// matching Chord's own `model_pull_timeout_secs: 600` default seen in its
/// test fixtures.
fn fetch_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("MINT_FETCH_MODEL_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(600),
    )
}

/// Best-effort parse of Chord's `{"error": "..."}` error body. `None` if the
/// body is missing/unparseable/lacks the field — callers fall back to a
/// generic message rather than failing on a body-parse hiccup.
fn parse_error_detail(body: &serde_json::Value) -> Option<String> {
    body.get("error").and_then(|v| v.as_str()).map(String::from)
}

/// Delegate a re-pull/re-quantize of `model` to Chord's `PullCoordinator` via
/// `POST {CHORD_CONTROL_URL}/api/models/{model}/pull`. Never panics — every
/// failure mode (missing config, transport error, non-2xx, unparseable body)
/// resolves to a value, never a propagated panic that could take down a
/// caller (the CLI, or breakfix running inside the supervisor daemon's single
/// tick task).
pub async fn fetch_model(model: &str) -> Result<PullOutcome, NotConfigured> {
    let base = config::chord_control_url().ok_or_else(|| {
        NotConfigured(
            "CHORD_CONTROL_URL not set — fetch-model requires Chord's control endpoint".into(),
        )
    })?;
    let url = format!(
        "{}/api/models/{}/pull",
        base.trim_end_matches('/'),
        model
    );
    let token = chord_auth_token();

    let client = match reqwest::Client::builder().timeout(fetch_timeout()).build() {
        Ok(c) => c,
        // A client-build failure is not a config problem (the caller DID
        // configure Chord) — surface it as a Failed outcome, not NotConfigured.
        Err(e) => return Ok(PullOutcome::Failed { detail: format!("http client build failed: {e}") }),
    };

    let mut req = client.post(&url);
    if let Some(t) = &token {
        req = req.header("authorization", format!("Bearer {t}"));
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(PullOutcome::Unreachable {
                detail: format!("chord control endpoint unreachable: {e}"),
            })
        }
    };

    let status = resp.status();
    // Parse the body ONCE regardless of status — every branch below only
    // reads from it, never re-awaits the response.
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

    Ok(interpret_response(status.as_u16(), &body, model))
}

/// Pure: map an HTTP status + parsed JSON body to a [`PullOutcome`]. Split
/// from the network call so the status/body → outcome mapping is
/// unit-testable without a live Chord instance or a mock HTTP server.
fn interpret_response(status: u16, body: &serde_json::Value, model: &str) -> PullOutcome {
    match status {
        200..=299 => PullOutcome::Warmed { model: model.to_string() },
        404 => PullOutcome::NotFound {
            detail: parse_error_detail(body)
                .unwrap_or_else(|| format!("unknown model or missing archive entry: {model}")),
        },
        507 => PullOutcome::InsufficientDiskSpace {
            detail: parse_error_detail(body)
                .unwrap_or_else(|| "insufficient disk space".to_string()),
        },
        401 | 403 => PullOutcome::Unauthorized,
        other => PullOutcome::Failed {
            detail: parse_error_detail(body).unwrap_or_else(|| format!("HTTP {other}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- interpret_response: pure status/body → outcome mapping ----

    #[test]
    fn success_status_is_warmed_regardless_of_body_shape() {
        // Covers both "freshly pulled" and "already warm" — Chord's response
        // does not distinguish them (see module doc).
        assert_eq!(
            interpret_response(200, &json!({"status":"warm","model":"qwen3-coder:30b"}), "qwen3-coder:30b"),
            PullOutcome::Warmed { model: "qwen3-coder:30b".to_string() }
        );
        // Even a 2xx with an unexpected/empty body is still Warmed — the
        // status code is authoritative, not the body shape.
        assert_eq!(
            interpret_response(204, &serde_json::Value::Null, "m:1"),
            PullOutcome::Warmed { model: "m:1".to_string() }
        );
    }

    #[test]
    fn not_found_uses_chord_error_message_when_present() {
        let body = json!({"error": "unknown model: bogus:1"});
        match interpret_response(404, &body, "bogus:1") {
            PullOutcome::NotFound { detail } => assert_eq!(detail, "unknown model: bogus:1"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn not_found_falls_back_to_generic_message_when_body_unparseable() {
        match interpret_response(404, &serde_json::Value::Null, "bogus:1") {
            PullOutcome::NotFound { detail } => assert!(detail.contains("bogus:1")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn insufficient_disk_space_maps_507() {
        let body = json!({"error": "insufficient disk space: need 20.00 GB, have 5.00 GB"});
        match interpret_response(507, &body, "big:70b") {
            PullOutcome::InsufficientDiskSpace { detail } => {
                assert!(detail.contains("insufficient disk space"))
            }
            other => panic!("expected InsufficientDiskSpace, got {other:?}"),
        }
    }

    #[test]
    fn unauthorized_maps_401_and_403() {
        assert_eq!(interpret_response(401, &serde_json::Value::Null, "m:1"), PullOutcome::Unauthorized);
        assert_eq!(interpret_response(403, &serde_json::Value::Null, "m:1"), PullOutcome::Unauthorized);
    }

    #[test]
    fn other_non_2xx_is_generic_failure_with_status_code() {
        match interpret_response(500, &serde_json::Value::Null, "m:1") {
            PullOutcome::Failed { detail } => assert!(detail.contains("500")),
            other => panic!("expected Failed, got {other:?}"),
        }
        match interpret_response(503, &json!({"error": "backend saturated"}), "m:1") {
            PullOutcome::Failed { detail } => assert_eq!(detail, "backend saturated"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn parse_error_detail_none_when_field_missing_or_wrong_type() {
        assert_eq!(parse_error_detail(&json!({})), None);
        assert_eq!(parse_error_detail(&json!({"error": 123})), None);
        assert_eq!(parse_error_detail(&serde_json::Value::Null), None);
        assert_eq!(
            parse_error_detail(&json!({"error": "bad"})),
            Some("bad".to_string())
        );
    }

    // ---- chord_auth_token: mirrors gpu_authority::chord_auth_token's rules ----

    #[test]
    #[serial_test::serial]
    fn chord_auth_token_trims_and_treats_blank_as_none() {
        std::env::set_var("CHORD_JWT", "  token-value  ");
        assert_eq!(chord_auth_token(), Some("token-value".to_string()));
        std::env::set_var("CHORD_JWT", "   ");
        assert_eq!(chord_auth_token(), None);
        std::env::remove_var("CHORD_JWT");
        assert_eq!(chord_auth_token(), None);
    }

    // ---- fetch_timeout: default + override + non-numeric/zero rejection ----

    #[test]
    #[serial_test::serial]
    fn fetch_timeout_defaults_and_overrides() {
        std::env::remove_var("MINT_FETCH_MODEL_TIMEOUT_SECS");
        assert_eq!(fetch_timeout(), Duration::from_secs(600));
        std::env::set_var("MINT_FETCH_MODEL_TIMEOUT_SECS", "0");
        assert_eq!(fetch_timeout(), Duration::from_secs(600)); // zero rejected
        std::env::set_var("MINT_FETCH_MODEL_TIMEOUT_SECS", "not-a-number");
        assert_eq!(fetch_timeout(), Duration::from_secs(600)); // unparseable rejected
        std::env::set_var("MINT_FETCH_MODEL_TIMEOUT_SECS", "120");
        assert_eq!(fetch_timeout(), Duration::from_secs(120));
        std::env::remove_var("MINT_FETCH_MODEL_TIMEOUT_SECS");
    }

    // ---- fetch_model: NotConfigured path (no network needed) ----

    #[tokio::test]
    #[serial_test::serial]
    async fn fetch_model_not_configured_when_control_url_unset() {
        std::env::remove_var("CHORD_CONTROL_URL");
        let result = fetch_model("qwen3-coder:30b").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().0.contains("CHORD_CONTROL_URL"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fetch_model_unreachable_when_control_url_points_nowhere() {
        // A syntactically valid but non-listening endpoint — never hangs
        // (fetch_timeout bounds it), resolves to Unreachable, never panics.
        std::env::set_var("CHORD_CONTROL_URL", "http://127.0.0.1:1");
        std::env::set_var("MINT_FETCH_MODEL_TIMEOUT_SECS", "2");
        let result = fetch_model("qwen3-coder:30b").await;
        std::env::remove_var("CHORD_CONTROL_URL");
        std::env::remove_var("MINT_FETCH_MODEL_TIMEOUT_SECS");
        match result {
            Ok(PullOutcome::Unreachable { .. }) => {}
            other => panic!("expected Ok(Unreachable), got {other:?}"),
        }
    }
}
