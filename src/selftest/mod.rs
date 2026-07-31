//! `agent_selftest` — the operator's high-leverage "test bot" (TERM egress side).
//!
//! This is the EGRESS/hub half of the Lumina self-test design: a single
//! callable Terminus tool that verifies each of an agent's functions from the
//! hub, **with a per-tool functional sweep as the headline feature** — for
//! EVERY registered tool it runs a SAFE, read-only probe and classifies the
//! outcome tri-state (working / not_configured / needs_args / broken /
//! skipped) so the operator immediately sees which tools actually work.
//!
//! It is modelled on the `dura_*` health tools (`crate::dura`): a `RustTool`
//! that reads all config from env helpers (`crate::config`), returns the
//! `NotConfigured` shape when a backend/env isn't provisioned, and never leaks
//! secrets/tokens/URLs in a detail string (detail strings are COARSE
//! categories only, mirroring dura's genericized error wording).
//!
//! ## What it does NOT do (documented FOLLOW-UP, not this phase)
//! The in-process lumina-core promotion of the existing `selftest_handler` into
//! a reusable `selftest::run(profile)` module, its startup + periodic
//! invocation, degraded-mode boot, and the Matrix alert are a LATER phase in
//! the design (they need in-process observability of tool_gate/router/persona
//! that only lumina-core has). This module is the callable HUB tool that is
//! invocable immediately via the loopback door and gives the operator the
//! per-tool report fastest.
//!
//! ## Safety model (the sweep never mutates) — LAYERED, fail-closed
//! A tool is only ever CALLED when EVERY gate below allows it; each gate can
//! only demote toward skip, never promote to a call (full detail on
//! [`decide_probe_action`], including the accepted residual risk):
//!   1. **Guarded registry (AUTHORITATIVE).** [`crate::approval::is_guarded`] on
//!      the bare name is the fleet's canonical machine-readable classifier for
//!      approval-gated/dangerous tools (the secrets-manager, config-management,
//!      agent-runner and scheduler tool families, plus `pg_ddl`/`pg_admin`/
//!      `pg_execute`/`git_public_mirror_*`). Checked FIRST and
//!      independent of any name heuristic, so a guarded tool whose name contains
//!      a read token (e.g. `infisical_get_secret`, `openhands_get_status`) is
//!      still skipped. This is the guarantee a name allowlist alone cannot give.
//!   2. **Destructive-name deny** ([`is_write_destructive`]) — second veto.
//!   3. **Required-args** — classified `needs_args`, never called.
//!   4. **Read-only allowlist** ([`is_safe_read`]) — the fail-closed default:
//!      only an affirmatively read-named, no-required-args tool reaches `Probe`;
//!      EVERYTHING else is SKIPPED. This is the fail-closed lesson (allowlist >
//!      denylist): a denylist fails OPEN because it can never enumerate every
//!      mutating verb (`ledger_transfer`/`ledger_append`).
//! When in doubt it SKIPS — coverage is always traded away in favour of never
//! mutating/spending/notifying.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::{register_all, ToolInfo, ToolRegistry};
use crate::tool::{RustTool, ToolOutput};

// ---------------------------------------------------------------------------
// Severity / status vocabulary
// ---------------------------------------------------------------------------

/// Severity of a check per the design's capability→check matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A real failure here makes overall `critical`.
    Critical,
    /// A real failure here makes overall (at worst) `degraded`.
    Degraded,
    /// Reported for visibility; never escalates overall on its own.
    Info,
}

/// Structured status of a single check entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    NotConfigured,
    Skip,
}

/// Overall roll-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Overall {
    Healthy,
    Degraded,
    Critical,
}

/// Tri-state (well, five-state) classification of a single tool probe in the
/// per-tool functional sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// A read-only/idempotent probe returned a non-error result.
    Working,
    /// Reachable, but its backend/env isn't provisioned (`NotConfigured`).
    NotConfigured,
    /// The tool works but needs parameters (`InvalidArgument` / required schema).
    NeedsArgs,
    /// A transport/backend error (Http/Execution/Database/etc.) — the tool is
    /// reachable-but-failing, the one bucket that dents overall health.
    Broken,
    /// A write/destructive/guarded tool that must never be probed.
    Skipped,
}

impl ProbeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeStatus::Working => "working",
            ProbeStatus::NotConfigured => "not_configured",
            ProbeStatus::NeedsArgs => "needs_args",
            ProbeStatus::Broken => "broken",
            ProbeStatus::Skipped => "skipped",
        }
    }
}

// ---------------------------------------------------------------------------
// Profile (family-agent-ready — data, not code)
// ---------------------------------------------------------------------------

/// A per-agent self-test profile. The probe engine itself is identity-agnostic;
/// everything agent-specific lives here so a future family agent is a new
/// profile, not new code.
#[derive(Debug, Clone)]
pub struct SelftestProfile {
    pub agent_identity: String,
    /// Named inference proxies to canary via Chord `/v1/chat/completions`.
    pub named_proxies: Vec<NamedProxy>,
    /// Tools whose absence from the live catalog is itself a failure (INFO-level
    /// report here; the in-process phase escalates these).
    pub critical_tools: Vec<String>,
}

/// A named inference proxy and how hard we hold it to account. `lumina-deep`
/// is DEGRADED (lazy GPU cold-load is tolerated) where `lumina`/`lumina-fast`
/// are CRITICAL.
#[derive(Debug, Clone)]
pub struct NamedProxy {
    pub name: String,
    pub severity: Severity,
}

impl SelftestProfile {
    /// The default lumina profile.
    pub fn lumina() -> Self {
        SelftestProfile {
            agent_identity: "lumina".to_string(),
            named_proxies: vec![
                NamedProxy {
                    name: "lumina".to_string(),
                    severity: Severity::Critical,
                },
                NamedProxy {
                    name: "lumina-fast".to_string(),
                    severity: Severity::Critical,
                },
                NamedProxy {
                    name: "lumina-deep".to_string(),
                    severity: Severity::Degraded,
                },
            ],
            // Hub-reachable tools whose absence/breakage from the EGRESS catalog
            // is itself a failure. Scoped to tools actually served by
            // `register_all` — `engram_query` is a lumina-core IN-PROCESS tool
            // (checked by the memory/embeddings canary + the later in-process
            // phase), NOT in the Terminus catalog, so listing it here would
            // false-flag "missing" on every run.
            critical_tools: vec!["time_now".to_string()],
        }
    }

    /// Resolve a profile by agent identity. Unknown identities get a generic
    /// profile keyed to their name (single `{identity}` proxy, no critical
    /// tools) rather than being rejected — a family agent works out of the box
    /// and its profile can be enriched later.
    pub fn for_identity(identity: &str) -> Self {
        match identity {
            "lumina" | "" => Self::lumina(),
            other => SelftestProfile {
                agent_identity: other.to_string(),
                named_proxies: vec![NamedProxy {
                    name: other.to_string(),
                    severity: Severity::Critical,
                }],
                critical_tools: Vec::new(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Result schema
// ---------------------------------------------------------------------------

/// One check entry in the structured report.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub id: String,
    pub capability: String,
    pub severity: Severity,
    pub status: CheckStatus,
    pub latency_ms: u64,
    /// Coarse, non-leaking category (never a raw token/URL/secret).
    pub detail: String,
}

/// One tool's row in the sweep matrix.
#[derive(Debug, Clone, Serialize)]
pub struct ToolProbe {
    pub name: String,
    pub status: String,
    pub latency_ms: u64,
    /// Coarse category only.
    pub detail: String,
}

/// The full structured report returned by `agent_selftest`.
#[derive(Debug, Clone, Serialize)]
pub struct SelftestReport {
    pub agent_identity: String,
    pub ts: String,
    pub overall: Overall,
    pub checks: Vec<CheckResult>,
    /// The per-tool functional sweep matrix (the headline feature).
    pub tool_matrix: Vec<ToolProbe>,
    /// Counts of the sweep by status.
    pub tool_counts: Value,
    /// Rollup of the sweep by tool-name prefix.
    pub by_prefix: Value,
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Pure classification logic (unit-tested)
// ---------------------------------------------------------------------------

/// Static deny-policy: tokens (whole `_`-separated segments) that mark a tool
/// as a mutation/write/destructive/side-effecting operation. Deliberately
/// BROAD — coverage is traded for safety, and the design's rule is "when
/// unsure, SKIP". Extends the task's explicit verb list with a conservative
/// safety margin (rename/approve/trigger/deploy/rotate/…).
const WRITE_VERB_TOKENS: &[&str] = &[
    // From the task's explicit list.
    "create",
    "delete",
    "send",
    "request",
    "organize",
    "cleanup",
    "set",
    "update",
    "add",
    "remove",
    "run",
    "push",
    "merge",
    "grab",
    "download",
    "execute",
    "ddl",
    "admin",
    // Conservative safety extension (still "when unsure, skip").
    "rename",
    "put",
    "post",
    "write",
    "edit",
    "patch",
    "approve",
    "reject",
    "trigger",
    "start",
    "stop",
    "restart",
    "reload",
    "deploy",
    "rotate",
    "provision",
    "enroll",
    "register",
    "unregister",
    "kill",
    "drop",
    "purge",
    "reset",
    "apply",
    "sync",
    "upload",
    "cancel",
    "retrain",
    "rebuild",
    "install",
    "grant",
    "revoke",
    "move",
    "copy",
    "backup",
    "restore",
    "wipe",
    "clear",
    "flush",
    "enable",
    "disable",
    "mute",
    "unmute",
    "onboard",
    "onboarding",
    "build",
    "dispatch",
    "publish",
    "import",
    "export",
    "save",
    "commit",
    "rollback",
    "prune",
    "gc",
    "seed",
    "insert",
    "upsert",
    // Reviewer-named gaps (codex/free) — a denylist can never be complete, so
    // these are belt-and-suspenders only; the AFFIRMATIVE read-only allowlist
    // ([`is_safe_read`]) is the primary, fail-closed gate.
    "transfer",
    "append",
    "submit",
    "schedule",
    "queue",
    "invoke",
    "call",
    "process",
    "handle",
    "transform",
    "modify",
    "change",
    "toggle",
    "pay",
    "spend",
    "buy",
    "sell",
    "order",
    "book",
    "cancel",
    "notify",
    "email",
    "message",
    "reply",
    "comment",
    "assign",
    "close",
    "open",
    "resolve",
    "escalate",
    "promote",
    "demote",
    "ban",
    "unban",
];

/// Name prefixes that mark an entire tool family as unsafe to probe.
///
/// These are TOOL-NAME prefixes, not host names: the values are matched with
/// `starts_with` by [`is_write_destructive`], so they are load-bearing for the
/// fail-closed write-deny gate and cannot be reworded or suffixed without
/// narrowing that gate (e.g. `"pve_"` would stop matching a `pvelist`-shaped
/// tool). Hence the explicit PII-gate exemption on the line below rather than a
/// rewrite.
const WRITE_PREFIXES: &[&str] = &["<host>", "ansible"]; // pii-test-fixture: tool-name prefixes for the write-deny gate, not host names (see doc above)

/// AFFIRMATIVE read-only allowlist — the PRIMARY, fail-closed gate. A tool is
/// only ever CALLED if its name affirmatively matches one of these read/query
/// semantics AND it is not in the deny set. Everything else — unknown or
/// ambiguous — is SKIPPED without calling. This is the fail-closed lesson
/// (allowlist > denylist): a denylist fails OPEN (any mutating tool whose name
/// lacks a denied token gets probed), so we never rely on it as the primary
/// gate. Matched as whole `_`-separated tokens (so `forget` never matches
/// `get`, and a mutating tool needs an actual read token to be eligible — and
/// even then the deny gate still vetoes it).
///
/// STRICTLY read-SEMANTIC verbs/nouns ONLY. A token that a MUTATING tool could
/// plausibly contain is NOT allowed here — it re-opens the fail-open hole. The
/// canonical example is `on`: it would allowlist `power_on` / `toggle_on` /
/// `turn_on` (all writes). Ambiguous tokens (`on`, `deck`, `today`, `recent`,
/// `recently`, `log`, `logs`, `view`, `state`, `available`, `domain`,
/// `current`, `latest`) were deliberately DROPPED; the specific legitimate read
/// tools that only matched via them are covered by exact name in
/// [`SAFE_READ_EXACT`] instead.
const SAFE_READ_TOKENS: &[&str] = &[
    "status",
    "health",
    "summary",
    "list",
    "get",
    "show",
    "info",
    "search",
    "query",
    "read",
    "check",
    "stats",
    "activity",
    "ping",
    "whoami",
    "models",
    "catalog",
    "balance",
    "recommend",
    "describe",
    "count",
    "capabilities",
    "version",
    "history",
    "metrics",
    "usage",
    "report",
    "detail",
    "inspect",
    "preview",
    "diff",
    "snapshot",
    "peek",
];

/// Exact tool names that are known read-only but whose names contain no
/// [`SAFE_READ_TOKENS`] token (e.g. the authoritative clock, or reads that only
/// matched via a now-dropped ambiguous token like `today`/`recent`/`on`).
/// Curated, not pattern-derived — every entry was verified read-only from its
/// tool description; extend only with tools likewise verified. The deny gate
/// ([`is_write_destructive`]) still runs first, so an exact-listed name that
/// somehow carried a write token would still be vetoed.
const SAFE_READ_EXACT: &[&str] = &[
    "time_now",
    "utc_now",
    "weather",
    "echo",
    // Reads that only matched via the dropped `on`/`deck` tokens.
    "media_on_deck",
    // Reads that only matched via the dropped `today`/`recent`/`recently` tokens.
    "vitals_today",
    "vitals_recent",
    "myelin_today",
    "ledger_recent",
    "seer_recent",
    "google_calendar_today",
    "media_recently_added",
    // Reads that only matched via the dropped `logs` token.
    "vector_logs",
    "portainer_container_logs",
];

/// Affirmative, fail-closed read-only test: `true` only when the tool's name
/// matches a curated safe-read token or exact name. Callers must ALSO confirm
/// the deny gate ([`is_write_destructive`]) does not veto it.
pub fn is_safe_read(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if SAFE_READ_EXACT.contains(&lower.as_str()) {
        return true;
    }
    lower.split('_').any(|tok| SAFE_READ_TOKENS.contains(&tok))
}

/// Returns `true` if a tool must NEVER be probed (write/destructive/guarded),
/// based purely on its name.
pub fn is_write_destructive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();

    for p in WRITE_PREFIXES {
        if lower.starts_with(p) {
            return true;
        }
    }

    // Any `_`-separated token that is a mutating verb makes the tool unsafe.
    // (Token match, not substring, so read tools like `model_advisor` or
    // `constellation_version` are not falsely flagged — but `soma_rename_agent`
    // IS caught on its `rename` token even though its last token is `agent`.)
    lower.split('_').any(|tok| WRITE_VERB_TOKENS.contains(&tok))
}

/// Does a tool's JSON-Schema `parameters` object declare a non-empty
/// `required` array?
pub fn schema_has_required(parameters: &Value) -> bool {
    parameters
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

/// Strip any federation/namespace prefix from an advertised tool name, yielding
/// the BARE name the guarded registry / the gateway key on. Mesh-federated
/// tools are advertised as `<namespace>__<bare>` (see
/// `crate::mesh::merge::split_namespaced`, the same derivation `mcp_server.rs`
/// uses before calling `approval::is_guarded`); a name with no `__` boundary is
/// already bare and returned unchanged.
pub fn bare_tool_name(name: &str) -> &str {
    crate::mesh::merge::split_namespaced(name)
        .map(|(_ns, bare)| bare)
        .unwrap_or(name)
}

/// The action the sweep should take for a given tool, decided WITHOUT calling
/// it. The default is SKIP — a tool is only ever probed when affirmatively
/// known read-only AND not vetoed by any stronger gate. Skip carries a reason
/// so the report says WHY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAction {
    /// Vetoed by the fleet's machine-readable guarded registry
    /// (`crate::approval::is_guarded`) — the STRONGEST, authoritative gate.
    /// Approval-gated/dangerous tools (the secrets-manager, config-management,
    /// agent-runner and scheduler tool families, plus `pg_ddl`/`pg_admin`/
    /// `pg_execute`/`git_public_mirror_*`) are skipped by REGISTRY,
    /// independent of what their name looks like.
    SkipGuarded,
    /// Vetoed by the destructive-name deny gate (write/mutation token/prefix).
    SkipDestructive,
    /// Not affirmatively read-only — the fail-closed default. Never called.
    SkipNotAllowlisted,
    /// Has required params — classified `needs_args` from the schema without
    /// fabricating arguments (never called).
    NeedsArgs,
    /// Affirmatively read-only and safe to probe with an empty (`{}`) arg set.
    Probe,
}

/// Decide, from the tool's name + schema alone, whether to skip it (and why),
/// classify it as needs-args, or probe it with `{}`.
///
/// LAYERED, fail-closed safety model — strongest/most-authoritative gate wins,
/// in this order (each gate can only ever DEMOTE toward skip, never promote to
/// a call):
///   1. **Guarded registry (AUTHORITATIVE).** `crate::approval::is_guarded` on
///      the BARE name is the fleet's canonical, machine-readable classifier for
///      approval-gated/dangerous tools. It is checked FIRST and independent of
///      any name heuristic, so e.g. `infisical_get_secret` / `infisical_status`
///      / `openhands_get_status` — which *contain read tokens* — are still
///      `SkipGuarded`, never probed. This is the guarantee a name-token
///      allowlist alone cannot provide.
///   2. **Destructive-name deny (second veto).** Any write/mutation token or
///      `WRITE_PREFIXES` tool-family prefix ⇒ `SkipDestructive`, even if a read token also
///      matched (`queue_status`).
///   3. **Required-args.** Any tool with required params ⇒ `NeedsArgs` — never
///      called, no fabricated arguments.
///   4. **Read-only allowlist (fail-closed default).** Only a bare name that
///      affirmatively matches [`is_safe_read`] with NO required args reaches
///      `Probe`. Everything else ⇒ `SkipNotAllowlisted`.
///
/// ## Accepted residual risk
/// A NON-guarded, read-token-named, no-required-args tool could in principle be
/// probed. That residual is bounded because: (a) `is_guarded` authoritatively
/// skips the entire dangerous/approval-gated class regardless of name; (b) the
/// destructive-verb denylist is a second veto; (c) the full current 381-tool
/// catalog was audited by hand so no real write tool matches the read
/// allowlist; and (d) any tool reaching `Probe` is BY DEFINITION non-guarded —
/// i.e. benign and not approval-gated. The only forward risk is a NEW,
/// unaudited tool that both carries a read token and is not registered as
/// guarded — lower-severity by construction (non-guarded ⇒ not dangerous), and
/// the mitigation is to register genuinely dangerous tools in
/// `approval::GUARDED_BARE_NAMES`, which this gate then honours automatically.
pub fn decide_probe_action(name: &str, parameters: &Value) -> ProbeAction {
    // All gates key on the BARE name so a federation-prefixed guarded tool
    // (`ns__infisical_get_secret`) is still caught.
    let bare = bare_tool_name(name);
    if crate::approval::is_guarded(bare) {
        ProbeAction::SkipGuarded
    } else if is_write_destructive(bare) {
        ProbeAction::SkipDestructive
    } else if schema_has_required(parameters) {
        ProbeAction::NeedsArgs
    } else if is_safe_read(bare) {
        ProbeAction::Probe
    } else {
        ProbeAction::SkipNotAllowlisted
    }
}

/// Classify the OUTCOME of an actual `{}` probe into a [`ProbeStatus`].
/// `None` represents a probe that timed out (no result returned in time).
pub fn classify_probe(result: Option<&Result<String, ToolError>>) -> ProbeStatus {
    match result {
        None => ProbeStatus::Broken, // timed out
        // LOW (acknowledged, no behaviour change): a handful of tools fold a
        // backend failure into an `Ok(json)` body carrying an `"error"` key
        // (e.g. the `lumina_*` web tools) rather than returning `Err`. Those
        // are classified `working` here because they returned a non-error
        // result at the tool-contract level — deep-parsing every tool's JSON
        // for an embedded error convention would be brittle and tool-specific,
        // so it is deliberately out of scope for the sweep's coarse tri-state.
        Some(Ok(_)) => ProbeStatus::Working,
        Some(Err(ToolError::NotConfigured(_))) => ProbeStatus::NotConfigured,
        Some(Err(ToolError::InvalidArgument(_))) => ProbeStatus::NeedsArgs,
        // Everything else is a reachable-but-failing transport/backend error.
        Some(Err(
            ToolError::Http(_)
            | ToolError::Execution(_)
            | ToolError::Database(_)
            | ToolError::NotFound(_)
            | ToolError::Conflict(_),
        )) => ProbeStatus::Broken,
    }
}

/// Compute the overall roll-up from the check list and whether the sweep found
/// any broken tools. `not_configured` NEVER escalates overall (INFO); only real
/// failures do, and overall is the worst severity that has one.
pub fn compute_overall(checks: &[CheckResult], sweep_has_broken: bool) -> Overall {
    let critical_fail = checks
        .iter()
        .any(|c| c.severity == Severity::Critical && c.status == CheckStatus::Fail);
    if critical_fail {
        return Overall::Critical;
    }
    let degraded_fail = checks
        .iter()
        .any(|c| c.severity == Severity::Degraded && c.status == CheckStatus::Fail);
    if degraded_fail || sweep_has_broken {
        return Overall::Degraded;
    }
    Overall::Healthy
}

/// The tool-name prefix used for the `by_prefix` rollup (segment before the
/// first `_`).
pub fn tool_prefix(name: &str) -> &str {
    name.split('_').next().unwrap_or(name)
}

/// Aggregate the sweep matrix into `{status: count}`.
pub fn count_by_status(matrix: &[ToolProbe]) -> Value {
    let mut working = 0u64;
    let mut not_configured = 0u64;
    let mut needs_args = 0u64;
    let mut broken = 0u64;
    let mut skipped = 0u64;
    for p in matrix {
        match p.status.as_str() {
            "working" => working += 1,
            "not_configured" => not_configured += 1,
            "needs_args" => needs_args += 1,
            "broken" => broken += 1,
            "skipped" => skipped += 1,
            _ => {}
        }
    }
    json!({
        "total": matrix.len(),
        "working": working,
        "not_configured": not_configured,
        "needs_args": needs_args,
        "broken": broken,
        "skipped": skipped,
    })
}

/// Aggregate the sweep matrix into `{prefix: {status: count}}` so the operator
/// sees e.g. "media: 3 working / 5 not_configured".
pub fn rollup_by_prefix(matrix: &[ToolProbe]) -> Value {
    use std::collections::BTreeMap;
    // BTreeMap keeps prefixes sorted for a stable, readable report.
    let mut map: BTreeMap<String, BTreeMap<&'static str, u64>> = BTreeMap::new();
    for p in matrix {
        let prefix = tool_prefix(&p.name).to_string();
        let entry = map.entry(prefix).or_default();
        let key: &'static str = match p.status.as_str() {
            "working" => "working",
            "not_configured" => "not_configured",
            "needs_args" => "needs_args",
            "broken" => "broken",
            "skipped" => "skipped",
            _ => "other",
        };
        *entry.entry(key).or_insert(0) += 1;
    }
    let obj: serde_json::Map<String, Value> = map
        .into_iter()
        .map(|(prefix, counts)| {
            let inner: serde_json::Map<String, Value> = counts
                .into_iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            (prefix, Value::Object(inner))
        })
        .collect();
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Timeouts (env-overridable, bounded defaults)
// ---------------------------------------------------------------------------

/// Clamp helper: parse an env u64, clamp into `[min, max]`, else `default`. A
/// misconfig can neither hang the sweep (unbounded timeout) nor fork-bomb it
/// (unbounded concurrency).
fn clamped_env_u64(key: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.clamp(min, max))
        .unwrap_or(default)
}

fn probe_timeout() -> Duration {
    // ≥1s, ≤30s (FIX 2 — bound the knob).
    Duration::from_secs(clamped_env_u64("SELFTEST_PROBE_TIMEOUT_SECS", 4, 1, 30))
}

fn chat_timeout() -> Duration {
    // ≥1s, ≤60s (lumina-deep may cold-load; still bounded).
    Duration::from_secs(clamped_env_u64("SELFTEST_CHAT_TIMEOUT_SECS", 20, 1, 60))
}

/// Max number of tool probes to run concurrently in the sweep, so a catalog of
/// ~200 tools doesn't serialize into minutes of unreachable-backend waits.
/// Clamped ≥1, ≤64 (FIX 2 — a misconfig can't fork-bomb the sweep).
fn probe_concurrency() -> usize {
    clamped_env_u64("SELFTEST_PROBE_CONCURRENCY", 16, 1, 64) as usize
}

// ---------------------------------------------------------------------------
// The per-tool functional sweep
// ---------------------------------------------------------------------------

/// Process-wide cached sweep registry, built exactly ONCE (FIX 3).
///
/// `register_all` is NOT side-effect-free: `crate::compiler::register` spawns a
/// durable-queue scheduler loop when it runs inside a tokio runtime with Redis
/// configured (guarded by a global `AtomicBool` so it can only ever spawn once
/// process-wide), and a few modules (`council`, `cortex`) allocate in-memory
/// stores. To avoid re-triggering those on every sweep, the sweep's registry is
/// built once here and reused. Reads (`list()`, `call()`) are `&self` and
/// concurrency-safe, so a shared `Arc` is sufficient. Note the live process
/// already built its own registry via `register_all`; this is a second,
/// long-lived copy dedicated to self-test probing (the compiler spawn-guard
/// makes the second `register_all` a no-op for the scheduler).
static SWEEP_REGISTRY: std::sync::OnceLock<Arc<ToolRegistry>> = std::sync::OnceLock::new();

fn sweep_registry() -> Arc<ToolRegistry> {
    SWEEP_REGISTRY
        .get_or_init(|| {
            let mut registry = ToolRegistry::new();
            register_all(&mut registry);
            Arc::new(registry)
        })
        .clone()
}

/// Run the per-tool functional sweep against the cached sweep registry (the
/// same catalog `agent_selftest` itself lives in). `self_name` is skipped so
/// the sweep never recurses into itself.
async fn run_tool_sweep(self_name: &str) -> Vec<ToolProbe> {
    let registry = sweep_registry();
    let catalog: Vec<ToolInfo> = registry.list();

    let timeout = probe_timeout();

    let probes = stream::iter(catalog.into_iter().filter(|t| t.name != self_name))
        .map(|info| {
            let registry = Arc::clone(&registry);
            async move { probe_one_tool(&registry, &info, timeout).await }
        })
        .buffer_unordered(probe_concurrency())
        .collect::<Vec<ToolProbe>>()
        .await;

    // Stable, readable ordering (buffer_unordered returns completion order).
    let mut probes = probes;
    probes.sort_by(|a, b| a.name.cmp(&b.name));
    probes
}

/// Probe a single tool, honouring the skip/needs-args/probe decision and the
/// per-probe timeout. Detail strings are COARSE categories only — never the
/// raw error text — so no backend URL/token can leak.
async fn probe_one_tool(registry: &ToolRegistry, info: &ToolInfo, timeout: Duration) -> ToolProbe {
    let start = Instant::now();
    let (status, detail) = match decide_probe_action(&info.name, &info.parameters) {
        ProbeAction::SkipGuarded => (
            ProbeStatus::Skipped,
            "approval-gated/guarded (registry) — not probed".to_string(),
        ),
        ProbeAction::SkipDestructive => (
            ProbeStatus::Skipped,
            "write/destructive — not probed".to_string(),
        ),
        ProbeAction::SkipNotAllowlisted => (
            ProbeStatus::Skipped,
            "not on read-only allowlist — not probed (fail-closed)".to_string(),
        ),
        ProbeAction::NeedsArgs => (
            ProbeStatus::NeedsArgs,
            "requires parameters (not probed with fabricated args)".to_string(),
        ),
        ProbeAction::Probe => {
            let call = registry.call(&info.name, json!({}));
            let outcome = tokio::time::timeout(timeout, call).await;
            match outcome {
                // Timed out.
                Err(_) => (ProbeStatus::Broken, "probe timed out".to_string()),
                // Registry returned (Some = tool found; None impossible here
                // since the name came from `list()`).
                Ok(result) => {
                    let status = classify_probe(result.as_ref());
                    (status, coarse_detail(status, result.as_ref()))
                }
            }
        }
    };

    ToolProbe {
        name: info.name.clone(),
        status: status.as_str().to_string(),
        latency_ms: start.elapsed().as_millis() as u64,
        detail,
    }
}

/// Map a probe status to a coarse, non-leaking detail string. The raw
/// `ToolError` message is deliberately NOT included (some backends' `Http`
/// errors could carry a URL) — only the category.
fn coarse_detail(status: ProbeStatus, _result: Option<&Result<String, ToolError>>) -> String {
    match status {
        ProbeStatus::Working => "ok (non-error result)".to_string(),
        ProbeStatus::NotConfigured => "reachable; backend/env not provisioned".to_string(),
        ProbeStatus::NeedsArgs => "reachable; requires parameters".to_string(),
        ProbeStatus::Broken => "transport/backend error".to_string(),
        ProbeStatus::Skipped => "write/destructive/guarded — not probed".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Cross-service canaries
// ---------------------------------------------------------------------------

fn http_client(timeout: Duration) -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| ToolError::Http(format!("client build: {e}")))
}

/// Attach the shared-secret service JWT Chord's `auth_check` expects, if the
/// signing secret is provisioned. Returns `None` when the secret is unset —
/// callers decide whether that is itself a failure (embeddings) or just means
/// "attempt without auth" (chat/agent on a loopback deploy).
fn service_bearer() -> Option<String> {
    crate::federation::mint_service_jwt().ok()
}

/// Pre-flight Chord's model list so the inference canary can name WHICH proxy
/// isn't served, not just "inference down". Best-effort: `None` on any error.
async fn chord_model_ids(client: &reqwest::Client, base: &str) -> Option<Vec<String>> {
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(tok) = service_bearer() {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let ids = body.get("data").and_then(|d| d.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .collect::<Vec<_>>()
    })?;
    Some(ids)
}

/// Inference canary: one short chat completion per named proxy via Chord's
/// `/v1/chat/completions`. Names which proxy failed (and whether it was even in
/// Chord's model list).
async fn check_inference(profile: &SelftestProfile) -> Vec<CheckResult> {
    let base = crate::config::chord_personal_federation_url();
    let timeout = chat_timeout();
    let client = match http_client(timeout) {
        Ok(c) => c,
        Err(_) => {
            return vec![CheckResult {
                id: "inference".to_string(),
                capability: "inference".to_string(),
                severity: Severity::Critical,
                status: CheckStatus::Fail,
                latency_ms: 0,
                detail: "could not build HTTP client".to_string(),
            }]
        }
    };

    let model_ids = chord_model_ids(&client, &base).await;

    let mut out = Vec::new();
    for proxy in &profile.named_proxies {
        let start = Instant::now();
        // LOW (acknowledged, no behaviour change): when the `/v1/models`
        // pre-flight itself failed (`model_ids == None`), we do NOT claim the
        // proxy is "missing from the catalog" — `unwrap_or(false)` means the
        // 404 detail simply omits the extra "(not present…)" note rather than
        // asserting an absence we couldn't actually confirm.
        let missing_from_catalog = model_ids
            .as_ref()
            .map(|ids| !ids.iter().any(|id| id == &proxy.name))
            .unwrap_or(false);

        let body = json!({
            "model": proxy.name,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "ping — reply with the single word: pong"}],
        });
        let url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
        let mut req = client.post(&url).json(&body);
        if let Some(tok) = service_bearer() {
            req = req.bearer_auth(tok);
        }

        let (status, detail) = match req.send().await {
            Ok(resp) => {
                let code = resp.status();
                if code.is_success() {
                    let ok_content = resp
                        .json::<Value>()
                        .await
                        .ok()
                        .and_then(|v| {
                            v.pointer("/choices/0/message/content")
                                .and_then(|c| c.as_str())
                                .map(|s| !s.trim().is_empty())
                        })
                        .unwrap_or(false);
                    if ok_content {
                        (
                            CheckStatus::Pass,
                            format!("proxy '{}' responded", proxy.name),
                        )
                    } else {
                        (
                            CheckStatus::Fail,
                            format!(
                                "proxy '{}' returned an empty/invalid completion",
                                proxy.name
                            ),
                        )
                    }
                } else if code.as_u16() == 404 {
                    let extra = if missing_from_catalog {
                        " (not present in Chord's model list)"
                    } else {
                        ""
                    };
                    (
                        CheckStatus::Fail,
                        format!("proxy '{}' → 404{extra}", proxy.name),
                    )
                } else if code.as_u16() == 401 || code.as_u16() == 403 {
                    (
                        CheckStatus::Fail,
                        format!("proxy '{}' → auth rejected ({})", proxy.name, code.as_u16()),
                    )
                } else {
                    (
                        CheckStatus::Fail,
                        format!("proxy '{}' → HTTP {}", proxy.name, code.as_u16()),
                    )
                }
            }
            Err(e) => {
                let kind = if e.is_timeout() {
                    "timeout"
                } else {
                    "unreachable"
                };
                (
                    CheckStatus::Fail,
                    format!("proxy '{}' → {kind}", proxy.name),
                )
            }
        };

        out.push(CheckResult {
            id: format!("inference.{}", proxy.name),
            capability: "inference".to_string(),
            severity: proxy.severity,
            status,
            latency_ms: start.elapsed().as_millis() as u64,
            detail,
        });
    }
    out
}

/// Build the tool-router canary POST body, matching Chord's `AgenticRequest`
/// contract (Chord/src/agentic/context.rs). REQUIRED fields — `messages`,
/// `user_id`, `model` — must ALL be present or axum's `Json` extractor rejects
/// the request with HTTP 422 before the router ever runs (the false-positive
/// this canary previously triggered by omitting `user_id`). Tool restriction is
/// expressed via `permissions` (a real optional field), NOT `allowed_tools`
/// (which Chord does not know), so the probe genuinely exercises a
/// tool-restricted execution.
fn tool_router_canary_body(model: &str, user_id: &str) -> Value {
    json!({
        "model": model,
        "user_id": user_id,
        "messages": [{
            "role": "user",
            "content": "What is the current UTC time? Use the time_now tool and answer briefly."
        }],
        // Chord's tool-restriction field (NOT `allowed_tools`).
        "permissions": ["time_now", "utc_now"],
    })
}

/// Tool-router canary: force a known read-only tool through Chord's
/// `/v1/agent/execute` and assert a plausible, non-error response. A missing
/// endpoint (404) or a 5xx/timeout is the CRITICAL router-failure signal; a 4xx
/// request-contract rejection is surfaced distinctly (our-bug / contract drift)
/// so it can never masquerade as a router outage.
async fn check_tool_router(profile: &SelftestProfile) -> CheckResult {
    let base = crate::config::chord_personal_federation_url();
    let timeout = chat_timeout();
    let start = Instant::now();
    let model = profile
        .named_proxies
        .first()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "lumina".to_string());
    // Chord requires a non-empty `user_id`; use the agent identity if we have
    // one, else a stable "selftest" principal.
    let user_id = if profile.agent_identity.trim().is_empty() {
        "selftest".to_string()
    } else {
        profile.agent_identity.clone()
    };

    let client = match http_client(timeout) {
        Ok(c) => c,
        Err(_) => {
            return CheckResult {
                id: "tool_router".to_string(),
                capability: "tool_router".to_string(),
                severity: Severity::Critical,
                status: CheckStatus::Fail,
                latency_ms: start.elapsed().as_millis() as u64,
                detail: "could not build HTTP client".to_string(),
            }
        }
    };

    let body = tool_router_canary_body(&model, &user_id);
    let url = format!("{}/v1/agent/execute", base.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body);
    if let Some(tok) = service_bearer() {
        req = req.bearer_auth(tok);
    }

    let (status, detail) = match req.send().await {
        Ok(resp) => {
            let code = resp.status();
            let c = code.as_u16();
            if code.is_success() {
                (
                    CheckStatus::Pass,
                    "agent/execute accepted a tool-restricted request".to_string(),
                )
            } else if c == 404 {
                // Endpoint absent = real router outage.
                (
                    CheckStatus::Fail,
                    "agent/execute endpoint missing (404)".to_string(),
                )
            } else if c == 401 || c == 403 {
                (
                    CheckStatus::Fail,
                    format!("agent/execute auth rejected ({c})"),
                )
            } else if code.is_client_error() {
                // Any OTHER 4xx (notably 422) is a REQUEST-CONTRACT problem —
                // our canary body vs Chord's AgenticRequest, or contract drift —
                // NOT a router outage. Surface it distinctly so it can never
                // masquerade as (or escalate like) a real Chord/router failure.
                (
                    CheckStatus::Fail,
                    format!(
                        "tool_router request rejected HTTP {c} — canary body vs \
                         AgenticRequest contract mismatch (not a router outage)"
                    ),
                )
            } else {
                // 5xx = a real Chord/router-side failure.
                (
                    CheckStatus::Fail,
                    format!("agent/execute → HTTP {c} (router/Chord failure)"),
                )
            }
        }
        Err(e) => {
            let kind = if e.is_timeout() {
                "timeout"
            } else {
                "unreachable"
            };
            (CheckStatus::Fail, format!("agent/execute → {kind}"))
        }
    };

    CheckResult {
        id: "tool_router".to_string(),
        capability: "tool_router".to_string(),
        severity: Severity::Critical,
        status,
        latency_ms: start.elapsed().as_millis() as u64,
        detail,
    }
}

/// Memory/embeddings canary: probe Chord `/v1/embeddings` the way engram does.
/// The known live regression is `CHORD_JWT`/service-secret not provisioned, so
/// engram stores WITHOUT embeddings — this flags that exact cause as a CRITICAL
/// memory failure.
async fn check_embeddings() -> CheckResult {
    let start = Instant::now();
    let url = crate::config::embeddings_url();
    let model = crate::config::embeddings_model();
    let timeout = Duration::from_millis(crate::config::embeddings_timeout_ms());

    // No service JWT could be minted ⇒ Chord's auth_check rejects and engram
    // silently stores without embeddings. FIX 5: `mint_service_jwt` can fail
    // for reasons other than an unset secret (bad/rotated key, clock skew), so
    // the detail names the neutral category — "embeddings auth/JWT
    // unavailable" — without over-claiming the specific cause.
    let bearer = service_bearer();
    if bearer.is_none() {
        return CheckResult {
            id: "embeddings".to_string(),
            capability: "memory".to_string(),
            severity: Severity::Critical,
            status: CheckStatus::Fail,
            latency_ms: start.elapsed().as_millis() as u64,
            detail: "embeddings auth/JWT unavailable — engram stores WITHOUT embeddings"
                .to_string(),
        };
    }

    let client = match http_client(timeout) {
        Ok(c) => c,
        Err(_) => {
            return CheckResult {
                id: "embeddings".to_string(),
                capability: "memory".to_string(),
                severity: Severity::Critical,
                status: CheckStatus::Fail,
                latency_ms: start.elapsed().as_millis() as u64,
                detail: "could not build HTTP client".to_string(),
            }
        }
    };

    let body = json!({ "model": model, "input": "selftest embedding canary" });
    let mut req = client.post(&url).json(&body);
    if let Some(tok) = bearer {
        req = req.bearer_auth(tok);
    }

    let (status, detail) = match req.send().await {
        Ok(resp) => {
            let code = resp.status();
            if code.is_success() {
                let has_vec = resp
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| {
                        v.pointer("/data/0/embedding")
                            .and_then(|e| e.as_array())
                            .map(|a| !a.is_empty())
                    })
                    .unwrap_or(false);
                if has_vec {
                    (
                        CheckStatus::Pass,
                        "embeddings reachable and returning vectors".to_string(),
                    )
                } else {
                    (
                        CheckStatus::Fail,
                        "embeddings returned no vector — memory would store unembedded".to_string(),
                    )
                }
            } else if code.as_u16() == 401 || code.as_u16() == 403 {
                (
                    CheckStatus::Fail,
                    "embeddings auth rejected — engram stores WITHOUT embeddings".to_string(),
                )
            } else {
                (
                    CheckStatus::Fail,
                    format!("embeddings → HTTP {}", code.as_u16()),
                )
            }
        }
        Err(e) => {
            let kind = if e.is_timeout() {
                "timeout"
            } else {
                "unreachable"
            };
            (CheckStatus::Fail, format!("embeddings → {kind}"))
        }
    };

    CheckResult {
        id: "embeddings".to_string(),
        capability: "memory".to_string(),
        severity: Severity::Critical,
        status,
        latency_ms: start.elapsed().as_millis() as u64,
        detail,
    }
}

/// Prometheus / service-reachability check (INFO). Reuses the `dura_*`
/// convention: `PROMETHEUS_URL` unset ⇒ `not_configured`, never a guessed host.
async fn check_prometheus() -> CheckResult {
    let start = Instant::now();
    // Same env knob `dura_*` reads (`PROMETHEUS_URL`); unset ⇒ not_configured,
    // never a guessed host.
    let url = match std::env::var("PROMETHEUS_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(u) => u,
        None => {
            return CheckResult {
                id: "prometheus".to_string(),
                capability: "observability".to_string(),
                severity: Severity::Info,
                status: CheckStatus::NotConfigured,
                latency_ms: start.elapsed().as_millis() as u64,
                detail: "PROMETHEUS_URL not set".to_string(),
            }
        }
    };

    let client = match http_client(Duration::from_secs(10)) {
        Ok(c) => c,
        Err(_) => {
            return CheckResult {
                id: "prometheus".to_string(),
                capability: "observability".to_string(),
                severity: Severity::Info,
                status: CheckStatus::Fail,
                latency_ms: start.elapsed().as_millis() as u64,
                detail: "could not build HTTP client".to_string(),
            }
        }
    };

    let q = format!("{}/api/v1/query", url.trim_end_matches('/'));
    let (status, detail) = match client.get(&q).query(&[("query", "up")]).send().await {
        Ok(resp) if resp.status().is_success() => {
            (CheckStatus::Pass, "prometheus reachable".to_string())
        }
        Ok(resp) => (
            CheckStatus::Fail,
            format!("prometheus → HTTP {}", resp.status().as_u16()),
        ),
        Err(e) => {
            let kind = if e.is_timeout() {
                "timeout"
            } else {
                "unreachable"
            };
            (CheckStatus::Fail, format!("prometheus → {kind}"))
        }
    };

    CheckResult {
        id: "prometheus".to_string(),
        capability: "observability".to_string(),
        severity: Severity::Info,
        status,
        latency_ms: start.elapsed().as_millis() as u64,
        detail,
    }
}

// ---------------------------------------------------------------------------
// Critical-tools cross-check (FIX 4)
// ---------------------------------------------------------------------------

/// Cross-check the profile's declared `critical_tools` against the sweep
/// matrix. A critical tool that is ABSENT from the catalog, or present-but-
/// `broken`, is a CRITICAL failure; present-but-`not_configured` is DEGRADED (a
/// provisioning gap, not an outright break). `working` / `needs_args` /
/// `skipped` all mean "present in the catalog" and therefore pass — a skipped
/// (e.g. write) critical tool is still confirmed present even though the sweep
/// never called it. Returns `None` when the profile declares no critical tools.
/// Tool names are configuration identifiers (not secrets) so they may appear in
/// the detail string.
pub fn check_critical_tools(
    profile: &SelftestProfile,
    matrix: &[ToolProbe],
) -> Option<CheckResult> {
    if profile.critical_tools.is_empty() {
        return None;
    }
    let mut missing = Vec::new();
    let mut broken = Vec::new();
    let mut not_configured = Vec::new();
    for want in &profile.critical_tools {
        match matrix.iter().find(|p| &p.name == want) {
            None => missing.push(want.as_str()),
            Some(p) => match p.status.as_str() {
                "broken" => broken.push(want.as_str()),
                "not_configured" => not_configured.push(want.as_str()),
                _ => {} // working / needs_args / skipped = present in catalog
            },
        }
    }

    let (severity, status, detail) = if !missing.is_empty() || !broken.is_empty() {
        (
            Severity::Critical,
            CheckStatus::Fail,
            format!("missing: {missing:?}; broken: {broken:?}"),
        )
    } else if !not_configured.is_empty() {
        (
            Severity::Degraded,
            CheckStatus::Fail,
            format!("not_configured: {not_configured:?}"),
        )
    } else {
        (
            Severity::Critical,
            CheckStatus::Pass,
            format!(
                "all {} critical tool(s) present & functional",
                profile.critical_tools.len()
            ),
        )
    };

    Some(CheckResult {
        id: "critical_tools".to_string(),
        capability: "tools".to_string(),
        severity,
        status,
        latency_ms: 0,
        detail,
    })
}

// ---------------------------------------------------------------------------
// Top-level run
// ---------------------------------------------------------------------------

/// Run the full egress self-test for `profile` and assemble the structured
/// report. `self_name` is the `agent_selftest` tool's own name (skipped in the
/// sweep to avoid recursion).
pub async fn run(profile: &SelftestProfile, self_name: &str) -> SelftestReport {
    // Run the cross-service canaries and the per-tool sweep concurrently.
    let (inference, router, embeddings, prometheus, matrix) = tokio::join!(
        check_inference(profile),
        check_tool_router(profile),
        check_embeddings(),
        check_prometheus(),
        run_tool_sweep(self_name),
    );

    let sweep_has_broken = matrix.iter().any(|p| p.status == "broken");

    let mut checks: Vec<CheckResult> = Vec::new();
    checks.extend(inference);
    checks.push(router);
    checks.push(embeddings);
    checks.push(prometheus);

    // Fold the sweep in as one check entry (broken tools = degraded).
    let broken_count = matrix.iter().filter(|p| p.status == "broken").count();
    checks.push(CheckResult {
        id: "tool_sweep".to_string(),
        capability: "tools".to_string(),
        severity: Severity::Degraded,
        status: if broken_count > 0 {
            CheckStatus::Fail
        } else {
            CheckStatus::Pass
        },
        latency_ms: matrix.iter().map(|p| p.latency_ms).max().unwrap_or(0),
        detail: format!("{} tools probed; {} broken", matrix.len(), broken_count),
    });

    // FIX 4: flag any declared critical tool that is absent/broken/unprovisioned.
    if let Some(crit_check) = check_critical_tools(profile, &matrix) {
        checks.push(crit_check);
    }

    let overall = compute_overall(&checks, sweep_has_broken);
    let tool_counts = count_by_status(&matrix);
    let by_prefix = rollup_by_prefix(&matrix);

    let crit = checks
        .iter()
        .filter(|c| c.severity == Severity::Critical && c.status == CheckStatus::Fail)
        .count();
    let summary = format!(
        "[{}] {:?} — {} checks ({} critical failing), tools: {} working / {} not_configured / \
         {} needs_args / {} broken / {} skipped",
        profile.agent_identity,
        overall,
        checks.len(),
        crit,
        tool_counts["working"],
        tool_counts["not_configured"],
        tool_counts["needs_args"],
        tool_counts["broken"],
        tool_counts["skipped"],
    );

    SelftestReport {
        agent_identity: profile.agent_identity.clone(),
        ts: chrono::Utc::now().to_rfc3339(),
        overall,
        checks,
        tool_matrix: matrix,
        tool_counts,
        by_prefix,
        summary,
    }
}

// ---------------------------------------------------------------------------
// The RustTool
// ---------------------------------------------------------------------------

pub struct AgentSelftest;

impl AgentSelftest {
    const NAME: &'static str = "agent_selftest";
}

#[async_trait]
impl RustTool for AgentSelftest {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Self-test an agent's functions from the hub. Enumerates the FULL tool \
         catalog and probes EACH tool with a SAFE read-only call, classifying it \
         working / not_configured / needs_args / broken / skipped (write/destructive \
         tools are never called). Also canaries the named inference proxies, the \
         tool-router (/v1/agent/execute), embeddings/memory, and Prometheus. \
         Returns a structured per-check + per-tool report. Optional `agent_identity` \
         (default 'lumina') selects the profile."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_identity": {
                    "type": "string",
                    "description": "Agent profile to test (default 'lumina'). Unknown \
                                    identities get a generic single-proxy profile.",
                    "default": "lumina"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let output = self.execute_structured(args).await?;
        Ok(output.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let identity = args
            .get("agent_identity")
            .and_then(|v| v.as_str())
            .unwrap_or("lumina");
        let profile = SelftestProfile::for_identity(identity);

        let report = run(&profile, Self::NAME).await;

        let structured = serde_json::to_value(&report)
            .map_err(|e| ToolError::Execution(format!("report serialize: {e}")))?;
        let text = serde_json::to_string_pretty(&structured)
            .map_err(|e| ToolError::Execution(format!("report render: {e}")))?;

        Ok(ToolOutput::with_structured(text, structured))
    }
}

/// Register the `agent_selftest` tool.
pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(AgentSelftest)) {
        tracing::warn!("selftest: failed to register agent_selftest: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── deny-policy / skip decision ────────────────────────────────────────

    #[test]
    fn write_verbs_are_denied() {
        for name in &[
            "gitea_create_pr",
            "plane_delete_issue",
            "relay_send",
            "muse_grab",
            "sonarr_download",
            "soma_rename_agent", // caught on the `rename` token, not the last token
            "dev_run",
            "foo_execute",
            "bar_ddl",
            "baz_admin",
            "reminder_add",
            "thing_set",
            "x_update",
            "y_remove",
            "z_push",
            "q_merge",
            "w_organize",
            "e_cleanup",
            "r_request",
            "vector_onboard",
        ] {
            assert!(is_write_destructive(name), "{name} should be denied");
        }
    }

    #[test]
    fn write_prefixes_are_denied() {
        assert!(is_write_destructive("pve_start_ct"));
        assert!(is_write_destructive("pvesh_get"));
        assert!(is_write_destructive("ansible_playbook"));
        assert!(is_write_destructive("ansible_run"));
    }

    #[test]
    fn read_tools_are_not_denied() {
        for name in &[
            "time_now",
            "utc_now",
            "health",
            "constellation_version",
            "engram_query",
            "dura_constellation_health",
            "model_advisor",
            "vitals_today",
            "gitea_list_identities",
            "weather",
            "net_ping",
            "searxng_search",
        ] {
            assert!(!is_write_destructive(name), "{name} should NOT be denied");
        }
    }

    // ── schema required detection + probe action ──────────────────────────

    #[test]
    fn schema_required_detects_nonempty_required_array() {
        assert!(schema_has_required(&json!({"required": ["x"]})));
        assert!(!schema_has_required(&json!({"required": []})));
        assert!(!schema_has_required(
            &json!({"type": "object", "properties": {}})
        ));
        assert!(!schema_has_required(&json!({})));
    }

    #[test]
    fn decide_probe_action_skip_beats_needs_args() {
        // Destructive AND has required params ⇒ still SkipDestructive (never call).
        let params = json!({"required": ["id"]});
        assert_eq!(
            decide_probe_action("plane_delete_issue", &params),
            ProbeAction::SkipDestructive
        );
    }

    #[test]
    fn decide_probe_action_needs_args_for_required_read_tool() {
        // On the read allowlist (`search`) but has required params ⇒ needs_args.
        let params = json!({"required": ["query"]});
        assert_eq!(
            decide_probe_action("skills_search", &params),
            ProbeAction::NeedsArgs
        );
    }

    #[test]
    fn decide_probe_action_probe_for_no_required_read_tool() {
        let params = json!({"type": "object", "properties": {}});
        assert_eq!(decide_probe_action("time_now", &params), ProbeAction::Probe);
    }

    // ── FIX 1: fail-closed allowlist is the PRIMARY gate ──────────────────

    #[test]
    fn is_safe_read_matches_read_semantics_and_exact_names() {
        for name in &[
            "media_domain_status",
            "soma_status",
            "myelin_today",
            "dura_constellation_health",
            "gitea_list_identities",
            "net_ping",
            "time_now", // exact-name allowlist (no read token)
            "utc_now",
        ] {
            assert!(is_safe_read(name), "{name} should be read-safe");
        }
    }

    #[test]
    fn on_token_write_family_is_not_read_safe() {
        // The `_on`-suffixed write family: `on` was REMOVED from the allowlist
        // tokens precisely because it would allowlist these mutating tools.
        // None must be probed.
        for name in &["power_on", "toggle_on", "turn_on", "enable_on", "lights_on"] {
            assert!(
                !is_safe_read(name),
                "{name} is a write and must NOT be read-safe"
            );
            // And end-to-end: they resolve to a Skip action, never Probe.
            let no_args = json!({"type": "object", "properties": {}});
            assert!(
                matches!(
                    decide_probe_action(name, &no_args),
                    ProbeAction::SkipDestructive | ProbeAction::SkipNotAllowlisted
                ),
                "{name} must be skipped, not probed"
            );
        }
    }

    #[test]
    fn media_on_deck_is_read_safe_via_exact_name() {
        // The one legit read tool that used to match via `on`/`deck` — now
        // covered by exact name so it still probes.
        assert!(is_safe_read("media_on_deck"));
        let no_args = json!({"type": "object", "properties": {}});
        assert_eq!(
            decide_probe_action("media_on_deck", &no_args),
            ProbeAction::Probe
        );
    }

    #[test]
    fn dropped_token_read_tools_covered_by_exact_name() {
        // Reads that only matched via now-dropped ambiguous tokens
        // (today/recent/logs) stay allowlisted via SAFE_READ_EXACT.
        for name in &[
            "vitals_today",
            "vitals_recent",
            "myelin_today",
            "ledger_recent",
            "seer_recent",
            "google_calendar_today",
            "media_recently_added",
            "vector_logs",
            "portainer_container_logs",
        ] {
            assert!(is_safe_read(name), "{name} should stay read-safe via exact");
        }
        // But the WRITE tools that shared those tokens are NOT covered.
        for name in &[
            "vitals_log_weight", // write
            "odyssey_log_trip",  // write
            "vitals_log_sleep",  // write
        ] {
            assert!(
                !is_safe_read(name),
                "{name} (a write) must NOT be read-safe"
            );
        }
    }

    #[test]
    fn is_safe_read_rejects_unknown_and_mutating_names() {
        // None of these carry a read token — the fail-closed default is SKIP.
        for name in &[
            "ledger_transfer",
            "ledger_append",
            "relay_dispatch",
            "thing_submit",
            "job_schedule",
            "task_enqueue",
            "widget_frobnicate",
        ] {
            assert!(!is_safe_read(name), "{name} must NOT be read-safe");
        }
    }

    #[test]
    fn fail_closed_mutating_no_arg_tool_is_skipped_not_probed() {
        // The exact CRITICAL flaw the reviewer named: a mutating tool with a
        // benign-looking no-arg name. `transfer`/`append` may be MISSING from a
        // denylist, but the allowlist is fail-closed so these are SKIPPED.
        let no_args = json!({"type": "object", "properties": {}});
        assert_eq!(
            decide_probe_action("ledger_transfer", &no_args),
            ProbeAction::SkipDestructive // caught by the belt-and-suspenders deny gate too
        );
        // A mutating tool whose verb is NOT in the deny set still never gets
        // probed — it falls through to the allowlist's fail-closed default.
        assert_eq!(
            decide_probe_action("widget_frobnicate", &no_args),
            ProbeAction::SkipNotAllowlisted
        );
        assert_eq!(
            decide_probe_action("account_liquidate", &no_args),
            ProbeAction::SkipNotAllowlisted
        );
    }

    #[test]
    fn fail_closed_known_read_tools_are_probed() {
        let no_args = json!({"type": "object", "properties": {}});
        assert_eq!(
            decide_probe_action("media_domain_status", &no_args),
            ProbeAction::Probe
        );
        assert_eq!(
            decide_probe_action("time_now", &no_args),
            ProbeAction::Probe
        );
        assert_eq!(
            decide_probe_action("soma_status", &no_args),
            ProbeAction::Probe
        );
    }

    #[test]
    fn deny_gate_vetoes_even_an_allowlist_match() {
        // `queue_status` matches the read token `status` but ALSO carries the
        // denied `queue` token — deny wins, never probed.
        let no_args = json!({"type": "object", "properties": {}});
        assert_eq!(
            decide_probe_action("queue_status", &no_args),
            ProbeAction::SkipDestructive
        );
    }

    // ── AUTHORITATIVE guarded-registry veto (codex's ask) ──────────────────

    #[test]
    fn guarded_registry_tools_are_skip_guarded_even_with_read_token_names() {
        // These are all in `approval::GUARDED_BARE_NAMES`. Several CONTAIN read
        // tokens (status/list/get) so a name-only allowlist would have PROBED
        // them — the machine-readable registry authoritatively skips them.
        let no_args = json!({"type": "object", "properties": {}});
        for name in &[
            "infisical_status",       // read token `status`
            "infisical_list_secrets", // read token `list`
            "infisical_get_secret",   // read token `get`
            "infisical_get_secrets_batch",
            "openhands_get_status",         // read tokens `get` + `status`
            "openhands_list_conversations", // read token `list`
            "ansible_last_run_status",      // read token `status`
            "ansible_run_playbook",
            "routines_pending",
            "routines_approve",
            "pg_ddl",
            "pg_admin",
            "pg_execute",
            "git_public_mirror_push",
        ] {
            assert!(
                crate::approval::is_guarded(name),
                "test premise: {name} must be in the guarded registry"
            );
            assert_eq!(
                decide_probe_action(name, &no_args),
                ProbeAction::SkipGuarded,
                "{name} must be SkipGuarded (registry veto), never probed"
            );
        }
    }

    #[test]
    fn bare_tool_name_strips_federation_namespace() {
        assert_eq!(
            bare_tool_name("myupstream__infisical_get_secret"),
            "infisical_get_secret"
        );
        // No `__` boundary ⇒ already bare.
        assert_eq!(bare_tool_name("time_now"), "time_now");
        // Only the FIRST `__` is the namespace boundary.
        assert_eq!(bare_tool_name("ns__foo__bar"), "foo__bar");
    }

    #[test]
    fn federation_prefixed_guarded_tool_is_still_skip_guarded() {
        // The guard must survive a namespace prefix (bare-name derivation).
        let no_args = json!({"type": "object", "properties": {}});
        assert_eq!(
            decide_probe_action("someupstream__infisical_get_secret", &no_args),
            ProbeAction::SkipGuarded
        );
        assert_eq!(
            decide_probe_action("fleet__pg_ddl", &no_args),
            ProbeAction::SkipGuarded
        );
    }

    #[test]
    fn guarded_veto_wins_even_over_needs_args() {
        // A guarded tool WITH required params is still SkipGuarded (the guard is
        // layer 1, above the required-args layer) — never NeedsArgs, never probed.
        let with_required = json!({"required": ["project"]});
        assert_eq!(
            decide_probe_action("infisical_get_secret", &with_required),
            ProbeAction::SkipGuarded
        );
    }

    #[test]
    fn probed_tools_are_never_guarded() {
        // Anything that reaches Probe is by definition non-guarded (the accepted
        // residual: probeable ⇒ benign, not approval-gated).
        let no_args = json!({"type": "object", "properties": {}});
        for name in &[
            "time_now",
            "media_domain_status",
            "soma_status",
            "media_on_deck",
        ] {
            assert!(!crate::approval::is_guarded(name));
            assert_eq!(decide_probe_action(name, &no_args), ProbeAction::Probe);
        }
    }

    // ── classify_probe: outcome → status ──────────────────────────────────

    #[test]
    fn classify_ok_is_working() {
        let r: Result<String, ToolError> = Ok("fine".into());
        assert_eq!(classify_probe(Some(&r)), ProbeStatus::Working);
    }

    #[test]
    fn classify_not_configured() {
        let r: Result<String, ToolError> = Err(ToolError::NotConfigured("X not set".into()));
        assert_eq!(classify_probe(Some(&r)), ProbeStatus::NotConfigured);
    }

    #[test]
    fn classify_invalid_argument_is_needs_args() {
        let r: Result<String, ToolError> = Err(ToolError::InvalidArgument("id required".into()));
        assert_eq!(classify_probe(Some(&r)), ProbeStatus::NeedsArgs);
    }

    #[test]
    fn classify_transport_errors_are_broken() {
        for e in [
            ToolError::Http("error sending request".into()),
            ToolError::Execution("panic".into()),
            ToolError::Database("pool".into()),
            ToolError::NotFound("x".into()),
            ToolError::Conflict("x".into()),
        ] {
            let r: Result<String, ToolError> = Err(e);
            assert_eq!(classify_probe(Some(&r)), ProbeStatus::Broken);
        }
    }

    #[test]
    fn classify_timeout_is_broken() {
        assert_eq!(classify_probe(None), ProbeStatus::Broken);
    }

    // ── overall severity math ─────────────────────────────────────────────

    fn chk(severity: Severity, status: CheckStatus) -> CheckResult {
        CheckResult {
            id: "t".into(),
            capability: "t".into(),
            severity,
            status,
            latency_ms: 0,
            detail: String::new(),
        }
    }

    #[test]
    fn overall_healthy_when_all_pass() {
        let checks = vec![
            chk(Severity::Critical, CheckStatus::Pass),
            chk(Severity::Degraded, CheckStatus::Pass),
        ];
        assert_eq!(compute_overall(&checks, false), Overall::Healthy);
    }

    #[test]
    fn overall_not_configured_does_not_escalate() {
        // A Critical-severity check that is merely not_configured must NOT make
        // overall critical (INFO posture for unprovisioned backends).
        let checks = vec![chk(Severity::Critical, CheckStatus::NotConfigured)];
        assert_eq!(compute_overall(&checks, false), Overall::Healthy);
    }

    #[test]
    fn overall_critical_on_critical_fail() {
        let checks = vec![
            chk(Severity::Critical, CheckStatus::Fail),
            chk(Severity::Degraded, CheckStatus::Fail),
        ];
        assert_eq!(compute_overall(&checks, true), Overall::Critical);
    }

    #[test]
    fn overall_degraded_on_degraded_fail_only() {
        let checks = vec![
            chk(Severity::Critical, CheckStatus::Pass),
            chk(Severity::Degraded, CheckStatus::Fail),
        ];
        assert_eq!(compute_overall(&checks, false), Overall::Degraded);
    }

    #[test]
    fn overall_degraded_when_sweep_has_broken() {
        let checks = vec![chk(Severity::Critical, CheckStatus::Pass)];
        assert_eq!(compute_overall(&checks, true), Overall::Degraded);
    }

    // ── aggregation ───────────────────────────────────────────────────────

    fn probe(name: &str, status: &str) -> ToolProbe {
        ToolProbe {
            name: name.into(),
            status: status.into(),
            latency_ms: 1,
            detail: String::new(),
        }
    }

    #[test]
    fn count_by_status_tallies_each_bucket() {
        let matrix = vec![
            probe("a_x", "working"),
            probe("a_y", "working"),
            probe("b_x", "not_configured"),
            probe("c_x", "needs_args"),
            probe("d_x", "broken"),
            probe("e_create", "skipped"),
        ];
        let counts = count_by_status(&matrix);
        assert_eq!(counts["total"], 6);
        assert_eq!(counts["working"], 2);
        assert_eq!(counts["not_configured"], 1);
        assert_eq!(counts["needs_args"], 1);
        assert_eq!(counts["broken"], 1);
        assert_eq!(counts["skipped"], 1);
    }

    #[test]
    fn rollup_by_prefix_groups_and_counts() {
        let matrix = vec![
            probe("media_scan", "working"),
            probe("media_status", "working"),
            probe("media_meta", "not_configured"),
            probe("time_now", "working"),
        ];
        let rollup = rollup_by_prefix(&matrix);
        assert_eq!(rollup["media"]["working"], 2);
        assert_eq!(rollup["media"]["not_configured"], 1);
        assert_eq!(rollup["time"]["working"], 1);
    }

    #[test]
    fn tool_prefix_takes_segment_before_first_underscore() {
        assert_eq!(tool_prefix("lumina_web_fetch"), "lumina");
        assert_eq!(tool_prefix("time_now"), "time");
        assert_eq!(tool_prefix("health"), "health");
    }

    // ── profile parameterization ──────────────────────────────────────────

    #[test]
    fn lumina_profile_has_three_named_proxies_with_deep_degraded() {
        let p = SelftestProfile::lumina();
        assert_eq!(p.agent_identity, "lumina");
        assert_eq!(p.named_proxies.len(), 3);
        let deep = p
            .named_proxies
            .iter()
            .find(|n| n.name == "lumina-deep")
            .unwrap();
        assert_eq!(deep.severity, Severity::Degraded);
        let fast = p
            .named_proxies
            .iter()
            .find(|n| n.name == "lumina-fast")
            .unwrap();
        assert_eq!(fast.severity, Severity::Critical);
    }

    #[test]
    fn for_identity_defaults_to_lumina() {
        assert_eq!(
            SelftestProfile::for_identity("lumina").agent_identity,
            "lumina"
        );
        assert_eq!(SelftestProfile::for_identity("").agent_identity, "lumina");
    }

    #[test]
    fn for_identity_generic_profile_is_parameterized_not_hardcoded() {
        let p = SelftestProfile::for_identity("aria");
        assert_eq!(p.agent_identity, "aria");
        assert_eq!(p.named_proxies.len(), 1);
        assert_eq!(p.named_proxies[0].name, "aria");
        assert!(p.critical_tools.is_empty());
    }

    // ── FIX 4: critical-tools cross-check ─────────────────────────────────

    fn profile_with_criticals(tools: &[&str]) -> SelftestProfile {
        let mut p = SelftestProfile::lumina();
        p.critical_tools = tools.iter().map(|s| s.to_string()).collect();
        p
    }

    #[test]
    fn critical_tools_none_declared_yields_no_check() {
        let mut p = SelftestProfile::lumina();
        p.critical_tools.clear();
        assert!(check_critical_tools(&p, &[]).is_none());
    }

    #[test]
    fn critical_tools_absent_is_critical_fail() {
        let p = profile_with_criticals(&["time_now"]);
        // Empty matrix ⇒ the tool is missing from the catalog.
        let c = check_critical_tools(&p, &[]).unwrap();
        assert_eq!(c.severity, Severity::Critical);
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(c.detail.contains("time_now"));
    }

    #[test]
    fn critical_tools_broken_is_critical_fail() {
        let p = profile_with_criticals(&["time_now"]);
        let matrix = vec![probe("time_now", "broken")];
        let c = check_critical_tools(&p, &matrix).unwrap();
        assert_eq!(c.severity, Severity::Critical);
        assert_eq!(c.status, CheckStatus::Fail);
    }

    #[test]
    fn critical_tools_not_configured_is_degraded_fail() {
        let p = profile_with_criticals(&["time_now"]);
        let matrix = vec![probe("time_now", "not_configured")];
        let c = check_critical_tools(&p, &matrix).unwrap();
        assert_eq!(c.severity, Severity::Degraded);
        assert_eq!(c.status, CheckStatus::Fail);
    }

    #[test]
    fn critical_tools_working_or_needs_args_pass() {
        let p = profile_with_criticals(&["time_now", "engram_query"]);
        let matrix = vec![
            probe("time_now", "working"),
            probe("engram_query", "needs_args"),
        ];
        let c = check_critical_tools(&p, &matrix).unwrap();
        assert_eq!(c.status, CheckStatus::Pass);
    }

    // ── tool metadata + registration ──────────────────────────────────────

    #[test]
    fn tool_metadata_is_stable() {
        let t = AgentSelftest;
        assert_eq!(t.name(), "agent_selftest");
        let params = t.parameters();
        assert_eq!(params["type"], "object");
        // No required params ⇒ callable with `{}`.
        assert!(!schema_has_required(&params));
    }

    #[test]
    fn register_adds_agent_selftest() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("agent_selftest"));
    }

    // ── tool_router canary body matches Chord's AgenticRequest contract ────

    #[test]
    fn tool_router_canary_body_has_required_fields_and_permissions() {
        let body = tool_router_canary_body("lumina", "lumina");
        // All three REQUIRED AgenticRequest fields present (omitting `user_id`
        // was the 422 false-positive root cause).
        assert_eq!(body["model"], "lumina");
        assert_eq!(body["user_id"], "lumina");
        assert!(body["messages"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false));
        // Tool restriction uses `permissions` (a real Chord field), NOT the
        // bogus `allowed_tools`.
        assert_eq!(body["permissions"], json!(["time_now", "utc_now"]));
        assert!(
            body.get("allowed_tools").is_none(),
            "must not send `allowed_tools` — Chord's field is `permissions`"
        );
    }

    #[test]
    fn tool_router_canary_user_id_falls_back_when_identity_blank() {
        // `check_tool_router` uses "selftest" when the profile identity is
        // blank; assert the body helper carries whatever principal it's given.
        let body = tool_router_canary_body("m", "selftest");
        assert_eq!(body["user_id"], "selftest");
    }

    #[test]
    fn agent_selftest_is_not_itself_flagged_destructive() {
        // The sweep must be willing to see itself in the catalog (it skips by
        // name, not by deny-policy) — and its own name must not read as a
        // write verb.
        assert!(!is_write_destructive("agent_selftest"));
    }
}
