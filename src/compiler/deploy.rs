//! BLD-13 — `compiler_deploy`: trigger-on-publish, fleet-wide.
//!
//! After a successful publish/promote (the store's `current` sha moves), the
//! change should land ON THE FLEET in seconds instead of waiting for the nightly
//! constellation-updater timer. `compiler_deploy(module, channel, hosts="all")`
//! TRIGGERS the already-deployed `constellation-update@<module>` systemd unit —
//! in its BLD-12 fetch mode — on each configured deploy host over the EXISTING
//! sanctioned host-reach path (the same BatchMode ssh reach BLD-08's
//! `compiler_status` uses to read `.deployed_sha` markers), then AGGREGATES a
//! per-host outcome.
//!
//! ## Division of responsibility (do NOT reimplement swap safety here)
//! The compiler ONLY triggers. The updater (BLD-12) still owns the whole swap:
//! fetch → sha-verify → backup → atomic-mv → restart → HEALTH-GATE → ROLLBACK →
//! marker. `compiler_deploy` never touches a binary, a symlink, or a health
//! check; it fires the unit and reports what the updater reports.
//!
//! ## Per-host outcome (unreachable / rollback are REPORTED, never masked)
//!   - `deployed`    — the updater swapped to a new version, health-gate passed.
//!   - `skipped`     — a no-op: the host was already on `current` (unchanged).
//!   - `rolled_back` — the updater swapped, the health-gate FAILED, and it rolled
//!                     back to the backup. Surfaced distinctly, never as success.
//!   - `failed`      — the updater ran but errored (e.g. missing/corrupt artifact).
//!   - `unreachable` — an ssh-level failure (connect/auth/timeout). One bad host
//!                     never aborts the fan-out; the others still proceed and the
//!                     nightly timer catches the straggler.
//!
//! ## Discipline
//! - **S1** — every host, unit name, systemctl invocation, marker path, timeout,
//!   and concurrency bound comes from config env with a GENERIC default (the
//!   `constellation-update@{module}.service` unit name and `/opt/{module}/...`
//!   marker are conventions, exactly like BLD-08's `.deployed_sha` default), never
//!   an infra literal. The only values surfaced back to a caller are the small,
//!   fixed outcome vocabulary + systemd `Result` enum + `rc` — no host/path echo.
//! - **S7** — the trigger authenticates with the ambient ssh key of the sanctioned
//!   reach path (same as BLD-08); it reads NO token/key/password from the env, so
//!   there is nothing secret-shaped to route through `SecretManager` here.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use super::status::{configured_deploy_hosts, DeployHost};

/// Env: the systemd unit-name template to trigger, `{module}` (and, if present,
/// `{channel}`) substituted. Generic FHS-style convention, overridable.
const COMPILER_DEPLOY_UNIT_TEMPLATE: &str = "COMPILER_DEPLOY_UNIT_TEMPLATE";
/// Env: the `systemctl` invocation prefix used for BOTH the `start` and the
/// read-only `show` query. Inserted verbatim (operator-trusted config) so a
/// topology needing elevation can set e.g. `sudo systemctl` or `systemctl --user`.
const COMPILER_DEPLOY_SYSTEMCTL: &str = "COMPILER_DEPLOY_SYSTEMCTL";
/// Env: the updater's optional per-module OUTCOME-token file, `{module}`
/// substituted. When the updater (BLD-12) writes a token here (`deployed` /
/// `rolled_back` / `skipped` / `failed`) the compiler reads it back to classify
/// the outcome authoritatively; absent, it degrades to the systemd `Result`.
const COMPILER_DEPLOY_RESULT_MARKER_TEMPLATE: &str = "COMPILER_DEPLOY_RESULT_MARKER_TEMPLATE";
/// Env: ssh connect + trigger timeout seconds. The trigger runs the updater
/// SYNCHRONOUSLY (fetch + swap + health-gate), so this is much larger than the
/// BLD-08 marker-read timeout.
const COMPILER_DEPLOY_TRIGGER_TIMEOUT_SECS: &str = "COMPILER_DEPLOY_TRIGGER_TIMEOUT_SECS";
/// Env: max concurrent host triggers.
const COMPILER_DEPLOY_MAX_CONCURRENCY: &str = "COMPILER_DEPLOY_MAX_CONCURRENCY";
/// Env: auto-fire `compiler_deploy` after a successful `compiler_release` promote
/// that actually flipped `current`. Truthy (`1`/`true`/`yes`/`on`) → on.
pub const COMPILER_AUTO_DEPLOY: &str = "COMPILER_AUTO_DEPLOY";

/// A generic unit-name convention (not an infra identifier), overridable.
const DEFAULT_UNIT_TEMPLATE: &str = "constellation-update@{module}.service";
/// The read-only marker convention (mirrors BLD-08's `/opt/{module}/.deployed_sha`).
const DEFAULT_RESULT_MARKER_TEMPLATE: &str = "/opt/{module}/.deploy_result";
const DEFAULT_SYSTEMCTL: &str = "systemctl";
const DEFAULT_TRIGGER_TIMEOUT_SECS: u64 = 300;
const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// The sentinel line the remote wrapper prints so we can parse the outcome from a
/// deterministic, redaction-safe token line (never free-form updater log output).
const RESULT_SENTINEL: &str = "COMPILER_DEPLOY";

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_truthy(key: &str) -> bool {
    env_nonempty(key)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn unit_template() -> String {
    env_nonempty(COMPILER_DEPLOY_UNIT_TEMPLATE).unwrap_or_else(|| DEFAULT_UNIT_TEMPLATE.to_string())
}

fn result_marker_template() -> String {
    env_nonempty(COMPILER_DEPLOY_RESULT_MARKER_TEMPLATE)
        .unwrap_or_else(|| DEFAULT_RESULT_MARKER_TEMPLATE.to_string())
}

fn systemctl_cmd() -> String {
    env_nonempty(COMPILER_DEPLOY_SYSTEMCTL).unwrap_or_else(|| DEFAULT_SYSTEMCTL.to_string())
}

fn trigger_timeout() -> Duration {
    let secs = env_nonempty(COMPILER_DEPLOY_TRIGGER_TIMEOUT_SECS)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TRIGGER_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn max_concurrency() -> usize {
    env_nonempty(COMPILER_DEPLOY_MAX_CONCURRENCY)
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENCY)
}

/// Substitute `{module}`/`{channel}` in a template.
fn render_template(template: &str, module: &str, channel: &str) -> String {
    template
        .replace("{module}", module)
        .replace("{channel}", channel)
}

/// Single-quote-escape one argument for the remote shell (`'` → `'\''`). The unit
/// name + marker path are operator-config + module-derived; quoting keeps a value
/// with shell metacharacters inert (defence-in-depth on top of segment validation).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ── Per-host outcome ─────────────────────────────────────────────────────────

/// The tri-plus-state outcome of triggering one host's updater.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployOutcome {
    /// Updater swapped to a new version and the health-gate passed.
    Deployed,
    /// No-op: the host was already on `current`.
    Skipped,
    /// Updater swapped, health-gate FAILED, rolled back to backup (NOT masked).
    RolledBack,
    /// Updater ran but errored (missing/corrupt artifact, restart failure, …).
    Failed,
    /// ssh-level failure: connect/auth/timeout. Others still proceed.
    Unreachable,
}

impl DeployOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            DeployOutcome::Deployed => "deployed",
            DeployOutcome::Skipped => "skipped",
            DeployOutcome::RolledBack => "rolled_back",
            DeployOutcome::Failed => "failed",
            DeployOutcome::Unreachable => "unreachable",
        }
    }

    /// Whether this outcome leaves the fleet fully converged for this host. A
    /// rollback / failure / unreachable is a NON-converged straggler the nightly
    /// timer must still catch.
    fn is_converged(self) -> bool {
        matches!(self, DeployOutcome::Deployed | DeployOutcome::Skipped)
    }
}

/// One host's deploy-trigger result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostDeployResult {
    pub host: String,
    pub outcome: DeployOutcome,
    /// A SMALL, fixed-vocabulary detail (`rc=… result=… token=…`) — never raw
    /// updater output, so no infra literal (S1) or secret (S7) can leak through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The aggregate report across every triggered host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeployReport {
    pub module: String,
    pub channel: String,
    pub results: Vec<HostDeployResult>,
    pub notes: Vec<String>,
}

impl DeployReport {
    /// Per-outcome counts: `(deployed, skipped, rolled_back, failed, unreachable)`.
    fn counts(&self) -> (usize, usize, usize, usize, usize) {
        let mut c = (0, 0, 0, 0, 0);
        for r in &self.results {
            match r.outcome {
                DeployOutcome::Deployed => c.0 += 1,
                DeployOutcome::Skipped => c.1 += 1,
                DeployOutcome::RolledBack => c.2 += 1,
                DeployOutcome::Failed => c.3 += 1,
                DeployOutcome::Unreachable => c.4 += 1,
            }
        }
        c
    }

    /// The number of hosts NOT fully converged (rolled_back + failed + unreachable)
    /// — the stragglers the nightly timer remains the catch-all for.
    fn stragglers(&self) -> usize {
        self.results
            .iter()
            .filter(|r| !r.outcome.is_converged())
            .count()
    }

    /// True iff any host did not converge (a partial fleet result).
    fn degraded(&self) -> bool {
        self.stragglers() > 0
    }

    fn summary(&self) -> String {
        let (dep, skip, rb, fail, unreach) = self.counts();
        format!(
            "compiler_deploy {module}/{channel}: {n} host(s) — {dep} deployed, {skip} skipped, \
             {rb} rolled_back, {fail} failed, {unreach} unreachable{tail}",
            module = self.module,
            channel = self.channel,
            n = self.results.len(),
            tail = if self.degraded() {
                format!(" [{} straggler(s); nightly timer catches them]", self.stragglers())
            } else {
                String::new()
            },
        )
    }

    pub fn to_payload(&self) -> Value {
        let (dep, skip, rb, fail, unreach) = self.counts();
        json!({
            "module": self.module,
            "channel": self.channel,
            "results": self.results,
            "counts": {
                "deployed": dep,
                "skipped": skip,
                "rolled_back": rb,
                "failed": fail,
                "unreachable": unreach,
                "total": self.results.len(),
            },
            "degraded": self.degraded(),
            "stragglers": self.stragglers(),
            "notes": self.notes,
        })
    }
}

// ── Host selection (`hosts="all"` or a label/target filter) ──────────────────

/// Parse the `hosts` arg. `"all"`/empty → every configured host. Otherwise a
/// `,`/`;`-separated list of host LABELS (or ssh targets) to restrict to.
fn select_hosts(all: &[DeployHost], filter: &str) -> (Vec<DeployHost>, Vec<String>) {
    let f = filter.trim();
    if f.is_empty() || f.eq_ignore_ascii_case("all") {
        return (all.to_vec(), Vec::new());
    }
    let wanted: Vec<String> = f
        .split(|c| c == ',' || c == ';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let mut chosen = Vec::new();
    let mut notes = Vec::new();
    for w in &wanted {
        match all
            .iter()
            .find(|h| h.label == *w || h.ssh_target == *w)
        {
            Some(h) if !chosen.iter().any(|c: &DeployHost| c.label == h.label) => {
                chosen.push(h.clone())
            }
            Some(_) => {} // already chosen (dedup)
            None => notes.push(format!("requested host {w:?} is not a configured deploy target")),
        }
    }
    (chosen, notes)
}

// ── Remote trigger command + argv (pure, offline-testable) ───────────────────

/// Render the remote shell command that TRIGGERS the updater synchronously and
/// prints a deterministic outcome line. It:
///   1. runs `<systemctl> start <unit>` (a `Type=oneshot` updater unit blocks
///      until the whole fetch→swap→health→rollback→marker flow finishes), captures
///      its exit code,
///   2. reads the systemd `Result` and the updater's optional outcome-token file,
///   3. prints `COMPILER_DEPLOY rc=<rc> result=<result> token=<token>`,
///   4. ALWAYS `exit 0` — so ssh's OWN exit code reflects only CONNECTIVITY (a
///      non-zero ssh exit ⇒ unreachable, never merely a failed deploy). This is
///      the same tri-state trick BLD-08 uses for its marker read.
///
/// `systemctl` is inserted verbatim (operator-trusted config, may be `sudo
/// systemctl`); the unit + marker path are shell-quoted.
pub fn render_remote_trigger_cmd(systemctl: &str, unit: &str, result_marker: &str) -> String {
    let u = shell_quote(unit);
    let m = shell_quote(result_marker);
    format!(
        "{systemctl} start {u}; __rc=$?; \
         __res=$({systemctl} show {u} --property=Result --value 2>/dev/null); \
         __tok=$(cat -- {m} 2>/dev/null); \
         printf '{sentinel} rc=%s result=%s token=%s\\n' \"$__rc\" \"$__res\" \"$__tok\"; \
         exit 0",
        sentinel = RESULT_SENTINEL
    )
}

/// Render the full ssh argv over the EXISTING sanctioned host-reach path — the
/// same BatchMode / non-known_hosts-mutating options BLD-08 uses, so no new
/// credential path is introduced. `ConnectTimeout` bounds a dead host; the outer
/// wall-clock timeout (applied by the caller) bounds the synchronous updater run.
pub fn render_trigger_argv(ssh_target: &str, remote_cmd: &str, connect_timeout_secs: u64) -> Vec<String> {
    vec![
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={connect_timeout_secs}"),
        // Reuse BLD-08's non-mutating host-key posture (never write known_hosts).
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        ssh_target.to_string(),
        remote_cmd.to_string(),
    ]
}

// ── Outcome classification (pure) ────────────────────────────────────────────

/// Parse the `rc` / `result` / `token` fields out of the remote wrapper's
/// sentinel line. Tolerant: a missing field ⇒ `None`/empty.
fn parse_result_line(body: &str) -> (Option<i64>, String, String) {
    let line = body
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with(RESULT_SENTINEL))
        .unwrap_or("");
    let field = |key: &str| -> Option<String> {
        line.split_whitespace().find_map(|tok| {
            tok.strip_prefix(&format!("{key}=")).map(str::to_string)
        })
    };
    let rc = field("rc").and_then(|v| v.parse::<i64>().ok());
    let result = field("result").unwrap_or_default();
    let token = field("token").unwrap_or_default();
    (rc, result, token)
}

/// Classify a reachable host's outcome from `(rc, systemd Result, updater token)`.
///
/// The updater's own outcome TOKEN is authoritative when present (it is the only
/// signal that distinguishes a rollback or a no-op from a plain success); absent a
/// recognized token we degrade to the systemd `Result` + exit code:
///   - rc == 0 (or `Result=success`) ⇒ `deployed`,
///   - otherwise ⇒ `failed`.
pub fn classify_reachable(rc: Option<i64>, result: &str, token: &str) -> DeployOutcome {
    match token.trim().to_ascii_lowercase().as_str() {
        "rolled_back" | "rolledback" | "rollback" => DeployOutcome::RolledBack,
        "deployed" | "updated" | "swapped" | "success" => DeployOutcome::Deployed,
        "skipped" | "noop" | "no-op" | "unchanged" | "up-to-date" | "current" => {
            DeployOutcome::Skipped
        }
        "failed" | "error" | "abort" | "aborted" => DeployOutcome::Failed,
        // No recognized token → fall back to the systemd signal.
        _ => {
            if rc == Some(0) || result.eq_ignore_ascii_case("success") {
                DeployOutcome::Deployed
            } else {
                DeployOutcome::Failed
            }
        }
    }
}

/// Build the short, redaction-safe `detail` string (fixed vocabulary only).
fn detail_string(rc: Option<i64>, result: &str, token: &str) -> Option<String> {
    let rc = rc.map(|n| n.to_string()).unwrap_or_else(|| "?".to_string());
    let result = if result.is_empty() { "?" } else { result };
    let token = if token.is_empty() { "-" } else { token };
    Some(format!("rc={rc} result={result} token={token}"))
}

// ── Remote execution (the real trigger path) ─────────────────────────────────

/// Whether the ssh command connected (exit 0, given the remote always `exit 0`),
/// carrying its stdout; else it is unreachable.
enum SshOutcome {
    Reachable(String),
    Unreachable,
}

/// Spawn the ssh trigger argv, bounded by `timeout`. Never errors — an unreachable
/// host is a normal, expected result classified as `Unreachable`. Because the
/// remote command always `exit 0`s, a non-zero ssh exit can ONLY be a connectivity
/// failure.
async fn ssh_trigger(argv: &[String], timeout: Duration) -> SshOutcome {
    use tokio::io::AsyncReadExt;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.stdin(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else {
        return SshOutcome::Unreachable;
    };
    let mut pipe = child.stdout.take();
    let read = async move {
        let mut buf = Vec::new();
        if let Some(p) = pipe.as_mut() {
            let _ = p.read_to_end(&mut buf).await;
        }
        buf
    };
    let out = tokio::spawn(read);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        _ => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            out.abort();
            return SshOutcome::Unreachable;
        }
    };
    let bytes = out.await.unwrap_or_default();
    if status.success() {
        SshOutcome::Reachable(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        SshOutcome::Unreachable
    }
}

/// Trigger one host and classify. Never errors.
async fn trigger_one(
    host: DeployHost,
    module: String,
    channel: String,
    timeout: Duration,
) -> HostDeployResult {
    let unit = render_template(&unit_template(), &module, &channel);
    let marker = render_template(&result_marker_template(), &module, &channel);
    let remote = render_remote_trigger_cmd(&systemctl_cmd(), &unit, &marker);
    let argv = render_trigger_argv(&host.ssh_target, &remote, timeout.as_secs());
    match ssh_trigger(&argv, timeout).await {
        SshOutcome::Unreachable => HostDeployResult {
            host: host.label,
            outcome: DeployOutcome::Unreachable,
            detail: None,
        },
        SshOutcome::Reachable(body) => {
            let (rc, result, token) = parse_result_line(&body);
            HostDeployResult {
                host: host.label,
                outcome: classify_reachable(rc, &result, &token),
                detail: detail_string(rc, &result, &token),
            }
        }
    }
}

// ── Aggregation (generic over the trigger fn so it is mockable in tests) ──────

/// Fan out `trigger` across `hosts` with bounded concurrency and collect a
/// per-host result for EVERY host (never dropped) — so an unreachable/rolled-back
/// host is always surfaced, never masked by its peers. Generic over the trigger
/// closure so tests can inject deterministic outcomes without spawning ssh.
async fn aggregate<F, Fut>(
    hosts: Vec<DeployHost>,
    concurrency: usize,
    trigger: F,
) -> Vec<HostDeployResult>
where
    F: Fn(DeployHost) -> Fut,
    Fut: std::future::Future<Output = HostDeployResult>,
{
    use futures_util::stream::{FuturesUnordered, StreamExt};
    let mut pending = FuturesUnordered::new();
    let mut iter = hosts.into_iter();
    for _ in 0..concurrency.max(1) {
        if let Some(h) = iter.next() {
            pending.push(trigger(h));
        }
    }
    let mut out = Vec::new();
    while let Some(res) = pending.next().await {
        out.push(res);
        if let Some(h) = iter.next() {
            pending.push(trigger(h));
        }
    }
    // Stable order for deterministic output.
    out.sort_by(|a, b| a.host.cmp(&b.host));
    out
}

/// The core deploy fan-out: resolve hosts, trigger each updater, aggregate. Shared
/// by the `compiler_deploy` tool and the promote auto-trigger. Never errors — an
/// empty/unconfigured fleet is a NOTE, not a failure (the nightly timer remains
/// the catch-all), so it can never block a publish/promote pipeline.
pub async fn deploy_report(module: &str, channel: &str, hosts_filter: &str) -> DeployReport {
    let all = configured_deploy_hosts();
    let (chosen, mut notes) = select_hosts(&all, hosts_filter);

    if all.is_empty() {
        notes.push(
            "COMPILER_DEPLOY_HOSTS unset — no deploy targets; the nightly timer remains the \
             catch-all"
                .to_string(),
        );
    } else if chosen.is_empty() {
        notes.push(
            "no configured deploy host matched the requested `hosts` filter".to_string(),
        );
    }

    let timeout = trigger_timeout();
    let module_s = module.to_string();
    let channel_s = channel.to_string();
    let results = aggregate(chosen, max_concurrency(), |h| {
        trigger_one(h, module_s.clone(), channel_s.clone(), timeout)
    })
    .await;

    let mut report = DeployReport {
        module: module.to_string(),
        channel: channel.to_string(),
        results,
        notes,
    };
    if report.degraded() {
        report.notes.push(format!(
            "{} host(s) did not converge (rolled_back/failed/unreachable) — nightly timer catches them",
            report.stragglers()
        ));
    }
    report
}

/// Auto-trigger hook for `compiler_release` promote. When `COMPILER_AUTO_DEPLOY`
/// is truthy, fire a fleet-wide deploy and return its payload (for attaching to the
/// promote result); otherwise `None`. Best-effort: it never errors and never
/// affects the promote's own success.
pub async fn auto_trigger_after_promote(module: &str, channel: &str) -> Option<Value> {
    if !env_truthy(COMPILER_AUTO_DEPLOY) {
        return None;
    }
    let report = deploy_report(module, channel, "all").await;
    Some(report.to_payload())
}

// ── The tool ─────────────────────────────────────────────────────────────────

struct CompilerDeploy;

#[async_trait]
impl RustTool for CompilerDeploy {
    fn name(&self) -> &str {
        "compiler_deploy"
    }

    fn description(&self) -> &str {
        "Trigger the constellation-updater fleet-wide after a publish/promote so a change lands \
         in seconds (nightly timers remain the catch-all). Fires the fetch-mode \
         `constellation-update@<module>` unit on each configured deploy host over the existing \
         host-reach path and aggregates a per-host outcome (deployed | skipped | rolled_back | \
         failed | unreachable). The compiler ONLY triggers; the updater owns the swap safety \
         (health-gate + rollback). Unreachable and rolled-back hosts are reported, never masked."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "module": {
                    "type": "string",
                    "description": "Module/repo to deploy (e.g. terminus, chord, harmony, lumina-core)."
                },
                "channel": {
                    "type": "string",
                    "default": "stable",
                    "description": "Channel whose `current` the updater fetches (typically the promote target)."
                },
                "hosts": {
                    "type": "string",
                    "default": "all",
                    "description": "\"all\" (every configured deploy host) or a comma/semicolon-separated list of host labels to restrict to."
                }
            },
            "required": ["module"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let module = super::str_arg(&args, "module")?;
        super::validate_segment("module", &module)?;
        let channel = args
            .get("channel")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "stable".to_string());
        super::validate_segment("channel", &channel)?;
        let hosts_filter = args
            .get("hosts")
            .and_then(Value::as_str)
            .unwrap_or("all")
            .to_string();

        let report = deploy_report(&module, &channel, &hosts_filter).await;
        let text = report.summary();
        Ok(ToolOutput::with_structured(text, report.to_payload()))
    }
}

/// Register the `compiler_deploy` tool on the registry.
pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(CompilerDeploy)) {
        tracing::error!("compiler: failed to register compiler_deploy: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<DeployHost> {
        vec![
            DeployHost {
                label: "host-a".into(),
                ssh_target: "u@host-a".into(),
            },
            DeployHost {
                label: "host-b".into(),
                ssh_target: "u@host-b".into(),
            },
            DeployHost {
                label: "host-c".into(),
                ssh_target: "u@host-c".into(),
            },
        ]
    }

    // ── Host selection ──────────────────────────────────────────────────────

    #[test]
    fn select_hosts_all_returns_everything() {
        let (chosen, notes) = select_hosts(&hosts(), "all");
        assert_eq!(chosen.len(), 3);
        assert!(notes.is_empty());
        let (chosen, _) = select_hosts(&hosts(), "  ");
        assert_eq!(chosen.len(), 3, "empty filter == all");
    }

    #[test]
    fn select_hosts_filters_by_label_and_target_and_notes_unknown() {
        let (chosen, notes) = select_hosts(&hosts(), "host-a, u@host-b ; nope");
        assert_eq!(
            chosen.iter().map(|h| h.label.as_str()).collect::<Vec<_>>(),
            vec!["host-a", "host-b"]
        );
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("nope"));
    }

    #[test]
    fn select_hosts_dedups() {
        let (chosen, _) = select_hosts(&hosts(), "host-a, host-a, u@host-a");
        assert_eq!(chosen.len(), 1);
    }

    // ── Outcome classification ──────────────────────────────────────────────

    #[test]
    fn classify_token_is_authoritative_including_rollback() {
        // A ROLLBACK is reported distinctly, even though rc/result look clean.
        assert_eq!(
            classify_reachable(Some(0), "success", "rolled_back"),
            DeployOutcome::RolledBack
        );
        assert_eq!(
            classify_reachable(Some(0), "success", "skipped"),
            DeployOutcome::Skipped
        );
        assert_eq!(
            classify_reachable(Some(0), "success", "deployed"),
            DeployOutcome::Deployed
        );
        assert_eq!(
            classify_reachable(Some(0), "success", "failed"),
            DeployOutcome::Failed
        );
    }

    #[test]
    fn classify_falls_back_to_systemd_signal_without_token() {
        assert_eq!(
            classify_reachable(Some(0), "success", ""),
            DeployOutcome::Deployed
        );
        assert_eq!(
            classify_reachable(Some(1), "failed", ""),
            DeployOutcome::Failed
        );
        // rc unknown but Result=success still deploys.
        assert_eq!(
            classify_reachable(None, "success", ""),
            DeployOutcome::Deployed
        );
        // rc unknown and no success signal → failed (fail-visible, not masked).
        assert_eq!(classify_reachable(None, "", ""), DeployOutcome::Failed);
    }

    #[test]
    fn parse_result_line_extracts_fields() {
        let body = "some updater chatter\nCOMPILER_DEPLOY rc=0 result=success token=rolled_back\n";
        let (rc, result, token) = parse_result_line(body);
        assert_eq!(rc, Some(0));
        assert_eq!(result, "success");
        assert_eq!(token, "rolled_back");
    }

    #[test]
    fn parse_result_line_tolerates_missing_fields_and_sentinel() {
        let (rc, result, token) = parse_result_line("no sentinel here");
        assert_eq!(rc, None);
        assert!(result.is_empty() && token.is_empty());
        // Empty token renders as `-` (a no-value marker), rc `?`.
        let (rc, result, token) = parse_result_line("COMPILER_DEPLOY rc=2 result=failed token=");
        assert_eq!(detail_string(rc, &result, &token).unwrap(), "rc=2 result=failed token=-");
    }

    // ── Remote command / argv shape (S1: no infra literals) ─────────────────

    #[test]
    fn remote_cmd_triggers_start_reads_result_and_always_exits_zero() {
        let cmd = render_remote_trigger_cmd(
            "systemctl",
            "<email>",
            "<path>/.deploy_result",
        );
        assert!(cmd.contains("systemctl start '<email>'"));
        assert!(cmd.contains("--property=Result --value"));
        assert!(cmd.contains("cat -- '<path>/.deploy_result'"));
        assert!(cmd.contains("COMPILER_DEPLOY rc="));
        // Always exit 0 so ssh's exit reflects only connectivity (tri-state trick).
        assert!(cmd.trim_end().ends_with("exit 0"));
    }

    #[test]
    fn remote_cmd_honors_a_privileged_systemctl_prefix() {
        let cmd = render_remote_trigger_cmd("sudo systemctl", "<email>", "/m");
        assert!(cmd.starts_with("sudo systemctl start "));
        assert!(cmd.contains("sudo systemctl show '<email>'"));
    }

    #[test]
    fn trigger_argv_uses_the_existing_nonmutating_reach_path() {
        let argv = render_trigger_argv("u@host", "echo hi", 300);
        assert_eq!(argv[0], "ssh");
        assert!(argv.iter().any(|a| a == "BatchMode=yes"));
        assert!(argv.iter().any(|a| a == "ConnectTimeout=300"));
        // Same non-mutating host-key posture as BLD-08's read path.
        assert!(!argv.iter().any(|a| a.contains("accept-new")));
        assert!(argv.iter().any(|a| a == "StrictHostKeyChecking=no"));
        assert!(argv.iter().any(|a| a == "UserKnownHostsFile=/dev/null"));
        assert!(argv.iter().any(|a| a == "u@host"));
        assert_eq!(argv.last().unwrap(), "echo hi");
    }

    #[test]
    fn render_template_substitutes_module_and_channel() {
        assert_eq!(
            render_template("constellation-update@{module}.service", "chord", "stable"),
            "<email>"
        );
        assert_eq!(
            render_template("/deploy/{module}/{channel}.tok", "harmony", "experimental"),
            "/deploy/harmony/experimental.tok"
        );
    }

    #[test]
    fn shell_quote_neutralizes_metacharacters() {
        assert_eq!(shell_quote("<email>"), "'<email>'");
        assert_eq!(shell_quote("a'b; rm -rf /"), "'a'\\''b; rm -rf /'");
    }

    // ── Aggregation (mock the trigger — no ssh) ─────────────────────────────

    /// A canned trigger that maps host label → outcome, for offline aggregation
    /// tests (mocks the reach/trigger entirely — no ssh).
    async fn canned(map: std::collections::HashMap<&'static str, DeployOutcome>) -> DeployReport {
        let hosts = hosts();
        let results = aggregate(hosts, 4, move |h| {
            let outcome = *map.get(h.label.as_str()).unwrap_or(&DeployOutcome::Failed);
            async move {
                HostDeployResult {
                    host: h.label,
                    outcome,
                    detail: Some(format!("mock={}", outcome.as_str())),
                }
            }
        })
        .await;
        DeployReport {
            module: "chord".into(),
            channel: "stable".into(),
            results,
            notes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn aggregate_reports_every_host_and_counts_per_outcome() {
        let map = std::collections::HashMap::from([
            ("host-a", DeployOutcome::Deployed),
            ("host-b", DeployOutcome::Skipped),
            ("host-c", DeployOutcome::RolledBack),
        ]);
        let report = canned(map).await;
        assert_eq!(report.results.len(), 3, "no host dropped");
        let (dep, skip, rb, fail, unreach) = report.counts();
        assert_eq!((dep, skip, rb, fail, unreach), (1, 1, 1, 0, 0));
        // The rollback is surfaced distinctly, not masked as success.
        let c = report.results.iter().find(|r| r.host == "host-c").unwrap();
        assert_eq!(c.outcome, DeployOutcome::RolledBack);
    }

    #[tokio::test]
    async fn unreachable_dest_is_reported_while_others_proceed() {
        let map = std::collections::HashMap::from([
            ("host-a", DeployOutcome::Deployed),
            ("host-b", DeployOutcome::Unreachable),
            ("host-c", DeployOutcome::Deployed),
        ]);
        let report = canned(map).await;
        // The unreachable host did NOT abort the fan-out: the other two deployed.
        let (dep, _skip, _rb, _fail, unreach) = report.counts();
        assert_eq!(dep, 2);
        assert_eq!(unreach, 1);
        let b = report.results.iter().find(|r| r.host == "host-b").unwrap();
        assert_eq!(b.outcome, DeployOutcome::Unreachable);
    }

    #[tokio::test]
    async fn partial_success_is_surfaced_as_degraded_with_stragglers() {
        let map = std::collections::HashMap::from([
            ("host-a", DeployOutcome::Deployed),
            ("host-b", DeployOutcome::Unreachable),
            ("host-c", DeployOutcome::RolledBack),
        ]);
        let report = canned(map).await;
        assert!(report.degraded(), "a partial fleet result is degraded");
        assert_eq!(report.stragglers(), 2, "unreachable + rolled_back are stragglers");
        let payload = report.to_payload();
        assert_eq!(payload["degraded"], json!(true));
        assert_eq!(payload["stragglers"], json!(2));
        assert_eq!(payload["counts"]["deployed"], json!(1));
        assert_eq!(payload["counts"]["rolled_back"], json!(1));
        assert_eq!(payload["counts"]["unreachable"], json!(1));
        // The summary names the straggler catch-all (nightly timer).
        assert!(report.summary().contains("straggler"));
    }

    #[tokio::test]
    async fn all_converged_is_not_degraded() {
        let map = std::collections::HashMap::from([
            ("host-a", DeployOutcome::Deployed),
            ("host-b", DeployOutcome::Skipped),
            ("host-c", DeployOutcome::Deployed),
        ]);
        let report = canned(map).await;
        assert!(!report.degraded());
        assert_eq!(report.stragglers(), 0);
    }

    #[test]
    fn payload_shape_is_stable() {
        let report = DeployReport {
            module: "chord".into(),
            channel: "stable".into(),
            results: vec![HostDeployResult {
                host: "host-a".into(),
                outcome: DeployOutcome::Deployed,
                detail: Some("rc=0 result=success token=deployed".into()),
            }],
            notes: vec!["n".into()],
        };
        let p = report.to_payload();
        assert_eq!(p["module"], json!("chord"));
        assert_eq!(p["channel"], json!("stable"));
        assert_eq!(p["results"][0]["host"], json!("host-a"));
        assert_eq!(p["results"][0]["outcome"], json!("deployed"));
        assert_eq!(p["counts"]["total"], json!(1));
        assert_eq!(p["degraded"], json!(false));
        assert!(p["notes"].is_array());
    }
}
