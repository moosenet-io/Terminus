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
//!    LHEG-02 (S109 lumina/harmony egress-client sprint) scaffolds `lumina`
//!    and `harmony` into [`AllowlistPolicy::from_env`]'s result as
//!    recognized entries — see [`SCAFFOLDED_IDENTITIES`] — so those two
//!    identities (LHEG-01 lets `lumina-core`/`harmony-core` enroll as them)
//!    always have a defined entry from the moment enrollment succeeds, not
//!    just implicit absence. LHEG-07 (this item) upgrades that scaffold
//!    from empty (deny-all) to a broad-allow-minus-sensitive-deny
//!    [`Grant::AllowDeny`] — see [`DEFAULT_SENSITIVE_DENY_PREFIXES`] — since
//!    hand-listing every one of the ~300 legitimate tool/route names each
//!    identity needs is impractical, and a bare `"*"` grant would reach the
//!    moose-scoped/sensitive routes (github/mirror/secrets-manager/ansible/
//!    etc.) this item exists to keep closed.
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

/// Identities scaffolded into every `from_env()`-built [`AllowlistPolicy`]
/// as recognized entries — LHEG-02 (Terminus S109 lumina/harmony
/// egress-client sprint). `lumina` and `harmony` are the Terminus
/// identities LHEG-01 lets `lumina-core`/`harmony-core` enroll as; this
/// scaffold exists so a freshly-enrolled identity has a defined entry in
/// the allowlist the moment enrollment succeeds, rather than relying on an
/// implicit "absent and therefore denied" gap. As of LHEG-07 the scaffold
/// default is [`Grant::AllowDeny`] with `allow: ["*"]` and
/// `deny: DEFAULT_SENSITIVE_DENY_PREFIXES` — broad utility access with the
/// moose-scoped/sensitive routes carved out — rather than LHEG-02's
/// original empty (deny-all) placeholder. Neither identity's default is
/// ever a bare `"*"` grant with no deny layer — see the S109 spec's
/// RESOLVED decision 2 (minimum-necessary allowlists, not `*`).
pub const SCAFFOLDED_IDENTITIES: &[&str] = &["lumina", "harmony"];

/// Tool-name / route PREFIXES denied by default to the [`SCAFFOLDED_IDENTITIES`]
/// (`lumina`, `harmony`) — LHEG-07. A deny entry matches an action if the
/// action equals the entry OR starts with it (`action.starts_with(prefix)`),
/// so e.g. `"github_"` catches `github_push_repo`, `github_create_repo`,
/// `github_list_repos`, etc. without enumerating each one. Rationale per
/// entry:
/// - `github_`, `git_public`, `git_private` — the GitHub push/mirror
///   surface. This is the specific hole LHEG-07 closes: a bare `"*"` grant
///   otherwise lets lumina/harmony reach `GITHUB_PAT_MOOSE`/mirror creds
///   "using Moose where available" via `crate::pki`'s credential
///   resolution, even though neither identity should ever push/mirror.
/// - `gitea_cargo_publish`, `gitea_cargo_yank` — publishing/yanking crates
///   from the internal registry is a release action, not routine
///   Plane/Gitea read-write egress work either identity legitimately does.
/// - `infisical_` — secret material. Per the standing "no self-serve
///   secrets" rule (see `feedback_no_self_serve_secrets` memory), no
///   non-`moose`/`claude` identity should be able to fetch secrets-manager
///   secrets directly.
/// - `ansible_`, `openhands_` — fleet-ops execution surfaces (playbook
///   runs, autonomous dev-agent triggers) that are moose-operator actions,
///   not something a personal-assistant or build-orchestrator identity
///   should trigger.
/// - `approval_` — the guarded-tool approval gate itself (grant/deny); an
///   identity approving its own guarded-tool requests would defeat the
///   human-in-the-loop point of that gate.
/// - `dev_write_file`, `dev_run_command`, `dev_trigger_openhands` — arbitrary
///   filesystem write / command execution / dev-agent triggering on the
///   dev box. (`dev_read_file`, `dev_list_workspaces`, `dev_open_workspace`
///   are NOT denied — read-only workspace introspection is legitimate
///   broad utility.)
/// - `routines_batch_` — bulk routine mutation (e.g.
///   `routines_batch_edit_notify_channel`) is an operator-scale action;
///   single-routine `routines_edit`/`routines_propose` are not denied.
/// - `soma_rename_agent`, `soma_skill_approve` — identity/skill-governance
///   actions scoped to the moose operator, not routine egress traffic.
pub const DEFAULT_SENSITIVE_DENY_PREFIXES: &[&str] = &[
    "github_",
    "git_public",
    "git_private",
    "gitea_cargo_publish",
    "gitea_cargo_yank",
    "infisical_",
    "ansible_",
    "openhands_",
    "approval_",
    "dev_write_file",
    "dev_run_command",
    "dev_trigger_openhands",
    "routines_batch_",
    "soma_rename_agent",
    "soma_skill_approve",
];

/// A single identity's grant, in either of two shapes:
///
/// - [`Grant::List`] — the original LHEG-02 form: a plain allow-list.
///   `"*"` allows every action, otherwise exact match only. No deny layer
///   at all — kept for back-compat with existing
///   `TERMINUS_GATEWAY_ALLOWLIST_JSON` configs (e.g. `moose`/`claude`'s
///   `["*"]` full-access entries) and hand-authored `AllowlistPolicy::new`
///   callers/tests.
/// - [`Grant::AllowDeny`] — LHEG-07: an `allow` list (checked exactly like
///   [`Grant::List`]) minus a `deny` set of PREFIXES that wins even over an
///   `allow: ["*"]` wildcard. This is what makes "broad access except the
///   sensitive stuff" expressible without hand-listing ~300 tool names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    List(Vec<String>),
    AllowDeny { allow: Vec<String>, deny: Vec<String> },
}

impl Grant {
    /// Whether this grant permits `action`. For [`Grant::AllowDeny`], a
    /// deny-prefix match wins even if `allow` contains `"*"` — deny is
    /// checked only after confirming `allow` would otherwise grant it, but
    /// its result overrides that grant unconditionally (no such thing as
    /// "denied but also separately allowed").
    fn permits(&self, action: &str) -> bool {
        match self {
            Grant::List(actions) => actions.iter().any(|a| a == "*" || a == action),
            Grant::AllowDeny { allow, deny } => {
                let allowed = allow.iter().any(|a| a == "*" || a == action);
                if !allowed {
                    return false;
                }
                !deny.iter().any(|d| action == d || action.starts_with(d.as_str()))
            }
        }
    }
}

impl From<Vec<String>> for Grant {
    fn from(actions: Vec<String>) -> Self {
        Grant::List(actions)
    }
}

/// The `TERMINUS_GATEWAY_ALLOWLIST_JSON` shape for one identity, as parsed
/// straight off the wire before being converted to a [`Grant`] — supports
/// BOTH the legacy bare-array form (`["a", "b", "*"]`) and the new
/// allow/deny object form (`{"allow": [...], "deny": [...]}`), so existing
/// env configs keep working unmodified.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum RawGrant {
    List(Vec<String>),
    AllowDeny {
        #[serde(default)]
        allow: Vec<String>,
        #[serde(default)]
        deny: Vec<String>,
    },
}

impl From<RawGrant> for Grant {
    fn from(raw: RawGrant) -> Self {
        match raw {
            RawGrant::List(actions) => Grant::List(actions),
            RawGrant::AllowDeny { allow, deny } => Grant::AllowDeny { allow, deny },
        }
    }
}

/// The scaffold entries themselves: each [`SCAFFOLDED_IDENTITIES`] identity
/// mapped to the LHEG-07 default posture — broad allow, sensitive routes
/// denied.
fn scaffold_defaults() -> HashMap<String, Grant> {
    SCAFFOLDED_IDENTITIES
        .iter()
        .map(|id| {
            (
                (*id).to_string(),
                Grant::AllowDeny {
                    allow: vec!["*".to_string()],
                    deny: DEFAULT_SENSITIVE_DENY_PREFIXES.iter().map(|s| s.to_string()).collect(),
                },
            )
        })
        .collect()
}

/// Per-identity allow policy: which tool names / inference routes each
/// enrolled identity may use. Config-driven
/// (`crate::config::gateway_allowlist_json`, a JSON object of
/// `identity -> [action, ...]` OR `identity -> {"allow": [...], "deny":
/// [...]}` — see [`Grant`]/[`RawGrant`]). Default-deny: an identity with no
/// entry in the policy at all is denied every action — see this module's
/// doc for why (no prior identity-scoped mechanism to fall back to, and the
/// TGW-04 spec item's edge case calls for a clean denial, not a silent
/// empty-catalog response).
#[derive(Debug, Clone, Default)]
pub struct AllowlistPolicy {
    entries: HashMap<String, Grant>,
}

impl AllowlistPolicy {
    /// Build a policy directly from a map — mainly for tests and for
    /// callers that already have the data in hand rather than as env JSON.
    /// Does NOT apply the [`SCAFFOLDED_IDENTITIES`] defaults (those are a
    /// `from_env()`-only convenience for the production entrypoint) — a
    /// caller using this constructor directly gets exactly the map it
    /// passed, nothing implicit added.
    pub fn new(entries: HashMap<String, Grant>) -> Self {
        Self { entries }
    }

    /// Build a policy from `crate::config::gateway_allowlist_json`, with
    /// [`SCAFFOLDED_IDENTITIES`] (`lumina`, `harmony`) always present as
    /// recognized entries defaulting to [`scaffold_defaults`]'s
    /// allow-broad-minus-sensitive-deny posture, unless the env JSON itself
    /// mentions them (LHEG-07) — env wins per-identity: any identity the
    /// env JSON mentions, including `lumina`/`harmony`, uses the env value
    /// in full, not a merge of the two grants. A malformed JSON value
    /// degrades to the scaffold-only policy (every non-scaffolded identity
    /// deny-all, `lumina`/`harmony` fall back to their safe default) rather
    /// than panicking the process at startup — a config typo should not
    /// crash the gateway, it should just deny everyone else until fixed
    /// (loudly logged so the operator notices).
    pub fn from_env() -> Self {
        let raw = crate::config::gateway_allowlist_json();
        let mut entries = scaffold_defaults();
        match serde_json::from_str::<HashMap<String, RawGrant>>(&raw) {
            Ok(parsed) => {
                entries.extend(parsed.into_iter().map(|(id, grant)| (id, Grant::from(grant))));
                Self { entries }
            }
            Err(e) => {
                tracing::error!(
                    "gateway_framework: TERMINUS_GATEWAY_ALLOWLIST_JSON is not valid JSON \
                     ({e}) -- falling back to the scaffold-only allowlist policy (deny-all \
                     except the lumina/harmony safe default)"
                );
                Self { entries }
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
    /// identity with no entry (default-deny), whose grant doesn't contain
    /// `action`/`"*"`, or (for an allow/deny grant) whose `action` matches
    /// a deny prefix even if it would otherwise be allowed.
    pub fn is_allowed(&self, identity: &str, action: &str) -> bool {
        match self.entries.get(identity) {
            Some(grant) => grant.permits(action),
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
        map.insert(
            identity.to_string(),
            Grant::List(actions.iter().map(|s| s.to_string()).collect()),
        );
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
        map.insert("dev-box".to_string(), Grant::List(vec!["*".to_string()]));
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

    // ── Grant: allow/deny object form (LHEG-07) ─────────────────────────

    #[test]
    fn allow_deny_grant_denies_a_prefix_even_under_wildcard_allow() {
        let grant = Grant::AllowDeny {
            allow: vec!["*".to_string()],
            deny: vec!["github_".to_string()],
        };
        assert!(grant.permits("reminder_poll"));
        assert!(!grant.permits("github_push_repo"));
        assert!(!grant.permits("github_create_repo"));
    }

    #[test]
    fn allow_deny_grant_exact_deny_entry_also_blocks_exact_action() {
        let grant = Grant::AllowDeny {
            allow: vec!["*".to_string()],
            deny: vec!["git_public".to_string()],
        };
        // Exact match on the deny entry itself.
        assert!(!grant.permits("git_public"));
        // Prefix match too.
        assert!(!grant.permits("git_public_mirror_push"));
    }

    #[test]
    fn allow_deny_grant_deny_cannot_grant_access_allow_didnt() {
        // An action absent from `allow` and absent from `deny` is still
        // denied -- deny only ever narrows, it never widens `allow`.
        let grant = Grant::AllowDeny {
            allow: vec!["ledger_accounts".to_string()],
            deny: vec!["github_".to_string()],
        };
        assert!(!grant.permits("gitea_list_identities"));
    }

    #[test]
    fn legacy_list_grant_has_no_deny_layer() {
        // `Grant::List` (the pre-LHEG-07 shape) has no deny concept at all
        // -- `"*"` really does mean everything, back-compat with existing
        // moose/claude/dev-box style configs.
        let grant = Grant::List(vec!["*".to_string()]);
        assert!(grant.permits("github_push_repo"));
        assert!(grant.permits("infisical_get_secret"));
    }

    // ── AllowlistPolicy::from_env parses both the legacy array form and
    //    the new allow/deny object form (LHEG-07) ───────────────────────

    #[test]
    fn from_env_legacy_array_form_still_works() {
        std::env::set_var(
            "TERMINUS_GATEWAY_ALLOWLIST_JSON",
            r#"{"moose": ["*"]}"#,
        );
        let policy = AllowlistPolicy::from_env();
        assert!(policy.is_allowed("moose", "literally_anything"));
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
    }

    #[test]
    fn from_env_allow_deny_object_form_parses_and_enforces_deny() {
        std::env::set_var(
            "TERMINUS_GATEWAY_ALLOWLIST_JSON",
            r#"{"harmony": {"allow": ["*"], "deny": ["infisical_", "ansible_"]}}"#,
        );
        let policy = AllowlistPolicy::from_env();
        assert!(policy.is_allowed("harmony", "plane_list_work_items"));
        assert!(!policy.is_allowed("harmony", "infisical_get_secret"));
        assert!(!policy.is_allowed("harmony", "ansible_run_playbook"));
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
    }

    // ── moose keeps full, unrestricted access ────────────────────────────

    #[test]
    fn moose_with_a_plain_wildcard_grant_reaches_every_route_including_sensitive_ones() {
        std::env::set_var("TERMINUS_GATEWAY_ALLOWLIST_JSON", r#"{"moose": ["*"]}"#);
        let policy = AllowlistPolicy::from_env();
        for action in ["github_push_repo", "git_public_mirror_push", "infisical_get_secret", "ansible_run_playbook"]
        {
            assert!(policy.is_allowed("moose", action), "moose must retain access to '{action}'");
        }
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
    }

    // ── LHEG-02/LHEG-07: lumina/harmony scaffold ─────────────────────────

    /// `lumina` and `harmony` are recognized by the allowlist with a
    /// defined default grant when no env override mentions them at all.
    #[test]
    fn lumina_and_harmony_are_scaffolded_by_default() {
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
        let policy = AllowlistPolicy::from_env();
        assert!(policy.has_any_entry("lumina"), "lumina must be a recognized identity");
        assert!(policy.has_any_entry("harmony"), "harmony must be a recognized identity");
    }

    /// LHEG-07 acceptance criterion: the default scaffold grants BOTH
    /// identities broad, ordinary tool/route access (not requiring a
    /// hand-maintained allow-list of ~300 names) ...
    #[tokio::test]
    async fn lumina_and_harmony_default_scaffold_allows_ordinary_routes() {
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
        let fw = framework_with(AllowlistPolicy::from_env(), 10);

        for id_str in SCAFFOLDED_IDENTITIES {
            let id = identity(id_str);
            for action in ["reminder_poll", "ledger_accounts", "/v1/chat/completions", "plane_list_work_items"] {
                let result = fw.guard(Some(&id), action, ActionKind::Tool).await;
                assert!(result.is_ok(), "{id_str} should be allowed '{action}' by the LHEG-07 default scaffold");
            }
        }
    }

    /// ... but DENIES every moose-scoped/sensitive route, closing the hole
    /// where a bare `"*"` grant would let lumina/harmony reach
    /// `GITHUB_PAT_MOOSE`/mirror creds "using Moose where available".
    #[tokio::test]
    async fn lumina_and_harmony_default_scaffold_denies_sensitive_routes() {
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
        let fw = framework_with(AllowlistPolicy::from_env(), 10);

        for id_str in SCAFFOLDED_IDENTITIES {
            let id = identity(id_str);
            for action in [
                "github_push_repo",
                "github_create_repo",
                "git_public_mirror_push",
                "git_private",
                "gitea_cargo_publish",
                "gitea_cargo_yank",
                "infisical_get_secret",
                "ansible_run_playbook",
                "openhands_run_task",
                "approval_grant",
                "dev_write_file",
                "dev_run_command",
                "dev_trigger_openhands",
                "routines_batch_edit_notify_channel",
                "soma_rename_agent",
                "soma_skill_approve",
            ] {
                let result = fw.guard(Some(&id), action, ActionKind::Tool).await;
                assert!(
                    result.is_err(),
                    "{id_str} must be denied for sensitive route '{action}' even under the broad default grant"
                );
                assert_eq!(result.unwrap_err().status(), StatusCode::FORBIDDEN);
            }
        }
    }

    /// Deny wins over allow: `lumina` is allowed an ordinary action
    /// (`reminder_poll`) but DENIED the specific moose-only routes named in
    /// the S109 spec's motivating example (github push, mirror push,
    /// secrets-manager get-secret) -- proving the deny layer, not just the
    /// absence of a grant, is what's blocking these.
    #[tokio::test]
    async fn deny_wins_over_allow_lumina_cannot_reach_github_mirror_or_secrets_manager() {
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
        let fw = framework_with(AllowlistPolicy::from_env(), 10);
        let id = identity("lumina");

        assert!(fw.guard(Some(&id), "reminder_poll", ActionKind::Tool).await.is_ok());

        for action in ["github_push_repo", "git_public_mirror_push", "infisical_get_secret"] {
            let result = fw.guard(Some(&id), action, ActionKind::Tool).await;
            assert!(result.is_err(), "lumina must be denied '{action}'");
            assert_eq!(result.unwrap_err().status(), StatusCode::FORBIDDEN);
        }
    }

    /// Env JSON still wins per-identity: if the operator's
    /// `TERMINUS_GATEWAY_ALLOWLIST_JSON` explicitly grants `lumina` a
    /// narrower allow/deny object, that grant is honored in full rather
    /// than being shadowed by the scaffold default.
    #[test]
    fn env_override_for_a_scaffolded_identity_still_wins() {
        std::env::set_var(
            "TERMINUS_GATEWAY_ALLOWLIST_JSON",
            r#"{"lumina": ["/v1/chat/completions"]}"#,
        );
        let policy = AllowlistPolicy::from_env();
        assert!(policy.is_allowed("lumina", "/v1/chat/completions"));
        assert!(!policy.is_allowed("lumina", "gitea_list_identities"));
        // harmony wasn't mentioned in the env override -- still scaffolded
        // to its LHEG-07 default (broad-minus-sensitive), not the narrow
        // env value given to lumina.
        assert!(policy.has_any_entry("harmony"));
        assert!(policy.is_allowed("harmony", "/v1/chat/completions"));
        assert!(!policy.is_allowed("harmony", "github_push_repo"));
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
    }

    /// A malformed `TERMINUS_GATEWAY_ALLOWLIST_JSON` still degrades to a
    /// safe policy: every non-scaffolded identity is deny-all, while
    /// `lumina`/`harmony` still fall back to their safe LHEG-07 default
    /// (broad ordinary access, sensitive routes denied) -- a config typo
    /// should not also strip the two enrolled identities of the deny-set
    /// that protects moose-only routes.
    #[test]
    fn malformed_env_json_still_scaffolds_lumina_and_harmony_safely() {
        std::env::set_var("TERMINUS_GATEWAY_ALLOWLIST_JSON", "not valid json");
        let policy = AllowlistPolicy::from_env();
        assert!(policy.has_any_entry("lumina"));
        assert!(policy.has_any_entry("harmony"));
        // Non-scaffolded identities: deny-all.
        assert!(!policy.is_allowed("anyone", "anything"));
        // Scaffolded identities: safe default, not deny-all and not
        // wide-open either.
        assert!(policy.is_allowed("lumina", "reminder_poll"));
        assert!(!policy.is_allowed("lumina", "github_push_repo"));
        assert!(policy.is_allowed("harmony", "reminder_poll"));
        assert!(!policy.is_allowed("harmony", "infisical_get_secret"));
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");
    }
}
