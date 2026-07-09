//! Gateway-level audit logging + S6 sanitization (TGW-04 — Terminus Primary
//! Gateway sprint, S108).
//!
//! This is a NEW, gateway-level audit record — distinct from (and does not
//! duplicate) any per-tool audit logging individual tool modules under
//! `register_all`/`register_personal` already do internally. It records the
//! outer shape every request through `terminus-primary` shares: who (the
//! mTLS-derived identity), what (tool name or inference route), what kind of
//! action, and the outcome — allowed+dispatched, denied (no identity, not
//! allowlisted, rate-limited), or dispatched-but-failed.
//!
//! ## S6 sanitization
//! Every free-text field that could carry caller-supplied or upstream
//! content (the `detail`/`reason` string) is passed through [`sanitize`]
//! before it reaches a log line: secret-shaped `key=value`/`key: value`
//! pairs (token/key/secret/password/credential/auth, case-insensitive) have
//! their value replaced with `***REDACTED***`, `Bearer <token>` /
//! `Authorization: ...` values are redacted the same way, and the result is
//! truncated to 200 chars + `...(truncated)` if longer. Raw JWTs, API keys,
//! and full request/response bodies must never reach [`AuditEntry::log`]
//! unsanitized — callers pass already-summarized detail strings, not raw
//! payloads.

use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

use crate::gateway_framework::ActionKind;

const REDACTED: &str = "***REDACTED***";
const MAX_DETAIL_CHARS: usize = 200;

/// `key=value` / `key: "value"` / `key: value` where `key` looks
/// secret-shaped. Value is any run of non-whitespace, non-separator chars
/// (quotes stop early so `"token": "abc", "next": "safe"` only redacts
/// `abc`).
fn secret_kv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(token|api[_-]?key|key|secret|password|credential|auth)\b"?\s*[:=]\s*"?'?([^\s"',}]+)"?'?"#,
        )
        .expect("SECRET_KV regex must compile")
    })
}

/// `Bearer <token>` / `Authorization: Bearer <token>` — redact the token
/// itself, keep the scheme visible for readability.
fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)\bBearer\s+([^\s"',}]+)"#).expect("BEARER regex must compile"))
}

/// Sanitize a free-text audit detail string per S6: redact secret-shaped
/// key/value pairs and bearer tokens, then truncate to
/// [`MAX_DETAIL_CHARS`] chars with a `...(truncated)` suffix if the
/// (already-redacted) text is longer.
pub fn sanitize(input: &str) -> String {
    let redacted_kv = secret_kv_re().replace_all(input, |caps: &regex::Captures| {
        format!("{}={}", &caps[1], REDACTED)
    });
    let redacted =
        bearer_re().replace_all(&redacted_kv, |_: &regex::Captures| format!("Bearer {REDACTED}"));

    let char_count = redacted.chars().count();
    if char_count > MAX_DETAIL_CHARS {
        let truncated: String = redacted.chars().take(MAX_DETAIL_CHARS).collect();
        format!("{truncated}...(truncated)")
    } else {
        redacted.into_owned()
    }
}

/// The outcome of a gated request, as recorded in the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    /// Passed identity + allowlist + rate-limit, dispatched, and the
    /// underlying tool/inference call itself succeeded.
    Success,
    /// Passed the gate and dispatched, but the underlying tool/inference
    /// call itself failed (a normal operational failure, not a gate denial).
    Failure,
    /// Rejected before dispatch: no mTLS identity present on a listener that
    /// requires one (fail-closed).
    DeniedNoIdentity,
    /// Rejected before dispatch: identity present but not allowlisted for
    /// this action.
    DeniedNotAllowlisted,
    /// Rejected before dispatch: identity's rate-limit budget for this
    /// action is exhausted.
    DeniedRateLimited,
}

impl AuditResult {
    pub fn is_denied(self) -> bool {
        !matches!(self, AuditResult::Success | AuditResult::Failure)
    }
}

/// One structured gateway audit record.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub identity: String,
    pub action: String,
    pub kind: ActionKind,
    pub result: AuditResult,
    /// Sanitized (already passed through [`sanitize`]), human-readable
    /// detail — e.g. "not allowlisted", "rate limit exceeded", or a
    /// summarized tool-error message. Never a raw payload.
    pub detail: Option<String>,
}

impl AuditEntry {
    /// Build an entry, sanitizing `detail` (if any) per S6 before it's
    /// stored — callers never need to remember to call [`sanitize`]
    /// themselves.
    pub fn new(
        identity: impl Into<String>,
        action: impl Into<String>,
        kind: ActionKind,
        result: AuditResult,
        detail: Option<&str>,
    ) -> Self {
        Self {
            identity: identity.into(),
            action: action.into(),
            kind,
            result,
            detail: detail.map(sanitize),
        }
    }

    /// Emit this entry as a structured `tracing` event on the
    /// `gateway_audit` target. A logging failure (e.g. a downstream
    /// subscriber that can't flush to disk) must never fail the underlying
    /// request — `tracing` events are fire-and-forget from the caller's
    /// perspective, so this can't itself return an `Err`; any subscriber-side
    /// write failure is that subscriber's own concern (typically its own
    /// stderr fallback), never propagated back into the request path. See
    /// the TGW-04 spec item's "audit log write failure" edge case.
    pub fn log(&self) {
        tracing::info!(
            target: "gateway_audit",
            identity = %self.identity,
            action = %self.action,
            kind = ?self.kind,
            result = ?self.result,
            detail = self.detail.as_deref().unwrap_or(""),
            "gateway_audit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_redacts_secret_kv_pairs() {
        // pii-test-fixture: synthetic token-shaped value, not a real credential.
        let input = r#"failed calling gitea with token=<REDACTED-SECRET> and next=safe"#; // pii-test-fixture
        let out = sanitize(input);
        assert!(!out.contains("<REDACTED-SECRET>"), "raw token leaked: {out}"); // pii-test-fixture
        assert!(out.contains("token=***REDACTED***"));
        assert!(out.contains("next=safe"), "unrelated field must be preserved: {out}");
    }

    #[test]
    fn sanitize_redacts_json_shaped_secret_fields() {
        // pii-test-fixture: synthetic key-shaped value, not a real credential.
        let input = r#"upstream body: {"api_key": "<REDACTED-SECRET>", "model": "test"}"#; // pii-test-fixture
        let out = sanitize(input);
        assert!(!out.contains("<REDACTED-SECRET>"), "raw api_key leaked: {out}"); // pii-test-fixture
        assert!(out.contains("model=test") || out.contains("\"model\": \"test\""));
    }

    #[test]
    fn sanitize_redacts_bearer_tokens() {
        // pii-test-fixture: synthetic JWT-shaped value, not a real token.
        let input = "Authorization: Bearer <REDACTED-SECRET>"; // pii-test-fixture
        let out = sanitize(input);
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"), "raw JWT leaked: {out}"); // pii-test-fixture
        assert!(out.contains("Bearer ***REDACTED***"));
    }

    #[test]
    fn sanitize_truncates_long_payloads() {
        let input = "x".repeat(500);
        let out = sanitize(&input);
        assert!(out.ends_with("...(truncated)"));
        // 200 chars of 'x' + the suffix.
        assert_eq!(out.len(), 200 + "...(truncated)".len());
    }

    #[test]
    fn sanitize_passes_short_clean_text_through_unchanged() {
        let input = "tool not found: bogus_thing";
        assert_eq!(sanitize(input), input);
    }

    #[test]
    fn audit_entry_new_sanitizes_detail() {
        let entry = AuditEntry::new(
            "dev-box",
            "gitea_list_identities",
            ActionKind::Tool,
            AuditResult::Failure,
            Some("token=supersecretvalue123"),
        );
        let detail = entry.detail.expect("detail present");
        assert!(!detail.contains("supersecretvalue123"));
        assert!(detail.contains("***REDACTED***"));
    }

    #[test]
    fn audit_result_is_denied_classification() {
        assert!(AuditResult::DeniedNoIdentity.is_denied());
        assert!(AuditResult::DeniedNotAllowlisted.is_denied());
        assert!(AuditResult::DeniedRateLimited.is_denied());
        assert!(!AuditResult::Success.is_denied());
        assert!(!AuditResult::Failure.is_denied());
    }

    #[test]
    fn log_does_not_panic() {
        // No subscriber installed in unit tests -- this just proves `log()`
        // never panics/blocks regardless of whether anything is listening,
        // matching the "audit-write failure must not block the request"
        // requirement (a no-op subscriber is the degenerate case of that).
        let entry = AuditEntry::new(
            "dev-box",
            "/v1/chat/completions",
            ActionKind::Inference,
            AuditResult::Success,
            None,
        );
        entry.log();
    }
}
