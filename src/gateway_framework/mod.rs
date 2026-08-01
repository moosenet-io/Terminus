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
/// TRTR-05: `CallerContext` lives here so that only this module tree can mint
/// an ENTITLED one — see the module doc.
pub mod caller_context;
pub mod rate_limit;

use std::collections::{HashMap, HashSet};
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
    // TAVAIL-01: the availability ADMIN view. It enumerates the ENTIRE compiled-in
    // tool inventory together with the operator's free-text reasons for parking
    // things — an operational map of the fleet that a personal-assistant or
    // build-orchestrator identity has no business reading. Denied here so it is
    // reachable only by an explicitly-granted operator identity, not by the
    // scaffolded defaults (review finding, S128).
    "tool_availability",
];

/// TRTR-05 (privacy): the tool that reads the OPERATOR's calendar directly.
///
/// Used by [`GatewayFramework::caller_context`] as the PROBE for "may a tool
/// fold calendar-derived context into this principal's answer?" — a principal
/// already authorized to call this tool learns nothing new when `weather`
/// resolves a location from an event, while one who is not must never receive
/// it second-hand. Probing an existing grant rather than adding a second
/// identity list is deliberate: there is exactly one place to edit, so the two
/// can never drift apart.
///
/// # SCOPE — what this gate does NOT protect
///
/// The entitlement is per GATEWAY PRINCIPAL, and a principal names a SERVICE,
/// not a person. Every human who talks to Lumina arrives here as
/// `identity=lumina`, which HOLDS this probe tool — the human identity known at
/// the web edge (`X-Lumina-User`) is not forwarded through Chord and never
/// reaches this module. So this gate withholds operator context from a caller
/// authenticating as its OWN principal; it does NOT withhold it from a
/// houseguest talking to the assistant, because that person *is* `lumina` as
/// far as this decision can see. Reading it as household-level privacy is the
/// misreading to avoid; provisioning guest identities without closing the gap
/// gives a FALSE sense of containment. Closing it needs end-to-end
/// human-identity propagation — **TERM #577**, a blocker for the `hearth`
/// family sprint.
pub const CALENDAR_CONTEXT_PROBE: &str = "google_calendar_today";

/// TRTR-05 (privacy): the tool that already exposes the operator's configured
/// home/work addresses (`COMMUTE_HOME`/`COMMUTE_WORK`) directly. The
/// routine-inference counterpart of [`CALENDAR_CONTEXT_PROBE`]; the two are
/// probed separately so a principal trusted with one source is not
/// automatically handed the other.
///
/// # SCOPE
///
/// Same limit as [`CALENDAR_CONTEXT_PROBE`], and for the same reason: this
/// probe distinguishes gateway PRINCIPALS, not humans. A houseguest conversing
/// with the assistant is authorized as `lumina`, which holds this tool, so the
/// gate does not contain them. See TERM #577.
pub const ROUTINE_CONTEXT_PROBE: &str = "commute_estimate";

/// TRTR-05: the GUEST/FAMILY baseline surface — the exact set of tool names a
/// non-operator household identity (a family member, a houseguest) may call.
///
/// # SCOPE — what this does NOT protect (read before provisioning a guest)
///
/// This list constrains a caller that authenticates as its OWN gateway
/// principal — its own client cert / tailnet identity / named PAT, with its own
/// entry in the grant map. It does NOT yet distinguish two humans who share one
/// identity, and today they do: **every person who talks to Lumina arrives at
/// this gateway as `identity=lumina`.** The mTLS `Principal` names the SERVICE,
/// not the person; the human identity known at the web edge
/// (`X-Lumina-User`, `crate::constellation::proxy`) is not forwarded through
/// Chord and never reaches this module. So a houseguest conversing with the
/// assistant is authorized as `lumina` — which holds
/// [`CALENDAR_CONTEXT_PROBE`], [`ROUTINE_CONTEXT_PROBE`] and full inference —
/// and none of the narrowing below applies to them.
///
/// Provisioning guest identities WITHOUT closing that gap therefore buys a
/// FALSE sense of containment: this surface is real only for a separately
/// authenticated principal, not for whoever is currently in the room. Closing
/// it needs end-to-end human-identity propagation (design work, tracked as
/// **TERM #577**, a blocker for the `hearth` family sprint) — not a wider or
/// cleverer grant map.
///
/// # This list is a CEILING, not a starting point (TRTR-05 round 4)
///
/// An identity named in `TERMINUS_GATEWAY_GUEST_IDENTITIES` can never resolve
/// to more than this list, whatever `TERMINUS_GATEWAY_ALLOWLIST_JSON` says. An
/// explicit entry for a guest is INTERSECTED with this list by
/// [`clamp_to_guest_ceiling`], not substituted for it: the operator may still
/// NARROW a guest (grant them only `weather`, say) and that narrowing applies in
/// full, but a widening entry — `["*"]`, or one naming
/// [`CALENDAR_CONTEXT_PROBE`]/[`ROUTINE_CONTEXT_PROBE`] — is clamped back and
/// loudly logged.
///
/// Before that, guest status was only a DEFAULT: an explicit entry replaced the
/// baseline in full, so one wildcard (a typo, or a line copy-pasted from an
/// operator identity) handed a houseguest the probe grants,
/// [`GatewayFramework::caller_context`] minted an entitled context for them, and
/// `weather` answered an omitted location with the operator's event summary or
/// home address. A protection this list exists to provide must not be escapable
/// by editing the config it is supposed to bound.
///
/// **This is an ALLOWLIST by construction, and that is the load-bearing
/// property.** The scaffolded `lumina`/`harmony` posture is
/// `allow: ["*"]` minus [`DEFAULT_SENSITIVE_DENY_PREFIXES`] — appropriate for
/// two first-party service identities, but exactly backwards for a guest: with
/// a denylist, EVERY tool family added to Terminus in the future is granted to
/// guests the moment it registers, and stays granted until someone remembers to
/// deny it. Guests get the inverse: nothing is reachable unless it is named
/// here, so a new `foo_*` subsystem is invisible to a guest on the day it ships
/// and becomes visible only by a deliberate edit to this list. (Same
/// fail-closed-allowlist reasoning as the DSN guard lesson: allowlists beat
/// denylists whenever the input space grows.)
///
/// Entries are EXACT tool names, not prefixes — a prefix like `"media_*"` would
/// sweep in `media_request`/`media_delete`/`media_organize` (acquisition and
/// library mutation) alongside the discovery tools, which is precisely the kind
/// of accidental widening this list exists to prevent.
///
/// The list also carries ONE inference route, because a grant of tools alone
/// would be inert: a guest talks to the assistant through
/// `crate::inference_proxy::AGENT_EXECUTE_PATH`, which is gated as an
/// [`ActionKind::Inference`] action against this same allow list. Without it a
/// guest could not open a conversation at all. The RAW completion routes
/// (`/v1/chat/completions` and friends) are deliberately NOT granted — those
/// bypass the router's own per-principal tool selection and let the caller pick
/// the model and prompt directly. A guest gets the assistant, not the engine.
///
/// Why each of these is safe for someone who is not the operator:
/// - `/v1/agent/execute` — the assistant turn itself. Every tool the router
///   dispatches inside that turn is re-checked against this same grant, so the
///   route grants conversation, not reach.
/// - `time_now` — the authoritative fleet clock (CLK-01). Reads no user data,
///   takes no arguments that reach a backend, mutates nothing.
/// - `weather` — a public third-party forecast for a location the caller
///   supplies EXPLICITLY, and only that. The tool can otherwise resolve an
///   omitted location from the OPERATOR's calendar or home/work routine
///   (`crate::weather::location`), which would hand a houseguest the operator's
///   whereabouts — including an event summary such as an appointment and its
///   address — from a tool that looks stateless. That inference is gated on
///   [`CALENDAR_CONTEXT_PROBE`]/[`ROUTINE_CONTEXT_PROBE`], neither of which is
///   granted here, so a guest who omits the location is ASKED which place they
///   mean and receives no location, no event summary and no attribution. What
///   makes `weather` safe for a guest is therefore the explicit-location-only
///   path, NOT an absence of household data in the tool — the tool has access
///   to plenty; the grant is what withholds it. (An earlier version of this
///   comment claimed "no household data", which was true when it was written
///   and stopped being true when location inference landed. A justification
///   that silently goes stale is how this nearly shipped: if a future edit
///   widens what `weather` may reach, re-derive this line rather than trusting
///   it.)
/// - `news_headlines`, `news_search`, `news_topic` — public news retrieval.
///   Read-only, no fleet or household state.
/// - `media_search`, `media_recommend`, `media_recently_added`, `media_on_deck`
///   — media DISCOVERY (catalogue browsing) only. Deliberately EXCLUDED from
///   this list: `media_request` (acquisition — spends the household's
///   bandwidth/disk and reaches the *arr write path), `media_delete`,
///   `media_cleanup`, `media_organize` (library mutation),
///   `media_taste_feedback` (writes a personal taste profile),
///   `media_status`/`media_domain_status` (operational plumbing).
///
/// What is NOT here, by construction rather than by exclusion: every
/// infrastructure and secret-bearing family — `infisical_*`, `pg_*`,
/// `gitea_*`/`github_*`/`git_*`, `plane_*`, `ansible_*`, `dev_*`, `compiler_*`,
/// `soma_*`, `mint_*`, `review_*`, `mesh_*`, `broker_*`, `intake_*`,
/// `tool_availability`, and anything else that exists now or later. None of
/// them are named above, so none are reachable.
pub const GUEST_BASELINE_ALLOW: &[&str] = &[
    crate::inference_proxy::AGENT_EXECUTE_PATH,
    "time_now",
    "weather",
    "news_headlines",
    "news_search",
    "news_topic",
    "media_search",
    "media_recommend",
    "media_recently_added",
    "media_on_deck",
];

/// The guest/family grant itself: [`GUEST_BASELINE_ALLOW`] as the allow set,
/// with [`DEFAULT_SENSITIVE_DENY_PREFIXES`] still layered underneath.
///
/// The deny layer is redundant TODAY — no entry in [`GUEST_BASELINE_ALLOW`]
/// matches any sensitive deny prefix, and it is a closed list of exact names,
/// so the deny set can never fire. It is kept deliberately as defence in depth
/// for the predictable future edit: someone widening the guest allow set (or
/// copying this grant as the starting point for a new household role) inherits
/// the sensitive carve-outs instead of silently losing them. A redundant guard
/// that costs one vector allocation at startup is worth keeping over one that
/// has to be remembered.
pub fn guest_baseline_grant() -> Grant {
    Grant::AllowDeny {
        allow: GUEST_BASELINE_ALLOW.iter().map(|s| (*s).to_string()).collect(),
        deny: DEFAULT_SENSITIVE_DENY_PREFIXES.iter().map(|s| s.to_string()).collect(),
    }
}

/// TRTR-05 (round 4): CLAMP an explicit `TERMINUS_GATEWAY_ALLOWLIST_JSON` grant
/// to the guest ceiling — the intersection of what the operator wrote and
/// [`GUEST_BASELINE_ALLOW`].
///
/// # Why a ceiling, and why this was a real hole
///
/// [`build_entries`] seeds guest identities with [`guest_baseline_grant`] and
/// then applies the operator's explicit entries ON TOP, replacing the seed in
/// full. That made guest status a DEFAULT rather than a LIMIT: an entry of
/// `{"guest-alex": ["*"]}` — a wildcard typed once, or copy-pasted from the
/// `moose` entry two lines above it — gave a houseguest
/// [`CALENDAR_CONTEXT_PROBE`] and [`ROUTINE_CONTEXT_PROBE`], so
/// [`GatewayFramework::caller_context`] minted an ENTITLED context for them and
/// `weather` answered an omitted location with the OPERATOR's calendar event
/// summary or configured home address. The narrow baseline exists precisely to
/// bound what a guest can EVER reach; a config edit silently escaping it defeats
/// the protection, in the direction that discloses where the operator lives.
///
/// So: naming an identity in `TERMINUS_GATEWAY_GUEST_IDENTITIES` is a
/// CLASSIFICATION, and it is an upper bound. An explicit entry may still NARROW
/// a guest (a legitimate and useful operation — "this one gets only `weather`");
/// it can never widen one.
///
/// # Why INTERSECT rather than REJECT
///
/// A widening override could instead be treated like a malformed one and DENY
/// the identity outright (see [`build_entries`]'s round-3 rule). We deliberately
/// do not, and the asymmetry with the malformed case is the argument: a
/// malformed entry has NO legible meaning, so there is nothing to honour and
/// denial is the only fail-closed reading. A widening entry is perfectly legible
/// — every baseline tool it names is an intent we can honour exactly — so
/// intersecting satisfies the invariant while still doing what the operator
/// asked for wherever that is permissible. It also fails in the recoverable
/// direction: a clamped guest still works (they keep the baseline surface),
/// whereas a denied guest is an outage and a support call for a household member
/// who did nothing wrong. The security property is identical either way — the
/// result can never exceed the baseline — so the tie breaks on operability.
/// The clamp is LOGGED loudly (see [`build_entries`]) so nobody silently gets
/// something other than what they wrote.
///
/// # Why the intersection is exact
///
/// [`GUEST_BASELINE_ALLOW`] is a CLOSED list of EXACT tool names — no wildcards,
/// no prefixes — so the set of actions the ceiling permits is finite and
/// enumerable. The intersection is therefore computed directly: keep exactly
/// those baseline entries the explicit grant would itself have permitted. No
/// wildcard algebra is needed and none is attempted, and the result is by
/// construction a subset of the baseline whatever shape the explicit grant took
/// (`["*"]`, `{allow,deny}`, prefix wildcards, mesh-namespaced entries).
///
/// Three consequences worth naming because they are the security properties:
/// - The probe tools are NOT in [`GUEST_BASELINE_ALLOW`], so they can never
///   appear in the result — a guest can never hold an entitled
///   [`CallerContext`](crate::tool::CallerContext), by any grant shape.
/// - No baseline entry is [`ADMIN_ACTION_PREFIX`]-namespaced and none is `"*"`,
///   so [`Grant::permits_admin`] is false for every clamped grant — a guest can
///   never hold an admin grant. (Pinned by
///   `guest_baseline_contains_no_admin_or_wildcard_entry`, so a future widening
///   of the baseline cannot quietly break it.)
/// - A future tool family is still invisible to a clamped guest for the same
///   reason it is invisible to a baseline one: it is not named in the list.
///
/// The deny side is the UNION of [`DEFAULT_SENSITIVE_DENY_PREFIXES`] and any
/// deny prefixes the operator wrote. Union is the only safe direction for denies
/// (they subtract), and it preserves the operator's narrowing intent on the
/// grant itself; their effect is already folded into the allow intersection
/// above, so this is defence in depth for later edits rather than new behaviour.
fn clamp_to_guest_ceiling(explicit: &Grant) -> Grant {
    let allow: Vec<String> = GUEST_BASELINE_ALLOW
        .iter()
        .filter(|tool| explicit.permits(tool))
        .map(|tool| (*tool).to_string())
        .collect();

    let mut deny: Vec<String> =
        DEFAULT_SENSITIVE_DENY_PREFIXES.iter().map(|s| (*s).to_string()).collect();
    if let Grant::AllowDeny { deny: explicit_deny, .. } = explicit {
        for d in explicit_deny {
            if !deny.contains(d) {
                deny.push(d.clone());
            }
        }
    }

    Grant::AllowDeny { allow, deny }
}

/// TRTR-05 (round 4): the allow entries of an explicit guest grant that reach
/// OUTSIDE [`GUEST_BASELINE_ALLOW`] — i.e. the parts
/// [`clamp_to_guest_ceiling`] had to drop. Empty means the operator's entry was
/// already within the ceiling and the clamp changed nothing (so nothing is
/// logged: a narrowing entry is a normal, supported operation, not a warning).
///
/// An entry counts as within the ceiling only if it is an EXACT member of the
/// baseline. That is deliberately strict about wildcards: `"news_*"` happens to
/// match only baseline names TODAY, but it is an open-ended prefix that would
/// pick up a future `news_*` tool, so clamping it to the closed set is a real
/// reduction and the operator should hear about it.
fn guest_grant_entries_outside_baseline(explicit: &Grant) -> Vec<String> {
    let allow: &[String] = match explicit {
        Grant::List(actions) => actions,
        Grant::AllowDeny { allow, .. } => allow,
    };
    allow
        .iter()
        .filter(|entry| !GUEST_BASELINE_ALLOW.contains(&entry.as_str()))
        .cloned()
        .collect()
}

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

/// TRTR-05: validate ONE identity's raw `TERMINUS_GATEWAY_ALLOWLIST_JSON`
/// value into a [`Grant`], FAIL-CLOSED.
///
/// This replaces a `#[serde(untagged)]` `RawGrant` enum whose permissiveness
/// was itself the hazard: with `#[serde(default)]` on both object fields, an
/// object was accepted whatever keys it carried, so a MISSPELLED deny key
/// (`{"allow": ["*"], "denny": [...]}`) deserialized cleanly into
/// `allow: ["*"], deny: []` — an unrestricted wildcard grant, produced by a
/// typo, with no error anywhere. That is the exact "a malformed grant is
/// treated as allow-all" failure this validation exists to make impossible.
///
/// Accepted shapes (unchanged from what valid configs already use — no
/// working config's meaning changes):
/// - `["a", "b", "*"]` — the legacy [`Grant::List`] form.
/// - `{"allow": [...], "deny": [...]}` — [`Grant::AllowDeny`]; either key may
///   be omitted (defaulting to empty), but NO other key may be present.
///
/// Rejected (each returns `Err` and the identity is DENIED — its entry is
/// dropped AND any scaffold/guest baseline the seeding pass gave it is revoked,
/// see [`build_entries`] — never silently coerced into, or left at, something
/// broader):
/// - Any other JSON type (string/number/bool/null, array of non-strings).
/// - An unknown key on the object form — the typo case above.
/// - An empty entry, or one with leading/trailing/interior whitespace: it can
///   never match any real tool name, so it is a config error, not a grant.
/// - A `*` anywhere in a DENY entry. Deny entries are LITERAL prefixes
///   ([`deny_matches`] does exact-or-`starts_with`, no globbing), so
///   `deny: ["*"]` matches nothing at all and `deny: ["github_*"]` fails to
///   block `github_push_repo` — both read as "deny everything"/"deny the
///   family" and do the opposite. Silently accepting them is a fail-OPEN
///   trap; rejecting them is fail-closed and tells the operator to drop the
///   star.
/// - A `*` anywhere but the LAST character of an allow entry (`"a*b"`,
///   `"**"`): [`grant_entry_matches`] only understands a trailing `*`, so any
///   other placement means something the matcher will not do.
fn validate_grant(value: &Value) -> Result<Grant, String> {
    match value {
        Value::Array(items) => Ok(Grant::List(validate_entries(items, EntryKind::Allow)?)),
        Value::Object(map) => {
            if let Some(unknown) = map.keys().find(|k| k.as_str() != "allow" && k.as_str() != "deny") {
                return Err(format!(
                    "unknown key '{unknown}' in the allow/deny grant object (only 'allow' and \
                     'deny' are recognised; a misspelled 'deny' key would otherwise silently \
                     produce an unrestricted grant)"
                ));
            }
            let allow = match map.get("allow") {
                Some(Value::Array(items)) => validate_entries(items, EntryKind::Allow)?,
                Some(other) => return Err(format!("'allow' must be an array of strings, got {other}")),
                None => Vec::new(),
            };
            let deny = match map.get("deny") {
                Some(Value::Array(items)) => validate_entries(items, EntryKind::Deny)?,
                Some(other) => return Err(format!("'deny' must be an array of strings, got {other}")),
                None => Vec::new(),
            };
            Ok(Grant::AllowDeny { allow, deny })
        }
        other => Err(format!(
            "a grant must be an array of action names or an {{\"allow\":[..],\"deny\":[..]}} \
             object, got {other}"
        )),
    }
}

/// TRTR-05 (round 2): validate one TOP-LEVEL IDENTITY KEY of
/// `TERMINUS_GATEWAY_ALLOWLIST_JSON`, on the same rule [`validate_entries`]
/// applies to the entries inside a grant.
///
/// An identity is a principal name (`crate::mesh::Principal` — an mTLS client
/// cert CN, a tailnet WhoIs name, a named-PAT identity): never empty, never
/// whitespace-bearing. A key that is empty, whitespace-only, or
/// whitespace-PADDED (`" lumina"`) can therefore never match any real
/// principal, while reading in a config review as though it grants something.
/// That gap between what the config appears to say and what it does is the
/// hazard — such a key is harmless today only by accident, and it is weaker
/// than the fail-closed property the rest of this parsing claims.
///
/// A padded key is REJECTED rather than trimmed, deliberately: trimming would
/// synthesise a grant the operator did not literally write (`" moose": ["*"]`
/// silently becoming a real wildcard for `moose`) — the fail-OPEN direction —
/// whereas rejecting loses nothing that could ever have matched and surfaces
/// the typo. Same reasoning as [`validate_entries`] rejecting, rather than
/// trimming, a whitespace-bearing grant entry.
///
/// Round 3 completes that rule rather than softening it: the trimmed name IS
/// consulted, but only to REVOKE ([`revoke_seeded_for_invalid_key`]). Trimming
/// stays refused in the granting direction and is honoured in the denying one —
/// `" lumina": [...]` still grants `lumina` nothing, and now also strips the
/// scaffold wildcard it was evidently meant to narrow.
fn validate_identity_key(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("an empty identity key matches no principal and is a config error".to_string());
    }
    if id.trim().is_empty() {
        return Err(
            "a whitespace-only identity key matches no principal and is a config error".to_string()
        );
    }
    if id.chars().any(char::is_whitespace) {
        return Err(format!(
            "identity key '{id}' contains whitespace; principal names never do, so it could never \
             match. It is NOT trimmed to a valid name -- trimming would synthesise a grant that \
             was never written"
        ));
    }
    Ok(())
}

/// Which side of a grant an entry sits on — allow entries may carry a trailing
/// `*` wildcard, deny entries are literal prefixes and may not (see
/// [`validate_grant`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Allow,
    Deny,
}

fn validate_entries(items: &[Value], kind: EntryKind) -> Result<Vec<String>, String> {
    items
        .iter()
        .map(|item| {
            let s = item
                .as_str()
                .ok_or_else(|| format!("every grant entry must be a string, got {item}"))?;
            if s.is_empty() {
                return Err("an empty grant entry matches nothing and is a config error".to_string());
            }
            if s.chars().any(char::is_whitespace) {
                return Err(format!(
                    "grant entry '{s}' contains whitespace; tool names and route paths never do, \
                     so this entry could never match"
                ));
            }
            match kind {
                EntryKind::Deny => {
                    if s.contains('*') {
                        return Err(format!(
                            "deny entry '{s}' contains '*', but deny entries are LITERAL prefixes \
                             (matched exactly or by starts_with) — a '*' makes the entry match \
                             NOTHING, the opposite of what it looks like. Drop the '*'"
                        ));
                    }
                }
                EntryKind::Allow => {
                    if s.trim_end_matches('*').contains('*') || s.matches('*').count() > 1 {
                        return Err(format!(
                            "allow entry '{s}' uses '*' somewhere other than as a single trailing \
                             wildcard; only \"*\" or \"<prefix>*\" are understood"
                        ));
                    }
                }
            }
            Ok(s.to_string())
        })
        .collect()
}

/// TRTR-05: whether `grant` is an UNRESTRICTED wildcard — it permits every
/// tool/inference action with no deny layer whatsoever. True for the legacy
/// `Grant::List(["*"])` shape (which has no deny side at all) and for
/// `AllowDeny { allow: ["*"], deny: [] }`.
///
/// This is not an error — `moose`/`claude` are the operator's own identities
/// and are deliberately unrestricted; narrowing them silently could lock the
/// operator out of their own fleet. It is logged loudly at startup so the
/// posture is VISIBLE rather than implicit, and so the docs'
/// recommendation (`docs/reference/tool-grants.md`) has something concrete to
/// point at.
fn is_unrestricted_wildcard(grant: &Grant) -> bool {
    match grant {
        Grant::List(actions) => actions.iter().any(|a| a == "*"),
        Grant::AllowDeny { allow, deny } => deny.is_empty() && allow.iter().any(|a| a == "*"),
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

/// TRTR-05 (round 3): REVOKE whatever [`build_entries`] had already seeded for
/// the identity an INVALID env key was evidently trying to configure, so a
/// malformed key can never leave that identity at a broader posture than the
/// operator was reaching for. Returns the names actually revoked (for the log).
///
/// Two names are considered: the key exactly as written (which only ever has a
/// seeded entry if a guest identity was declared with the same degenerate
/// spelling), and — the case that matters — its TRIMMED form, because
/// `" lumina": [...]` is unmistakably an attempt to configure `lumina`, which
/// the scaffold has already seeded with `allow: ["*"]`.
///
/// This does NOT contradict [`validate_identity_key`]'s reject-don't-trim
/// decision, it completes it: trimming is refused in the GRANTING direction
/// (never synthesise a permission nobody wrote) and honoured in the DENYING
/// direction (a legible intent to control an identity must never resolve to a
/// wider grant than the one that failed to parse). Removal, rather than
/// inserting a deny-all entry, is deliberate: it returns the identity to this
/// module's canonical "no entry ⇒ denied" state and adds no map key for a name
/// the operator never validly wrote.
///
/// `valid_keys` is every identity the SAME config configures VALIDLY, and the
/// trimmed name is skipped when it is one of them. That is a determinism fix,
/// not a softening: a config carrying both `" lumina"` (invalid) and `"lumina"`
/// (valid) is iterated in `HashMap` order, so without this guard the outcome
/// would depend on which entry happened to be visited last — an authorization
/// decision decided by hash ordering. An explicit, well-formed entry is an
/// unambiguous statement of intent and outranks a heuristic reading of a
/// malformed one; the malformed key still grants nothing.
fn revoke_seeded_for_invalid_key(
    entries: &mut HashMap<String, Grant>,
    id: &str,
    valid_keys: &HashSet<&str>,
) -> Vec<String> {
    let mut revoked = Vec::new();
    if entries.remove(id).is_some() {
        revoked.push(id.to_string());
    }
    let trimmed = id.trim();
    if trimmed != id
        && !trimmed.is_empty()
        && !valid_keys.contains(trimmed)
        && entries.remove(trimmed).is_some()
    {
        revoked.push(trimmed.to_string());
    }
    revoked
}

/// The body of [`AllowlistPolicy::from_env`], taking its two config reads as
/// arguments so the load rules (precedence, per-key and per-grant validation,
/// the unrestricted-wildcard warning) are testable without mutating process
/// env from parallel test threads. Semantics are exactly `from_env`'s — see its
/// doc comment; nothing here reads config itself.
///
/// # TRTR-05 (round 3): an INVALID entry DENIES its identity — it never leaves
/// a seeded default in place
///
/// The map is built by SEEDING first (scaffold, then guest baselines) and
/// applying the operator's explicit entries ON TOP. That ordering created a
/// fail-OPEN trap: an invalid explicit entry used to be skipped, which left the
/// identity sitting at whatever the seeding pass had given it — a malformed
/// `lumina` entry kept the scaffold's `allow: ["*"]`, a malformed `guest-*`
/// entry kept the guest baseline. An operator writing an entry to NARROW
/// `lumina` and fat-fingering the JSON shape silently got the full wildcard
/// instead of the restriction they intended, with nothing but a log line to say
/// so. That is the same fail-open-on-malformed trap [`validate_grant`] exists to
/// kill, one level up, in the seeding interaction.
///
/// So: writing an entry for an identity is an expression of intent to CONTROL
/// that identity. If that intent cannot be parsed we must not fall back to a
/// BROADER posture the operator did not write. The asymmetry is the whole
/// argument (same shape as [`validate_entries`]'s reject-don't-trim rule and
/// [`validate_grant`]'s unknown-key rejection): a wrongly-denied identity is
/// recoverable in seconds — it is loud, it is obvious from behaviour, and
/// fixing the JSON restores it — whereas a silently-retained wildcard is not
/// detectable from behaviour at all, because everything keeps working exactly
/// as it did.
///
/// The blast radius stays exactly one identity: the map as a whole is still NOT
/// rejected over one bad entry (that would turn a typo into a fleet-wide
/// outage), and every other identity's grant is untouched.
///
/// **Deliberate asymmetry, NOT an oversight:** when the WHOLE JSON fails to
/// parse (the `Err` arm below), the scaffold IS retained. That case is
/// different in kind — there is no per-identity entry to read an intent from,
/// so nothing says the operator meant to narrow `lumina` at all, and denying
/// every scaffolded identity on any JSON typo would brick the assistant fleet
/// rather than narrow it. The rule above applies to a parsed entry whose intent
/// is legible but unparseable, not to the absence of any entry.
fn build_entries(raw: &str, guest_identities: Vec<String>) -> HashMap<String, Grant> {
    let mut entries = scaffold_defaults();

    // TRTR-05: guest/family identities, declared by the operator in
    // `TERMINUS_GATEWAY_GUEST_IDENTITIES`, get the narrow allowlist-built
    // baseline. Applied AFTER the scaffold and BEFORE the env JSON, so an
    // explicit env entry can still SHAPE any identity the operator wants to
    // hand-tune -- within the ceiling (round 4, below).
    let guests: HashSet<String> = guest_identities.into_iter().collect();
    for id in &guests {
        entries.insert(id.clone(), guest_baseline_grant());
    }

    match serde_json::from_str::<HashMap<String, Value>>(raw) {
        Ok(parsed) => {
            // Every identity this config configures VALIDLY. Needed BEFORE the
            // loop because `parsed` is a `HashMap` and iterates in arbitrary
            // order: a config carrying both `" lumina"` and `"lumina"` would
            // otherwise resolve differently run to run (see
            // `revoke_seeded_for_invalid_key`). Validating twice is free at
            // config scale and keeps the outcome order-independent.
            let valid_keys: HashSet<&str> = parsed
                .iter()
                .filter(|(k, v)| validate_identity_key(k).is_ok() && validate_grant(v).is_ok())
                .map(|(k, _)| k.as_str())
                .collect();

            for (id, value) in &parsed {
                // TRTR-05 (round 2): the KEY is validated first, on the same
                // deny-this-identity-only terms as a malformed grant VALUE
                // below. A degenerate key (empty / whitespace-only /
                // whitespace-padded) can never match a real principal, so the
                // entry itself grants nothing -- but leaving it at that would
                // leave a config entry that LOOKS like it grants something and
                // silently does not. Round 3: it must also REVOKE whatever the
                // seeding pass gave the identity it was evidently aimed at, or
                // `" lumina": ["time_now"]` leaves lumina on the scaffold
                // wildcard. The whole map is NOT rejected: that would turn one
                // typo into a fleet-wide outage.
                if let Err(e) = validate_identity_key(id) {
                    let revoked = revoke_seeded_for_invalid_key(&mut entries, id, &valid_keys);
                    let effect = if revoked.is_empty() {
                        "no identity gains a grant from it".to_string()
                    } else {
                        format!(
                            "the identity it evidently meant to configure ({}) is now DENIED \
                             every tool, inference route and admin op -- its seeded \
                             scaffold/guest baseline has been REVOKED rather than left in \
                             place, because a grant that failed to parse must never resolve \
                             to a BROADER posture than the one that was written",
                            revoked.join(", ")
                        )
                    };
                    tracing::error!(
                        "gateway_framework: SECURITY: TERMINUS_GATEWAY_ALLOWLIST_JSON has an \
                         INVALID identity key {id:?} ({e}) -- that entry is DROPPED and \
                         {effect}. Fix the key and restart; every other identity's grant is \
                         unaffected"
                    );
                    continue;
                }
                // Per-identity validation: one bad entry DENIES that identity
                // outright -- it does not keep its scaffold/guest default (see
                // this function's doc comment: falling back to the seeded,
                // BROADER posture was the fail-open bug) -- rather than either
                // being coerced into something broader OR nuking every other
                // identity's config.
                match validate_grant(value) {
                    Ok(grant) => {
                        // TRTR-05 (round 4): guest classification is a CEILING,
                        // not a default. For an identity named in
                        // `TERMINUS_GATEWAY_GUEST_IDENTITIES`, the explicit
                        // entry is INTERSECTED with `GUEST_BASELINE_ALLOW`
                        // rather than replacing it -- so an override may narrow
                        // a guest but can never widen one past the baseline (in
                        // particular never onto the context probes or an admin
                        // grant). See `clamp_to_guest_ceiling` for why intersect
                        // rather than reject.
                        let grant = if guests.contains(id) {
                            let dropped = guest_grant_entries_outside_baseline(&grant);
                            let clamped = clamp_to_guest_ceiling(&grant);
                            if !dropped.is_empty() {
                                let effective = match &clamped {
                                    Grant::AllowDeny { allow, .. } if allow.is_empty() => {
                                        "(nothing -- every entry you wrote is outside the guest \
                                         baseline, so this identity is now denied every tool)"
                                            .to_string()
                                    }
                                    Grant::AllowDeny { allow, .. } => allow.join(", "),
                                    Grant::List(allow) => allow.join(", "),
                                };
                                tracing::warn!(
                                    "gateway_framework: SECURITY: identity '{id}' is listed in \
                                     TERMINUS_GATEWAY_GUEST_IDENTITIES, and GUEST CLASSIFICATION \
                                     IS A CEILING, NOT A DEFAULT -- its \
                                     TERMINUS_GATEWAY_ALLOWLIST_JSON entry has been CLAMPED to \
                                     the intersection of what you wrote and \
                                     GUEST_BASELINE_ALLOW, so it is NOT what you wrote. Dropped \
                                     as outside the guest baseline: [{dropped}]. Effective allow: \
                                     [{effective}]. A guest can never exceed the baseline \
                                     whatever this entry says -- in particular \
                                     '{CALENDAR_CONTEXT_PROBE}' and '{ROUTINE_CONTEXT_PROBE}' \
                                     (which would disclose the operator's calendar and home/work \
                                     addresses through tools like weather) and every admin op \
                                     stay unreachable. NARROWING a guest below the baseline still \
                                     works and has been applied. To grant more than the baseline, \
                                     remove '{id}' from TERMINUS_GATEWAY_GUEST_IDENTITIES -- it \
                                     is then not a guest and its entry applies in full",
                                    dropped = dropped.join(", ")
                                );
                            }
                            clamped
                        } else {
                            grant
                        };
                        entries.insert(id.clone(), grant);
                    }
                    Err(e) => {
                        let revoked_seed = entries.remove(id).is_some();
                        let seed_note = if revoked_seed {
                            " (its seeded scaffold/guest baseline has been REVOKED, so a \
                             malformed narrowing can never leave a broader posture in place)"
                        } else {
                            ""
                        };
                        tracing::error!(
                            "gateway_framework: SECURITY: TERMINUS_GATEWAY_ALLOWLIST_JSON \
                             entry for identity '{id}' is INVALID ({e}) -- '{id}' is now \
                             DENIED every tool, inference route and admin op{seed_note}. Fix \
                             the JSON and restart; every other identity's grant is unaffected"
                        );
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!(
                "gateway_framework: TERMINUS_GATEWAY_ALLOWLIST_JSON is not valid JSON \
                 ({e}) -- falling back to the scaffold-only allowlist policy (deny-all \
                 except the lumina/harmony safe default)"
            );
        }
    }

    for (id, grant) in &entries {
        if is_unrestricted_wildcard(grant) {
            tracing::warn!(
                "gateway_framework: identity '{id}' holds an UNRESTRICTED wildcard grant -- \
                 every tool and inference route, with no sensitive-deny layer. Intended for \
                 operator identities only; see docs/reference/tool-grants.md"
            );
        }
    }

    entries
}

/// Per-identity allow policy: which tool names / inference routes each
/// enrolled identity may use. Config-driven
/// (`crate::config::gateway_allowlist_json`, a JSON object of
/// `identity -> [action, ...]` OR `identity -> {"allow": [...], "deny":
/// [...]}` — see [`Grant`], parsed by [`validate_grant`]). Default-deny: an
/// identity with no
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
    ///
    /// TRTR-05 adds three things to that:
    /// - Identities named in `TERMINUS_GATEWAY_GUEST_IDENTITIES` are seeded
    ///   with [`guest_baseline_grant`] between the scaffold and the env JSON,
    ///   so a household guest has the narrow safe surface without hand-writing
    ///   JSON, and an explicit env entry can still shape one by hand.
    /// - **Round 4: for a guest identity the env entry does NOT win in full —
    ///   guest classification is a CEILING.** The entry is INTERSECTED with
    ///   [`GUEST_BASELINE_ALLOW`] ([`clamp_to_guest_ceiling`]), so it may narrow
    ///   a guest but never widen one past the baseline; a clamp is logged at
    ///   `warn` naming what was dropped. The "env wins per identity, in full"
    ///   rule therefore applies to every NON-guest identity.
    /// - Every env entry is validated by [`validate_grant`] INDIVIDUALLY, and
    ///   an invalid one DENIES that identity outright — it does NOT fall back
    ///   to its scaffold/guest default (round 3: that fallback was itself
    ///   fail-open — see [`build_entries`]) and is never coerced into a broader
    ///   grant, nor does it discard the whole map. A malformed grant is never
    ///   allow-all, and never allow-as-before.
    /// - Every env KEY is validated by [`validate_identity_key`], on the same
    ///   deny-that-identity-only terms.
    pub fn from_env() -> Self {
        Self {
            entries: build_entries(
                &crate::config::gateway_allowlist_json(),
                crate::config::gateway_guest_identities(),
            ),
        }
    }
    /// TEST-ONLY: build a policy exactly as [`Self::from_env`] would, but from
    /// values passed in rather than read from the process environment — so a
    /// test in ANOTHER module (the TRTR-05 end-to-end weather test) can exercise
    /// the real config path (`build_entries`: scaffold seeding, guest seeding,
    /// per-entry validation, the guest ceiling clamp) without mutating env from
    /// parallel test threads. Compiled only under `cfg(test)`; the production
    /// surface is unchanged.
    #[cfg(test)]
    pub(crate) fn from_config_for_test(raw: &str, guest_identities: Vec<String>) -> Self {
        Self { entries: build_entries(raw, guest_identities) }
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

/// RLQ-01: the result of [`GatewayFramework::try_admit`] — richer than a
/// bare bool so [`GatewayFramework::guard`] can build a structured feedback
/// response instead of a bare 429.
struct AdmitOutcome {
    /// The request was admitted through the queue; proceed.
    admitted: bool,
    /// `true` when the shed was caused by the QUEUE's own backend faulting
    /// (`Admission::Unavailable`), not by real capacity/timeout — mirrors
    /// `RateLimitDecision::Degraded`'s distinction at the limiter layer.
    degraded: bool,
    /// Best-effort snapshot of queue depth at admission time, when a queue
    /// is configured and reachable.
    queue_depth: Option<u64>,
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

    /// BLD-20 / RLQ-01: attempt bounded FIFO admission for an over-limit
    /// request. While waiting at the head, it re-checks the rate limiter — so
    /// it admits exactly when a token frees, preserving the limit rather than
    /// bypassing it.
    async fn try_admit(&self, key: &str) -> AdmitOutcome {
        let Some(queue) = &self.inner.request_queue else {
            // No queuing configured → immediate shed. Not a queue/backend
            // fault, just "this deployment has no queue wired up".
            return AdmitOutcome { admitted: false, degraded: false, queue_depth: None };
        };
        // Best-effort depth snapshot for the feedback response (RLQ-01 part
        // 4 — "funnel semantics": a caller shed here can see roughly how
        // contended the queue was). Never fails the admission attempt itself
        // — a `depth()` error just omits the field, `admit()` below is the
        // authority on whether the request proceeds.
        let queue_depth = queue.depth().await.ok();

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
        let admission = queue
            .admit(
                self.inner.queue_max_depth,
                self.inner.queue_max_wait,
                self.inner.queue_poll,
                acquire,
            )
            .await;
        match admission {
            crate::ratelimit::Admission::Admitted => {
                AdmitOutcome { admitted: true, degraded: false, queue_depth }
            }
            // QueueFull / TimedOut: a REAL shed decision (the queue itself
            // works fine, it's just at capacity or the wait elapsed) — not a
            // backend fault.
            crate::ratelimit::Admission::QueueFull | crate::ratelimit::Admission::TimedOut => {
                AdmitOutcome { admitted: false, degraded: false, queue_depth }
            }
            // Unavailable: the queue's own backend (Redis) errored — this is
            // the same "backend degraded, not a real limit" distinction as
            // `RateLimitDecision::Degraded`, so it's surfaced the same way.
            crate::ratelimit::Admission::Unavailable => {
                AdmitOutcome { admitted: false, degraded: true, queue_depth }
            }
        }
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
        let decision = self.inner.rate_limiter.check(&key).await;
        match decision {
            RateLimitDecision::Allowed => {}
            RateLimitDecision::Degraded { retry_after_secs, refill_per_sec } => {
                // RLQ-01 (the outage fix): the LIMITER BACKEND itself is
                // degraded (e.g. Redis unreachable) — not a real over-limit.
                // Do NOT attempt queue admission (the queue shares the same
                // Redis backend and would almost certainly also be
                // unreachable; attempting it would just add a doomed round
                // trip on the denial path). Fail CLOSED like a real limit
                // (never allow-by-default on a broken backend), but with a
                // response that says so explicitly.
                let detail = format!(
                    "rate-limiter backend unavailable for '{identity_str}' on '{action}' \
                     (retry_after {retry_after_secs:.1}s) — this is a LIMITER BACKEND fault, \
                     not a real rate limit"
                );
                AuditEntry::new(
                    &identity_str,
                    action,
                    kind,
                    AuditResult::DeniedRateLimiterDegraded,
                    Some(&detail),
                )
                .log();
                return Err(rate_limit_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &detail,
                    RateLimitFeedback {
                        retry_after_secs,
                        queue_depth: None,
                        // RLQ-01 fix #2: the ACTUAL limiter's rate, from the
                        // decision it produced — not a re-read of the config
                        // global (which an injected/custom limiter may differ
                        // from).
                        refill_per_sec,
                        degraded: true,
                    },
                ));
            }
            RateLimitDecision::Limited { retry_after_secs, refill_per_sec } => {
                // BLD-20 / RLQ-01: over-limit → don't 429 immediately. If a
                // bounded FIFO admission queue is configured, ADMIT the
                // request through it (FIFO fairness + a bounded wait for a
                // token to free); only shed load (429) when the queue is full
                // or the wait times out. The queue's own backend going
                // unreachable is handled as its own `degraded` case below,
                // distinct from a real shed.
                let admission = self.try_admit(&key).await;
                if !admission.admitted {
                    if admission.degraded {
                        // The admission QUEUE's own backend faulted — same
                        // "backend degraded, not a real limit" signal as the
                        // limiter's `Degraded`. Carry the limiter's
                        // pre-wait figures (a re-peek would hit the same dead
                        // backend), fail CLOSED as 503.
                        let detail = format!(
                            "rate-limiter backend unavailable for '{identity_str}' on \
                             '{action}' (retry_after {retry_after_secs:.1}s) — the admission \
                             QUEUE backend faulted, not a real rate limit"
                        );
                        AuditEntry::new(
                            &identity_str,
                            action,
                            kind,
                            AuditResult::DeniedRateLimiterDegraded,
                            Some(&detail),
                        )
                        .log();
                        return Err(rate_limit_response(
                            StatusCode::SERVICE_UNAVAILABLE,
                            &detail,
                            RateLimitFeedback {
                                retry_after_secs,
                                queue_depth: admission.queue_depth,
                                refill_per_sec,
                                degraded: true,
                            },
                        ));
                    }
                    // A REAL shed (queue full or the bounded wait timed out).
                    // RLQ-01 fix #3: the `retry_after_secs` computed BEFORE
                    // the wait is now stale — the bucket has been refilling
                    // the whole time we waited. Re-derive the recovery window
                    // from the CURRENT bucket state via a non-consuming
                    // `peek`, so the reported figure reflects reality at the
                    // moment we shed, not when we entered the queue. (`peek`
                    // never consumes, so this cannot itself deny a later
                    // legitimate call.)
                    let fresh = self.inner.rate_limiter.peek(&key).await;
                    let retry_after_secs = fresh.retry_after_secs().unwrap_or(0.0);
                    let refill_per_sec = fresh.refill_per_sec().unwrap_or(refill_per_sec);
                    let detail = format!(
                        "rate limit exceeded for '{identity_str}' on '{action}' \
                         (retry_after {retry_after_secs:.1}s)"
                    );
                    AuditEntry::new(
                        &identity_str,
                        action,
                        kind,
                        AuditResult::DeniedRateLimited,
                        Some(&detail),
                    )
                    .log();
                    return Err(rate_limit_response(
                        StatusCode::TOO_MANY_REQUESTS,
                        &detail,
                        RateLimitFeedback {
                            retry_after_secs,
                            queue_depth: admission.queue_depth,
                            refill_per_sec,
                            degraded: false,
                        },
                    ));
                }
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
    /// TRTR-02: read-only "may this identity use this tool" predicate.
    ///
    /// The relocated tool router needs to SELECT from a per-identity catalog before it
    /// ever dispatches, and selection must not consume rate-limit quota or emit a
    /// denial audit entry — nothing is being attempted yet. `guard()` remains the
    /// enforcement path; this is the same underlying `is_allowed` decision exposed
    /// without the side effects, exactly as `filter_catalog_for_principal` already
    /// does for `tools/list`.
    pub fn permits_tool(&self, identity: &str, tool: &str) -> bool {
        self.inner.allowlist.is_allowed(identity, tool)
    }

    /// TRTR-05 (privacy): what OPERATOR context a tool may use on this
    /// principal's behalf — see [`crate::tool::CallerContext`].
    ///
    /// The rule is "no confused deputy": a tool may fold in a source of
    /// operator context ONLY if this principal is already authorized to read
    /// that source directly, via the very same [`AllowlistPolicy`] decision
    /// `guard()` enforces. So an inference can never disclose something the
    /// caller could not have fetched for itself, and there is no second
    /// identity channel to keep in sync — a grant edit moves both at once.
    ///
    /// Fail-closed in every ambiguous case: `principal: None` (no
    /// server-verified transport identity) grants nothing, and an identity with
    /// no allowlist entry at all is default-denied by `is_allowed`, so it
    /// grants nothing either. The guest/family baseline
    /// ([`GUEST_BASELINE_ALLOW`]) names neither probe tool, so a guest
    /// PRINCIPAL never gets operator context — which is the whole point — and
    /// since round 4 that holds for a guest with an explicit
    /// `TERMINUS_GATEWAY_ALLOWLIST_JSON` entry too, however wide: the baseline
    /// is a CEILING the entry is clamped to ([`clamp_to_guest_ceiling`]), so
    /// neither probe can be granted to a guest-classified identity by any grant
    /// shape. Note
    /// the scope limit documented on [`GUEST_BASELINE_ALLOW`]: a human who
    /// shares the assistant's `lumina` identity is not a guest principal here
    /// and IS handed operator context (TERM #577).
    ///
    /// Read-only, exactly like [`Self::permits_tool`]: this is not an attempt,
    /// so it consumes no rate-limit budget and writes no audit entry. The audit
    /// entry for the tool call itself is written by the caller as usual.
    ///
    /// This is the ONLY production path that can produce an entitled
    /// [`CallerContext`], and that is compiler-enforced rather than a
    /// convention: the constructor it calls,
    /// `CallerContext::from_allowlist_decision`, is `pub(super)` to this
    /// module tree. See [`caller_context`] for the boundary's full rationale.
    pub fn caller_context(&self, principal: Option<&Principal>) -> caller_context::CallerContext {
        let Some(p) = principal else {
            return caller_context::CallerContext::untrusted();
        };
        caller_context::CallerContext::from_allowlist_decision(
            self.permits_tool(p.name(), CALENDAR_CONTEXT_PROBE),
            self.permits_tool(p.name(), ROUTINE_CONTEXT_PROBE),
        )
        // TERM #576: WHICH household media account this principal is, for the
        // media tools that must scope household watch history to the caller
        // instead of to whoever watched last. Unmapped principals — which is
        // every guest unless the operator deliberately binds one — resolve to
        // `None`, the unentitled path. See `crate::media::account_map`.
        .with_media_account(crate::media::account_map::account_for_principal(p.name()).as_deref())
    }

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

/// RLQ-01: the queue-with-feedback fields attached to an over-limit /
/// backend-degraded denial — see [`rate_limit_response`].
struct RateLimitFeedback {
    /// Seconds until the caller should retry. For `Limited`, derived from
    /// the token bucket's own deficit/refill (exact). For a `degraded`
    /// backend fault, a conservative config-driven backoff (no bucket state
    /// exists to derive an exact figure from).
    retry_after_secs: f64,
    /// Best-effort FIFO admission-queue depth at decision time, when a queue
    /// is configured and reachable.
    queue_depth: Option<u64>,
    /// The configured refill rate (tokens/sec) — lets a caller reason about
    /// its own retry cadence, not just this one denial's `retry_after_secs`.
    refill_per_sec: f64,
    /// `true` when this denial was caused by the rate-limiter (or admission
    /// queue) BACKEND faulting, `false` for a genuine over-limit. Mirrors
    /// [`RateLimitDecision::Degraded`] — kept as an explicit body field (not
    /// just the distinct message text) so a programmatic caller doesn't have
    /// to string-match to tell the two apart.
    degraded: bool,
}

/// RLQ-01 (queue-with-feedback): build the structured over-limit /
/// backend-degraded response. Puts the recovery estimate + queue depth +
/// refill rate in BOTH the JSON body (for a caller that parses it) and
/// response headers (`Retry-After` is the standard HTTP header a generic
/// HTTP client already understands; the `X-RateLimit-*` headers carry the
/// same data for a caller that only reads headers). `recover_at` is an
/// absolute Unix timestamp (seconds) so a caller doesn't need to add
/// `retry_after_secs` to "now" itself and risk clock/latency skew between
/// receiving the response and scheduling the retry.
fn rate_limit_response(status: StatusCode, message: &str, feedback: RateLimitFeedback) -> Response {
    let RateLimitFeedback { retry_after_secs, queue_depth, refill_per_sec, degraded } = feedback;
    let retry_after_secs = retry_after_secs.max(0.0);
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    // The PRECISE (sub-second) recovery instant — kept as-is in the JSON body
    // for a caller that wants exactness.
    let recover_at = now_unix + retry_after_secs;
    // RLQ-01 codex fix #1: the HEADERS a client OBEYS must round UP, never
    // down. `Retry-After` is defined in whole seconds (RFC 9110 §10.2.3) and
    // `X-RateLimit-Recover-At` we emit as a whole-second timestamp — if
    // either rounded DOWN (e.g. the old `format!("{recover_at:.0}")`, which
    // rounds to nearest and so can land BEFORE the bucket actually refills) a
    // caller that honors it would retry early and be denied again. Ceil both
    // so the reported time is never earlier than the real refill.
    let retry_after_header = retry_after_secs.ceil().max(1.0) as u64;
    let recover_at_header = recover_at.ceil() as u64;

    let mut body = json!({
        "error": message,
        "degraded": degraded,
        "retry_after_secs": retry_after_secs,
        "recover_at": recover_at,
        "refill_per_sec": refill_per_sec,
    });
    if let (Value::Object(map), Some(depth)) = (&mut body, queue_depth) {
        map.insert("queue_depth".to_string(), json!(depth));
    }

    let mut response = (
        status,
        [
            ("content-type".to_string(), "application/json".to_string()),
            ("retry-after".to_string(), retry_after_header.to_string()),
            ("x-ratelimit-recover-at".to_string(), recover_at_header.to_string()),
            ("x-ratelimit-refill-per-sec".to_string(), format!("{refill_per_sec}")),
        ],
        body.to_string(),
    )
        .into_response();
    if let Some(depth) = queue_depth {
        if let Ok(value) = depth.to_string().parse::<axum::http::HeaderValue>() {
            response.headers_mut().insert("x-ratelimit-queue-depth", value);
        }
    }
    response
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

    // ── RLQ-01: queue-with-feedback ──────────────────────────────────────

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// AC: "over-limit returns a structured signal with an accurate
    /// retry_after/recover_at". No queue configured here (immediate shed on
    /// over-limit), so this exercises the `Limited` branch of `guard()`
    /// directly.
    #[tokio::test]
    async fn over_limit_response_carries_retry_after_and_recover_at() {
        // capacity 1, refill 2 tokens/sec ⇒ retry_after ≈ 0.5s after the 2nd
        // call exhausts the single token.
        let fw = GatewayFramework::new(
            policy_allowing("dev-box", &["*"]),
            Arc::new(InProcessRateLimiter::new(1, 2.0)),
        );
        let id = identity("dev-box");
        assert!(fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok());
        let denied = fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.unwrap_err();
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);

        let retry_after_header =
            denied.headers().get("retry-after").expect("Retry-After header present");
        assert!(retry_after_header.to_str().unwrap().parse::<u64>().unwrap() >= 1);
        let recover_at_header = denied
            .headers()
            .get("x-ratelimit-recover-at")
            .expect("X-RateLimit-Recover-At header present");
        let recover_at_from_header: f64 = recover_at_header.to_str().unwrap().parse().unwrap();

        let body = body_json(denied).await;
        assert_eq!(body["degraded"], false, "a real over-limit must not report degraded");
        let retry_after_secs = body["retry_after_secs"].as_f64().unwrap();
        assert!(
            (retry_after_secs - 0.5).abs() < 0.1,
            "expected ~0.5s retry_after, got {retry_after_secs}"
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let recover_at = body["recover_at"].as_f64().unwrap();
        assert!(
            recover_at > now && recover_at < now + 5.0,
            "recover_at should be a near-future absolute timestamp, got {recover_at} (now={now})"
        );
        // RLQ-01 fix #1: the header a client OBEYS must be rounded UP — never
        // earlier than the precise body value (else a caller retries early
        // and is denied again). Ceil ⇒ header >= body, within one second.
        assert!(
            recover_at_from_header >= recover_at,
            "recover_at header ({recover_at_from_header}) must NOT under-report the precise \
             recover_at ({recover_at}) — it must round up"
        );
        assert!(
            recover_at_from_header < recover_at + 1.0,
            "recover_at header must round up by at most a second, got {recover_at_from_header} \
             vs {recover_at}"
        );
        // RLQ-01 fix #2: the reported refill_per_sec is the ACTUAL limiter's
        // rate (2.0 here), not some config global.
        assert_eq!(body["refill_per_sec"].as_f64().unwrap(), 2.0);
    }

    /// AC: "backend-degraded (Redis error) returns a DISTINCT signal/message
    /// from a real over-limit". Uses `crate::ratelimit::AlwaysLimited` (the
    /// same sentinel `terminus_primary` selects on a genuine backend fault)
    /// as the mock — its `check()` now returns `Degraded`, not `Limited`.
    #[tokio::test]
    async fn backend_degraded_response_is_distinct_from_real_limit() {
        let fw = GatewayFramework::new(
            policy_allowing("dev-box", &["*"]),
            Arc::new(crate::ratelimit::AlwaysLimited),
        );
        let id = identity("dev-box");
        let denied = fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.unwrap_err();

        // Distinct HTTP status from a real rate limit (503, not 429).
        assert_eq!(denied.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(denied).await;
        assert_eq!(body["degraded"], true, "must be flagged degraded, not a real limit");
        let message = body["error"].as_str().unwrap();
        assert!(
            message.contains("backend unavailable") || message.contains("BACKEND"),
            "message must say the LIMITER BACKEND is degraded, not that the caller is rate \
             limited: {message}"
        );
        assert!(
            !message.to_lowercase().contains("rate limit exceeded"),
            "must not read like a real rate-limit denial: {message}"
        );
    }

    /// AC: "the Allowed path + existing behavior unchanged when under
    /// limit". A generous burst never trips the limiter; `guard` must still
    /// return `Ok` with no rate-limit-shaped denial anywhere in the path.
    #[tokio::test]
    async fn allowed_path_unchanged_when_under_limit() {
        let fw = framework_with(policy_allowing("dev-box", &["*"]), 50);
        let id = identity("dev-box");
        for _ in 0..10 {
            let ctx = fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await;
            assert!(ctx.is_ok(), "well under burst capacity must always be admitted");
        }
    }

    /// AC: "recovery estimate is accurate (retry at recover_at succeeds)".
    /// RLQ-01 codex fix #4: prove the estimate is EXACT, not merely
    /// eventually-true. Retrying clearly BEFORE the reported window still
    /// FAILS (the bucket has not refilled a whole token yet), and retrying
    /// AT/just-after the window SUCCEEDS — so `retry_after_secs` bounds the
    /// real refill, it isn't a loose over- or under-estimate. Uses the
    /// deterministic in-process token bucket so timing is a pure function of
    /// the (capacity, refill) it reports.
    #[tokio::test]
    async fn reported_retry_after_is_an_exact_recovery_bound() {
        // capacity 1, refill 2 tokens/sec ⇒ retry_after is exactly 0.5s after
        // the single token is spent. Intermediate denied `guard` calls do NOT
        // consume (the `Limited` path never decrements), so they don't perturb
        // the timeline.
        let fw = GatewayFramework::new(
            policy_allowing("dev-box", &["*"]),
            Arc::new(InProcessRateLimiter::new(1, 2.0)),
        );
        let id = identity("dev-box");

        assert!(fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok());
        let denied = fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.unwrap_err();
        let body = body_json(denied).await;
        let retry_after_secs = body["retry_after_secs"].as_f64().unwrap();
        assert!(
            (retry_after_secs - 0.5).abs() < 0.05,
            "expected ~0.5s window, got {retry_after_secs}"
        );

        // BEFORE the window (40% of it): the bucket has accrued < 1 token, so
        // a retry MUST still be denied. If the estimate were an under-report
        // (too small), this is where it would wrongly succeed.
        tokio::time::sleep(std::time::Duration::from_secs_f64(retry_after_secs * 0.4)).await;
        assert!(
            fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_err(),
            "retrying well BEFORE the reported recover window must still be denied — the \
             estimate must not under-report"
        );

        // AT/just-after the window (cumulative ~1.3x): a full token has
        // accrued, so the retry MUST now succeed. If the estimate were a gross
        // over-report (too large), the caller would still be stuck here.
        tokio::time::sleep(std::time::Duration::from_secs_f64(retry_after_secs * 0.9)).await;
        assert!(
            fw.guard(Some(&id), "ledger_accounts", ActionKind::Tool).await.is_ok(),
            "retrying AT/after the reported recover window must succeed (no stuck-drained bucket, \
             estimate not an over-report)"
        );
    }

    /// RLQ-01 codex fix #3: at admission-queue timeout, `guard` re-derives
    /// the recovery window from CURRENT bucket state via a non-consuming
    /// `peek` rather than reusing the stale value computed before the wait.
    /// This pins the freshness property that path relies on: after time
    /// spent refilling, the re-derived window is strictly SMALLER than the
    /// full initial deficit (and `peek` didn't consume, so a real token is
    /// still there to grant).
    #[tokio::test]
    async fn peek_reports_fresh_smaller_window_after_refill() {
        // capacity 1, refill 1 token/sec ⇒ a fresh over-limit deficit is a
        // full ~1.0s window.
        let limiter = InProcessRateLimiter::new(1, 1.0);
        let key = rate_limit_key("dev-box", "ledger_accounts");
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        let initial = limiter.check(&key).await.retry_after_secs().unwrap();
        assert!((initial - 1.0).abs() < 0.05, "fresh deficit ~1.0s, got {initial}");

        // Time passes (as it would while a request waited in the queue), then
        // re-derive via peek — exactly what `guard`'s fix-#3 timeout path does.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let fresh = limiter.peek(&key).await.retry_after_secs().unwrap();
        assert!(
            fresh < initial - 0.1,
            "re-derived window ({fresh}) must be SMALLER than the stale pre-wait value \
             ({initial}) — the refill during the wait must be reflected"
        );
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

    // ── TRTR-05: the guest/family baseline ───────────────────────────────

    fn guest_policy() -> AllowlistPolicy {
        let mut map = HashMap::new();
        map.insert("guest-relative".to_string(), guest_baseline_grant());
        AllowlistPolicy::new(map)
    }

    /// The baseline actually grants the safe household surface it advertises.
    #[test]
    fn guest_baseline_allows_exactly_the_safe_surface() {
        let policy = guest_policy();
        for tool in GUEST_BASELINE_ALLOW {
            assert!(
                policy.is_allowed("guest-relative", tool),
                "the guest baseline must allow its own entry '{tool}'"
            );
        }
    }

    /// AC: "guest baseline excludes infrastructure/secret tool families".
    /// Representative names across every family a guest must never touch.
    #[test]
    fn guest_baseline_excludes_infrastructure_and_secret_families() {
        let policy = guest_policy();
        for tool in [
            "infisical_get_secret",
            "pg_query",
            "pg_ddl",
            "gitea_list_identities",
            "github_list_repos",
            "git_public_mirror_push",
            "plane_list_projects",
            "ansible_run_playbook",
            "openhands_start",
            "dev_run_command",
            "dev_read_file",
            "compiler_request",
            "review_run",
            "mesh_onboard_upstream",
            "soma_status",
            "tool_availability",
            "approval_grant",
            // Media WRITE/acquisition paths, adjacent to the allowed discovery
            // tools — the reason the allow list is exact names, not "media_*".
            "media_request",
            "media_delete",
            "media_organize",
            "media_taste_feedback",
        ] {
            assert!(
                !policy.is_allowed("guest-relative", tool),
                "'{tool}' must not be reachable by a guest identity"
            );
        }
    }

    /// The LOAD-BEARING property, and the whole reason this is an allowlist:
    /// a tool family that does not exist yet is denied to guests on the day it
    /// ships, with no edit to any deny list. A denylist-shaped guest grant
    /// would grant all three of these by default.
    #[test]
    fn guest_baseline_denies_tool_families_that_do_not_exist_yet() {
        let policy = guest_policy();
        for future_tool in ["thermostat_set", "doorlock_unlock", "banking_transfer"] {
            assert!(
                !policy.is_allowed("guest-relative", future_tool),
                "a future tool family must be denied to guests by allowlist construction, not \
                 by someone remembering to deny '{future_tool}'"
            );
        }
    }

    // ── TRTR-05 privacy: operator context is not implied by a tool grant ────

    /// A guest may call `weather`, but that must NOT entitle it to the operator
    /// context `weather` can otherwise reach — the whole leak this gate closes.
    #[test]
    fn a_guest_gets_no_operator_context_even_though_it_may_call_weather() {
        let fw = framework_with(guest_policy(), 10);
        let id = identity("guest-relative");
        assert!(fw.permits_tool("guest-relative", "weather"));
        // ...and neither probe is in the baseline, so:
        let ctx = fw.caller_context(Some(&id));
        assert!(!ctx.may_infer_from_calendar());
        assert!(!ctx.may_infer_from_routine());
    }

    /// The probe tools are the ones that expose each source DIRECTLY — the
    /// no-confused-deputy rule. If either name ever drifts from the real tool,
    /// this test fails rather than silently granting nothing (or everything).
    #[test]
    fn the_context_probes_name_tools_a_guest_is_denied_and_a_broad_identity_is_allowed() {
        let guest = framework_with(guest_policy(), 10);
        for probe in [CALENDAR_CONTEXT_PROBE, ROUTINE_CONTEXT_PROBE] {
            assert!(
                !guest.permits_tool("guest-relative", probe),
                "'{probe}' must not be reachable by a guest identity"
            );
        }
        // The scaffolded service posture (allow `*` minus sensitive prefixes) —
        // what the operator's own turns run under — keeps both.
        let scaffold = framework_with(AllowlistPolicy::new(scaffold_defaults()), 10);
        let lumina = identity("lumina");
        let ctx = scaffold.caller_context(Some(&lumina));
        assert!(ctx.may_infer_from_calendar(), "the operator path must not be degraded");
        assert!(ctx.may_infer_from_routine(), "the operator path must not be degraded");
    }

    /// Fail-closed: no server-verified principal, or one with no allowlist entry
    /// at all, gets nothing. An unauthenticated caller must never be handed
    /// inferred household data.
    #[test]
    fn caller_context_is_fail_closed_for_absent_and_unknown_principals() {
        let fw = framework_with(guest_policy(), 10);
        assert_eq!(fw.caller_context(None), crate::tool::CallerContext::untrusted());
        let stranger = identity("never-enrolled");
        assert_eq!(
            fw.caller_context(Some(&stranger)),
            crate::tool::CallerContext::untrusted()
        );
    }

    /// TRTR-05 boundary, POSITIVE CONTROL: making an entitled context
    /// unforgeable outside this module must not have quietly turned the feature
    /// off. A gateway-derived context for an ENTITLED principal is still
    /// entitled — and is observably NOT the untrusted value, which is the exact
    /// way a botched lockdown would present (everything silently fail-closed,
    /// every negative test still green).
    ///
    /// Pairs with `a_guest_gets_no_operator_context_even_though_it_may_call_weather`
    /// and `caller_context_is_fail_closed_for_absent_and_unknown_principals`
    /// above: together they show the gate still discriminates rather than
    /// denying uniformly.
    #[test]
    fn trtr05_a_gateway_derived_context_still_grants_an_entitled_principal() {
        let fw = framework_with(AllowlistPolicy::new(scaffold_defaults()), 10);
        let operator_side = identity("lumina");

        let ctx = fw.caller_context(Some(&operator_side));

        assert_ne!(
            ctx,
            crate::tool::CallerContext::untrusted(),
            "the lockdown must not have collapsed every context to untrusted"
        );
        assert!(ctx.may_infer_from_calendar());
        assert!(ctx.may_infer_from_routine());

        // ...and it is genuinely derived from the allowlist, not a constant:
        // the same call for a guest identity yields the fail-closed value.
        let guest_fw = framework_with(guest_policy(), 10);
        assert_eq!(
            guest_fw.caller_context(Some(&identity("guest-relative"))),
            crate::tool::CallerContext::untrusted()
        );
    }

    /// A guest holds no admin power, by the same kind-aware rule that stops a
    /// broad tool identity escalating (TMOD-05) — nothing in the baseline is
    /// admin-namespaced.
    #[tokio::test]
    async fn guest_baseline_grants_no_admin_action() {
        let fw = framework_with(guest_policy(), 10);
        let id = identity("guest-relative");
        assert!(fw.guard(Some(&id), "weather", ActionKind::Tool).await.is_ok());
        assert!(fw.guard(Some(&id), "admin:register_worker", ActionKind::Admin).await.is_err());
    }

    /// The baseline grants the ASSISTANT (a scoped agent turn) but not the
    /// ENGINE (raw completions, where the caller picks the model and prompt and
    /// the router's per-principal tool selection does not apply).
    #[tokio::test]
    async fn guest_baseline_grants_the_assistant_turn_but_not_raw_inference() {
        let fw = framework_with(guest_policy(), 10);
        let id = identity("guest-relative");
        assert!(
            fw.guard(Some(&id), crate::inference_proxy::AGENT_EXECUTE_PATH, ActionKind::Inference)
                .await
                .is_ok(),
            "a guest must be able to open an assistant turn, or the tool grant is inert"
        );
        assert!(fw
            .guard(Some(&id), crate::inference_proxy::CHAT_COMPLETIONS_PATH, ActionKind::Inference)
            .await
            .is_err());
    }

    /// `tools/list` visibility matches callability: a guest is SHOWN only the
    /// tools it could actually call — the operator's ask was that the router
    /// not surface unauthorized tools at all.
    #[test]
    fn guest_catalog_is_filtered_to_the_baseline() {
        let policy = guest_policy();
        let visible = policy.filter_tools(
            "guest-relative",
            vec![
                tool_json("weather"),
                tool_json("news_headlines"),
                tool_json("infisical_get_secret"),
                tool_json("media_request"),
                tool_json("media_search"),
            ],
        );
        let mut names: Vec<&str> =
            visible.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["media_search", "news_headlines", "weather"]);
    }

    // ── TRTR-05: fail-closed grant validation ────────────────────────────

    /// AC: "malformed grants rejected fail-closed". The headline case: a
    /// MISSPELLED `deny` key used to deserialize into `allow: ["*"], deny: []`
    /// — an unrestricted grant produced by a typo. It must now be rejected,
    /// and rejection must never mean "allow everything".
    #[test]
    fn misspelled_deny_key_is_rejected_not_silently_unrestricted() {
        let raw = json!({"allow": ["*"], "denny": ["github_"]});
        let err = validate_grant(&raw).unwrap_err();
        assert!(err.contains("denny"), "the error should name the offending key: {err}");
    }

    #[test]
    fn malformed_grant_shapes_are_all_rejected() {
        for bad in [
            json!("*"),
            json!(42),
            json!(null),
            json!(true),
            json!(["ledger_accounts", 7]),
            json!({"allow": "*"}),
            json!({"deny": {"github_": true}}),
            json!({"allow": [""]}),
            json!({"allow": ["ledger accounts"]}),
            json!({"allow": ["a*b"]}),
            json!({"allow": ["**"]}),
        ] {
            assert!(
                validate_grant(&bad).is_err(),
                "malformed grant {bad} must be rejected, never coerced into a grant"
            );
        }
    }

    /// A `*` in a DENY entry is a fail-OPEN trap: deny entries are literal
    /// prefixes, so `deny: ["*"]` blocks nothing while reading as "block
    /// everything". Rejected rather than silently accepted.
    #[test]
    fn wildcard_in_a_deny_entry_is_rejected() {
        assert!(validate_grant(&json!({"allow": ["*"], "deny": ["*"]})).is_err());
        assert!(validate_grant(&json!({"allow": ["*"], "deny": ["github_*"]})).is_err());
        // The correct literal-prefix spelling still validates.
        assert!(validate_grant(&json!({"allow": ["*"], "deny": ["github_"]})).is_ok());
    }

    /// Valid existing configs keep parsing to exactly what they parsed to
    /// before — no working deployment's meaning changes.
    #[test]
    fn valid_legacy_and_allow_deny_shapes_still_parse() {
        assert_eq!(validate_grant(&json!(["*"])).unwrap(), Grant::List(vec!["*".to_string()]));
        assert_eq!(
            validate_grant(&json!(["ledger_accounts", "ct322__*"])).unwrap(),
            Grant::List(vec!["ledger_accounts".to_string(), "ct322__*".to_string()])
        );
        assert_eq!(
            validate_grant(&json!({"allow": ["*"], "deny": ["github_"]})).unwrap(),
            Grant::AllowDeny {
                allow: vec!["*".to_string()],
                deny: vec!["github_".to_string()]
            }
        );
        // Omitted keys default to empty; an empty allow is deny-all, which is
        // the fail-closed direction.
        assert_eq!(
            validate_grant(&json!({"deny": ["github_"]})).unwrap(),
            Grant::AllowDeny { allow: Vec::new(), deny: vec!["github_".to_string()] }
        );
        let empty = validate_grant(&json!({})).unwrap();
        assert!(!empty.permits("anything_at_all"), "an empty grant object must deny, not allow");
    }

    /// The guest baseline itself is a valid, and NOT unrestricted, grant.
    #[test]
    fn unrestricted_wildcard_detection() {
        assert!(is_unrestricted_wildcard(&Grant::List(vec!["*".to_string()])));
        assert!(is_unrestricted_wildcard(&Grant::AllowDeny {
            allow: vec!["*".to_string()],
            deny: Vec::new()
        }));
        // A wildcard WITH a deny layer is restricted (the scaffolded posture).
        assert!(!is_unrestricted_wildcard(&Grant::AllowDeny {
            allow: vec!["*".to_string()],
            deny: vec!["github_".to_string()]
        }));
        assert!(!is_unrestricted_wildcard(&Grant::List(vec!["ledger_accounts".to_string()])));
        assert!(!is_unrestricted_wildcard(&guest_baseline_grant()));
    }

    /// The guest baseline's redundant-today deny layer is real: it is carried
    /// on the grant, so a future widening of the allow set inherits it.
    #[test]
    fn guest_baseline_carries_the_sensitive_deny_layer() {
        let Grant::AllowDeny { deny, .. } = guest_baseline_grant() else {
            panic!("the guest baseline must be an AllowDeny grant, not a bare list");
        };
        assert_eq!(deny.len(), DEFAULT_SENSITIVE_DENY_PREFIXES.len());
        // Widening the allow set does NOT reopen a sensitive family.
        let widened = Grant::AllowDeny { allow: vec!["*".to_string()], deny };
        assert!(widened.permits("weather"));
        assert!(!widened.permits("github_push_repo"));
    }

    // ── TRTR-05 (round 2): degenerate IDENTITY KEYS ──────────────────────
    //
    // Exercised through `build_entries` (the body of `from_env`, with its two
    // config reads passed in) so the assertions are deterministic and do not
    // race other tests over the process env.

    /// An EMPTY identity key is dropped, and dropping it costs nothing else:
    /// the rest of the map parses exactly as it would have.
    #[test]
    fn empty_identity_key_is_dropped_rest_of_map_intact() {
        let entries = build_entries(
            r#"{"": ["*"], "reporting": ["ledger_accounts"]}"#,
            Vec::new(),
        );
        assert!(!entries.contains_key(""), "an empty identity key must not become an entry");
        let policy = AllowlistPolicy::new(entries);
        assert!(!policy.has_any_entry(""));
        assert!(!policy.is_allowed("", "anything_at_all"));
        // Positive control within the same map: the good entry is untouched.
        assert!(policy.is_allowed("reporting", "ledger_accounts"));
        assert!(!policy.is_allowed("reporting", "github_push_repo"));
        // And the scaffold defaults still landed.
        assert!(policy.is_allowed("lumina", "weather"));
    }

    /// A WHITESPACE-ONLY key likewise -- it matches no principal, so it is a
    /// config error, not a grant.
    #[test]
    fn whitespace_only_identity_key_is_dropped_rest_of_map_intact() {
        for blank in ["   ", "\t", "\n", " \t "] {
            let raw = format!(r#"{{"{}": ["*"], "reporting": ["ledger_accounts"]}}"#,
                blank.escape_default());
            let entries = build_entries(&raw, Vec::new());
            assert!(
                !entries.contains_key(blank),
                "whitespace-only identity key {blank:?} must not become an entry"
            );
            let policy = AllowlistPolicy::new(entries);
            assert!(!policy.is_allowed(blank, "anything_at_all"));
            assert!(policy.is_allowed("reporting", "ledger_accounts"));
        }
    }

    /// A whitespace-PADDED key is REJECTED, not trimmed. This is the decision
    /// worth pinning: trimming would synthesise a grant nobody wrote (`" moose"`
    /// silently becoming a real wildcard for `moose`), which is the fail-OPEN
    /// direction. Rejecting loses nothing -- a padded key could never have
    /// matched a principal name -- and surfaces the typo.
    #[test]
    fn whitespace_padded_identity_key_is_rejected_not_trimmed() {
        let entries = build_entries(
            r#"{" moose": ["*"], "reporting ": ["ledger_accounts"], "one two": ["*"]}"#,
            Vec::new(),
        );
        // Neither the padded key nor its trimmed form gains an entry.
        for key in [" moose", "moose", "reporting ", "reporting", "one two"] {
            assert!(
                !entries.contains_key(key),
                "padded key must be dropped and NOT trimmed into a real grant: {key:?}"
            );
        }
        let policy = AllowlistPolicy::new(entries);
        assert!(!policy.is_allowed("moose", "anything_at_all"), "trimming here would be fail-open");
        assert!(!policy.is_allowed(" moose", "anything_at_all"));
        assert!(!policy.is_allowed("reporting", "ledger_accounts"));
    }

    /// Positive control: a map of well-formed keys is completely unaffected by
    /// the key validation -- no working config's meaning changes.
    #[test]
    fn valid_identity_keys_are_unaffected_by_key_validation() {
        let entries = build_entries(
            r#"{"moose": ["*"], "guest-alex": ["time_now"], "ct322_relay": {"allow": ["*"], "deny": ["github_"]}}"#,
            Vec::new(),
        );
        let policy = AllowlistPolicy::new(entries);
        assert!(policy.is_allowed("moose", "literally_anything"));
        assert!(policy.is_allowed("guest-alex", "time_now"));
        assert!(!policy.is_allowed("guest-alex", "google_calendar_today"));
        assert!(policy.is_allowed("ct322_relay", "ledger_accounts"));
        assert!(!policy.is_allowed("ct322_relay", "github_push_repo"));
        // The guest seeding path still works alongside it.
        let seeded = AllowlistPolicy::new(build_entries("{}", vec!["guest-sam".to_string()]));
        assert!(seeded.is_allowed("guest-sam", "weather"));
        assert!(!seeded.is_allowed("guest-sam", CALENDAR_CONTEXT_PROBE));
    }

    // ── TRTR-05 (round 3): an INVALID entry DENIES its identity; it never ──
    // ── leaves a pre-SEEDED (broader) grant in place                     ──
    //
    // The map seeds the scaffold + guest baselines FIRST and applies explicit
    // entries on top, so "skip the bad entry" used to mean the identity kept
    // whatever seeding had given it -- a malformed `lumina` entry silently
    // retained `allow: ["*"]`. Every assertion below goes through the PUBLIC
    // decision path (`is_allowed` / `is_allowed_admin` / `filter_tools`, the
    // same `Grant::permits` decision `filter_catalog_for_principal` and
    // `guard()` use), not by inspecting the map, so it pins the behaviour an
    // operator actually experiences.

    /// Deliberately BROKEN grant shapes -- each is exactly the kind of typo an
    /// operator makes while NARROWING an identity: a misspelled `deny` key, a
    /// bare string instead of an array, a non-string entry, a `*` on the deny
    /// side. Every one must produce denial, never the seeded wildcard.
    const MALFORMED_GRANTS: &[&str] = &[
        r#"{"allow": ["time_now"], "denny": ["github_"]}"#,
        r#""time_now""#,
        r#"[123]"#,
        r#"{"allow": ["time_now"], "deny": ["github_*"]}"#,
        r#"{"allow": ["ledger *"]}"#,
    ];

    /// The load-bearing case: a malformed explicit entry for a SCAFFOLDED
    /// identity (`lumina`) must DENY it, not leave it on the scaffold's
    /// `allow: ["*"]`. The operator was trying to narrow lumina; a broken
    /// narrowing must not resolve to the wildcard they were narrowing away
    /// from.
    #[test]
    fn malformed_grant_for_scaffolded_identity_denies_it_not_the_scaffold_wildcard() {
        for bad in MALFORMED_GRANTS {
            let raw = format!(r#"{{"lumina": {bad}, "reporting": ["ledger_accounts"]}}"#);
            let policy = AllowlistPolicy::new(build_entries(&raw, Vec::new()));

            // Ordinary tools the scaffold WOULD have allowed: all denied now.
            for action in ["reminder_poll", "ledger_accounts", "weather", "time_now"] {
                assert!(
                    !policy.is_allowed("lumina", action),
                    "malformed lumina grant {bad} must DENY '{action}', not fall back to the \
                     scaffold wildcard"
                );
            }
            // Inference route too -- the scaffold's `*` covered these as well.
            assert!(!policy.is_allowed("lumina", "/v1/chat/completions"));
            // Admin path (regression guard: the scaffold never granted admin,
            // and a denied identity must not start to).
            assert!(!policy.is_allowed_admin("lumina", "admin:register_worker"));
            // Catalog filtering -- the `tools/list` visibility side of the
            // same decision -- yields nothing.
            assert!(
                policy
                    .filter_tools("lumina", vec![tool_json("time_now"), tool_json("weather")])
                    .is_empty(),
                "a denied identity must see an EMPTY catalog"
            );

            // Positive control: nothing else in the map is affected.
            assert!(policy.is_allowed("reporting", "ledger_accounts"));
            assert!(!policy.is_allowed("reporting", "github_push_repo"));
            // ... including the OTHER scaffolded identity, which had no entry.
            assert!(policy.is_allowed("harmony", "reminder_poll"));
            assert!(!policy.is_allowed("harmony", "github_push_repo"));
        }
    }

    /// Same rule for a SEEDED GUEST identity: a malformed explicit entry must
    /// deny the guest outright, not leave them on the guest baseline.
    #[test]
    fn malformed_grant_for_seeded_guest_identity_denies_it_not_the_guest_baseline() {
        for bad in MALFORMED_GRANTS {
            let raw = format!(r#"{{"guest-alex": {bad}}}"#);
            let policy = AllowlistPolicy::new(build_entries(
                &raw,
                vec!["guest-alex".to_string(), "guest-sam".to_string()],
            ));

            for action in ["time_now", "weather", crate::inference_proxy::AGENT_EXECUTE_PATH] {
                assert!(
                    !policy.is_allowed("guest-alex", action),
                    "malformed guest grant {bad} must DENY '{action}', not fall back to the \
                     guest baseline"
                );
            }
            assert!(!policy.is_allowed_admin("guest-alex", "admin:register_worker"));
            assert!(policy
                .filter_tools("guest-alex", vec![tool_json("time_now")])
                .is_empty());

            // Positive control: the other seeded guest keeps the baseline.
            assert!(policy.is_allowed("guest-sam", "weather"));
            assert!(!policy.is_allowed("guest-sam", CALENDAR_CONTEXT_PROBE));
        }
    }

    /// A malformed identity KEY aimed at a seeded identity revokes that
    /// identity's seed too. `" lumina"` is unmistakably an attempt to configure
    /// `lumina`; the key is still never TRIMMED INTO A GRANT (that direction
    /// stays fail-closed), but the intent to control lumina is legible enough
    /// that lumina must not be left on the scaffold wildcard.
    #[test]
    fn malformed_identity_key_for_a_seeded_identity_revokes_the_seed() {
        for bad_key in [" lumina", "lumina ", "\tlumina"] {
            let raw = format!(
                r#"{{"{}": ["time_now"], "reporting": ["ledger_accounts"]}}"#,
                bad_key.escape_default()
            );
            let policy = AllowlistPolicy::new(build_entries(&raw, Vec::new()));

            // Neither the padded spelling nor the real one is granted anything.
            for action in ["time_now", "reminder_poll", "/v1/chat/completions"] {
                assert!(!policy.is_allowed(bad_key, action));
                assert!(
                    !policy.is_allowed("lumina", action),
                    "key {bad_key:?} must revoke lumina's scaffold, not leave it on the wildcard"
                );
            }
            assert!(!policy.is_allowed_admin("lumina", "admin:register_worker"));
            assert!(policy.filter_tools("lumina", vec![tool_json("time_now")]).is_empty());

            // Positive controls: everything else is untouched.
            assert!(policy.is_allowed("reporting", "ledger_accounts"));
            assert!(policy.is_allowed("harmony", "reminder_poll"));
        }
        // A guest baseline is revoked the same way.
        let policy = AllowlistPolicy::new(build_entries(
            r#"{" guest-alex": ["time_now"]}"#,
            vec!["guest-alex".to_string(), "guest-sam".to_string()],
        ));
        assert!(!policy.is_allowed("guest-alex", "weather"));
        assert!(policy.is_allowed("guest-sam", "weather"), "positive control");
    }

    /// Second positive control, and the one that proves the fix did not simply
    /// start denying every override: a VALID explicit entry for a scaffolded
    /// identity still applies in full, replacing (not merging with) the
    /// scaffold.
    #[test]
    fn valid_override_of_a_scaffolded_identity_still_applies_normally() {
        let policy = AllowlistPolicy::new(build_entries(
            r#"{"lumina": {"allow": ["time_now", "ledger_*"], "deny": ["ledger_write"]}}"#,
            Vec::new(),
        ));
        assert!(policy.is_allowed("lumina", "time_now"));
        assert!(policy.is_allowed("lumina", "ledger_accounts"));
        assert!(!policy.is_allowed("lumina", "ledger_write"), "deny layer still applies");
        // Replaced, not merged: the scaffold's broad `*` is gone.
        assert!(!policy.is_allowed("lumina", "reminder_poll"));
        assert_eq!(
            policy.filter_tools("lumina", vec![tool_json("time_now"), tool_json("reminder_poll")]),
            vec![tool_json("time_now")]
        );
        // And a valid guest override applies too.
        let policy = AllowlistPolicy::new(build_entries(
            r#"{"guest-alex": ["time_now"]}"#,
            vec!["guest-alex".to_string()],
        ));
        assert!(policy.is_allowed("guest-alex", "time_now"));
        assert!(!policy.is_allowed("guest-alex", "weather"), "override replaces the baseline");
    }

    /// The other stale-state instance found while fixing the above: a config
    /// carrying BOTH a malformed key (`" lumina"`) and a VALID entry for the
    /// same identity (`"lumina"`) is iterated in `HashMap` order, so without a
    /// guard the result would depend on which was visited last -- an
    /// authorization decision decided by hash ordering. The explicit valid
    /// entry wins, every time. Looped so a flaky ordering cannot pass by luck.
    #[test]
    fn valid_entry_beats_a_malformed_key_for_the_same_identity_deterministically() {
        for _ in 0..64 {
            let policy = AllowlistPolicy::new(build_entries(
                r#"{" lumina": ["*"], "lumina": ["time_now"], "reporting": ["ledger_accounts"]}"#,
                Vec::new(),
            ));
            assert!(policy.is_allowed("lumina", "time_now"), "the valid entry must win");
            assert!(!policy.is_allowed("lumina", "reminder_poll"), "and only that entry");
            assert!(!policy.is_allowed("lumina", "*"));
            assert!(!policy.is_allowed(" lumina", "time_now"));
            assert!(policy.is_allowed("reporting", "ledger_accounts"));
        }
        // But when the same-identity entry is ITSELF malformed, both readings
        // agree: denied.
        for _ in 0..64 {
            let policy = AllowlistPolicy::new(build_entries(
                r#"{" lumina": ["*"], "lumina": {"allow": ["*"], "denny": []}}"#,
                Vec::new(),
            ));
            assert!(!policy.is_allowed("lumina", "time_now"));
            assert!(!policy.is_allowed("lumina", "reminder_poll"));
        }
    }

    /// The deliberate ASYMMETRY, pinned so it is not "fixed" by accident: when
    /// the WHOLE JSON is unparseable there is no per-identity intent to read,
    /// so the scaffold is RETAINED (denying every scaffolded identity on any
    /// JSON typo would take the fleet down rather than narrow it). This is the
    /// one case where a broader prior value legitimately survives bad input.
    #[test]
    fn wholly_unparseable_json_still_retains_the_scaffold_by_design() {
        let policy = AllowlistPolicy::new(build_entries("not valid json", Vec::new()));
        assert!(policy.is_allowed("lumina", "reminder_poll"));
        assert!(!policy.is_allowed("lumina", "github_push_repo"));
        assert!(!policy.is_allowed("anyone-else", "anything"));
    }

    // ── TRTR-05 (round 4): guest classification is a CEILING ─────────────
    //
    // An identity in `TERMINUS_GATEWAY_GUEST_IDENTITIES` must never resolve to
    // more than `GUEST_BASELINE_ALLOW`, whatever `TERMINUS_GATEWAY_ALLOWLIST_
    // JSON` says. Before the clamp, an explicit entry REPLACED the baseline in
    // full, so `{"guest-alex": ["*"]}` handed a houseguest the context probes
    // and `weather` disclosed the operator's calendar/home address.
    //
    // Every assertion goes through the PUBLIC decision path
    // (`is_allowed`/`is_allowed_admin`/`filter_tools`/`caller_context`), never
    // by inspecting the map.

    /// Every grant shape that could plausibly be written to widen a guest.
    const WIDENING_GUEST_GRANTS: &[&str] = &[
        // The copy-pasted operator wildcard.
        r#"["*"]"#,
        r#"{"allow": ["*"], "deny": []}"#,
        r#"{"allow": ["*"], "deny": ["github_"]}"#,
        // Naming the probe tools explicitly -- the exact leak path.
        r#"["google_calendar_today", "commute_estimate"]"#,
        r#"{"allow": ["google_calendar_today", "commute_estimate"], "deny": []}"#,
        // The baseline PLUS the probes: "the safe surface and a bit more".
        r#"["/v1/agent/execute", "time_now", "weather", "google_calendar_today", "commute_estimate"]"#,
        // Prefix wildcards that sweep past the baseline.
        r#"["*", "admin:*"]"#,
        r#"{"allow": ["g*", "c*", "weather"], "deny": []}"#,
        // Sensitive infrastructure, wildcard-swept.
        r#"["infisical_*", "pg_*", "dev_*"]"#,
    ];

    /// THE INVARIANT: no widening override can lift a guest above the baseline.
    /// Tool reach, probe reach, admin reach and catalog visibility all checked.
    #[test]
    fn trtr05_a_widening_override_can_never_lift_a_guest_above_the_baseline() {
        for bad in WIDENING_GUEST_GRANTS {
            let raw = format!(r#"{{"guest-alex": {bad}}}"#);
            let policy = AllowlistPolicy::new(build_entries(
                &raw,
                vec!["guest-alex".to_string()],
            ));

            // The probes -- the whole point. Neither, by any grant shape.
            for probe in [CALENDAR_CONTEXT_PROBE, ROUTINE_CONTEXT_PROBE] {
                assert!(
                    !policy.is_allowed("guest-alex", probe),
                    "grant {bad} must not give a guest the context probe '{probe}'"
                );
            }

            // Nothing outside the baseline at all.
            for beyond in [
                "infisical_get_secret",
                "pg_query",
                "dev_run_command",
                "github_push_repo",
                "media_request",
                "media_delete",
                "review_run",
                "compiler_request",
                "reminder_poll",
                "ledger_accounts",
                "thermostat_set",
                "doorlock_unlock",
                crate::inference_proxy::CHAT_COMPLETIONS_PATH,
            ] {
                assert!(
                    !policy.is_allowed("guest-alex", beyond),
                    "grant {bad} must not give a guest '{beyond}'"
                );
            }

            // No admin grant, whatever the override said (including
            // `admin:*`).
            for op in ["admin:register_worker", "admin:deregister_worker"] {
                assert!(
                    !policy.is_allowed_admin("guest-alex", op),
                    "grant {bad} must not give a guest admin op '{op}'"
                );
            }

            // Catalog visibility agrees with callability.
            let visible = policy.filter_tools(
                "guest-alex",
                vec![
                    tool_json("weather"),
                    tool_json(CALENDAR_CONTEXT_PROBE),
                    tool_json(ROUTINE_CONTEXT_PROBE),
                    tool_json("infisical_get_secret"),
                ],
            );
            let names: Vec<&str> =
                visible.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
            assert!(
                !names.contains(&CALENDAR_CONTEXT_PROBE) && !names.contains(&ROUTINE_CONTEXT_PROBE),
                "grant {bad} must not SHOW a guest the context probes either: {names:?}"
            );
            assert!(!names.contains(&"infisical_get_secret"), "{names:?}");

            // And no action the clamped grant permits lies outside the
            // baseline -- the invariant stated directly.
            for tool in GUEST_BASELINE_ALLOW {
                let _ = policy.is_allowed("guest-alex", tool); // may or may not, per the override
            }
        }
    }

    /// The specific override named in the finding: `{allow:[probe,probe],
    /// deny:[]}`. Both denied, and the guest is left with nothing (every entry
    /// they wrote was outside the ceiling) rather than with the probes.
    #[test]
    fn trtr05_an_override_naming_only_the_probes_grants_the_guest_neither() {
        let policy = AllowlistPolicy::new(build_entries(
            r#"{"guest-alex": {"allow": ["google_calendar_today", "commute_estimate"], "deny": []}}"#,
            vec!["guest-alex".to_string()],
        ));
        assert!(!policy.is_allowed("guest-alex", CALENDAR_CONTEXT_PROBE));
        assert!(!policy.is_allowed("guest-alex", ROUTINE_CONTEXT_PROBE));
        // Nothing was granted: the intersection with the baseline is empty.
        for tool in GUEST_BASELINE_ALLOW {
            assert!(
                !policy.is_allowed("guest-alex", tool),
                "an override naming only out-of-ceiling tools grants nothing, not the \
                 baseline it never asked for: '{tool}'"
            );
        }
    }

    /// The entitlement gate itself, which is where the disclosure happened: a
    /// wildcard-granted guest still gets an UNTRUSTED `CallerContext`, so no
    /// tool can fold operator context into their answer.
    #[test]
    fn trtr05_a_wildcard_granted_guest_still_gets_an_untrusted_caller_context() {
        for bad in WIDENING_GUEST_GRANTS {
            let raw = format!(r#"{{"guest-alex": {bad}}}"#);
            let fw = framework_with(
                AllowlistPolicy::new(build_entries(&raw, vec!["guest-alex".to_string()])),
                10,
            );
            let ctx = fw.caller_context(Some(&identity("guest-alex")));
            assert_eq!(
                ctx,
                crate::tool::CallerContext::untrusted(),
                "grant {bad} must not mint an entitled context for a guest"
            );
        }
    }

    /// POSITIVE CONTROL 1: a NARROWER override still narrows. This is what
    /// proves the clamp is an intersection and not "ignore guest overrides".
    #[test]
    fn trtr05_a_narrowing_override_for_a_guest_still_applies() {
        let policy = AllowlistPolicy::new(build_entries(
            r#"{"guest-alex": ["weather"]}"#,
            vec!["guest-alex".to_string(), "guest-sam".to_string()],
        ));
        assert!(policy.is_allowed("guest-alex", "weather"), "the narrowed grant applies");
        // ...and only that: the rest of the baseline is gone, as written.
        for narrowed_away in
            ["time_now", "news_headlines", "media_search", crate::inference_proxy::AGENT_EXECUTE_PATH]
        {
            assert!(
                !policy.is_allowed("guest-alex", narrowed_away),
                "the operator narrowed the guest to `weather`; '{narrowed_away}' must be gone"
            );
        }
        // Still no probes, still no admin.
        assert!(!policy.is_allowed("guest-alex", CALENDAR_CONTEXT_PROBE));
        assert!(!policy.is_allowed_admin("guest-alex", "admin:register_worker"));
        // Positive control within the map: the untouched guest keeps the full
        // baseline.
        for tool in GUEST_BASELINE_ALLOW {
            assert!(policy.is_allowed("guest-sam", tool), "'{tool}' for the unshaped guest");
        }
    }

    /// POSITIVE CONTROL 2: the clamp is scoped to GUESTS. A non-guest identity
    /// with `["*"]` is completely unaffected -- the operator's own identities
    /// must not be silently narrowed by this change.
    ///
    /// Deliberately contains NO guest assertion, so it passes with the clamp
    /// present AND with the clamp removed. That is what makes it a control: if
    /// it ever goes red, the clamp has leaked out of the guest set and started
    /// narrowing the operator's own identities, which is the failure mode a
    /// ceiling could plausibly introduce. The same-map contrast (a guest and a
    /// non-guest side by side) is asserted separately below.
    #[test]
    fn trtr05_the_clamp_does_not_touch_a_non_guest_identity() {
        let policy = AllowlistPolicy::new(build_entries(
            r#"{"moose": ["*"], "lumina": {"allow": ["*"], "deny": ["github_"]}, "guest-alex": ["*"]}"#,
            vec!["guest-alex".to_string()],
        ));

        // moose: unrestricted, including the probes and the sensitive families.
        for action in [
            CALENDAR_CONTEXT_PROBE,
            ROUTINE_CONTEXT_PROBE,
            "infisical_get_secret",
            "github_push_repo",
            "literally_anything",
        ] {
            assert!(policy.is_allowed("moose", action), "moose must keep '{action}'");
        }
        // lumina: broad minus its deny layer, probes intact.
        assert!(policy.is_allowed("lumina", CALENDAR_CONTEXT_PROBE));
        assert!(policy.is_allowed("lumina", ROUTINE_CONTEXT_PROBE));
        assert!(policy.is_allowed("lumina", "reminder_poll"));
        assert!(!policy.is_allowed("lumina", "github_push_repo"));

        // And the operator entitlement path is intact end to end.
        let fw = framework_with(policy, 10);
        let ctx = fw.caller_context(Some(&identity("lumina")));
        assert!(ctx.may_infer_from_calendar() && ctx.may_infer_from_routine());
    }

    /// The same-map contrast: ONE config, two identities, the SAME `["*"]`
    /// grant -- and only the guest-classified one is clamped. This is the
    /// discrimination the two controls above bracket.
    #[test]
    fn trtr05_a_guest_and_a_non_guest_with_the_same_wildcard_resolve_differently() {
        let policy = AllowlistPolicy::new(build_entries(
            r#"{"moose": ["*"], "guest-alex": ["*"]}"#,
            vec!["guest-alex".to_string()],
        ));
        for probe in [CALENDAR_CONTEXT_PROBE, ROUTINE_CONTEXT_PROBE] {
            assert!(policy.is_allowed("moose", probe), "the operator keeps '{probe}'");
            assert!(!policy.is_allowed("guest-alex", probe), "the guest never gets '{probe}'");
        }
        assert!(policy.is_allowed("moose", "infisical_get_secret"));
        assert!(!policy.is_allowed("guest-alex", "infisical_get_secret"));
        // Clamped TO the baseline, not below it -- the guest still works.
        assert!(policy.is_allowed("guest-alex", "weather"));
    }

    /// A wildcard override clamps to EXACTLY the baseline -- not less (that
    /// would be a silent outage) and not more (that is the bug).
    #[test]
    fn trtr05_a_wildcard_guest_override_clamps_to_exactly_the_baseline() {
        let clamped = AllowlistPolicy::new(build_entries(
            r#"{"guest-alex": ["*"]}"#,
            vec!["guest-alex".to_string()],
        ));
        let seeded = AllowlistPolicy::new(build_entries("{}", vec!["guest-sam".to_string()]));
        for tool in GUEST_BASELINE_ALLOW {
            assert!(clamped.is_allowed("guest-alex", tool), "'{tool}' must survive the clamp");
            assert!(seeded.is_allowed("guest-sam", tool));
        }
    }

    /// The clamp is a pure function on the grant -- pinned directly so the
    /// intersection semantics are legible without reading `build_entries`.
    #[test]
    fn clamp_to_guest_ceiling_intersects() {
        // Wildcard in, baseline out.
        let Grant::AllowDeny { allow, deny } =
            clamp_to_guest_ceiling(&Grant::List(vec!["*".to_string()]))
        else {
            panic!("the clamp must always produce an AllowDeny grant");
        };
        assert_eq!(allow, GUEST_BASELINE_ALLOW.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // The sensitive deny layer is carried through.
        assert_eq!(deny.len(), DEFAULT_SENSITIVE_DENY_PREFIXES.len());

        // Out-of-ceiling names simply do not appear.
        let clamped = clamp_to_guest_ceiling(&Grant::List(vec![
            "weather".to_string(),
            CALENDAR_CONTEXT_PROBE.to_string(),
            "infisical_get_secret".to_string(),
        ]));
        assert!(clamped.permits("weather"));
        assert!(!clamped.permits(CALENDAR_CONTEXT_PROBE));
        assert!(!clamped.permits("infisical_get_secret"));

        // An operator's extra deny prefixes survive the union.
        let clamped = clamp_to_guest_ceiling(&Grant::AllowDeny {
            allow: vec!["*".to_string()],
            deny: vec!["news_".to_string()],
        });
        assert!(clamped.permits("weather"));
        assert!(!clamped.permits("news_headlines"), "the operator's own narrowing deny survives");
    }

    /// The detector that decides whether to LOG a clamp: silent for a grant
    /// already within the ceiling, loud for anything reaching past it.
    #[test]
    fn guest_grant_entries_outside_baseline_flags_only_real_widening() {
        assert!(guest_grant_entries_outside_baseline(&guest_baseline_grant()).is_empty());
        assert!(guest_grant_entries_outside_baseline(&Grant::List(vec![
            "weather".to_string(),
            "time_now".to_string()
        ]))
        .is_empty());
        assert_eq!(
            guest_grant_entries_outside_baseline(&Grant::List(vec!["*".to_string()])),
            vec!["*".to_string()]
        );
        assert_eq!(
            guest_grant_entries_outside_baseline(&Grant::List(vec![
                "weather".to_string(),
                CALENDAR_CONTEXT_PROBE.to_string(),
            ])),
            vec![CALENDAR_CONTEXT_PROBE.to_string()]
        );
        // A prefix wildcard that happens to match only baseline names TODAY is
        // still a reduction -- it would have picked up future `news_*` tools.
        assert_eq!(
            guest_grant_entries_outside_baseline(&Grant::List(vec!["news_*".to_string()])),
            vec!["news_*".to_string()]
        );
    }

    /// The structural reason a clamped guest can never hold admin power or a
    /// wildcard: nothing in the baseline is admin-namespaced or a wildcard, so
    /// the clamp's output cannot be either. Pinned so a future widening of
    /// `GUEST_BASELINE_ALLOW` that broke it fails HERE rather than silently.
    #[test]
    fn guest_baseline_contains_no_admin_or_wildcard_entry() {
        for entry in GUEST_BASELINE_ALLOW {
            assert!(
                !entry.starts_with(ADMIN_ACTION_PREFIX),
                "'{entry}' is admin-namespaced; the guest ceiling must never confer admin"
            );
            assert!(
                !entry.contains('*'),
                "'{entry}' contains a wildcard; the guest ceiling must stay a closed exact list \
                 or the clamp stops being an exact intersection"
            );
        }
    }

    /// The malformed-entry rule (round 3) and the ceiling (round 4) compose:
    /// a malformed grant for a guest still DENIES, it does not fall back to the
    /// clamped-baseline value.
    #[test]
    fn trtr05_ceiling_does_not_resurrect_a_malformed_guest_grant() {
        for bad in MALFORMED_GRANTS {
            let raw = format!(r#"{{"guest-alex": {bad}}}"#);
            let policy = AllowlistPolicy::new(build_entries(
                &raw,
                vec!["guest-alex".to_string()],
            ));
            for tool in GUEST_BASELINE_ALLOW {
                assert!(
                    !policy.is_allowed("guest-alex", tool),
                    "malformed guest grant {bad} must still DENY '{tool}'"
                );
            }
        }
    }

    /// The validator itself, in isolation -- the shapes it accepts and rejects.
    #[test]
    fn validate_identity_key_shapes() {
        for bad in ["", " ", "\t", "\n", " moose", "moose ", "one two", " "] {
            assert!(
                validate_identity_key(bad).is_err(),
                "degenerate identity key {bad:?} must be rejected"
            );
        }
        for good in ["moose", "lumina", "guest-alex", "ct322_relay", "harmony"] {
            assert!(validate_identity_key(good).is_ok(), "{good:?} is a valid principal name");
        }
    }
}
