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
    /// action is exhausted (a REAL over-limit).
    DeniedRateLimited,
    /// RLQ-01: rejected before dispatch, but NOT because of a real
    /// over-limit — the rate-limiter BACKEND itself is degraded (e.g. Redis
    /// unreachable, or a misconfigured `REDIS_URL`). Kept distinct from
    /// `DeniedRateLimited` in the audit trail for the same reason
    /// `RateLimitDecision::Degraded` is kept distinct from `Limited`: a
    /// backend outage must be diagnosable from the log, not indistinguishable
    /// from ordinary throttling.
    DeniedRateLimiterDegraded,
}

impl AuditResult {
    pub fn is_denied(self) -> bool {
        !matches!(self, AuditResult::Success | AuditResult::Failure)
    }
}

/// MESH-10: the gate's decision for a request, independent of whatever
/// happened during dispatch afterward. `Allow` covers a request that
/// cleared identity + allowlist + rate-limit, whether the underlying
/// dispatched call itself then succeeded or failed (see [`AuditResult`] for
/// that finer distinction) — `Deny`/`ApprovalRequired`/`TransportFailure`
/// never dispatch to a tool/upstream at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    /// Cleared the gate; dispatched (locally, or to a federated upstream).
    Allow,
    /// Rejected before dispatch: no identity, not allowlisted, or rate
    /// limited. Never silent — always audited (see `AuditEntry::log`).
    Deny,
    /// A guarded tool required (and did not yet have) human approval — the
    /// call was NOT dispatched. See `crate::approval`'s "APPROVAL REQUIRED"
    /// gate.
    ApprovalRequired,
    /// Cleared the gate, but the request could not be routed at all: a
    /// federated (mesh) upstream was unreachable/unhealthy, or the call to
    /// it failed at the transport level before the upstream could even
    /// attempt the tool. Distinct from `AuditResult::Failure`, which covers
    /// an upstream/tool that *was* reached and returned an application-level
    /// error.
    TransportFailure,
}

impl AuditDecision {
    /// The coarse decision implied by a legacy [`AuditResult`] alone, for
    /// callers (most of the codebase, pre-MESH-10) that only ever
    /// distinguish success/failure/denied and never had the federated
    /// context to know about `ApprovalRequired`/`TransportFailure`.
    fn from_result(result: AuditResult) -> Self {
        if result.is_denied() {
            AuditDecision::Deny
        } else {
            AuditDecision::Allow
        }
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
    /// summarized tool-error message (which, for a federated call, is a
    /// sanitized/truncated summary of the args and/or result — see
    /// `crate::mcp_server`'s federated `tools/call` dispatch). Never a raw
    /// payload.
    pub detail: Option<String>,
    /// MESH-10: the canonical, resolved caller identity (mTLS-derived
    /// `Principal::name()`) that this request was attributed to. Equal to
    /// `identity` today — kept as a distinct field because `identity` is a
    /// generic gate-level label (it's also `ANONYMOUS_IDENTITY` for the
    /// no-identity-at-all denial case) while `principal` specifically means
    /// "the resolved caller", which is what a federated-audit reviewer
    /// wants to key on.
    pub principal: String,
    /// MESH-10: the mesh namespace this call was routed to, e.g. `Some("ns")`
    /// for a `ns__tool` federated call. `None` for a local (non-federated)
    /// call.
    pub upstream: Option<String>,
    /// MESH-10: the advertised tool name as the caller sent it (namespaced
    /// for a federated call, e.g. `ns__tool`). Equal to `action` for a
    /// `Tool`-kind entry.
    pub tool_advertised: String,
    /// MESH-10: the bare tool name actually dispatched — the namespace
    /// prefix stripped for a federated call (e.g. `tool` for `ns__tool`).
    /// Equal to `tool_advertised` for a local call.
    pub tool_bare: String,
    /// MESH-10: the gate's decision — see [`AuditDecision`].
    pub decision: AuditDecision,
}

impl AuditEntry {
    /// Build an entry, sanitizing `detail` (if any) per S6 before it's
    /// stored — callers never need to remember to call [`sanitize`]
    /// themselves.
    ///
    /// This is the pre-MESH-10 constructor, kept unchanged so every
    /// existing call site keeps compiling: it fills the new federated-audit
    /// fields with the non-federated defaults (`principal` = `identity`,
    /// `tool_advertised`/`tool_bare` = `action`, `upstream` = `None`,
    /// `decision` derived from `result`). Use [`AuditEntry::new_federated`]
    /// when the caller has real federated context to record.
    pub fn new(
        identity: impl Into<String>,
        action: impl Into<String>,
        kind: ActionKind,
        result: AuditResult,
        detail: Option<&str>,
    ) -> Self {
        let identity = identity.into();
        let action = action.into();
        let decision = AuditDecision::from_result(result);
        Self {
            principal: identity.clone(),
            tool_advertised: action.clone(),
            tool_bare: action.clone(),
            upstream: None,
            decision,
            identity,
            action,
            kind,
            result,
            detail: detail.map(sanitize),
        }
    }

    /// MESH-10: build a federated-audit entry with full context — the
    /// canonical principal, the upstream/namespace (if any) the call was
    /// routed to, both the advertised and bare tool names, and the gate's
    /// explicit [`AuditDecision`] (which, unlike `result`, can express
    /// `ApprovalRequired`/`TransportFailure`, not just allow/deny).
    ///
    /// `detail` is sanitized exactly like [`AuditEntry::new`] — pass a
    /// short, already-summarized string (e.g. a sanitized/truncated dump of
    /// the call's args, or a tool-error message), never a raw payload; a
    /// secret-shaped value in it is redacted by [`sanitize`] before this
    /// entry is ever logged.
    #[allow(clippy::too_many_arguments)]
    pub fn new_federated(
        principal: impl Into<String>,
        upstream: Option<String>,
        tool_advertised: impl Into<String>,
        tool_bare: impl Into<String>,
        kind: ActionKind,
        result: AuditResult,
        decision: AuditDecision,
        detail: Option<&str>,
    ) -> Self {
        let principal = principal.into();
        let tool_advertised = tool_advertised.into();
        Self {
            identity: principal.clone(),
            action: tool_advertised.clone(),
            kind,
            result,
            detail: detail.map(sanitize),
            principal,
            upstream,
            tool_advertised,
            tool_bare: tool_bare.into(),
            decision,
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
            principal = %self.principal,
            upstream = self.upstream.as_deref().unwrap_or(""),
            tool_advertised = %self.tool_advertised,
            tool_bare = %self.tool_bare,
            decision = ?self.decision,
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

    // ── MESH-10: federated audit trail ─────────────────────────────────────

    #[test]
    fn new_federated_populates_principal_upstream_and_tool_names() {
        let entry = AuditEntry::new_federated(
            "dev-box",
            Some("gitea-remote".to_string()),
            "gitea-remote__list_identities",
            "list_identities",
            ActionKind::Tool,
            AuditResult::Success,
            AuditDecision::Allow,
            None,
        );
        assert_eq!(entry.principal, "dev-box");
        assert_eq!(entry.upstream.as_deref(), Some("gitea-remote"));
        assert_eq!(entry.tool_advertised, "gitea-remote__list_identities");
        assert_eq!(entry.tool_bare, "list_identities");
        assert_eq!(entry.decision, AuditDecision::Allow);
    }

    #[test]
    fn new_federated_local_call_has_no_upstream() {
        let entry = AuditEntry::new_federated(
            "dev-box",
            None,
            "ledger_accounts",
            "ledger_accounts",
            ActionKind::Tool,
            AuditResult::Success,
            AuditDecision::Allow,
            None,
        );
        assert_eq!(entry.upstream, None);
    }

    #[test]
    fn new_federated_deny_is_never_silent_and_carries_deny_decision() {
        let entry = AuditEntry::new_federated(
            "dev-box",
            Some("gitea-remote".to_string()),
            "gitea-remote__list_identities",
            "list_identities",
            ActionKind::Tool,
            AuditResult::DeniedNotAllowlisted,
            AuditDecision::Deny,
            Some("identity 'dev-box' is not allowlisted for 'gitea-remote__list_identities'"),
        );
        assert_eq!(entry.decision, AuditDecision::Deny);
        assert!(entry.result.is_denied());
        assert!(entry.detail.is_some(), "a denial must always produce a logged detail, never a silent drop");
    }

    #[test]
    fn new_federated_transport_failure_is_audited_not_dropped() {
        let entry = AuditEntry::new_federated(
            "dev-box",
            Some("gitea-remote".to_string()),
            "gitea-remote__list_identities",
            "list_identities",
            ActionKind::Tool,
            AuditResult::Failure,
            AuditDecision::TransportFailure,
            Some("mesh upstream \"gitea-remote\" unavailable"),
        );
        assert_eq!(entry.decision, AuditDecision::TransportFailure);
        assert!(entry.detail.is_some());
    }

    #[test]
    fn new_federated_redacts_secret_shaped_args_before_write() {
        // pii-test-fixture: synthetic token-shaped value, not a real credential.
        let args_summary = r#"args: {"token": "<REDACTED-SECRET>", "repo": "safe-repo"}"#; // pii-test-fixture
        let entry = AuditEntry::new_federated(
            "dev-box",
            Some("gitea-remote".to_string()),
            "gitea-remote__create_repo",
            "create_repo",
            ActionKind::Tool,
            AuditResult::Success,
            AuditDecision::Allow,
            Some(args_summary),
        );
        let detail = entry.detail.expect("detail present");
        assert!(!detail.contains("<REDACTED-SECRET>"), "raw secret leaked into audit: {detail}"); // pii-test-fixture
        assert!(detail.contains("REDACTED"));
        assert!(detail.contains("safe-repo"), "unrelated field must be preserved: {detail}");
    }

    #[test]
    fn new_matches_new_federated_defaults_for_non_federated_callers() {
        // Existing (pre-MESH-10) call sites keep compiling and keep producing
        // sensible values for the new fields: no upstream, decision derived
        // from `result`, tool_advertised/tool_bare == action.
        let allowed = AuditEntry::new("dev-box", "ledger_accounts", ActionKind::Tool, AuditResult::Success, None);
        assert_eq!(allowed.principal, "dev-box");
        assert_eq!(allowed.upstream, None);
        assert_eq!(allowed.tool_advertised, "ledger_accounts");
        assert_eq!(allowed.tool_bare, "ledger_accounts");
        assert_eq!(allowed.decision, AuditDecision::Allow);

        let denied = AuditEntry::new(
            "dev-box",
            "ledger_accounts",
            ActionKind::Tool,
            AuditResult::DeniedNotAllowlisted,
            None,
        );
        assert_eq!(denied.decision, AuditDecision::Deny);
    }
}
