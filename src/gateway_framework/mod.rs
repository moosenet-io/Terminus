//! Uniform per-request gateway pipeline (TGW-04 — Terminus Primary Gateway
//! sprint, S108): mTLS identity → allowlist → rate-limit → dispatch → audit,
//! applied identically to BOTH request paths `terminus-primary` serves —
//! tool calls (TGW-01/TGW-02's core + federated-personal dispatch inside
//! `crate::mcp_server::handle_mcp`'s `tools/call` branch) and inference
//! proxying (TGW-03's `crate::inference_proxy` routes) — so the framework is
//! one shared thing both routes go through, not two divergent bolt-ons.
//!
//! ## Stages
//! 1. **Identity** — [`GatewayFramework::guard`] takes an
//!    `Option<&crate::mesh::Principal>` (MESH-06) — the single, reconciled
//!    identity `crate::mesh::PrincipalResolver` would produce from the
//!    caller's mTLS-derived identity (`crate::pki::mtls::ClientIdentity`,
//!    extracted by `crate::pki::mtls::run_listener` and attached to the
//!    request's extensions *by the server*, post-handshake) and/or tailnet
//!    WhoIs identity (`crate::mesh::TailnetIdentity`, MESH-05). Existing
//!    callers that only ever had a `ClientIdentity` keep working via
//!    [`crate::mesh::Principal`]'s `From<&ClientIdentity>` conversion (see
//!    that impl's doc for why it's a direct, resolver-bypassing mapping
//!    today — full resolver wiring into the live request path is MESH-07).
//!    This module never trusts a client-supplied identity field/header —
//!    `guard` treats `None` as fail-closed (see below), never as "identity
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
use serde_json::{json, Value};

use crate::mesh::Principal;
use audit::{AuditDecision, AuditEntry, AuditResult};
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
    /// TMOD-05: a broker admin-control-plane request (worker
    /// register/deregister/health/list) — `action` is an
    /// [`ADMIN_ACTION_PREFIX`]-prefixed `"admin:<op>"` label (e.g.
    /// `"admin:register_worker"`), never a bare tool name, so an admin audit
    /// entry is never confusable with a `Tool`-kind one sharing the same
    /// identity/action string, AND — critically — so admin authorization can
    /// be made KIND-AWARE: an `Admin` action is authorized ONLY by an
    /// explicitly admin-scoped grant entry, never by a generic tool wildcard
    /// (see [`AllowlistPolicy::is_allowed_admin`] / [`Grant::permits_admin`]).
    Admin,
}

/// The action-string namespace every [`ActionKind::Admin`] action carries
/// (`crate::broker::control` emits `"admin:register_worker"`,
/// `"admin:deregister_worker"`, `"admin:health_worker"`,
/// `"admin:list_workers"`). Authorization for an `Admin` action requires a
/// grant entry WITHIN this namespace — an admin-scoped exact entry or an
/// `"admin:*"`/`"admin:<prefix>*"` wildcard — never a bare `"*"` tool
/// wildcard. This is what prevents a broad tool/inference identity
/// (`Grant::List(["*"])` / `allow: ["*"]`) from silently escalating to
/// worker-control admin (a route-hijack privilege escalation).
pub const ADMIN_ACTION_PREFIX: &str = "admin:";

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
    ///
    /// MESH-08: `action` may now be a plain local tool/route name OR a mesh
    /// namespaced name (`<namespace>__<tool>`, see
    /// [`crate::mesh::merge::namespaced`]) — an allow ENTRY may itself be a
    /// bare wildcard (`"*"`), an exact plain/namespaced name
    /// (`"ct322__ledger_add"`), or a namespace wildcard
    /// (`"ct322__*"`, matching every tool exported by that one upstream) via
    /// [`grant_entry_matches`]. A DENY entry is checked against `action`
    /// verbatim AND, when `action` is namespaced, against its bare (post-`__`)
    /// tool name as well — see [`deny_matches`] for why: this is what makes
    /// [`DEFAULT_SENSITIVE_DENY_PREFIXES`] (authored against bare names like
    /// `"github_"`) continue to close a sensitive tool re-exported through
    /// ANY upstream namespace, not just the local/bare form.
    fn permits(&self, action: &str) -> bool {
        match self {
            Grant::List(actions) => actions.iter().any(|a| grant_entry_matches(a, action)),
            Grant::AllowDeny { allow, deny } => {
                let allowed = allow.iter().any(|a| grant_entry_matches(a, action));
                if !allowed {
                    return false;
                }
                !deny.iter().any(|d| deny_matches(d, action))
            }
        }
    }

    /// TMOD-05: whether this grant EXPLICITLY authorizes an admin `action`
    /// (an [`ADMIN_ACTION_PREFIX`]-namespaced string). Identical in shape to
    /// [`Grant::permits`] (deny still wins for an `AllowDeny` grant), but the
    /// allow side uses [`admin_entry_matches`] instead of
    /// [`grant_entry_matches`] — so a bare `"*"` tool wildcard NEVER
    /// satisfies an admin action; only an admin-namespace-scoped entry
    /// (`"admin:*"`, `"admin:<prefix>*"`, or an exact `"admin:<op>"`) does.
    /// This is the kind-aware authorization the admin surface requires: a
    /// broad tool identity is not, by that fact alone, a worker-control
    /// admin.
    fn permits_admin(&self, action: &str) -> bool {
        match self {
            Grant::List(actions) => actions.iter().any(|a| admin_entry_matches(a, action)),
            Grant::AllowDeny { allow, deny } => {
                let allowed = allow.iter().any(|a| admin_entry_matches(a, action));
                if !allowed {
                    return false;
                }
                !deny.iter().any(|d| deny_matches(d, action))
            }
        }
    }
}

/// Whether allow/list `entry` matches `action`. Three shapes:
/// - `"*"` — matches everything.
/// - `"<prefix>*"` (any other entry ending in `*`) — matches every `action`
///   starting with `prefix`. This is what lets an allow entry like
///   `"ct322__*"` grant an entire mesh upstream namespace, or (equally)
///   `"github_*"` grant a local prefix, without hand-listing every tool name
///   — additive over the pre-MESH-08 behavior, where only the bare `"*"`
///   entry had any wildcard meaning at all (a non-`"*"` entry was always an
///   exact match), so no existing config's meaning changes.
/// - anything else — exact match only, the original (pre-MESH-08) behavior.
fn grant_entry_matches(entry: &str, action: &str) -> bool {
    if entry == "*" {
        return true;
    }
    match entry.strip_suffix('*') {
        Some(prefix) => action.starts_with(prefix),
        None => entry == action,
    }
}

/// TMOD-05: whether allow/list `entry` EXPLICITLY authorizes admin `action`
/// (an [`ADMIN_ACTION_PREFIX`]-namespaced string). Deliberately STRICTER
/// than [`grant_entry_matches`]: a bare `"*"` (or any wildcard whose prefix
/// is not itself within the admin namespace) does NOT match — an admin
/// action is granted only by
/// - an exact admin entry (`entry == action`, e.g.
///   `"admin:register_worker"`), or
/// - an admin-namespace-scoped wildcard whose prefix starts with
///   [`ADMIN_ACTION_PREFIX`] (e.g. `"admin:*"`, `"admin:reg*"`).
///
/// So `Grant::List(["*"])` — a full tool wildcard — authorizes every TOOL
/// call but NO admin op; only a grant that names the `admin:` namespace
/// does. This is the fix for the privilege-escalation gap where a generic
/// tool wildcard silently authorized worker register/deregister.
fn admin_entry_matches(entry: &str, action: &str) -> bool {
    if entry == action {
        // An exact match is always explicit -- but only an admin-namespaced
        // action can reach here as an `Admin`-kind action anyway; guard
        // against a mis-scoped caller by still requiring the namespace.
        return action.starts_with(ADMIN_ACTION_PREFIX);
    }
    match entry.strip_suffix('*') {
        // A wildcard counts ONLY if its prefix is itself admin-scoped, so a
        // bare "*" (prefix "") or a non-admin prefix ("tool_*") never grants
        // an admin action.
        Some(prefix) => prefix.starts_with(ADMIN_ACTION_PREFIX) && action.starts_with(prefix),
        None => false,
    }
}

/// Whether deny-prefix `entry` matches `action`, per [`Grant::AllowDeny`]'s
/// existing exact-or-prefix rule — applied to `action` as given AND, when
/// `action` is a mesh namespaced name (`<namespace>__<tool>`), to its bare
/// tool name too (MESH-08). This composition is deliberate: a deny entry
/// like `"github_"` in [`DEFAULT_SENSITIVE_DENY_PREFIXES`] was authored
/// against bare local tool names, from before any upstream could re-export a
/// same-named sensitive tool under a namespace prefix. Without this bare-name
/// fallback, `"ct322__github_push_repo"` would slip past a deny entry that
/// very obviously means to block it — the sensitive-deny prefixes are
/// meant to compose WITH namespacing, not be shadowed by it.
fn deny_matches(entry: &str, action: &str) -> bool {
    if action == entry || action.starts_with(entry) {
        return true;
    }
    if let Some((_, bare)) = crate::mesh::merge::split_namespaced(action) {
        if bare == entry || bare.starts_with(entry) {
            return true;
        }
    }
    false
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

    /// TMOD-05: whether `identity` may perform an ADMIN `action` (an
    /// [`ADMIN_ACTION_PREFIX`]-namespaced string). Same default-deny posture
    /// as [`Self::is_allowed`] (no entry ⇒ denied), but backed by
    /// [`Grant::permits_admin`] instead of [`Grant::permits`], so a generic
    /// tool wildcard (`"*"`) does NOT authorize an admin op — only an
    /// explicit admin-scoped grant does. [`GatewayFramework::guard`] routes
    /// every [`ActionKind::Admin`] request through THIS check rather than
    /// [`Self::is_allowed`], closing the wildcard-tool-grant privilege
    /// escalation onto the worker-control surface.
    pub fn is_allowed_admin(&self, identity: &str, action: &str) -> bool {
        match self.entries.get(identity) {
            Some(grant) => grant.permits_admin(action),
            None => false,
        }
    }

    /// MESH-08: filter a `tools/list` catalog (a `Vec` of MCP `Tool` JSON
    /// objects, each with a `"name"` field — the same shape
    /// [`crate::mesh::merge::MergedCatalog::tools`] and
    /// `src/mcp_server.rs`'s `tools/list` handler already build) down to
    /// exactly the tools `identity` may CALL per this policy. A tool object
    /// with no `"name"` field at all (should not happen in practice, but
    /// this is a filter, not a validator) is dropped rather than kept —
    /// fail-closed, consistent with `is_allowed`'s own default-deny.
    ///
    /// This is the single source of truth both `tools/list` visibility and
    /// `tools/call` enforcement are checked against ([`Self::is_allowed`] is
    /// exactly what [`GatewayFramework::guard`] calls for the `tools/call`
    /// gate) — a tool this method keeps is always also callable, and a tool
    /// it drops is always also denied at call time, by construction (same
    /// underlying `Grant::permits` decision, same `action` string: the
    /// tool's advertised `"name"`, namespaced or not).
    pub fn filter_tools(&self, identity: &str, tools: Vec<Value>) -> Vec<Value> {
        tools
            .into_iter()
            .filter(|t| match t.get("name").and_then(|n| n.as_str()) {
                Some(name) => self.is_allowed(identity, name),
                None => false,
            })
            .collect()
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
    /// MESH-10: the mesh namespace this call was routed to, if any — set via
    /// [`Self::with_upstream`] once the caller (`crate::mcp_server`) has
    /// resolved the `tools/call` route. `None` for local/personal-federated
    /// dispatch and for every non-`Tool` (inference) request.
    upstream: Option<String>,
    /// MESH-10: the bare (un-namespaced) tool name actually dispatched.
    /// Equal to `action` until/unless [`Self::with_upstream`] overrides it.
    tool_bare: String,
}

impl GatewayContext {
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// MESH-10: attach federated-dispatch context — the mesh namespace this
    /// call routed to, and the bare tool name forwarded to that upstream —
    /// before calling [`Self::record_result`]. Local (non-federated) call
    /// sites never call this, leaving `upstream` `None` and `tool_bare`
    /// equal to the advertised `action`, exactly as constructed by
    /// [`GatewayFramework::guard`].
    pub fn with_upstream(mut self, upstream: impl Into<String>, tool_bare: impl Into<String>) -> Self {
        self.upstream = Some(upstream.into());
        self.tool_bare = tool_bare.into();
        self
    }

    /// Record the terminal outcome of a request this context already
    /// cleared the gate for, and audit it. Call exactly once, after
    /// dispatch completes (success or failure) — `guard()` already audited
    /// any denial that happened before dispatch, so this is the ONE place
    /// the "dispatched" branch of the audit trail is written, keeping the
    /// invariant "exactly one audit entry per request" true whether the
    /// request was denied or completed.
    ///
    /// `detail` is passed through `audit::sanitize` (via
    /// `AuditEntry::new_federated`) before it's logged — pass a short
    /// summary (e.g. a tool error's `Display` output, or a sanitized args
    /// dump), never a raw payload.
    ///
    /// MESH-10: if `detail` carries `crate::approval`'s "APPROVAL REQUIRED"
    /// marker (a guarded local tool that was gated but NOT dispatched), the
    /// decision recorded is [`AuditDecision::ApprovalRequired`] rather than
    /// `Allow`, even though this context already cleared the identity/
    /// allowlist/rate-limit gate — the approval gate is a second, tool-level
    /// gate this framework doesn't itself enforce but must still audit.
    pub fn record_result(&self, success: bool, detail: Option<&str>) {
        self.record_outcome(None, success, detail);
    }

    /// MESH-10: like [`Self::record_result`], but for the case dispatch
    /// couldn't even be attempted at the transport level — a federated (mesh)
    /// upstream that's unhealthy/unregistered, or a network-level failure
    /// calling one that IS registered. Always audited (never a silent drop):
    /// records [`AuditDecision::TransportFailure`] rather than `Allow`, so a
    /// reviewer can tell "upstream unreachable" apart from "upstream reached,
    /// but the tool call itself errored" ([`Self::record_result`] with
    /// `success: false`).
    pub fn record_transport_failure(&self, detail: Option<&str>) {
        self.record_outcome(Some(AuditDecision::TransportFailure), false, detail);
    }

    fn record_outcome(&self, decision_override: Option<AuditDecision>, success: bool, detail: Option<&str>) {
        let result = if success { AuditResult::Success } else { AuditResult::Failure };
        let decision = decision_override.unwrap_or_else(|| {
            if detail.map(is_approval_required_marker).unwrap_or(false) {
                AuditDecision::ApprovalRequired
            } else {
                AuditDecision::Allow
            }
        });
        AuditEntry::new_federated(
            &self.identity,
            self.upstream.clone(),
            &self.action,
            &self.tool_bare,
            self.kind,
            result,
            decision,
            detail,
        )
        .log();
    }
}

/// MESH-10: detect `crate::approval`'s "APPROVAL REQUIRED" gate marker in an
/// (unsanitized) detail string. A plain substring check on the exact marker
/// text `approval.rs` emits — kept local rather than importing
/// `crate::approval` to avoid coupling this module to tool-gate internals
/// for a single string constant.
fn is_approval_required_marker(detail: &str) -> bool {
    detail.contains("APPROVAL REQUIRED")
}

struct GatewayFrameworkInner {
    allowlist: AllowlistPolicy,
    rate_limiter: Arc<dyn RateLimiter>,
    /// BLD-20: bounded FIFO admission queue for over-limit requests. `None` =
    /// no queuing (immediate 429 on over-limit) — the case for the in-process
    /// limiter / tests. `Some` when the shared Redis is configured.
    request_queue: Option<Arc<crate::ratelimit::RequestQueue>>,
    queue_max_depth: i64,
    queue_max_wait: std::time::Duration,
    queue_poll: std::time::Duration,
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
    /// Build with an explicit limiter and NO admission queue (immediate 429 on
    /// over-limit). Used by tests and the in-process path.
    pub fn new(allowlist: AllowlistPolicy, rate_limiter: Arc<dyn RateLimiter>) -> Self {
        Self::with_queue(allowlist, rate_limiter, None)
    }

    /// Build with an explicit limiter and an optional bounded FIFO admission
    /// queue (BLD-20). Queue knobs come from `crate::config`.
    pub fn with_queue(
        allowlist: AllowlistPolicy,
        rate_limiter: Arc<dyn RateLimiter>,
        request_queue: Option<Arc<crate::ratelimit::RequestQueue>>,
    ) -> Self {
        Self {
            inner: Arc::new(GatewayFrameworkInner {
                allowlist,
                rate_limiter,
                request_queue,
                queue_max_depth: crate::config::gateway_queue_max_depth(),
                queue_max_wait: crate::config::gateway_queue_max_wait(),
                queue_poll: crate::config::gateway_queue_poll(),
            }),
        }
    }

    /// Build the production framework from env config
    /// (`crate::config::gateway_allowlist_json` +
    /// `crate::config::gateway_rate_limit_burst`/`gateway_rate_limit_refill_per_sec`)
    /// — what `terminus_primary`'s `main()` calls. When the shared Redis is
    /// configured, the Redis limiter AND the FIFO admission queue are built from
    /// the SAME pool; see [`Self::rate_limiter_from_env`] for the selection rule.
    pub fn from_env() -> Self {
        let allowlist = AllowlistPolicy::from_env();
        // Build both proxy consumers (limiter + queue) from ONE shared backend.
        if let Some(backend) = crate::redis::RedisBackend::from_env() {
            let limiter = Arc::new(crate::ratelimit::RedisRateLimiter::from_env(backend.clone()));
            let queue = Arc::new(crate::ratelimit::RequestQueue::new(backend, "proxy"));
            return Self::with_queue(allowlist, limiter, Some(queue));
        }
        // Redis not constructed → no queue; pick the limiter by config-presence.
        Self::with_queue(allowlist, Self::rate_limiter_from_env(), None)
    }

    /// Select the proxy rate-limiter backend (BLD-20). Every request already
    /// passes through `self.inner.rate_limiter.check(..)` in [`guard`] — this
    /// only chooses WHICH limiter backs that check:
    ///
    /// - When the shared Redis is configured (`REDIS_URL`, materialized from the
    ///   vault), use the durable, cross-instance, atomic-Lua
    ///   [`crate::ratelimit::RedisRateLimiter`]: limits then hold across a
    ///   gateway restart and across multiple gateway instances, and an
    ///   unreachable Redis **fails CLOSED** (the limiter returns `Limited` →
    ///   `guard` denies with a 429) so a Redis outage can never become an
    ///   un-throttled flood at the backends (BLD-20 EDGE CASE).
    /// - Otherwise (only when `REDIS_URL` is genuinely ABSENT) fall back to the
    ///   interim in-process token bucket.
    ///
    /// The selection is gated on whether Redis is CONFIGURED (the URL is
    /// present), NOT on whether a live connection can be made — backend
    /// construction is lazy (no connect), so a configured-but-unreachable Redis
    /// still selects the Redis limiter and fails CLOSED at runtime rather than
    /// silently downgrading to in-process at construction. If the URL is present
    /// but unparseable (a hard misconfiguration), we select a fail-closed
    /// sentinel — never a silent downgrade.
    ///
    /// NOTE (scope): this is the PROXY rate-limiter consumer of the BLD-20
    /// Redis, wired here. The other two consumers — sccache shared cache
    /// (BLD-05) and the compiler queue/scheduler state (BLD-06) — are wired by
    /// those items, not BLD-20; the shared client + namespaces they use live in
    /// `crate::redis`.
    fn rate_limiter_from_env() -> Arc<dyn RateLimiter> {
        if crate::redis::resolve_url().is_none() {
            // Redis genuinely not configured → the interim in-process limiter.
            return Arc::new(InProcessRateLimiter::from_env());
        }
        // REDIS_URL is set ⇒ a Redis-backed limiter MUST be selected.
        match crate::redis::RedisBackend::from_env() {
            Some(backend) => Arc::new(crate::ratelimit::RedisRateLimiter::from_env(backend)),
            None => {
                // Configured but the URL would not parse — do NOT downgrade to
                // in-process (that would drop the cross-instance + fail-closed
                // guarantees). Fail CLOSED and surface the misconfiguration.
                tracing::error!(
                    "REDIS_URL is set but unparseable; proxy rate-limiter selecting the \
                     fail-closed sentinel (all requests denied until REDIS_URL is fixed)"
                );
                Arc::new(crate::ratelimit::AlwaysLimited)
            }
        }
    }

    /// BLD-20: attempt bounded FIFO admission for an over-limit request. Returns
    /// `true` if the request was admitted (proceed), `false` if it should be
    /// 429'd. `false` when no queue is configured (immediate shed), or on
    /// `QueueFull`/`TimedOut`/`Unavailable` (Redis down ⇒ fail CLOSED). While
    /// waiting at the head, it re-checks the rate limiter — so it admits exactly
    /// when a token frees, preserving the limit rather than bypassing it.
    async fn try_admit(&self, key: &str) -> bool {
        let Some(queue) = &self.inner.request_queue else {
            return false; // no queuing configured → immediate 429
        };
        // The queue allocates a GLOBALLY-UNIQUE ticket internally (per-instance
        // salt + Redis-atomic INCR) — no caller-side counter, so two gateway
        // instances can never collide on a ticket for the same rate-limit key.
        let limiter = self.inner.rate_limiter.clone();
        let k = key.to_string();
        let acquire = || {
            let limiter = limiter.clone();
            let k = k.clone();
            async move { limiter.check(&k).await == RateLimitDecision::Allowed }
        };
        matches!(
            queue
                .admit(
                    self.inner.queue_max_depth,
                    self.inner.queue_max_wait,
                    self.inner.queue_poll,
                    acquire,
                )
                .await,
            crate::ratelimit::Admission::Admitted
        )
    }

    /// Gate one request. `principal` must come from a server-verified
    /// transport identity only (see this module's doc) — `None` fails
    /// closed unconditionally, before any allowlist/rate-limit check.
    /// [`Principal::name`] is the key used for both the allowlist lookup and
    /// the audit trail.
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
        principal: Option<&Principal>,
        action: &str,
        kind: ActionKind,
    ) -> Result<GatewayContext, Response> {
        let identity_str = match principal {
            Some(p) => p.name().to_string(),
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

        // TMOD-05: authorization is KIND-AWARE. An `Admin` action is checked
        // against `is_allowed_admin` (which requires an EXPLICIT admin-scoped
        // grant -- a bare tool `"*"` wildcard never satisfies it); every
        // other kind uses the ordinary tool/route `is_allowed`. This is what
        // stops a broad tool/inference identity from silently escalating onto
        // the worker-control admin surface.
        let permitted = match kind {
            ActionKind::Admin => self.inner.allowlist.is_allowed_admin(&identity_str, action),
            ActionKind::Tool | ActionKind::Inference => self.inner.allowlist.is_allowed(&identity_str, action),
        };
        if !permitted {
            let detail = if kind == ActionKind::Admin {
                // Name-only: identity + action, never why-not internals.
                format!(
                    "identity '{identity_str}' lacks an explicit admin grant for '{action}' \
                     (a generic tool wildcard does not authorize admin ops)"
                )
            } else if self.inner.allowlist.has_any_entry(&identity_str) {
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
            // BLD-20: over-limit → don't 429 immediately. If a bounded FIFO
            // admission queue is configured, ADMIT the request through it
            // (FIFO fairness + a bounded wait for a token to free); only shed
            // load (429) when the queue is full or the wait times out. Redis
            // unreachable ⇒ fail CLOSED (429), never admit unbounded.
            if !self.try_admit(&key).await {
                let detail = format!("rate limit exceeded for '{identity_str}' on '{action}'");
                AuditEntry::new(&identity_str, action, kind, AuditResult::DeniedRateLimited, Some(&detail))
                    .log();
                return Err(denied_response(StatusCode::TOO_MANY_REQUESTS, &detail));
            }
        }

        Ok(GatewayContext {
            identity: identity_str,
            action: action.to_string(),
            kind,
            upstream: None,
            tool_bare: action.to_string(),
        })
    }

    /// MESH-08: filter a merged `tools/list` catalog down to exactly what
    /// `principal` may call — visibility/enforcement parity with
    /// [`Self::guard`]'s `tools/call` gate, both ultimately backed by the
    /// same [`AllowlistPolicy::is_allowed`] decision per tool name.
    ///
    /// `principal: None` (no server-verified transport identity — the exact
    /// condition [`Self::guard`] fails closed on) returns an EMPTY catalog,
    /// never the unfiltered input — a caller with no identity at all must
    /// never be shown tools it could not subsequently call, mirroring
    /// `guard`'s own fail-closed rule for the missing-identity case.
    pub fn filter_catalog_for_principal(&self, principal: Option<&Principal>, tools: Vec<Value>) -> Vec<Value> {
        match principal {
            Some(p) => self.inner.allowlist.filter_tools(p.name(), tools),
            None => Vec::new(),
        }
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

    fn identity(s: &str) -> Principal {
        Principal::new(s, crate::mesh::PrincipalSource::MtlsCert)
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

    // ── TMOD-05: kind-aware admin authz (privilege-escalation fix) ────────

    /// A generic tool wildcard (`"*"`) authorizes every TOOL/INFERENCE action
    /// but NO `ActionKind::Admin` action — a broad tool identity cannot
    /// silently become a worker-control admin.
    #[tokio::test]
    async fn tool_wildcard_does_not_authorize_admin_actions() {
        let fw = framework_with(policy_allowing("broad-tool-id", &["*"]), 10);
        let id = identity("broad-tool-id");

        // Same identity, same "*" grant: tool call allowed, admin denied.
        assert!(
            fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok(),
            "the tool wildcard must still allow ordinary tool calls (no regression)"
        );
        let admin = fw.guard(Some(&id), "admin:register_worker", ActionKind::Admin).await;
        assert!(admin.is_err(), "a bare tool wildcard must NOT authorize an admin op");
        assert_eq!(admin.unwrap_err().status(), StatusCode::FORBIDDEN);
    }

    /// An explicit admin-scoped grant (`"admin:*"`) authorizes admin actions.
    #[tokio::test]
    async fn explicit_admin_wildcard_authorizes_admin_actions() {
        let fw = framework_with(policy_allowing("worker-admin", &["admin:*"]), 10);
        let id = identity("worker-admin");
        assert!(fw.guard(Some(&id), "admin:register_worker", ActionKind::Admin).await.is_ok());
        assert!(fw.guard(Some(&id), "admin:deregister_worker", ActionKind::Admin).await.is_ok());
    }

    /// An exact admin entry authorizes exactly that admin op and no other.
    #[tokio::test]
    async fn exact_admin_entry_is_scoped_to_that_op() {
        let fw = framework_with(policy_allowing("scoped-admin", &["admin:list_workers"]), 10);
        let id = identity("scoped-admin");
        assert!(fw.guard(Some(&id), "admin:list_workers", ActionKind::Admin).await.is_ok());
        // A different admin op is NOT granted by the exact single-op entry.
        assert!(fw.guard(Some(&id), "admin:register_worker", ActionKind::Admin).await.is_err());
    }

    /// The new admin rule does not touch ordinary tool authorization: an
    /// identity with a specific (non-admin) tool grant is unaffected — the
    /// tool it holds is still allowed, and it holds no admin power.
    #[tokio::test]
    async fn non_admin_tool_authorization_is_unaffected_by_the_admin_rule() {
        let fw = framework_with(policy_allowing("dev-box", &["ledger_accounts", "admin:health_worker"]), 10);
        let id = identity("dev-box");
        // Tool call: unchanged, still allowed by the specific entry.
        assert!(fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok());
        // The explicit admin entry it DOES hold works for its op...
        assert!(fw.guard(Some(&id), "admin:health_worker", ActionKind::Admin).await.is_ok());
        // ...but not for an admin op it wasn't granted.
        assert!(fw.guard(Some(&id), "admin:register_worker", ActionKind::Admin).await.is_err());
    }

    /// An `AllowDeny` grant with `allow: ["*"]` (broad tool access) still
    /// grants no admin op — the deny layer isn't even needed; the `"*"` in
    /// `allow` simply doesn't match an admin action under the kind-aware rule.
    #[tokio::test]
    async fn allow_deny_star_grant_still_denies_admin() {
        let mut map = HashMap::new();
        map.insert(
            "scaffolded".to_string(),
            Grant::AllowDeny { allow: vec!["*".to_string()], deny: vec!["github_".to_string()] },
        );
        let fw = framework_with(AllowlistPolicy::new(map), 10);
        let id = identity("scaffolded");
        assert!(fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok());
        assert!(
            fw.guard(Some(&id), "admin:register_worker", ActionKind::Admin).await.is_err(),
            "an allow:[\"*\"] tool grant must not authorize admin either"
        );
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

    // ── MESH-08: per-upstream, per-tool RBAC over namespaced tools ──────

    fn tool_json(name: &str) -> Value {
        json!({"name": name, "description": "d", "inputSchema": {"type": "object"}})
    }

    /// Namespace-wildcard allow entry (`"ct322__*"`) grants every tool under
    /// that one namespace, but a narrower `deny` prefix on the same
    /// namespace still wins -- and `tools/list` visibility (`filter_tools`)
    /// agrees exactly with `tools/call` enforcement (`is_allowed`) for every
    /// tool checked, proving a hidden tool is also uncallable and a visible
    /// one is also callable.
    #[tokio::test]
    async fn namespace_wildcard_allow_with_narrower_deny_prefix_list_and_call_agree() {
        fn ct322_viewer_map() -> HashMap<String, Grant> {
            let mut map = HashMap::new();
            map.insert(
                "ct322-viewer".to_string(),
                Grant::AllowDeny {
                    allow: vec!["ct322__*".to_string()],
                    deny: vec!["ct322__vitals_".to_string()],
                },
            );
            map
        }
        let policy = AllowlistPolicy::new(ct322_viewer_map());
        let fw = framework_with(AllowlistPolicy::new(ct322_viewer_map()), 10);
        let id = identity("ct322-viewer");

        let catalog = vec![
            tool_json("ct322__ledger_add"),
            tool_json("ct322__vitals_get"),
            tool_json("other__ledger_add"),
            tool_json("plain_local_tool"),
        ];
        let visible = policy.filter_tools("ct322-viewer", catalog);
        let visible_names: Vec<&str> =
            visible.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();

        assert!(visible_names.contains(&"ct322__ledger_add"));
        assert!(!visible_names.contains(&"ct322__vitals_get"), "denied prefix must be hidden");
        assert!(!visible_names.contains(&"other__ledger_add"), "other namespace must be hidden");
        assert!(!visible_names.contains(&"plain_local_tool"), "un-granted local tool must be hidden");

        // Enforcement agrees with visibility for every candidate tool.
        for name in ["ct322__ledger_add", "ct322__vitals_get", "other__ledger_add", "plain_local_tool"] {
            let call_ok = fw.guard(Some(&id), name, ActionKind::Tool).await.is_ok();
            let list_ok = visible_names.contains(&name);
            assert_eq!(call_ok, list_ok, "list/call parity violated for '{name}'");
        }
    }

    /// Deny-prefix precedence is preserved for namespaced names even under a
    /// bare `allow: ["*"]` wildcard grant (not just a namespace-scoped
    /// wildcard) -- and the sensitive-deny prefix composes with namespacing:
    /// a bare sensitive name re-exported under ANY `<ns>__` prefix stays
    /// denied by default, exactly like the un-namespaced form.
    #[tokio::test]
    async fn deny_prefix_beats_wildcard_allow_on_namespaced_tool_and_composes_with_sensitive_defaults() {
        let mut map = HashMap::new();
        map.insert(
            "broad-id".to_string(),
            Grant::AllowDeny {
                allow: vec!["*".to_string()],
                deny: DEFAULT_SENSITIVE_DENY_PREFIXES.iter().map(|s| s.to_string()).collect(),
            },
        );
        let policy = AllowlistPolicy::new(map);

        assert!(policy.is_allowed("broad-id", "ct322__ledger_add"));
        assert!(
            !policy.is_allowed("broad-id", "ct322__github_push_repo"),
            "a sensitive bare name under a mesh namespace prefix must stay denied by default"
        );
        assert!(!policy.is_allowed("broad-id", "github_push_repo"), "un-namespaced sensitive name still denied");
    }

    /// An unmapped principal gets an EMPTY filtered catalog (not the
    /// unfiltered input) and every call is denied -- default-deny extends
    /// cleanly to the list-filter path, not just `guard`.
    #[tokio::test]
    async fn unmapped_principal_gets_empty_catalog_and_every_call_denied() {
        let policy = AllowlistPolicy::default();
        let fw = framework_with(AllowlistPolicy::default(), 10);
        let id = identity("totally-unmapped");

        let catalog = vec![tool_json("ct322__ledger_add"), tool_json("plain_local_tool")];
        let visible = policy.filter_tools("totally-unmapped", catalog.clone());
        assert!(visible.is_empty(), "unmapped principal must see an empty catalog");

        for tool in &catalog {
            let name = tool.get("name").and_then(|n| n.as_str()).unwrap();
            assert!(fw.guard(Some(&id), name, ActionKind::Tool).await.is_err());
        }
    }

    /// `GatewayFramework::filter_catalog_for_principal` with `principal:
    /// None` returns an empty catalog too -- mirroring `guard`'s own
    /// fail-closed behavior for a missing identity, never the raw
    /// unfiltered input.
    #[test]
    fn filter_catalog_for_principal_none_is_empty() {
        let fw = framework_with(policy_allowing("dev-box", &["*"]), 10);
        let catalog = vec![tool_json("ledger_accounts")];
        let filtered = fw.filter_catalog_for_principal(None, catalog);
        assert!(filtered.is_empty());
    }

    /// A namespace wildcard grant referencing a namespace with no live
    /// upstream is simply inert (matches nothing that catalog build ever
    /// produces) -- no error, no special-casing needed. Modeled here as: the
    /// grant matches an action string with that prefix if one is ever
    /// presented (pre-authoring for a not-yet-deployed upstream is allowed),
    /// but an empty catalog filters down to empty regardless.
    #[test]
    fn namespace_grant_for_unregistered_upstream_is_inert_not_an_error() {
        let mut map = HashMap::new();
        map.insert(
            "future-viewer".to_string(),
            Grant::List(vec!["notyetdeployed__*".to_string()]),
        );
        let policy = AllowlistPolicy::new(map);
        // No upstream by that namespace exists in this test's catalog at
        // all -- filtering just yields nothing, no panic/error.
        let visible = policy.filter_tools("future-viewer", vec![tool_json("plain_local_tool")]);
        assert!(visible.is_empty());
        // But the grant is still syntactically live: if that upstream is
        // deployed later and starts exporting tools, they'd immediately be
        // visible without any policy change.
        assert!(policy.is_allowed("future-viewer", "notyetdeployed__some_tool"));
    }

    /// Existing single-identity (non-mesh) callers are unaffected: a plain
    /// `Grant::List` grant with no namespaced entries behaves identically to
    /// pre-MESH-08 for both call-gating and list-filtering.
    #[tokio::test]
    async fn plain_grant_additive_no_namespacing_behavior_unchanged() {
        let policy = policy_allowing("dev-box", &["ledger_accounts"]);
        let fw = framework_with(policy_allowing("dev-box", &["ledger_accounts"]), 10);
        let id = identity("dev-box");

        let visible = policy.filter_tools("dev-box", vec![tool_json("ledger_accounts"), tool_json("other_tool")]);
        let names: Vec<&str> = visible.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
        assert_eq!(names, vec!["ledger_accounts"]);

        assert!(fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok());
        assert!(fw.guard(Some(&id), "other_tool", ActionKind::Tool).await.is_err());
    }
}
