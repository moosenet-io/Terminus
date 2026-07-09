//! Uniform per-request gateway pipeline (TGW-04 — Terminus Primary Gateway
//! sprint, S108): mTLS identity → allowlist → rate-limit → dispatch → audit,
//! applied identically to BOTH request paths `terminus-primary` serves —
//! tool calls (TGW-01/TGW-02's core + federated-personal dispatch inside
//! `crate::mcp_server::handle_mcp`'s `tools/call` branch) and inference
//! proxying (TGW-03's `crate::inference_proxy` routes) — so the framework is
//! one shared thing both routes go through, not two divergent bolt-ons.
//!
//! ## Stages
//! 1. **Identity** — the caller's mTLS-derived identity
//!    (`crate::pki::mtls::ClientIdentity`), extracted by
//!    `crate::pki::mtls::run_listener` and attached to the request's
//!    extensions *by the server*, post-handshake. This module never trusts
//!    a client-supplied identity field/header — [`GatewayFramework::guard`]
//!    takes only an `Option<&ClientIdentity>` sourced from that extension,
//!    and treats `None` as fail-closed (see below), never as "identity
//!    unknown, proceed anyway".
//! 2. **Allowlist** — [`AllowlistPolicy`]: a per-identity, config-driven
//!    allow list of tool names / inference routes. Default-deny: an
//!    identity with no configured entry at all is denied every action (see
//!    the TGW-04 spec item's "newly-enrolled identity, no allowlist entry
//!    yet" edge case) — this is NOT a global allowlist with per-identity
//!    exceptions, it is per-identity from the start, since no
//!    identity-scoped allowlist mechanism existed in this codebase before
//!    this item (confirmed by searching for prior "allowlist" hits — the
//!    existing ones are all for unrelated things: SSH command allowlists,
//!    a secret-manager key allowlist, etc., not tool/route access control).
//! 3. **Rate-limit** — `crate::gateway_framework::rate_limit`: an interim
//!    in-process token bucket per `(identity, action)`. Explicitly scoped as
//!    replaceable by a later Redis-backed limiter (Phase P4 / S100
//!    relocation, out of scope here) — see that module's doc.
//! 4. **Dispatch** — NOT performed by this module. `guard()` returns an
//!    `Ok(GatewayContext)` the caller (the tool-call or inference-proxy
//!    handler) uses to perform its own dispatch exactly as it already does
//!    — this module only gates entry and records the outcome, it does not
//!    reimplement tool/inference dispatch.
//! 5. **Audit** — `crate::gateway_framework::audit`: a structured,
//!    S6-sanitized log entry for EVERY request, whether denied at any gate
//!    stage or dispatched. `guard()` itself logs denials (the request never
//!    reaches dispatch, so there is no later point to log from); callers
//!    must call [`GatewayContext::record_result`] after dispatch completes
//!    to log the terminal success/failure outcome — see that method's doc
//!    for why a single audit write per request (not two) is deliberate.
//!
//! ## Fail-closed, always
//! [`GatewayFramework::guard`] with `identity: None` NEVER returns
//! `Ok(..)` — this is the "fail-closed if absent on the mTLS listener"
//! requirement: a request that reaches `terminus-primary` without a
//! server-verified mTLS identity attached is rejected before any allowlist
//! or rate-limit check even runs (there is no identity to check either
//! against), and the denial is audited under a synthetic `"anonymous"`
//! identity label (never fabricated as if it were real).

pub mod audit;
pub mod rate_limit;

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

use crate::pki::mtls::ClientIdentity;
use audit::{AuditEntry, AuditResult};
use rate_limit::{rate_limit_key, InProcessRateLimiter, RateLimitDecision, RateLimiter};

/// Label recorded in the audit log when no mTLS identity is present at all
/// (the request is denied before this label could ever be used to check an
/// allowlist or rate limit — it exists purely so the audit trail has
/// something other than an empty string to key on).
pub const ANONYMOUS_IDENTITY: &str = "anonymous";

/// What kind of action a gated request is attempting — carried through to
/// the audit log so a reviewer can tell tool-dispatch traffic from
/// inference-proxy traffic at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// A `tools/call` dispatch (core, locally-served, or federated to
    /// the personal-registry host via `crate::federation`) — `action` is the tool name.
    Tool,
    /// An inference-proxy request (`crate::inference_proxy`) — `action` is
    /// the route path (e.g. `/v1/chat/completions`).
    Inference,
}

/// Per-identity allow policy: which tool names / inference routes each
/// enrolled identity may use. Config-driven
/// (`crate::config::gateway_allowlist_json`, a JSON object of
/// `identity -> [action, ...]`); a `"*"` entry in an identity's array
/// allows every action for that identity. Default-deny: an identity with no
/// entry in the policy at all is denied every action — see this module's
/// doc for why (no prior identity-scoped mechanism to fall back to, and the
/// TGW-04 spec item's edge case calls for a clean denial, not a silent
/// empty-catalog response).
#[derive(Debug, Clone, Default)]
pub struct AllowlistPolicy {
    entries: HashMap<String, Vec<String>>,
}

impl AllowlistPolicy {
    /// Build a policy directly from a map — mainly for tests and for
    /// callers that already have the data in hand rather than as env JSON.
    pub fn new(entries: HashMap<String, Vec<String>>) -> Self {
        Self { entries }
    }

    /// Build a policy from `crate::config::gateway_allowlist_json`. A
    /// malformed JSON value degrades to an empty (default-deny-everything)
    /// policy rather than panicking the process at startup — a config typo
    /// should not crash the gateway, it should just deny everyone until
    /// fixed (loudly logged so the operator notices).
    pub fn from_env() -> Self {
        let raw = crate::config::gateway_allowlist_json();
        match serde_json::from_str::<HashMap<String, Vec<String>>>(&raw) {
            Ok(entries) => Self { entries },
            Err(e) => {
                tracing::error!(
                    "gateway_framework: TERMINUS_GATEWAY_ALLOWLIST_JSON is not valid JSON \
                     ({e}) -- falling back to an empty (deny-all) allowlist policy"
                );
                Self::default()
            }
        }
    }

    /// Whether `identity` is a known entry in the policy at all (distinct
    /// from `is_allowed`, which also checks the specific action) — used to
    /// distinguish "identity has zero configured permissions" from
    /// "identity has permissions but not for this action" in audit detail
    /// text.
    pub fn has_any_entry(&self, identity: &str) -> bool {
        self.entries.contains_key(identity)
    }

    /// Whether `identity` may perform `action`, per policy. `false` for any
    /// identity with no entry (default-deny) or whose entry doesn't contain
    /// `action` or `"*"`.
    pub fn is_allowed(&self, identity: &str, action: &str) -> bool {
        match self.entries.get(identity) {
            Some(actions) => actions.iter().any(|a| a == "*" || a == action),
            None => false,
        }
    }
}

/// Everything a caller needs to finish handling a gated request: the
/// resolved identity/action/kind, used to build the terminal audit entry
/// once dispatch completes.
#[derive(Debug)]
pub struct GatewayContext {
    identity: String,
    action: String,
    kind: ActionKind,
}

impl GatewayContext {
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Record the terminal outcome of a request this context already
    /// cleared the gate for, and audit it. Call exactly once, after
    /// dispatch completes (success or failure) — `guard()` already audited
    /// any denial that happened before dispatch, so this is the ONE place
    /// the "dispatched" branch of the audit trail is written, keeping the
    /// invariant "exactly one audit entry per request" true whether the
    /// request was denied or completed.
    ///
    /// `detail` is passed through `audit::sanitize` (via `AuditEntry::new`)
    /// before it's logged — pass a short summary (e.g. a tool error's
    /// `Display` output), never a raw payload.
    pub fn record_result(&self, success: bool, detail: Option<&str>) {
        let result = if success { AuditResult::Success } else { AuditResult::Failure };
        AuditEntry::new(&self.identity, &self.action, self.kind, result, detail).log();
    }
}

struct GatewayFrameworkInner {
    allowlist: AllowlistPolicy,
    rate_limiter: Arc<dyn RateLimiter>,
}

/// The shared gateway pipeline itself: owns the allowlist policy and rate
/// limiter for one `terminus-primary` process, and gates every request
/// through [`guard`](Self::guard) before the caller's own dispatch logic
/// runs.
#[derive(Clone)]
pub struct GatewayFramework {
    inner: Arc<GatewayFrameworkInner>,
}

impl std::fmt::Debug for GatewayFramework {
    // `Arc<dyn RateLimiter>` carries no `Debug` impl (and shouldn't need
    // one) -- this manual impl exists purely so `GatewayFramework` can be
    // embedded in structs that derive `Debug` (e.g.
    // `crate::pki::server::GatewayServerConfig`) without forcing that on
    // the rate-limiter trait.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayFramework").finish_non_exhaustive()
    }
}

impl GatewayFramework {
    pub fn new(allowlist: AllowlistPolicy, rate_limiter: Arc<dyn RateLimiter>) -> Self {
        Self {
            inner: Arc::new(GatewayFrameworkInner { allowlist, rate_limiter }),
        }
    }

    /// Build the production framework from env config
    /// (`crate::config::gateway_allowlist_json` +
    /// `crate::config::gateway_rate_limit_burst`/`gateway_rate_limit_refill_per_sec`)
    /// — what `terminus_primary`'s `main()` calls.
    pub fn from_env() -> Self {
        Self::new(
            AllowlistPolicy::from_env(),
            Arc::new(InProcessRateLimiter::from_env()),
        )
    }

    /// Gate one request. `identity` must come from the mTLS-verified
    /// `ClientIdentity` request extension only (see this module's doc) —
    /// `None` fails closed unconditionally, before any allowlist/rate-limit
    /// check.
    ///
    /// - `Err(response)` — the request is denied; `response` is a ready-to-
    ///   return `403` (missing identity or not allowlisted) or `429` (rate
    ///   limited) `axum::response::Response`. The denial has ALREADY been
    ///   audited by the time this returns — the caller doesn't need to (and
    ///   shouldn't) log it again.
    /// - `Ok(ctx)` — the request cleared identity + allowlist + rate-limit.
    ///   The caller performs its own dispatch, then MUST call
    ///   `ctx.record_result(..)` exactly once to complete the audit trail.
    pub async fn guard(
        &self,
        identity: Option<&ClientIdentity>,
        action: &str,
        kind: ActionKind,
    ) -> Result<GatewayContext, Response> {
        let identity_str = match identity {
            Some(id) => id.as_str().to_string(),
            None => {
                AuditEntry::new(
                    ANONYMOUS_IDENTITY,
                    action,
                    kind,
                    AuditResult::DeniedNoIdentity,
                    Some("no mTLS-verified client identity on this request"),
                )
                .log();
                return Err(denied_response(
                    StatusCode::FORBIDDEN,
                    "no mTLS-verified client identity present on this request",
                ));
            }
        };

        if !self.inner.allowlist.is_allowed(&identity_str, action) {
            let detail = if self.inner.allowlist.has_any_entry(&identity_str) {
                format!("identity '{identity_str}' is not allowlisted for '{action}'")
            } else {
                format!("identity '{identity_str}' has no allowlist entries configured")
            };
            AuditEntry::new(&identity_str, action, kind, AuditResult::DeniedNotAllowlisted, Some(&detail))
                .log();
            return Err(denied_response(StatusCode::FORBIDDEN, &detail));
        }

        let key = rate_limit_key(&identity_str, action);
        if self.inner.rate_limiter.check(&key).await == RateLimitDecision::Limited {
            let detail = format!("rate limit exceeded for '{identity_str}' on '{action}'");
            AuditEntry::new(&identity_str, action, kind, AuditResult::DeniedRateLimited, Some(&detail))
                .log();
            return Err(denied_response(StatusCode::TOO_MANY_REQUESTS, &detail));
        }

        Ok(GatewayContext {
            identity: identity_str,
            action: action.to_string(),
            kind,
        })
    }
}

fn denied_response(status: StatusCode, message: &str) -> Response {
    (status, [("content-type", "application/json")], json!({"error": message}).to_string())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn identity(s: &str) -> ClientIdentity {
        ClientIdentity(s.to_string())
    }

    fn policy_allowing(identity: &str, actions: &[&str]) -> AllowlistPolicy {
        let mut map = HashMap::new();
        map.insert(identity.to_string(), actions.iter().map(|s| s.to_string()).collect());
        AllowlistPolicy::new(map)
    }

    fn framework_with(policy: AllowlistPolicy, burst: u32) -> GatewayFramework {
        GatewayFramework::new(policy, Arc::new(InProcessRateLimiter::new(burst, 1000.0)))
    }

    // ── Fail-closed on missing identity ────────────────────────────────

    #[tokio::test]
    async fn missing_identity_is_denied_before_any_allowlist_check() {
        let fw = framework_with(policy_allowing("dev-box", &["*"]), 10);
        let result = fw.guard(None, "ledger_accounts", ActionKind::Tool).await;
        assert!(result.is_err());
        let resp = result.unwrap_err();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // ── Allowlist ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn allowed_identity_and_tool_clears_the_gate() {
        let fw = framework_with(policy_allowing("dev-box", &["ledger_accounts"]), 10);
        let id = identity("dev-box");
        let ctx = fw
            .guard(Some(&id), "ledger_accounts", ActionKind::Tool)
            .await
            .expect("configured identity+action should clear the gate");
        assert_eq!(ctx.identity(), "dev-box");
    }

    #[tokio::test]
    async fn wildcard_allows_every_action_for_that_identity() {
        let fw = framework_with(policy_allowing("harmony-primary", &["*"]), 10);
        let id = identity("harmony-primary");
        assert!(fw.guard(Some(&id), "anything_at_all", ActionKind::Tool).await.is_ok());
        assert!(fw
            .guard(Some(&id), "/v1/chat/completions", ActionKind::Inference)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn identity_not_on_allowlist_at_all_is_denied() {
        let fw = framework_with(AllowlistPolicy::default(), 10);
        let id = identity("brand-new-client");
        let result = fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn identity_allowlisted_for_a_different_action_is_denied() {
        let fw = framework_with(policy_allowing("dev-box", &["ledger_accounts"]), 10);
        let id = identity("dev-box");
        let result = fw.guard(Some(&id), "gitea_list_identities", ActionKind::Tool).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status(), StatusCode::FORBIDDEN);
    }

    // ── Rate limit ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn rate_limit_trips_after_burst_exhausted() {
        let fw = framework_with(policy_allowing("dev-box", &["*"]), 2);
        let id = identity("dev-box");

        assert!(fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok());
        assert!(fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok());
        let third = fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await;
        assert!(third.is_err(), "third call within the burst window should be rate-limited");
        assert_eq!(third.unwrap_err().status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn rate_limit_is_keyed_per_identity_and_action_independently() {
        let mut map = HashMap::new();
        map.insert("dev-box".to_string(), vec!["*".to_string()]);
        let fw = framework_with(AllowlistPolicy::new(map), 1);
        let id = identity("dev-box");

        assert!(fw.guard(Some(&id), "tool_a", ActionKind::Tool).await.is_ok());
        // Different action for the same identity has its own budget.
        assert!(fw.guard(Some(&id), "tool_b", ActionKind::Tool).await.is_ok());
        // But repeating tool_a again is now limited.
        assert!(fw.guard(Some(&id), "tool_a", ActionKind::Tool).await.is_err());
    }

    // ── Uniform pipeline: same code path for tool vs inference actions ──

    #[tokio::test]
    async fn same_guard_call_handles_both_action_kinds() {
        let fw = framework_with(policy_allowing("dev-box", &["*"]), 10);
        let id = identity("dev-box");

        let tool_ctx = fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.unwrap();
        let inference_ctx = fw
            .guard(Some(&id), "/v1/chat/completions", ActionKind::Inference)
            .await
            .unwrap();
        // Both went through the exact same `GatewayFramework::guard` method
        // -- the only difference is the `ActionKind` tag carried through to
        // the audit entry, proving one shared pipeline, not two.
        tool_ctx.record_result(true, None);
        inference_ctx.record_result(true, None);
    }

    // ── record_result / audit shape (no panics, sanitizes detail) ───────

    #[tokio::test]
    async fn record_result_success_and_failure_do_not_panic() {
        let fw = framework_with(policy_allowing("dev-box", &["*"]), 10);
        let id = identity("dev-box");
        let ctx = fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.unwrap();
        ctx.record_result(true, None);

        let ctx2 = fw.guard(Some(&id), "gitea_list_identities", ActionKind::Tool).await.unwrap();
        ctx2.record_result(false, Some("upstream token=shouldnotleak failed"));
    }

    // ── AllowlistPolicy::from_env malformed JSON -> empty, not a panic ──

    #[test]
    fn allowlist_from_env_malformed_json_degrades_to_deny_all() {
        std::env::set_var("TERMINUS_GATEWAY_ALLOWLIST_JSON", "not valid json");
        let policy = AllowlistPolicy::from_env();
        assert!(!policy.is_allowed("anyone", "anything"));
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
    }

    #[test]
    fn allowlist_from_env_parses_configured_policy() {
        std::env::set_var(
            "TERMINUS_GATEWAY_ALLOWLIST_JSON",
            r#"{"dev-box": ["ledger_accounts", "*"]}"#,
        );
        let policy = AllowlistPolicy::from_env();
        assert!(policy.is_allowed("dev-box", "ledger_accounts"));
        assert!(policy.is_allowed("dev-box", "literally_anything"));
        assert!(!policy.is_allowed("someone-else", "ledger_accounts"));
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
    }
}
