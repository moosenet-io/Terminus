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
//! ## Safety model (the sweep never mutates)
//! The sweep classifies every tool tri-state and NEVER calls a
//! write/destructive/guarded tool. The skip decision is driven by a STATIC
//! deny-policy over the tool name ([`is_write_destructive`]) plus a
//! "needs required args ⇒ don't fabricate args, classify from schema" rule
//! ([`decide_probe_action`]). When in doubt, it SKIPS — coverage is always
//! traded away in favour of never mutating/spending/notifying.

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
            critical_tools: vec!["time_now".to_string(), "engram_query".to_string()],
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
];

/// Name prefixes that mark an entire tool family as unsafe to probe.
const WRITE_PREFIXES: &[&str] = &["<host>", "ansible"];

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

/// The action the sweep should take for a given tool, decided WITHOUT calling
/// it. Skip (destructive) takes precedence over everything; a tool that would
/// need fabricated args is classified `NeedsArgs` from its schema rather than
/// probed with guessed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAction {
    /// Never call — destructive/guarded.
    Skip,
    /// Has required params — classify `needs_args` without calling.
    NeedsArgs,
    /// Safe to probe with an empty (`{}`) argument set.
    Probe,
}

/// Decide, from the tool's name + schema alone, whether to skip it, classify
/// it as needs-args, or probe it with `{}`.
pub fn decide_probe_action(name: &str, parameters: &Value) -> ProbeAction {
    if is_write_destructive(name) {
        ProbeAction::Skip
    } else if schema_has_required(parameters) {
        ProbeAction::NeedsArgs
    } else {
        ProbeAction::Probe
    }
}

/// Classify the OUTCOME of an actual `{}` probe into a [`ProbeStatus`].
/// `None` represents a probe that timed out (no result returned in time).
pub fn classify_probe(result: Option<&Result<String, ToolError>>) -> ProbeStatus {
    match result {
        None => ProbeStatus::Broken, // timed out
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

fn probe_timeout() -> Duration {
    let secs = std::env::var("SELFTEST_PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(4);
    Duration::from_secs(secs)
}

fn chat_timeout() -> Duration {
    let secs = std::env::var("SELFTEST_CHAT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(20);
    Duration::from_secs(secs)
}

/// Max number of tool probes to run concurrently in the sweep, so a catalog of
/// ~200 tools doesn't serialize into minutes of unreachable-backend waits.
fn probe_concurrency() -> usize {
    std::env::var("SELFTEST_PROBE_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|c| *c > 0)
        .unwrap_or(16)
}

// ---------------------------------------------------------------------------
// The per-tool functional sweep
// ---------------------------------------------------------------------------

/// Run the per-tool functional sweep against a freshly-built copy of the same
/// registry `agent_selftest` itself lives in. Building a fresh `ToolRegistry`
/// via `register_all` is the established in-crate pattern (see
/// `registry::personal_only_tool_metadata`) — the tool has no handle to the
/// live registry it was dispatched from, and rebuilding is cheap (each tool
/// module's `register` only constructs boxed structs + reqwest clients).
///
/// `self_name` is skipped so the sweep never recurses into itself.
async fn run_tool_sweep(self_name: &str) -> Vec<ToolProbe> {
    let mut registry = ToolRegistry::new();
    register_all(&mut registry);
    let registry = Arc::new(registry);
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
        ProbeAction::Skip => (
            ProbeStatus::Skipped,
            "write/destructive/guarded — not probed".to_string(),
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

/// Tool-router canary: force a known read-only tool through Chord's
/// `/v1/agent/execute` and assert a plausible, non-error response. Best-effort
/// on the exact request/response shape (the orchestrator live-verifies on
/// deploy); a missing endpoint (404) is the CRITICAL signal this catches.
async fn check_tool_router(profile: &SelftestProfile) -> CheckResult {
    let base = crate::config::chord_personal_federation_url();
    let timeout = chat_timeout();
    let start = Instant::now();
    let model = profile
        .named_proxies
        .first()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "lumina".to_string());

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

    let body = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": "What is the current UTC time? Use the time_now tool and answer briefly."
        }],
        "allowed_tools": ["time_now", "utc_now"],
    });
    let url = format!("{}/v1/agent/execute", base.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body);
    if let Some(tok) = service_bearer() {
        req = req.bearer_auth(tok);
    }

    let (status, detail) = match req.send().await {
        Ok(resp) => {
            let code = resp.status();
            if code.is_success() {
                (
                    CheckStatus::Pass,
                    "agent/execute accepted a tool-forced request".to_string(),
                )
            } else if code.as_u16() == 404 {
                (
                    CheckStatus::Fail,
                    "agent/execute endpoint missing (404)".to_string(),
                )
            } else if code.as_u16() == 401 || code.as_u16() == 403 {
                (
                    CheckStatus::Fail,
                    format!("agent/execute auth rejected ({})", code.as_u16()),
                )
            } else {
                (
                    CheckStatus::Fail,
                    format!("agent/execute → HTTP {}", code.as_u16()),
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

    // The exact known-broken condition: no service JWT secret ⇒ Chord's
    // auth_check rejects, engram silently stores without embeddings.
    let bearer = service_bearer();
    if bearer.is_none() {
        return CheckResult {
            id: "embeddings".to_string(),
            capability: "memory".to_string(),
            severity: Severity::Critical,
            status: CheckStatus::Fail,
            latency_ms: start.elapsed().as_millis() as u64,
            detail: "embeddings auth not provisioned (service JWT secret unset) — \
                     engram stores WITHOUT embeddings"
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
        // Destructive AND has required params ⇒ still Skip (never call).
        let params = json!({"required": ["id"]});
        assert_eq!(
            decide_probe_action("plane_delete_issue", &params),
            ProbeAction::Skip
        );
    }

    #[test]
    fn decide_probe_action_needs_args_for_required_read_tool() {
        let params = json!({"required": ["query"]});
        assert_eq!(
            decide_probe_action("engram_query", &params),
            ProbeAction::NeedsArgs
        );
    }

    #[test]
    fn decide_probe_action_probe_for_no_required_read_tool() {
        let params = json!({"type": "object", "properties": {}});
        assert_eq!(decide_probe_action("time_now", &params), ProbeAction::Probe);
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

    #[test]
    fn agent_selftest_is_not_itself_flagged_destructive() {
        // The sweep must be willing to see itself in the catalog (it skips by
        // name, not by deny-policy) — and its own name must not read as a
        // write verb.
        assert!(!is_write_destructive("agent_selftest"));
    }
}
