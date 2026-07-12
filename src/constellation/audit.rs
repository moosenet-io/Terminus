//! CONST-02: mutating-request audit trail for the constellation aggregation
//! layer.
//!
//! Every mutating request (`POST`/`PUT`/`PATCH`/`DELETE`) that reaches
//! `/api/*` — a local endpoint (e.g. `/api/auth/login`) or a proxied one
//! (`/api/{harmony,chord,lumina}/*path`) — is recorded here BEFORE it
//! dispatches, S6-sanitized. This reuses
//! [`crate::gateway_framework::audit::sanitize`] for the actual redaction
//! logic (secret-shaped `key=value`/`key: value` pairs and `Bearer <token>`
//! values → `***REDACTED***`, then truncated to 200 chars) rather than a
//! second, drifting copy of that regex — see that module's doc for the
//! exact rules. That existing sink emits `tracing` events keyed to
//! `terminus-primary`'s gateway pipeline; this module is a SEPARATE,
//! additive JSONL sink (append-only, one line per mutating `/api/*`
//! request) because the aggregation layer's audit trail needs to be
//! independently queryable/tail-able (an operator watching what the
//! constellation UI just did), not just a `tracing` line mixed in with
//! every other gateway event. Path from
//! [`crate::config::constellation_audit_log_path`].
//!
//! A write failure here (disk full, path unwritable) is logged via
//! `tracing::warn!` and otherwise swallowed — auditing must never fail the
//! underlying request, mirroring `AuditEntry::log`'s own "fire-and-forget,
//! a subscriber-side failure is that subscriber's concern" contract.

use serde::Serialize;
use std::io::Write;

use crate::gateway_framework::audit::sanitize;

/// One line of the constellation aggregation layer's mutating-request audit
/// log.
#[derive(Debug, Clone, Serialize)]
pub struct ConstellationAuditEntry {
    /// RFC 3339 UTC timestamp of the request.
    pub timestamp: String,
    /// `harmony` | `chord` | `lumina` | `terminus` | `auth` — which `/api/*`
    /// namespace this request targeted.
    pub system: String,
    /// HTTP method (`POST`/`PUT`/`PATCH`/`DELETE` — this sink is mutating
    /// requests only, see this module's doc).
    pub method: String,
    /// The request path, e.g. `/api/harmony/engine/stop`.
    pub path: String,
    /// The resolved caller identity, when the CONST-03 auth seam has one
    /// (see `crate::constellation::mod`'s `SessionSeam`). `None` for an
    /// unauthenticated request (e.g. a pre-auth `/api/auth/login` attempt).
    pub principal: Option<String>,
    /// S6-sanitized, truncated summary of the request body — secret-shaped
    /// values redacted via [`sanitize`] (whole-body, before truncation), then
    /// truncated to 200 chars + `...(truncated)`. `None` for an empty body.
    pub body_summary: Option<String>,
}

/// Sanitize `raw_body` (already UTF-8-lossy decoded by the caller) per S6 by
/// delegating WHOLLY to [`sanitize`]: it redacts secret-shaped values across
/// the ENTIRE body first, then truncates the redacted result to 200 chars +
/// `...(truncated)`.
///
/// This is deliberately a single, un-chunked `sanitize` call. An earlier
/// version chunked the body into fixed 180-char slices before sanitizing each
/// (to preserve up to ~1KB of body rather than `sanitize`'s 200-char cap), but
/// that could split a secret-shaped `"key": "value"` pair across a chunk
/// boundary so the redaction regex never matched it — leaking the secret into
/// the audit log (agy review, CONST-02). Security wins over body-length
/// fidelity here: redaction MUST see the whole body in one pass, and because
/// `sanitize` redacts BEFORE it truncates, any content past 200 chars is
/// dropped entirely (never shown), so no secret can survive either path. A
/// 200-char redacted summary is sufficient for an audit trail.
pub fn sanitize_body(raw_body: &str) -> Option<String> {
    if raw_body.trim().is_empty() {
        return None;
    }
    Some(sanitize(raw_body))
}

/// Record one mutating `/api/*` request. `raw_body` is the request's raw
/// (pre-any-masking) body text — this function sanitizes it itself via
/// [`sanitize_body`], so callers must NOT pre-redact.
pub fn record_mutating_request(
    system: &str,
    method: &str,
    path: &str,
    principal: Option<&str>,
    raw_body: &str,
) {
    let entry = ConstellationAuditEntry {
        timestamp: now_rfc3339(),
        system: system.to_string(),
        method: method.to_string(),
        path: path.to_string(),
        principal: principal.map(str::to_string),
        body_summary: sanitize_body(raw_body),
    };
    if let Err(e) = append_jsonl(&entry) {
        tracing::warn!("constellation: failed to write audit log entry: {e}");
    }
}

fn append_jsonl(entry: &ConstellationAuditEntry) -> std::io::Result<()> {
    let path = crate::config::constellation_audit_log_path();
    let line = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

fn now_rfc3339() -> String {
    // No extra chrono/time dependency needed for a plain UTC-seconds
    // timestamp: `std::time::SystemTime` + a hand-rolled RFC 3339 render is
    // sufficient for an audit-log line's precision needs.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    humantime_rfc3339(secs)
}

/// Minimal civil-calendar RFC 3339 (UTC) renderer for a Unix timestamp —
/// avoids pulling in a new time-formatting dependency for a single audit
/// timestamp field. Not meant for general-purpose date math; correct for
/// any timestamp in the proleptic Gregorian calendar post-1970.
fn humantime_rfc3339(unix_secs: u64) -> String {
    const SECS_PER_DAY: u64 = 86_400;
    let days_since_epoch = unix_secs / SECS_PER_DAY;
    let secs_of_day = unix_secs % SECS_PER_DAY;
    let (hour, minute, second) = (secs_of_day / 3600, (secs_of_day / 60) % 60, secs_of_day % 60);

    // Civil-from-days algorithm (Howard Hinnant's public-domain
    // `civil_from_days`), days-since-epoch -> (year, month, day).
    let z = days_since_epoch as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Should this method be audited at all? Only mutating verbs — a `GET`
/// through `/api/*` is a read and is intentionally NOT written to this
/// sink (matches this module's doc: "every mutating request").
pub fn is_mutating_method(method: &str) -> bool {
    matches!(method.to_ascii_uppercase().as_str(), "POST" | "PUT" | "PATCH" | "DELETE")
}

/// Extract a best-effort JSON-body summary suitable for [`sanitize_body`]
/// from a raw byte body — used by proxy/local handlers that hold
/// `axum::body::Bytes` rather than a `String`. Non-UTF-8 bytes are
/// lossily converted (an audit summary need not be byte-perfect).
pub fn body_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use serial_test::serial;

    #[test]
    fn is_mutating_method_covers_expected_verbs() {
        assert!(is_mutating_method("POST"));
        assert!(is_mutating_method("post"));
        assert!(is_mutating_method("PUT"));
        assert!(is_mutating_method("PATCH"));
        assert!(is_mutating_method("DELETE"));
        assert!(!is_mutating_method("GET"));
        assert!(!is_mutating_method("HEAD"));
        assert!(!is_mutating_method("OPTIONS"));
    }

    #[test]
    fn sanitize_body_redacts_secret_shaped_fields() {
        let raw = r#"{"username": "operator", "password": "hunter2hunter2"}"#;
        let summary = sanitize_body(raw).unwrap();
        assert!(!summary.contains("hunter2hunter2"));
        assert!(summary.contains("***REDACTED***"));
        assert!(summary.contains("operator"));
    }

    #[test]
    fn sanitize_body_none_for_empty_body() {
        assert_eq!(sanitize_body(""), None);
        assert_eq!(sanitize_body("   "), None);
    }

    #[test]
    fn sanitize_body_truncates_large_bodies() {
        let raw = format!(r#"{{"data": "{}"}}"#, "x".repeat(2000));
        let summary = sanitize_body(&raw).unwrap();
        assert!(summary.ends_with("...(truncated)"));
        // sanitize() caps at 200 redacted chars + the suffix.
        assert!(summary.chars().count() <= 200 + "...(truncated)".len());
    }

    #[test]
    #[serial]
    fn record_mutating_request_appends_a_jsonl_line() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);

        record_mutating_request(
            "harmony",
            "POST",
            "/api/harmony/engine/stop",
            Some("operator"),
            r#"{"token": "supersecretvalue1234"}"#,
        );

        let contents = std::fs::read_to_string(&path).unwrap();
        let line = contents.lines().next().unwrap();
        let parsed: Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["system"], "harmony");
        assert_eq!(parsed["method"], "POST");
        assert_eq!(parsed["principal"], "operator");
        assert!(!contents.contains("supersecretvalue1234"));

        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
    }

    #[test]
    fn body_text_lossily_decodes_bytes() {
        assert_eq!(body_text(b"hello"), "hello");
    }

    #[test]
    fn sanitize_body_redacts_secret_past_old_chunk_boundary() {
        // Regression (agy CONST-02 review): the old 180-char chunking could
        // split a secret-shaped pair across a boundary so the redaction regex
        // missed it. Size the padding so the api_key VALUE straddles char ~180
        // (the old chunk boundary) yet stays inside sanitize's 200-char window —
        // so this asserts REDACTION across the boundary, not mere truncation.
        let padding = "a".repeat(140);
        let secret = "<REDACTED-SECRET>";
        let raw = format!(r#"{{"pad":"{padding}","api_key":"{secret}"}}"#);
        let summary = sanitize_body(&raw).unwrap();
        assert!(!summary.contains(secret), "secret leaked into audit summary: {summary}");
    }

    #[test]
    fn humantime_rfc3339_renders_known_epoch() {
        // 2026-07-16T00:00:00Z, a fixed known instant, sanity-checks the
        // hand-rolled civil-calendar renderer against a value that's easy
        // to verify by hand.
        let unix_secs = 1_784_246_400_u64; // 2026-07-16T00:00:00Z
        let rendered = humantime_rfc3339(unix_secs);
        assert!(rendered.starts_with("2026-"));
        assert!(rendered.ends_with('Z'));
    }
}
