//! BLD-13 — `compiler_deploy`: trigger-on-publish, fleet-wide.
//!
//! After a successful publish/promote (the store's `current` sha moves), the
//! change should land ON THE FLEET in seconds instead of waiting for the nightly
//! constellation-updater timer. `compiler_deploy(module, channel, hosts="all")`
//! TRIGGERS the already-deployed `constellation-update@<module>` systemd unit —
//! in its BLD-12 fetch mode — on each configured deploy host over the EXISTING
//! sanctioned host-reach path (the SINGLE shared `status::sanctioned_ssh_argv`
//! BLD-08's `compiler_status` uses to read `.deployed_sha` markers — deploy defines
//! no ssh option set of its own), then AGGREGATES a per-host outcome.
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
//!                     back to the backup. Surfaced distinctly, never as success. A
//!                     trusted `rolled_back` marker is AUTHORITATIVE over the exit
//!                     code (a rollback legitimately exits non-zero), so it is never
//!                     masked into a generic `failed`.
//!   - `failed`      — the updater ran but errored (e.g. missing/corrupt artifact) —
//!                     a trusted `failed` marker, OR (with NO trusted marker) a
//!                     non-zero `systemctl start` rc / a non-success systemd `Result`.
//!                     The rc gate applies only to SUCCESS outcomes + the no-marker
//!                     path — it never overrides a trusted non-success marker.
//!   - `timed_out`   — the host was REACHED and the updater triggered, but the
//!                     synchronous run exceeded the trigger budget: an in-flight/
//!                     hung deploy of unknown outcome, surfaced DISTINCTLY from
//!                     `unreachable` (a slow deploy is not a connectivity failure).
//!   - `unknown`     — the outcome cannot be trusted as a success: the updater wrote
//!                     a token the compiler does not recognize, OR the wrapper's exit
//!                     code could not be parsed (a stale/damaged sentinel that still
//!                     says `result=success` is NOT trusted without a real `rc == 0`).
//!                     Non-converged; the raw token is never surfaced.
//!   - `unreachable` — an ssh-level CONNECT/AUTH failure (never a run timeout). One
//!                     bad host never aborts the fan-out; the others still proceed
//!                     and the nightly timer catches the straggler.
//!
//! ## Discipline
//! - **S1 (no raw echo)** — every host, unit name, systemctl invocation, marker
//!   path, timeout, and concurrency bound comes from config env with a GENERIC
//!   default (the `constellation-update@{module}.service` unit and `/opt/{module}/…`
//!   marker are conventions, like BLD-08's `.deployed_sha`), never an infra literal.
//!   The ONLY thing surfaced back to a caller is the fixed outcome vocabulary + an
//!   integer `rc` — the raw updater marker token, free-form `Result` text, and any
//!   caller-supplied requested-host string are NEVER echoed (an unknown host is a
//!   count; a bad marker classifies to `unknown`; `detail` is `outcome=… rc=…`).
//!   `COMPILER_DEPLOY_SYSTEMCTL` is a CONSTRAINED command (bare tokens invoking
//!   `systemctl`), not arbitrary shell — metacharacters are rejected with a config
//!   error, so nothing unsanitized reaches the remote shell.
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

use super::status::{configured_deploy_hosts, sanctioned_ssh_argv, DeployHost};

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
/// Env: the RUN budget (seconds) for the SYNCHRONOUS updater once connected (fetch
/// + swap + health-gate), so it is much larger than the BLD-08 marker-read timeout.
/// The OUTER wall-clock timeout is this PLUS the connect budget (see below), so it
/// is always strictly greater than the ssh `ConnectTimeout` — a connect/auth hang
/// surfaces as ssh's OWN non-zero exit (→ `unreachable`) before the outer timer
/// could fire, so `timed_out` only ever means "connected, updater ran too long."
const COMPILER_DEPLOY_TRIGGER_TIMEOUT_SECS: &str = "COMPILER_DEPLOY_TRIGGER_TIMEOUT_SECS";
/// Env: the ssh CONNECT budget (seconds) — passed as ssh `ConnectTimeout`. Kept
/// small; a dead/hung host fails connect within this and is reported `unreachable`.
const COMPILER_DEPLOY_CONNECT_TIMEOUT_SECS: &str = "COMPILER_DEPLOY_CONNECT_TIMEOUT_SECS";
/// Env: max concurrent host triggers.
const COMPILER_DEPLOY_MAX_CONCURRENCY: &str = "COMPILER_DEPLOY_MAX_CONCURRENCY";
/// Env: auto-fire `compiler_deploy` after a successful `compiler_release` promote
/// that actually flipped `current`. Truthy (`1`/`true`/`yes`/`on`) → on.
pub const COMPILER_AUTO_DEPLOY: &str = "COMPILER_AUTO_DEPLOY";
/// Env: the SMALL best-effort budget (seconds) the auto-after-promote deploy may run
/// INLINE before the promote returns. If the fleet fan-out finishes within it, the
/// per-host report is attached to the promote result; otherwise the deploy continues
/// DETACHED and the promote returns promptly (never held hostage by a long fleet
/// deploy). Only affects the AUTO path — the manual `compiler_deploy` tool stays fully
/// synchronous.
const COMPILER_AUTO_DEPLOY_INLINE_BUDGET_SECS: &str = "COMPILER_AUTO_DEPLOY_INLINE_BUDGET_SECS";

/// A generic unit-name convention (not an infra identifier), overridable.
const DEFAULT_UNIT_TEMPLATE: &str = "constellation-update@{module}.service";
/// The read-only marker convention (mirrors BLD-08's `/opt/{module}/.deployed_sha`).
const DEFAULT_RESULT_MARKER_TEMPLATE: &str = "/opt/{module}/.deploy_result";
const DEFAULT_SYSTEMCTL: &str = "systemctl";
const DEFAULT_TRIGGER_TIMEOUT_SECS: u64 = 300;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_MAX_CONCURRENCY: usize = 4;
/// Default inline budget for the auto-after-promote deploy (seconds) — short, so the
/// promote is not delayed; the deploy continues detached past it.
const DEFAULT_AUTO_DEPLOY_INLINE_BUDGET_SECS: u64 = 10;
/// HARD CEILING on concurrent host triggers, regardless of config — a malformed/huge
/// `COMPILER_DEPLOY_MAX_CONCURRENCY` can never spawn an absurd number of workers.
const MAX_CONCURRENCY_CEILING: usize = 64;
/// Clamp for any parsed `*_TIMEOUT_SECS` (6 hours) — a huge-but-parse-valid value is
/// clamped rather than risking Duration-arithmetic overflow. Well above any real
/// synchronous updater run.
const MAX_TIMEOUT_SECS: u64 = 6 * 60 * 60;

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

/// A single token of the systemctl command is SAFE iff it is a bare word of
/// `[A-Za-z0-9._/-]` (a binary name / absolute path / a `-n`-style flag) — NO shell
/// metacharacters, whitespace-within, or quoting. This is what makes it safe to
/// insert verbatim into the remote shell command.
fn is_safe_systemctl_token(t: &str) -> bool {
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// Whether a token is `systemctl` or a `.../systemctl` absolute/relative path — the
/// only permitted EXECUTABLE for the deploy trigger.
fn is_systemctl_exe(t: &str) -> bool {
    t == "systemctl" || t.ends_with("/systemctl")
}

/// Whether a token is an ACCEPTED `sudo` option — ONLY the non-interactive flag, in
/// its short/long/bundled forms (`-n`, `--non-interactive`, or a bundled short group
/// that is nothing but `n`s like `-nn`). Every OTHER sudo flag is rejected — crucially
/// the ARGUMENT-TAKING ones (`-u user`, `-g group`, `-h host`, `-p prompt`, `-C fd`,
/// `-r role`, `-T timeout`, `-U user`, …), whose argument the naive "skip all dash
/// flags" parser would mistake for the executable (`sudo -u systemctl` → sudo reads
/// `systemctl` as the USERNAME). Only `-n` is ever needed here.
fn is_accepted_sudo_flag(t: &str) -> bool {
    t == "-n"
        || t == "--non-interactive"
        // A bundled short group of only `n` (e.g. `-nn`); `-n` takes no argument, so
        // bundling is unambiguous and can never swallow the executable.
        || (t.starts_with('-')
            && !t.starts_with("--")
            && t.len() > 1
            && t[1..].chars().all(|c| c == 'n'))
}

/// Validate `COMPILER_DEPLOY_SYSTEMCTL`: it is a CONSTRAINED systemctl invocation,
/// **not** arbitrary operator shell. It must be a whitespace-separated token list of
/// safe bare words (`[A-Za-z0-9._/-]`, no shell metacharacters/control chars). The
/// permitted grammar is TIGHT (findings 1 & 3): an optional leading `sudo`, then
/// optionally ONLY the non-interactive flag `-n` (no other sudo option — especially
/// no argument-taking flag like `-u`/`-g`/`-h`/`-p` that could make sudo read the
/// next token as a username/host and treat `systemctl` as data), then the EXECUTABLE
/// which MUST be `systemctl` (or a `.../systemctl` path), then — CRUCIALLY — NOTHING
/// after it: the override is EXECUTABLE-PREFIX ONLY (no verb AND no flag). The deploy
/// trigger must be SYNCHRONOUS — the wrapper supplies `start <unit>` and blocks until
/// the BLD-12 unit finishes so it can classify this run's authoritative outcome — so
/// no trailing flag that changes blocking/result semantics is permitted (notably
/// `--no-block`, which would make `systemctl start` return before the updater writes
/// its marker / completes rollback/health-gate). So EXACTLY `systemctl`, `sudo
/// systemctl`, `sudo -n systemctl`, `/usr/bin/systemctl`, `sudo -n /usr/bin/systemctl`
/// are accepted; `sudo -u systemctl`, `reboot systemctl`, `systemctl reboot`,
/// `systemctl --no-block`, `systemctl -q`, `systemctl --user` are all REJECTED. Any
/// metacharacter, a disallowed sudo flag, a non-systemctl executable, or ANY trailing
/// token is a clear config error (never inserted into the remote shell). Empty/unset ⇒
/// the default `systemctl`. The error message NEVER echoes the raw value back (S1).
fn validate_systemctl_cmd(raw: &str) -> Result<String, ToolError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(DEFAULT_SYSTEMCTL.to_string());
    }
    // Reject any control character (incl. embedded newline/tab/CR, which
    // `split_whitespace` would otherwise silently swallow) or shell metacharacter
    // outright, BEFORE tokenizing — defence in depth over the per-token allowlist.
    const META: &[char] = &[
        ';', '|', '&', '$', '>', '<', '`', '\\', '(', ')', '{', '}', '[', ']', '*', '?', '!',
        '~', '#', '"', '\'', '=', ':',
    ];
    if raw.chars().any(|c| c.is_control() || META.contains(&c)) {
        return Err(ToolError::InvalidArgument(format!(
            "{COMPILER_DEPLOY_SYSTEMCTL} must be a constrained systemctl command \
             (no control characters or shell metacharacters); rejected"
        )));
    }
    let toks: Vec<&str> = raw.split_whitespace().collect();
    if !toks.iter().all(|t| is_safe_systemctl_token(t)) {
        return Err(ToolError::InvalidArgument(format!(
            "{COMPILER_DEPLOY_SYSTEMCTL} must be a constrained systemctl command \
             (bare tokens of [A-Za-z0-9._/-] only — no shell metacharacters); rejected"
        )));
    }
    // Locate the EXECUTABLE under the TIGHT grammar: optional `sudo`, then ONLY `-n`
    // (never any other/argument-taking sudo flag), then `systemctl`.
    let mut i = 0;
    if toks.get(i) == Some(&"sudo") {
        i += 1;
        while matches!(toks.get(i), Some(t) if t.starts_with('-')) {
            if !is_accepted_sudo_flag(toks[i]) {
                return Err(ToolError::InvalidArgument(format!(
                    "{COMPILER_DEPLOY_SYSTEMCTL}: the only sudo option permitted before \
                     `systemctl` is `-n` (no other/argument-taking flag); rejected"
                )));
            }
            i += 1;
        }
    }
    match toks.get(i) {
        Some(exe) if is_systemctl_exe(exe) => {}
        _ => {
            return Err(ToolError::InvalidArgument(format!(
                "{COMPILER_DEPLOY_SYSTEMCTL} executable must be `systemctl` (optionally via a \
                 leading `sudo [-n]`); e.g. `systemctl` or `sudo -n systemctl`; rejected"
            )));
        }
    }
    // EXECUTABLE-PREFIX ONLY: reject ALL trailing tokens after `systemctl` — no verbs
    // AND no flags. The deploy trigger must be SYNCHRONOUS: the wrapper itself supplies
    // `start <unit>` and blocks until the BLD-12 unit finishes
    // (fetch→swap→health→rollback→marker) so it can classify THIS run's authoritative
    // per-host outcome. A trailing flag that changes blocking/result semantics —
    // notably `--no-block` (`systemctl start` returns BEFORE the updater writes its
    // marker or completes rollback/health-gate) — would break that contract, so no
    // trailing token is permitted at all. The accepted set is exactly `[sudo [-n]]
    // systemctl` (`systemctl`, `/usr/bin/systemctl`, `sudo systemctl`, `sudo -n
    // systemctl`, `sudo -n /usr/bin/systemctl`).
    if i + 1 < toks.len() {
        return Err(ToolError::InvalidArgument(format!(
            "{COMPILER_DEPLOY_SYSTEMCTL} must be exactly `[sudo [-n]] systemctl` with NO trailing \
             token (no verb and no flag — the trigger supplies `start <unit>` and must stay \
             synchronous); rejected"
        )));
    }
    Ok(raw.to_string())
}

/// The raw configured systemctl command (unvalidated) — validated by
/// [`validate_systemctl_cmd`] before use.
fn systemctl_env_raw() -> String {
    env_nonempty(COMPILER_DEPLOY_SYSTEMCTL).unwrap_or_default()
}

/// Parse a `*_TIMEOUT_SECS` env value, CLAMPED to `[1, MAX_TIMEOUT_SECS]` so a huge
/// (but parse-valid) value can never overflow Duration arithmetic downstream; a
/// `0`/unparseable/absent value falls back to `default`. Pure over `raw` for testing.
fn parse_timeout_secs(raw: Option<String>, default: u64) -> Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
        .min(MAX_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// The RUN budget for the synchronous updater (post-connect), clamped.
fn trigger_timeout() -> Duration {
    parse_timeout_secs(
        env_nonempty(COMPILER_DEPLOY_TRIGGER_TIMEOUT_SECS),
        DEFAULT_TRIGGER_TIMEOUT_SECS,
    )
}

/// The ssh CONNECT budget (passed as `ConnectTimeout`), clamped.
fn connect_timeout() -> Duration {
    parse_timeout_secs(
        env_nonempty(COMPILER_DEPLOY_CONNECT_TIMEOUT_SECS),
        DEFAULT_CONNECT_TIMEOUT_SECS,
    )
}

/// The OUTER wall-clock timeout for the whole trigger: STRICTLY GREATER than the ssh
/// `ConnectTimeout` (it is connect + run, +1s of slack). This headroom guarantees a
/// connect/auth hang is surfaced as ssh's own non-zero exit (→ `unreachable`) BEFORE
/// the outer timer fires — so `timed_out` can only mean "connected, updater ran too
/// long," never "couldn't connect." Uses SATURATING addition so no combination of
/// inputs can overflow/panic (both inputs are already clamped by `parse_timeout_secs`,
/// so the saturation is a belt-and-suspenders guard). Pure over its inputs for testing.
fn outer_timeout(connect: Duration, run: Duration) -> Duration {
    connect
        .saturating_add(run)
        .saturating_add(Duration::from_secs(1))
}

/// The configured max concurrent host triggers (`0`/unparseable/absent → the default),
/// itself capped at `MAX_CONCURRENCY_CEILING`. The EFFECTIVE worker count is further
/// bounded to the number of selected hosts by [`aggregate`] — see [`effective_concurrency`].
fn max_concurrency() -> usize {
    env_nonempty(COMPILER_DEPLOY_MAX_CONCURRENCY)
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENCY)
        .min(MAX_CONCURRENCY_CEILING)
}

/// The EFFECTIVE number of concurrent workers: `0` for an EMPTY host list (spawn
/// nothing), otherwise `min(configured, host_count, ceiling)` floored at 1. This is
/// what stops a huge/malformed `COMPILER_DEPLOY_MAX_CONCURRENCY` from spawning an
/// absurd number of workers (never more than there are hosts, never above the
/// ceiling). Pure for unit-testing.
fn effective_concurrency(configured: usize, host_count: usize) -> usize {
    if host_count == 0 {
        return 0;
    }
    configured
        .min(host_count)
        .min(MAX_CONCURRENCY_CEILING)
        .max(1)
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
    /// The host was REACHED and the updater was triggered, but the synchronous
    /// run exceeded `COMPILER_DEPLOY_TRIGGER_TIMEOUT_SECS` — the deploy is
    /// in-flight/hung/unknown, NOT a connectivity failure. Surfaced distinctly so a
    /// slow/stuck deploy is never masked as `unreachable`.
    TimedOut,
    /// The updater wrote an outcome token the compiler does NOT recognize (not in
    /// the fixed vocabulary). A non-converged, must-not-trust outcome — the raw
    /// token is NEVER surfaced (S1/S7), only this classification.
    Unknown,
    /// ssh-level CONNECT/AUTH failure (never a timeout). Others still proceed.
    Unreachable,
}

impl DeployOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            DeployOutcome::Deployed => "deployed",
            DeployOutcome::Skipped => "skipped",
            DeployOutcome::RolledBack => "rolled_back",
            DeployOutcome::Failed => "failed",
            DeployOutcome::TimedOut => "timed_out",
            DeployOutcome::Unknown => "unknown",
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

/// Per-outcome tallies across a fleet fan-out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    deployed: usize,
    skipped: usize,
    rolled_back: usize,
    failed: usize,
    timed_out: usize,
    unknown: usize,
    unreachable: usize,
}

impl DeployReport {
    /// Per-outcome counts.
    fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for r in &self.results {
            match r.outcome {
                DeployOutcome::Deployed => c.deployed += 1,
                DeployOutcome::Skipped => c.skipped += 1,
                DeployOutcome::RolledBack => c.rolled_back += 1,
                DeployOutcome::Failed => c.failed += 1,
                DeployOutcome::TimedOut => c.timed_out += 1,
                DeployOutcome::Unknown => c.unknown += 1,
                DeployOutcome::Unreachable => c.unreachable += 1,
            }
        }
        c
    }

    /// The number of hosts NOT fully converged (rolled_back + failed + timed_out +
    /// unreachable) — the stragglers the nightly timer remains the catch-all for.
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
        let c = self.counts();
        format!(
            "compiler_deploy {module}/{channel}: {n} host(s) — {dep} deployed, {skip} skipped, \
             {rb} rolled_back, {fail} failed, {to} timed_out, {unk} unknown, {unreach} unreachable{tail}",
            module = self.module,
            channel = self.channel,
            n = self.results.len(),
            dep = c.deployed,
            skip = c.skipped,
            rb = c.rolled_back,
            fail = c.failed,
            to = c.timed_out,
            unk = c.unknown,
            unreach = c.unreachable,
            tail = if self.degraded() {
                format!(" [{} straggler(s); nightly timer catches them]", self.stragglers())
            } else {
                String::new()
            },
        )
    }

    pub fn to_payload(&self) -> Value {
        let c = self.counts();
        json!({
            "module": self.module,
            "channel": self.channel,
            "results": self.results,
            "counts": {
                "deployed": c.deployed,
                "skipped": c.skipped,
                "rolled_back": c.rolled_back,
                "failed": c.failed,
                "timed_out": c.timed_out,
                "unknown": c.unknown,
                "unreachable": c.unreachable,
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
    let mut unmatched = 0usize;
    for w in &wanted {
        match all
            .iter()
            .find(|h| h.label == *w || h.ssh_target == *w)
        {
            Some(h) if !chosen.iter().any(|c: &DeployHost| c.label == h.label) => {
                chosen.push(h.clone())
            }
            Some(_) => {} // already chosen (dedup)
            // Finding 3: NEVER echo the raw requested-host string back (it can carry
            // ssh targets / arbitrary caller input / infra literals). Count only.
            None => unmatched += 1,
        }
    }
    let mut notes = Vec::new();
    if unmatched > 0 {
        notes.push(format!(
            "{unmatched} requested host(s) not in the configured deploy set (ignored)"
        ));
    }
    (chosen, notes)
}

// ── Remote trigger command + argv (pure, offline-testable) ───────────────────

/// Normalize a configured `systemctl` prefix to be NON-INTERACTIVE (finding 2):
/// when it uses `sudo`, inject `-n` (`--non-interactive`) so sudo NEVER blocks on
/// a password prompt — `BatchMode=yes` bounds ssh's own auth but NOT sudo's, so a
/// sudo needing a password would otherwise hang for the entire per-host trigger
/// timeout. With `-n`, a sudo that would prompt instead fails IMMEDIATELY (non-zero
/// rc → a `failed` outcome), so a missing/expired sudo credential is a fast,
/// visible config/permission failure, never a hang. A prefix without `sudo` (the
/// default bare `systemctl`) is returned unchanged; an already-`-n` prefix is left
/// as-is (idempotent).
fn ensure_non_interactive_sudo(prefix: &str) -> String {
    let toks: Vec<&str> = prefix.split_whitespace().collect();
    if !toks.iter().any(|t| *t == "sudo") {
        return toks.join(" ");
    }
    let already_non_interactive = toks
        .iter()
        .any(|t| *t == "-n" || *t == "--non-interactive");
    let mut out: Vec<String> = Vec::with_capacity(toks.len() + 1);
    let mut injected = false;
    for t in &toks {
        out.push((*t).to_string());
        if *t == "sudo" && !already_non_interactive && !injected {
            out.push("-n".to_string());
            injected = true;
        }
    }
    out.join(" ")
}

/// Render the remote shell command that TRIGGERS the updater synchronously and
/// prints a deterministic outcome line. It:
///   1. `rm -f`s any prior marker, then CAPTURES whether the marker is now provably
///      ABSENT (`__cleared=1` iff `[ -e marker ]` is false after the rm). This is the
///      TRULY RUN-SCOPED gate (finding 3): it does NOT depend on `rm`'s exit code or
///      any second-granularity mtime — either the old marker is gone (so any marker
///      present afterward was written by THIS run) or it survived (a root-owned marker
///      we could not remove) in which case we do NOT trust it at all,
///   2. runs `<systemctl> start <unit>` (a `Type=oneshot` updater unit blocks until
///      the whole fetch→swap→health→rollback→marker flow finishes), captures its
///      exit code — with `sudo` forced non-interactive (`-n`) so it fails fast
///      instead of hanging on a password prompt,
///   3. reads the systemd `Result`, then reads the outcome-token file ONLY IF the
///      pre-trigger clear SUCCEEDED (`__cleared=1`). The token is SANITIZED against
///      sentinel spoofing (finding 2): `head -n1` takes only the first line and
///      `tr -cd 'A-Za-z0-9_-'` strips it to a safe charset — so a malformed marker
///      containing a newline + a forged `COMPILER_DEPLOY … token=deployed` line can
///      neither inject a second sentinel line nor smuggle spaces/metacharacters,
///   4. prints `COMPILER_DEPLOY rc=<rc> result=<result> token=<token>` (exactly one
///      sentinel line — the token can never contain a newline),
///   5. ALWAYS `exit 0` — so ssh's OWN exit code reflects only CONNECTIVITY (a
///      non-zero ssh exit ⇒ unreachable, never merely a failed deploy). This is
///      the same tri-state trick BLD-08 uses for its marker read.
///
/// `systemctl` is inserted verbatim after non-interactive normalization
/// (operator-trusted config, may be `sudo systemctl`); the unit + marker path are
/// shell-quoted. No `${…}` brace-expansion is used (it would collide with `format!`).
pub fn render_remote_trigger_cmd(systemctl: &str, unit: &str, result_marker: &str) -> String {
    let systemctl = ensure_non_interactive_sudo(systemctl);
    let u = shell_quote(unit);
    let m = shell_quote(result_marker);
    format!(
        "rm -f -- {m} 2>/dev/null; \
         if [ -e {m} ]; then __cleared=0; else __cleared=1; fi; \
         {systemctl} start {u}; __rc=$?; \
         __res=$({systemctl} show {u} --property=Result --value 2>/dev/null); \
         if [ \"$__cleared\" = 1 ]; then __tok=$(head -n1 -- {m} 2>/dev/null | tr -cd 'A-Za-z0-9_-'); else __tok=; fi; \
         printf '{sentinel} rc=%s result=%s token=%s\\n' \"$__rc\" \"$__res\" \"$__tok\"; \
         exit 0",
        sentinel = RESULT_SENTINEL
    )
}

// The ssh reach is NOT defined here: the trigger fans out over the SINGLE shared
// `status::sanctioned_ssh_argv` (BLD-08), so the sanctioned option set has exactly
// one authoritative definition and the deploy trigger can never drift from it.

// ── Outcome classification (pure) ────────────────────────────────────────────

/// Parse the `rc` / `result` / `token` fields out of the remote wrapper's sentinel
/// line. Tolerant: a missing field ⇒ `None`/empty.
///
/// SENTINEL-SPOOFING DEFENCE (finding 2, Rust side): the real wrapper emits EXACTLY
/// ONE sentinel line, and it sanitizes the marker token so it can never contain a
/// newline. If the body somehow carries MORE THAN ONE `COMPILER_DEPLOY …` line — a
/// marker that forged one — the token is untrustworthy, so we substitute a
/// non-vocabulary sentinel token (`__ambiguous_sentinel__`) that `classify_reachable`
/// maps to `unknown`, never a masked `deployed`. rc/result still come from the real
/// trailing line (rc is `$?`, result is `systemctl show` — neither is marker-derived).
fn parse_result_line(body: &str) -> (Option<i64>, String, String) {
    let sentinel_lines: Vec<&str> = body
        .lines()
        .filter(|l| l.trim_start().starts_with(RESULT_SENTINEL))
        .collect();
    let line = sentinel_lines.last().copied().unwrap_or("");
    let field = |key: &str| -> Option<String> {
        line.split_whitespace().find_map(|tok| {
            tok.strip_prefix(&format!("{key}=")).map(str::to_string)
        })
    };
    let rc = field("rc").and_then(|v| v.parse::<i64>().ok());
    let result = field("result").unwrap_or_default();
    // More than one sentinel line ⇒ a spoof/garbage stream (the real wrapper emits
    // exactly one) ⇒ do NOT trust the token; force `unknown` via a non-vocabulary
    // value rather than degrading to a possibly-`deployed` empty-token path.
    let token = if sentinel_lines.len() > 1 {
        "__ambiguous_sentinel__".to_string()
    } else {
        field("token").unwrap_or_default()
    };
    (rc, result, token)
}

/// Classify a reachable host's outcome from `(rc, systemd Result, updater token)`.
///
/// PRECEDENCE — the TRUSTED marker token is consulted FIRST, and the `rc` gate applies
/// ONLY to success outcomes:
///
/// 1. A TRUSTED marker with a NON-SUCCESS authoritative token is AUTHORITATIVE OVER
///    `rc` (finding, cycle-9): `rolled_back` ⇒ `rolled_back` and `failed` ⇒ `failed`,
///    EVEN with a non-zero `systemctl start` rc — a rollback legitimately exits
///    non-zero, and its own marker is the ground truth, so it must never be masked
///    into a generic `failed`. (The marker is only read when the wrapper's pre-trigger
///    `rm` cleared any prior marker and it survived sanitization/single-sentinel, so a
///    trusted token is genuinely THIS run's.)
///
/// 2. A TRUSTED marker with a SUCCESS token (`deployed`/`skipped`/no-op) is trusted
///    ONLY when a REAL `rc == 0` was parsed — the exit code gates SUCCESS so a
///    failed/absent-rc start can't be masked as `deployed`. Otherwise ⇒ `unknown`.
///
/// 3. A NON-EMPTY but UNRECOGNIZED token ⇒ `unknown` (non-converged, must-not-trust).
///
/// 4. NO trusted marker (empty token) ⇒ classify from systemd `Result` AND `rc`: a
///    non-zero rc ⇒ `failed`; `rc == 0` + `Result=success` ⇒ `deployed`; `rc == 0` +
///    a non-success `Result` ⇒ `failed`; `rc == 0` + an indeterminate `Result` ⇒
///    `unknown` (exit code alone is not enough).
///
/// The raw token/Result are never returned; only the classified outcome is.
pub fn classify_reachable(rc: Option<i64>, result: &str, token: &str) -> DeployOutcome {
    let started_ok = rc == Some(0);
    let token = token.trim().to_ascii_lowercase();
    match token.as_str() {
        // (1) NON-SUCCESS authoritative marker — beats rc (a rollback exits non-zero).
        "rolled_back" | "rolledback" | "rollback" => DeployOutcome::RolledBack,
        "failed" | "error" | "abort" | "aborted" => DeployOutcome::Failed,
        // (2) SUCCESS marker — trusted ONLY with a real parsed rc==0, else `unknown`.
        "deployed" | "updated" | "swapped" | "success" => {
            if started_ok {
                DeployOutcome::Deployed
            } else {
                DeployOutcome::Unknown
            }
        }
        "skipped" | "noop" | "no-op" | "unchanged" | "up-to-date" | "current" => {
            if started_ok {
                DeployOutcome::Skipped
            } else {
                DeployOutcome::Unknown
            }
        }
        // (4) No trusted marker → systemd `Result` AND rc. A non-zero rc is `failed`
        // here (no authoritative marker to say otherwise).
        "" => {
            if matches!(rc, Some(code) if code != 0) {
                DeployOutcome::Failed
            } else {
                classify_absent_marker(started_ok, result)
            }
        }
        // (3) A non-empty token we don't recognize → `unknown` (never trusted as
        // success, never echoed).
        _ => DeployOutcome::Unknown,
    }
}

/// The three kinds of systemd unit `Result` (`systemctl show -p Result`).
enum ResultKind {
    /// `success`.
    Success,
    /// A definite non-success (`failed`/`timeout`/`exit-code`/`signal`/`core-dump`/
    /// `watchdog`/`start-limit-hit`/…) — any recognized non-`success` value.
    Failure,
    /// Empty/unreadable — we could not determine the unit's result.
    Indeterminate,
}

fn systemd_result_kind(result: &str) -> ResultKind {
    let r = result.trim();
    if r.is_empty() {
        ResultKind::Indeterminate
    } else if r.eq_ignore_ascii_case("success") {
        ResultKind::Success
    } else {
        ResultKind::Failure
    }
}

/// Classify an ABSENT-MARKER run from `rc` AND the systemd `Result` (finding 2):
/// `deployed` requires BOTH `rc == 0` AND `Result=success`; a non-success Result is
/// `failed` (never deployed); an indeterminate Result is `unknown` when the start
/// looked ok (exit code alone is not enough) and `failed` otherwise (fail-visible).
fn classify_absent_marker(started_ok: bool, result: &str) -> DeployOutcome {
    match systemd_result_kind(result) {
        // A definite non-success Result is a failure even with rc==0.
        ResultKind::Failure => DeployOutcome::Failed,
        // Success Result trusted only WITH a real rc==0; else not trusted → unknown.
        ResultKind::Success if started_ok => DeployOutcome::Deployed,
        ResultKind::Success => DeployOutcome::Unknown,
        // No Result to corroborate: rc==0 alone is not enough → unknown; no rc → failed.
        ResultKind::Indeterminate if started_ok => DeployOutcome::Unknown,
        ResultKind::Indeterminate => DeployOutcome::Failed,
    }
}

/// Build the short, redaction-safe `detail` string. FIXED VOCABULARY ONLY (finding
/// 2): it surfaces the CLASSIFIED outcome plus the integer `rc` — it NEVER echoes
/// the raw updater marker token or free-form `Result` text, so a marker carrying a
/// path / secret-shaped / malformed string can never reach the structured output.
fn detail_string(outcome: DeployOutcome, rc: Option<i64>) -> Option<String> {
    let rc = rc.map(|n| n.to_string()).unwrap_or_else(|| "?".to_string());
    Some(format!("outcome={} rc={rc}", outcome.as_str()))
}

// ── Remote execution (the real trigger path) ─────────────────────────────────

/// The three distinct ways an ssh trigger can end (finding 1):
///   - `Reachable(stdout)` — ssh exited 0 (the remote always `exit 0`s), carrying
///     the outcome line to classify,
///   - `Unreachable` — an ssh-level CONNECT/AUTH failure: a spawn error, or a
///     non-zero ssh exit (255), which — because the remote always `exit 0`s — can
///     ONLY be ssh's own connect/auth/host-key error (incl. ssh's `ConnectTimeout`
///     firing), never a slow deploy,
///   - `TimedOut` — the OUTER wall-clock timeout fired: the host was reached and the
///     synchronous updater run simply took too long. This is an in-flight/hung
///     deploy of UNKNOWN outcome, NOT a connectivity failure, so it must NEVER be
///     reported as `unreachable`.
enum SshOutcome {
    Reachable(String),
    Unreachable,
    TimedOut,
}

/// Spawn the ssh trigger argv, bounded by `timeout`. Never errors. Distinguishes a
/// connectivity failure (`Unreachable`) from the outer run timeout (`TimedOut`) so
/// a reachable-but-slow deploy is not masked as unreachable.
async fn ssh_trigger(argv: &[String], timeout: Duration) -> SshOutcome {
    use tokio::io::AsyncReadExt;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.stdin(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else {
        // Could not even spawn ssh — a local/connectivity failure.
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
        // A wait error (rare) is treated as a connectivity failure.
        Ok(Err(_)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            out.abort();
            return SshOutcome::Unreachable;
        }
        // The OUTER wall-clock timeout fired: reached, but the synchronous updater
        // run exceeded the budget → TimedOut (distinct from unreachable).
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            out.abort();
            return SshOutcome::TimedOut;
        }
    };
    let bytes = out.await.unwrap_or_default();
    if status.success() {
        // ssh exited 0 → the remote wrapper ran; classify from its outcome line.
        SshOutcome::Reachable(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        // A non-zero ssh exit (the remote always `exit 0`s, so this is ssh's own
        // 255 connect/auth/host-key error) → unreachable.
        SshOutcome::Unreachable
    }
}

/// Trigger one host and classify. Never errors. `systemctl` is the ALREADY-VALIDATED
/// constrained command (see [`validate_systemctl_cmd`]) — trigger_one never reads it
/// raw from the env.
async fn trigger_one(
    host: DeployHost,
    module: String,
    channel: String,
    systemctl: String,
    connect: Duration,
    outer: Duration,
) -> HostDeployResult {
    let unit = render_template(&unit_template(), &module, &channel);
    let marker = render_template(&result_marker_template(), &module, &channel);
    let remote = render_remote_trigger_cmd(&systemctl, &unit, &marker);
    // Reuse the SINGLE sanctioned ssh reach shared with compiler_status (BLD-08).
    // ssh `ConnectTimeout` = the small CONNECT budget; the OUTER wall-clock bound
    // (`outer` > connect) is applied by `ssh_trigger`. So a connect/auth hang exits
    // ssh (255 → unreachable) BEFORE the outer timer can fire (finding 1).
    let argv = sanctioned_ssh_argv(&host.ssh_target, &remote, connect.as_secs());
    match ssh_trigger(&argv, outer).await {
        SshOutcome::Unreachable => HostDeployResult {
            host: host.label,
            outcome: DeployOutcome::Unreachable,
            detail: None,
        },
        SshOutcome::TimedOut => HostDeployResult {
            host: host.label,
            outcome: DeployOutcome::TimedOut,
            detail: detail_string(DeployOutcome::TimedOut, None),
        },
        SshOutcome::Reachable(body) => {
            let (rc, result, token) = parse_result_line(&body);
            // Classify through the FIXED VOCABULARY; the raw token/result are used
            // ONLY to classify and are never surfaced (detail is outcome+rc).
            let outcome = classify_reachable(rc, &result, &token);
            HostDeployResult {
                host: host.label,
                outcome,
                detail: detail_string(outcome, rc),
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
    // Bound the worker count to min(configured, hosts, ceiling) BEFORE the prime loop —
    // so a huge/malformed `concurrency` (or an empty host list) can never spin an
    // absurd number of iterations. Empty hosts ⇒ 0 workers ⇒ the loop and the whole
    // aggregate return promptly.
    let workers = effective_concurrency(concurrency, hosts.len());
    let mut iter = hosts.into_iter();
    for _ in 0..workers {
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

    // Finding 4: validate the constrained systemctl command ONCE, up front. On a bad
    // config we do NOT trigger with an unsafe command — every chosen host is marked
    // `failed` (a visible, non-masked config error), never silently skipped.
    let systemctl = match validate_systemctl_cmd(&systemctl_env_raw()) {
        Ok(s) => s,
        Err(_) => {
            let results = chosen
                .into_iter()
                .map(|h| HostDeployResult {
                    host: h.label,
                    outcome: DeployOutcome::Failed,
                    detail: detail_string(DeployOutcome::Failed, None),
                })
                .collect();
            notes.push(
                "COMPILER_DEPLOY_SYSTEMCTL is not a valid constrained systemctl command \
                 (disallowed characters, or it does not invoke systemctl) — not triggered"
                    .to_string(),
            );
            let mut report = DeployReport {
                module: module.to_string(),
                channel: channel.to_string(),
                results,
                notes,
            };
            if report.degraded() {
                report.notes.push(format!(
                    "{} host(s) did not converge (config error) — nightly timer catches them",
                    report.stragglers()
                ));
            }
            return report;
        }
    };

    // Connect budget and OUTER wall-clock (strictly greater) so a connect/auth hang
    // is `unreachable`, never `timed_out` (finding 1).
    let connect = connect_timeout();
    let outer = outer_timeout(connect, trigger_timeout());
    let module_s = module.to_string();
    let channel_s = channel.to_string();
    let results = aggregate(chosen, max_concurrency(), |h| {
        trigger_one(
            h,
            module_s.clone(),
            channel_s.clone(),
            systemctl.clone(),
            connect,
            outer,
        )
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
            "{} host(s) did not converge (rolled_back/failed/timed_out/unknown/unreachable) — nightly timer catches them",
            report.stragglers()
        ));
    }
    report
}

/// The inline budget the auto-after-promote deploy may run before the promote
/// returns (clamped like the other timeouts).
fn auto_deploy_inline_budget() -> Duration {
    parse_timeout_secs(
        env_nonempty(COMPILER_AUTO_DEPLOY_INLINE_BUDGET_SECS),
        DEFAULT_AUTO_DEPLOY_INLINE_BUDGET_SECS,
    )
}

/// The `auto_deploy` payload returned when the fan-out did NOT finish within the
/// inline budget (or its task errored): the promote returns promptly and the deploy
/// continues DETACHED. Fixed-vocabulary, no infra echoed.
fn detached_auto_deploy_note(reason: &str) -> Value {
    json!({
        "kicked_off": true,
        "awaited": false,
        "reason": reason,
        "note": "auto-deploy kicked off in the background and not awaited (the promote is not \
                 held hostage by the fleet deploy); query compiler_status / compiler_deploy for \
                 per-host results",
    })
}

/// Run a fleet-deploy future on a BACKGROUND task and wait AT MOST `budget` for it. If
/// it finishes in time, return its report payload (attached inline to the promote);
/// otherwise return promptly with a detached note — the task is left running (dropping
/// the `JoinHandle` detaches, never aborts, the task), so the deploy continues in the
/// background. Generic over the future so it is unit-testable with a mock deploy.
async fn attach_deploy_within_budget<F>(fut: F, budget: Duration) -> Value
where
    F: std::future::Future<Output = DeployReport> + Send + 'static,
{
    let handle = tokio::spawn(fut);
    match tokio::time::timeout(budget, handle).await {
        // Finished within budget → attach the real per-host report.
        Ok(Ok(report)) => report.to_payload(),
        // The background task panicked (should not happen — deploy_report never
        // panics); surface a detached note rather than propagating.
        Ok(Err(_join_err)) => detached_auto_deploy_note("auto-deploy task error"),
        // Budget exceeded → return promptly; the task keeps running detached.
        Err(_elapsed) => detached_auto_deploy_note("inline budget exceeded"),
    }
}

/// Auto-trigger hook for `compiler_release` promote. When `COMPILER_AUTO_DEPLOY` is
/// truthy, fire a fleet-wide deploy and return a payload to attach to the promote
/// result; otherwise `None`. BEST-EFFORT AND NON-BLOCKING: the fan-out runs on a
/// background task and is awaited only up to a small inline budget
/// (`COMPILER_AUTO_DEPLOY_INLINE_BUDGET_SECS`). A long/6h fleet deploy therefore NEVER
/// holds the promote response hostage — the promote's own success/latency is
/// independent of the deploy outcome. (The manual `compiler_deploy` tool remains fully
/// synchronous — only THIS auto path is budgeted/detached.)
pub async fn auto_trigger_after_promote(module: &str, channel: &str) -> Option<Value> {
    if !env_truthy(COMPILER_AUTO_DEPLOY) {
        return None;
    }
    // If there is no tokio runtime (should not happen in the live tool path), fall
    // back to a plain inline await rather than panicking on `tokio::spawn`.
    if tokio::runtime::Handle::try_current().is_err() {
        return Some(deploy_report(module, channel, "all").await.to_payload());
    }
    let (m, c) = (module.to_string(), channel.to_string());
    let fut = async move { deploy_report(&m, &c, "all").await };
    Some(attach_deploy_within_budget(fut, auto_deploy_inline_budget()).await)
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
         failed | timed_out | unknown | unreachable). The compiler ONLY triggers; the updater \
         owns the swap safety (health-gate + rollback). Rolled-back, timed-out, unknown, and \
         unreachable hosts are all reported, never masked."
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

        // Tool-level ARGUMENT errors (a malformed `module`/`channel`/`hosts` the
        // caller passed) are `InvalidArgument` above. But an OPERATOR-CONFIG failure
        // (a malformed `COMPILER_DEPLOY_SYSTEMCTL`) is NOT a caller error: it flows
        // through `deploy_report`, which marks every chosen host `failed` with a
        // config-error note (no raw value echoed) — so the direct tool returns the
        // SAME best-effort per-host aggregate as the auto-promote hook, rather than
        // aborting with a bare error that drops the report.
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
    fn select_hosts_filters_by_label_and_target_and_counts_unknown_without_echo() {
        // Finding 3: an unknown/garbage requested host is NOT echoed verbatim — it is
        // reported only by count (it can carry ssh targets / arbitrary caller input).
        let (chosen, notes) =
            select_hosts(&hosts(), "host-a, u@host-b ; nope$secret@<internal-ip> ; junk");
        assert_eq!(
            chosen.iter().map(|h| h.label.as_str()).collect::<Vec<_>>(),
            vec!["host-a", "host-b"]
        );
        assert_eq!(notes.len(), 1);
        // Count present; raw unknown strings absent.
        assert!(notes[0].contains('2'), "{}", notes[0]);
        assert!(!notes[0].contains("nope"));
        assert!(!notes[0].contains("secret"));
        assert!(!notes[0].contains("<internal-ip>"));
        assert!(!notes[0].contains("junk"));
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
        // rc unknown and no success signal → failed (fail-visible, not masked).
        assert_eq!(classify_reachable(None, "", ""), DeployOutcome::Failed);
    }

    #[test]
    fn unparseable_rc_never_trusts_success() {
        // Finding 1: without a REAL parsed `rc == 0`, a converged/success outcome is
        // NOT trusted — a stale/damaged sentinel that still says `result=success`, or
        // a `deployed`/`skipped` token with no exit code, degrades to `unknown` (never
        // masked as a successful deploy).
        assert_eq!(
            classify_reachable(None, "success", ""),
            DeployOutcome::Unknown,
            "absent rc + Result=success must NOT be trusted as deployed"
        );
        assert_eq!(
            classify_reachable(None, "success", "deployed"),
            DeployOutcome::Unknown,
            "absent rc + deployed token must NOT be trusted"
        );
        assert_eq!(
            classify_reachable(None, "", "skipped"),
            DeployOutcome::Unknown,
            "absent rc + skipped token must NOT be trusted"
        );
        // A REAL parsed rc==0 + Result=success still deploys.
        assert_eq!(
            classify_reachable(Some(0), "success", ""),
            DeployOutcome::Deployed
        );
        assert_eq!(
            classify_reachable(Some(0), "success", "deployed"),
            DeployOutcome::Deployed
        );
        // Failure/rollback outcomes ARE still reported without a parsed rc (reporting
        // a failure can't mask a success).
        assert_eq!(
            classify_reachable(None, "", "rolled_back"),
            DeployOutcome::RolledBack
        );
        assert_eq!(classify_reachable(None, "", "failed"), DeployOutcome::Failed);
    }

    #[test]
    fn absent_marker_gates_on_result_and_rc() {
        // Finding 2: with an ABSENT marker, `deployed` requires BOTH rc==0 AND
        // Result=success. A non-success Result is `failed` even with rc==0 (a
        // non-success Result must never be reported as deployed); an indeterminate
        // (empty/unreadable) Result with rc==0 is `unknown` (exit code alone is not
        // enough).
        assert_eq!(
            classify_reachable(Some(0), "success", ""),
            DeployOutcome::Deployed,
            "rc=0 + Result=success → deployed"
        );
        assert_eq!(
            classify_reachable(Some(0), "failed", ""),
            DeployOutcome::Failed,
            "rc=0 + Result=failed → failed (non-success Result never deployed)"
        );
        assert_eq!(
            classify_reachable(Some(0), "timeout", ""),
            DeployOutcome::Failed,
            "rc=0 + Result=timeout → failed"
        );
        assert_eq!(
            classify_reachable(Some(0), "core-dump", ""),
            DeployOutcome::Failed,
            "rc=0 + Result=core-dump → failed"
        );
        assert_eq!(
            classify_reachable(Some(0), "", ""),
            DeployOutcome::Unknown,
            "rc=0 + indeterminate Result → unknown (exit code alone insufficient)"
        );
    }

    #[test]
    fn nonzero_start_rc_is_failed_despite_stale_success_result() {
        // Finding 3: a non-zero `systemctl start` rc must NOT be overridden by a
        // stale `Result=success` (a previous run's cached systemd Result).
        assert_eq!(
            classify_reachable(Some(1), "success", ""),
            DeployOutcome::Failed
        );
        assert_eq!(
            classify_reachable(Some(3), "success", ""),
            DeployOutcome::Failed
        );
    }

    #[test]
    fn nonzero_start_rc_downgrades_success_token_to_unknown() {
        // A SUCCESS marker token (`deployed`/`skipped`) with a non-zero start rc is NOT
        // trusted as success — success requires rc==0 — so it degrades to `unknown`
        // (never a masked `deployed`).
        assert_eq!(
            classify_reachable(Some(1), "success", "deployed"),
            DeployOutcome::Unknown
        );
        assert_eq!(
            classify_reachable(Some(1), "", "skipped"),
            DeployOutcome::Unknown
        );
    }

    #[test]
    fn trusted_non_success_marker_is_authoritative_over_rc() {
        // Cycle-9 finding: a TRUSTED non-success marker beats the rc gate — a rollback
        // legitimately exits non-zero, so a `rolled_back` marker must be reported as
        // `rolled_back`, NEVER masked into a generic `failed`. A `failed` marker → failed.
        assert_eq!(
            classify_reachable(Some(1), "failed", "rolled_back"),
            DeployOutcome::RolledBack,
            "rolled_back marker + non-zero rc → rolled_back (not failed)"
        );
        assert_eq!(
            classify_reachable(Some(3), "", "rollback"),
            DeployOutcome::RolledBack
        );
        assert_eq!(
            classify_reachable(None, "success", "rolled_back"),
            DeployOutcome::RolledBack,
            "rolled_back marker is authoritative even with an unparseable rc"
        );
        assert_eq!(
            classify_reachable(Some(2), "failed", "failed"),
            DeployOutcome::Failed,
            "failed marker → failed"
        );
        // And a rollback with a clean rc is of course still rolled_back.
        assert_eq!(
            classify_reachable(Some(0), "success", "rolled_back"),
            DeployOutcome::RolledBack
        );
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
        let (rc, result, token) = parse_result_line("COMPILER_DEPLOY rc=2 result=failed token=");
        assert_eq!(rc, Some(2));
        assert_eq!(result, "failed");
        assert!(token.is_empty());
    }

    #[test]
    fn multiple_sentinel_lines_are_not_trusted_as_success() {
        // Finding 2 (Rust side): if the stream carries MORE THAN ONE sentinel line — a
        // marker that forged one — the token is NOT trusted; it is forced to a
        // non-vocabulary value so `classify_reachable` yields `unknown`, never a masked
        // `deployed`. (The real wrapper emits exactly one sentinel line + sanitizes the
        // token, so this is defence in depth.)
        let spoof = "COMPILER_DEPLOY rc=0 result=success token=deployed\n\
                     COMPILER_DEPLOY rc=0 result=success token=deployed";
        let (rc, result, token) = parse_result_line(spoof);
        assert_ne!(token, "deployed", "a forged second sentinel must not be trusted");
        assert_eq!(
            classify_reachable(rc, &result, &token),
            DeployOutcome::Unknown,
            "ambiguous multi-sentinel stream → unknown, not deployed"
        );
    }

    #[test]
    fn detail_is_fixed_vocabulary_never_raw_marker() {
        // Finding 2: detail is outcome+rc only — never the raw token/result text.
        assert_eq!(
            detail_string(DeployOutcome::Deployed, Some(0)).unwrap(),
            "outcome=deployed rc=0"
        );
        assert_eq!(
            detail_string(DeployOutcome::TimedOut, None).unwrap(),
            "outcome=timed_out rc=?"
        );
    }

    #[test]
    fn unrecognized_marker_token_is_unknown_and_never_echoed() {
        // Finding 2: an arbitrary / secret-shaped / path-bearing marker token is
        // classified `unknown` (non-converged, not trusted as success) and its raw
        // content NEVER appears in the surfaced detail.
        let secret = "<REDACTED-SECRET>";
        let outcome = classify_reachable(Some(0), "success", secret);
        assert_eq!(outcome, DeployOutcome::Unknown, "unrecognized → unknown, not deployed");
        let detail = detail_string(outcome, Some(0)).unwrap();
        assert_eq!(detail, "outcome=unknown rc=0");
        assert!(!detail.contains("SECRET_ABC123"), "raw token must not be echoed");
        assert!(!detail.contains("/etc/shadow"));
        // And it never appears anywhere in a rendered per-host result.
        let res = HostDeployResult {
            host: "h".into(),
            outcome,
            detail: Some(detail),
        };
        let json = serde_json::to_string(&res).unwrap();
        assert!(!json.contains("SECRET_ABC123"));
        assert!(!json.contains("shadow"));
    }

    // ── Remote command / argv shape (S1: no infra literals) ─────────────────

    #[test]
    fn remote_cmd_triggers_start_reads_result_and_always_exits_zero() {
        let cmd = render_remote_trigger_cmd(
            "systemctl",
            "<email>",
            "<path>/.deploy_result",
        );
        // Finding 3: the marker is `rm`'d, and whether it is now provably ABSENT is
        // captured (`__cleared`) — the run-scoped gate. No mtime / run-reference logic.
        let rm_at = cmd.find("rm -f -- '<path>/.deploy_result'").expect("pre-trigger rm");
        let cleared_at = cmd.find("if [ -e '<path>/.deploy_result' ]; then __cleared=0")
            .expect("clear-succeeded captured");
        let start_at = cmd.find("systemctl start").unwrap();
        assert!(rm_at < cleared_at && cleared_at < start_at, "rm then clear-check then trigger");
        // The token is read ONLY when the clear succeeded.
        assert!(cmd.contains("if [ \"$__cleared\" = 1 ]; then __tok="), "cleared gate: {cmd}");
        // Finding 2: the token is sanitized (first line only + safe charset) so a
        // malformed marker can't inject a second sentinel line.
        assert!(cmd.contains("head -n1 -- '<path>/.deploy_result'"), "first-line only: {cmd}");
        assert!(cmd.contains("tr -cd 'A-Za-z0-9_-'"), "safe-charset strip: {cmd}");
        // No leftover mtime/run-reference machinery.
        assert!(!cmd.contains("__refmt") && !cmd.contains("__floor") && !cmd.contains("stat -c %Y"),
            "no mtime/run-reference logic remains: {cmd}");
        assert!(cmd.contains("systemctl start '<email>'"));
        assert!(cmd.contains("--property=Result --value"));
        assert!(cmd.contains("COMPILER_DEPLOY rc="));
        // Always exit 0 so ssh's exit reflects only connectivity (tri-state trick).
        assert!(cmd.trim_end().ends_with("exit 0"));
    }

    #[test]
    fn remote_cmd_forces_non_interactive_sudo() {
        // Finding 2: a `sudo` prefix is made non-interactive (`-n`) so a password
        // prompt fails fast instead of hanging for the whole trigger timeout.
        let cmd = render_remote_trigger_cmd("sudo systemctl", "<email>", "/m");
        assert!(cmd.contains("sudo -n systemctl start "), "{cmd}");
        assert!(cmd.contains("sudo -n systemctl show '<email>'"), "{cmd}");
        // No bare `sudo systemctl` (would be interactive) survives.
        assert!(!cmd.contains("sudo systemctl"), "{cmd}");
    }

    #[test]
    fn validate_systemctl_cmd_allows_constrained_rejects_metacharacters() {
        // Finding 4: plain `systemctl`, `sudo -n systemctl`, an absolute path, and
        // `sudo systemctl` are accepted (constrained bare-token commands).
        assert_eq!(validate_systemctl_cmd("systemctl").unwrap(), "systemctl");
        assert_eq!(
            validate_systemctl_cmd("sudo -n systemctl").unwrap(),
            "sudo -n systemctl"
        );
        assert_eq!(validate_systemctl_cmd("sudo systemctl").unwrap(), "sudo systemctl");
        assert_eq!(
            validate_systemctl_cmd("/usr/bin/systemctl").unwrap(),
            "/usr/bin/systemctl"
        );
        // Empty/unset → default.
        assert_eq!(validate_systemctl_cmd("   ").unwrap(), "systemctl");

        // Any shell metacharacter is rejected as a config error.
        for bad in [
            "systemctl; rm -rf /",
            "systemctl && curl evil",
            "systemctl | tee x",
            "systemctl $(id)",
            "systemctl > /etc/x",
            "systemctl `id`",
            "sudo\nsystemctl",
        ] {
            assert!(
                matches!(validate_systemctl_cmd(bad), Err(ToolError::InvalidArgument(_))),
                "must reject: {bad:?}"
            );
        }
        // Finding 3: the EXECUTABLE must be systemctl. A command whose executable is
        // NOT systemctl is rejected even if a later arg happens to be `systemctl`.
        for bad in [
            "sudo reboot",
            "reboot systemctl",         // executable is `reboot`, not systemctl
            "sudo reboot systemctl",    // after sudo, executable is `reboot`
            "sudo -n reboot systemctl", // after sudo -n, executable is `reboot`
            "curl systemctl",
        ] {
            assert!(
                matches!(validate_systemctl_cmd(bad), Err(ToolError::InvalidArgument(_))),
                "executable must be systemctl; must reject: {bad:?}"
            );
        }
        // Finding 1: the ONLY sudo option permitted before `systemctl` is `-n`. Any
        // OTHER sudo flag is rejected — especially the ARGUMENT-TAKING ones, which the
        // naive "skip all dash flags" parser would let bypass (e.g. `sudo -u systemctl`
        // makes sudo read `systemctl` as the USERNAME and run whatever follows).
        for bad in [
            "sudo -u systemctl",
            "sudo -g x systemctl",
            "sudo -h h systemctl",
            "sudo -p p systemctl",
            "sudo -C 3 systemctl",
            "sudo -r role systemctl",
            "sudo -U user systemctl",
            "sudo -i systemctl", // any non-`-n` flag, even argument-less, is out
            "sudo -n -u systemctl",
        ] {
            assert!(
                matches!(validate_systemctl_cmd(bad), Err(ToolError::InvalidArgument(_))),
                "only `-n` may precede systemctl; must reject: {bad:?}"
            );
        }
        // EXECUTABLE-PREFIX ONLY: reject ALL trailing tokens after `systemctl` — no
        // verbs AND no flags. A trailing flag that changes blocking/result semantics
        // (esp. `--no-block`, which returns before the updater finishes) would break
        // the SYNCHRONOUS-deploy contract; the wrapper owns `start <unit>`.
        for bad in [
            "systemctl reboot",       // verb
            "systemctl start",        // verb
            "systemctl stop",         // verb
            "sudo -n systemctl poweroff",
            "/usr/bin/systemctl enable",
            "systemctl --no-block",   // flag that breaks blocking semantics
            "systemctl -q",           // any trailing flag is out
            "systemctl --user",
            "sudo -n systemctl --no-block",
        ] {
            assert!(
                matches!(validate_systemctl_cmd(bad), Err(ToolError::InvalidArgument(_))),
                "no trailing token allowed (executable-prefix only); must reject: {bad:?}"
            );
        }
        // …and the accepted forms are EXACTLY `[sudo [-n]] systemctl` — nothing after
        // the executable.
        assert_eq!(
            validate_systemctl_cmd("sudo -n /usr/bin/systemctl").unwrap(),
            "sudo -n /usr/bin/systemctl"
        );
        assert_eq!(
            validate_systemctl_cmd("sudo --non-interactive systemctl").unwrap(),
            "sudo --non-interactive systemctl"
        );

        // The error never echoes the raw (potentially sensitive) value back.
        if let Err(ToolError::InvalidArgument(m)) = validate_systemctl_cmd("systemctl; secret123") {
            assert!(!m.contains("secret123"), "error must not echo raw value: {m}");
        } else {
            panic!("expected rejection");
        }
    }

    #[test]
    fn ensure_non_interactive_sudo_cases() {
        // Bare systemctl: unchanged.
        assert_eq!(ensure_non_interactive_sudo("systemctl"), "systemctl");
        // sudo → sudo -n.
        assert_eq!(ensure_non_interactive_sudo("sudo systemctl"), "sudo -n systemctl");
        // Already non-interactive: idempotent (no double -n).
        assert_eq!(ensure_non_interactive_sudo("sudo -n systemctl"), "sudo -n systemctl");
        assert_eq!(
            ensure_non_interactive_sudo("sudo --non-interactive systemctl"),
            "sudo --non-interactive systemctl"
        );
    }

    #[test]
    fn trigger_reuses_the_single_shared_sanctioned_reach() {
        // Finding 1: the deploy trigger fans out over the SAME shared
        // `status::sanctioned_ssh_argv` the read path uses — deploy.rs defines no
        // ssh option set of its own. Assert the shared helper carries the sanctioned
        // non-mutating posture, and that the marker read is built on top of it.
        let argv = sanctioned_ssh_argv("u@host", "echo hi", 300);
        assert_eq!(argv[0], "ssh");
        assert!(argv.iter().any(|a| a == "BatchMode=yes"));
        assert!(argv.iter().any(|a| a == "ConnectTimeout=300"));
        assert!(!argv.iter().any(|a| a.contains("accept-new")));
        assert!(argv.iter().any(|a| a == "StrictHostKeyChecking=no"));
        assert!(argv.iter().any(|a| a == "UserKnownHostsFile=/dev/null"));
        assert!(argv.iter().any(|a| a == "u@host"));
        assert_eq!(argv.last().unwrap(), "echo hi");
        // The BLD-08 marker read is the SAME reach with a cat remote → single source.
        let read = super::super::status::render_marker_read_argv("u@host", "/m", 300);
        assert_eq!(read[..read.len() - 1], argv[..argv.len() - 1]);
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
        let c = report.counts();
        assert_eq!(
            (c.deployed, c.skipped, c.rolled_back, c.failed, c.timed_out, c.unreachable),
            (1, 1, 1, 0, 0, 0)
        );
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
        let c = report.counts();
        assert_eq!(c.deployed, 2);
        assert_eq!(c.unreachable, 1);
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
    async fn timed_out_dest_is_a_distinct_straggler_not_unreachable() {
        // Finding 1: a reached-but-slow host is `timed_out`, counted distinctly from
        // `unreachable`, and is a non-converged straggler (never masked as either
        // success or a connectivity failure).
        let map = std::collections::HashMap::from([
            ("host-a", DeployOutcome::Deployed),
            ("host-b", DeployOutcome::TimedOut),
            ("host-c", DeployOutcome::Unreachable),
        ]);
        let report = canned(map).await;
        let c = report.counts();
        assert_eq!(c.timed_out, 1);
        assert_eq!(c.unreachable, 1);
        assert!(report.degraded());
        assert_eq!(report.stragglers(), 2, "timed_out + unreachable are stragglers");
        let payload = report.to_payload();
        assert_eq!(payload["counts"]["timed_out"], json!(1));
        assert_eq!(payload["counts"]["unreachable"], json!(1));
        let b = report.results.iter().find(|r| r.host == "host-b").unwrap();
        assert_eq!(b.outcome, DeployOutcome::TimedOut);
        assert!(report.summary().contains("timed_out"));
    }

    // ── ssh_trigger: timeout != unreachable (finding 1) ─────────────────────

    #[tokio::test]
    async fn ssh_trigger_run_timeout_is_timed_out_not_unreachable() {
        // The child is reachable (spawns, runs) but exceeds the wall-clock budget →
        // TimedOut, NOT Unreachable.
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 5".to_string(),
        ];
        assert!(matches!(
            ssh_trigger(&argv, Duration::from_millis(300)).await,
            SshOutcome::TimedOut
        ));
    }

    #[tokio::test]
    async fn ssh_trigger_nonzero_exit_is_unreachable() {
        // The remote always exits 0, so a non-zero exit == ssh's own 255
        // connect/auth error → Unreachable.
        let argv = vec!["sh".to_string(), "-c".to_string(), "exit 255".to_string()];
        assert!(matches!(
            ssh_trigger(&argv, Duration::from_secs(2)).await,
            SshOutcome::Unreachable
        ));
    }

    #[tokio::test]
    async fn ssh_trigger_spawn_failure_is_unreachable() {
        let argv = vec!["this-binary-does-not-exist-xyz".to_string()];
        assert!(matches!(
            ssh_trigger(&argv, Duration::from_secs(2)).await,
            SshOutcome::Unreachable
        ));
    }

    #[tokio::test]
    async fn ssh_trigger_exit0_is_reachable_with_body() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'COMPILER_DEPLOY rc=0 result=success token=deployed\\n'".to_string(),
        ];
        let SshOutcome::Reachable(body) = ssh_trigger(&argv, Duration::from_secs(2)).await else {
            panic!("expected Reachable");
        };
        let (rc, result, token) = parse_result_line(&body);
        assert_eq!(classify_reachable(rc, &result, &token), DeployOutcome::Deployed);
    }

    #[test]
    fn outer_timeout_has_headroom_over_connect() {
        // Finding 1: the OUTER wall-clock is STRICTLY greater than the connect budget,
        // so ssh's own ConnectTimeout (== connect) fires (→ unreachable) before the
        // outer timer could misfire as timed_out.
        let connect = Duration::from_secs(10);
        let outer = outer_timeout(connect, Duration::from_secs(300));
        assert!(outer > connect, "outer must exceed the connect budget");
        assert_eq!(outer, Duration::from_secs(311));
        // Even with a tiny run budget, outer still exceeds connect.
        assert!(outer_timeout(Duration::from_secs(10), Duration::from_secs(0)) > connect);
    }

    #[test]
    fn parse_timeout_secs_clamps_and_defaults() {
        // Finding 2: a huge (but parse-valid) value CLAMPS to the max, never overflows.
        assert_eq!(
            parse_timeout_secs(Some(u64::MAX.to_string()), 300),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
        assert_eq!(
            parse_timeout_secs(Some("999999999999".into()), 300),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
        // A normal value passes through.
        assert_eq!(parse_timeout_secs(Some("120".into()), 300), Duration::from_secs(120));
        // 0 / unparseable / absent → the safe default.
        assert_eq!(parse_timeout_secs(Some("0".into()), 300), Duration::from_secs(300));
        assert_eq!(parse_timeout_secs(Some("nope".into()), 300), Duration::from_secs(300));
        assert_eq!(parse_timeout_secs(None, 300), Duration::from_secs(300));
    }

    #[test]
    fn outer_timeout_saturates_without_panic_on_absurd_inputs() {
        // Finding 2: even with absurd Durations (which the clamp prevents in practice),
        // the saturating arithmetic yields a valid Duration and never panics, and it is
        // still > the connect budget.
        let connect = Duration::from_secs(MAX_TIMEOUT_SECS);
        let run = Duration::from_secs(MAX_TIMEOUT_SECS);
        let outer = outer_timeout(connect, run);
        assert!(outer > connect, "outer > connect even at the clamp ceiling");
        // A pathological pre-clamp value can't overflow either (saturates to Duration::MAX).
        let huge = Duration::MAX;
        let outer = outer_timeout(huge, huge);
        assert_eq!(outer, Duration::MAX, "saturates, never panics");
    }

    #[test]
    fn effective_concurrency_bounds_workers() {
        // Finding 1: never more than the configured value, the host count, or the ceiling.
        assert_eq!(effective_concurrency(1_000_000, 3), 3, "capped to host count");
        assert_eq!(
            effective_concurrency(1_000_000, 10_000),
            MAX_CONCURRENCY_CEILING,
            "capped to the hard ceiling"
        );
        assert_eq!(effective_concurrency(2, 8), 2, "honors a small configured value");
        assert_eq!(effective_concurrency(1_000_000, 0), 0, "empty host list → zero workers");
        assert_eq!(effective_concurrency(0, 4), 1, "0 configured floors at 1 worker");
    }

    #[tokio::test]
    async fn aggregate_bounds_workers_and_returns_all_hosts() {
        // A huge configured concurrency with N hosts spawns at most min(N, ceiling)
        // workers (not the huge value) and still returns EVERY host's result. The
        // worker count is observed via a shared max-in-flight counter.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let hosts: Vec<DeployHost> = (0..5)
            .map(|n| DeployHost {
                label: format!("host-{n}"),
                ssh_target: format!("u@host-{n}"),
            })
            .collect();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let results = aggregate(hosts, usize::MAX, |h| {
            let in_flight = in_flight.clone();
            let max_seen = max_seen.clone();
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                HostDeployResult {
                    host: h.label,
                    outcome: DeployOutcome::Deployed,
                    detail: None,
                }
            }
        })
        .await;
        assert_eq!(results.len(), 5, "every host's result returned");
        let peak = max_seen.load(Ordering::SeqCst);
        assert!(peak <= 5, "never more workers than hosts; peak={peak}");
        assert!(peak <= MAX_CONCURRENCY_CEILING, "never above the ceiling; peak={peak}");
    }

    #[tokio::test]
    async fn aggregate_empty_hosts_spawns_none_and_returns_promptly() {
        // An empty host list with a huge configured concurrency must NOT loop/spawn.
        let results = aggregate(Vec::new(), usize::MAX, |h: DeployHost| async move {
            HostDeployResult { host: h.label, outcome: DeployOutcome::Deployed, detail: None }
        })
        .await;
        assert!(results.is_empty());
    }

    fn sample_report() -> DeployReport {
        DeployReport {
            module: "chord".into(),
            channel: "stable".into(),
            results: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn auto_deploy_fast_fan_out_attaches_report_inline() {
        // A fan-out that finishes within the budget → the real per-host report is
        // attached inline (not a detached note).
        let payload =
            attach_deploy_within_budget(async { sample_report() }, Duration::from_secs(5)).await;
        assert_eq!(payload["module"], json!("chord"));
        assert_eq!(payload["channel"], json!("stable"));
        assert!(
            payload.get("awaited").is_none(),
            "an inline report has no detached-note fields: {payload}"
        );
    }

    #[tokio::test]
    async fn auto_deploy_slow_fan_out_returns_promptly_detached() {
        // Finding 2: a SLOW fleet deploy must NOT delay the promote past the small
        // inline budget — it returns promptly with a detached note; the deploy keeps
        // running in the background.
        let start = std::time::Instant::now();
        let slow = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            sample_report()
        };
        let payload = attach_deploy_within_budget(slow, Duration::from_millis(100)).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "the promote must not block on the slow deploy; elapsed={elapsed:?}"
        );
        assert_eq!(payload["kicked_off"], json!(true));
        assert_eq!(
            payload["awaited"], json!(false),
            "budget exceeded → detached note, not the (unfinished) report"
        );
        // Never masks/echoes anything infra-shaped.
        assert!(payload.get("module").is_none());
    }

    #[tokio::test]
    async fn ssh_trigger_connect_fail_before_outer_deadline_is_unreachable() {
        // Simulate ssh's ConnectTimeout firing: the process exits non-zero (255)
        // shortly BEFORE the (larger) outer deadline → Unreachable, NOT TimedOut.
        // This is the guarantee `outer > connect` buys us.
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 0.2; exit 255".to_string(),
        ];
        assert!(matches!(
            ssh_trigger(&argv, Duration::from_secs(3)).await,
            SshOutcome::Unreachable
        ));
    }

    // ── Marker run-scoping + spoof-sanitization (findings 2 & 3) end-to-end via `sh` ──

    async fn run_wrapper(systemctl: &str, unit: &str, marker: &std::path::Path) -> String {
        let cmd = render_remote_trigger_cmd(systemctl, unit, &marker.to_string_lossy());
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .await
            .expect("run wrapper");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Build a FAKE `systemctl` script whose `start` action runs `start_action` (a
    /// shell snippet — typically a `printf … > <marker>` that writes the outcome
    /// marker) and whose `show` prints nothing. Returns its path (kept alive by `dir`).
    fn fake_systemctl(dir: &std::path::Path, start_action: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-systemctl.sh");
        let body =
            format!("#!/bin/sh\ncase \"$1\" in\n  start) {start_action} ;;\nesac\nexit 0\n");
        std::fs::write(&script, &body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn count_sentinel_lines(body: &str) -> usize {
        body.lines()
            .filter(|l| l.trim_start().starts_with(RESULT_SENTINEL))
            .count()
    }

    #[tokio::test]
    async fn unremovable_marker_is_not_trusted() {
        // Finding 3: a pre-existing marker the wrapper's `rm` CANNOT clear must NOT be
        // trusted (degrade to Result+rc). Root-independent: the marker is a DIRECTORY,
        // which `rm -f` can never remove (no `-r`) regardless of privileges — so the
        // clear provably fails and any prior "result" is ignored. `systemctl`=`true`
        // writes nothing.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("deploy_result");
        std::fs::create_dir(&marker).unwrap();
        let out = run_wrapper("true", "unit", &marker).await;
        let (rc, result, token) = parse_result_line(&out);
        assert!(
            token.is_empty(),
            "an unremovable pre-existing marker must not be trusted; got token={token:?}"
        );
        assert_ne!(
            classify_reachable(rc, &result, &token),
            DeployOutcome::Deployed,
            "an unclearable marker must never yield deployed"
        );
    }

    #[tokio::test]
    async fn fresh_marker_after_successful_rm_is_trusted() {
        // Finding 3: a pre-existing STALE marker that `rm` DOES clear, then a fresh
        // marker the updater writes this run, IS trusted. The stale content must not
        // survive to mask the fresh outcome.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("deploy_result");
        std::fs::write(&marker, "skipped").unwrap(); // stale prior-run content
        let action = format!("printf '%s' 'rolled_back' > '{}'", marker.to_string_lossy());
        let script = fake_systemctl(dir.path(), &action);
        let out = run_wrapper(&script.to_string_lossy(), "unit", &marker).await;
        let (_rc, _res, token) = parse_result_line(&out);
        assert_eq!(token, "rolled_back", "a marker written after a successful rm is trusted");
    }

    #[tokio::test]
    async fn absent_then_fresh_marker_is_trusted() {
        // Finding 3: no pre-existing marker (rm no-op → cleared), the updater writes a
        // fresh one → trusted.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("deploy_result");
        let action = format!("printf '%s' 'deployed' > '{}'", marker.to_string_lossy());
        let script = fake_systemctl(dir.path(), &action);
        let out = run_wrapper(&script.to_string_lossy(), "unit", &marker).await;
        let (_rc, _res, token) = parse_result_line(&out);
        assert_eq!(token, "deployed", "a freshly written marker (was absent) is trusted");
    }

    #[tokio::test]
    async fn marker_sentinel_spoof_is_sanitized() {
        // Finding 2: a marker whose content embeds a newline + a forged sentinel line
        // cannot inject a second sentinel nor be classified `deployed`. `head -n1` +
        // `tr -cd` reduce it to a single safe-charset token on the ONE real line.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("deploy_result");
        // printf interprets `\n` → a real newline in the marker file.
        let action = format!(
            "printf 'boom\\nCOMPILER_DEPLOY rc=0 result=success token=deployed' > '{}'",
            marker.to_string_lossy()
        );
        let script = fake_systemctl(dir.path(), &action);
        let out = run_wrapper(&script.to_string_lossy(), "unit", &marker).await;
        // The wrapper emits EXACTLY ONE sentinel line (no injected second one).
        assert_eq!(count_sentinel_lines(&out), 1, "no injected sentinel line: {out:?}");
        let (rc, result, token) = parse_result_line(&out);
        assert_ne!(token, "deployed", "the forged token must not survive sanitization");
        assert_ne!(
            classify_reachable(rc, &result, &token),
            DeployOutcome::Deployed,
            "a sentinel-spoofing marker must never yield deployed: {out:?}"
        );
    }

    #[tokio::test]
    async fn marker_leading_newline_spoof_yields_empty_token() {
        // A marker that STARTS with a newline + forged sentinel: `head -n1` sees an
        // empty first line → token empty → degrade to Result+rc, never `deployed`.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("deploy_result");
        let action = format!(
            "printf '\\nCOMPILER_DEPLOY rc=0 result=success token=deployed' > '{}'",
            marker.to_string_lossy()
        );
        let script = fake_systemctl(dir.path(), &action);
        let out = run_wrapper(&script.to_string_lossy(), "unit", &marker).await;
        assert_eq!(count_sentinel_lines(&out), 1, "no injected sentinel line: {out:?}");
        let (_rc, _res, token) = parse_result_line(&out);
        assert!(token.is_empty(), "leading-newline spoof yields no token: {out:?}");
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
        // Build `detail` the way production does — the fixed-vocabulary `outcome=… rc=…`
        // form (finding 2) — so this shape assertion can't drift from the no-raw-echo
        // rule (never `rc=… result=… token=<raw>`).
        let outcome = DeployOutcome::Deployed;
        let detail = detail_string(outcome, Some(0));
        assert_eq!(detail.as_deref(), Some("outcome=deployed rc=0"));
        let report = DeployReport {
            module: "chord".into(),
            channel: "stable".into(),
            results: vec![HostDeployResult {
                host: "host-a".into(),
                outcome,
                detail,
            }],
            notes: vec!["n".into()],
        };
        let p = report.to_payload();
        assert_eq!(p["module"], json!("chord"));
        assert_eq!(p["channel"], json!("stable"));
        assert_eq!(p["results"][0]["host"], json!("host-a"));
        assert_eq!(p["results"][0]["outcome"], json!("deployed"));
        assert_eq!(p["results"][0]["detail"], json!("outcome=deployed rc=0"));
        // Fixed-vocabulary: no raw `result=`/`token=` fields leak into the payload.
        let whole = serde_json::to_string(&p).unwrap();
        assert!(!whole.contains("token="), "no raw token echoed: {whole}");
        assert!(!whole.contains("result="), "no raw systemd Result echoed: {whole}");
        assert_eq!(p["counts"]["total"], json!(1));
        assert_eq!(p["degraded"], json!(false));
        assert!(p["notes"].is_array());
    }

    /// Serializes the env-mutating test below (env is process-global).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn malformed_systemctl_config_surfaces_in_aggregate_not_bare_error() {
        // A malformed OPERATOR-CONFIG `COMPILER_DEPLOY_SYSTEMCTL` must NOT abort the
        // direct tool with a bare InvalidArgument that drops the per-host report — it
        // must surface in the STRUCTURED aggregate (every chosen host `failed` + a
        // config-error note, no raw value echoed), IDENTICAL to the auto-promote hook
        // path (both go through `deploy_report`).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("COMPILER_DEPLOY_HOSTS", "host-a|u@host-a; host-b|u@host-b");
        let raw = "systemctl; secret_raw_123"; // metacharacter → malformed
        std::env::set_var("COMPILER_DEPLOY_SYSTEMCTL", raw);

        // (1) The DIRECT tool returns Ok(structured aggregate), NOT a bare error.
        let out = CompilerDeploy
            .execute_structured(json!({"module": "chord", "channel": "stable"}))
            .await
            .expect("malformed operator config must not abort the tool with a bare error");
        let payload = out.structured.clone().unwrap();

        // Every chosen host is `failed`; a config-error note NAMES the problem.
        assert_eq!(payload["counts"]["failed"], json!(2));
        assert_eq!(payload["counts"]["total"], json!(2));
        for r in payload["results"].as_array().unwrap() {
            assert_eq!(r["outcome"], json!("failed"));
        }
        assert_eq!(payload["degraded"], json!(true));
        let notes = serde_json::to_string(&payload["notes"]).unwrap();
        assert!(notes.contains("COMPILER_DEPLOY_SYSTEMCTL"), "note names the config: {notes}");

        // (2) S1: the raw malformed config value is NEVER echoed anywhere.
        let whole = serde_json::to_string(&payload).unwrap();
        assert!(!whole.contains("secret_raw_123"), "raw config must not be echoed: {whole}");
        assert!(!out.text.contains("secret_raw_123"));

        // (3) CONSISTENCY: the direct-tool aggregate == the promote-hook aggregate.
        let hook = deploy_report("chord", "stable", "all").await;
        assert_eq!(payload, hook.to_payload(), "direct tool matches the promote-hook report");

        std::env::remove_var("COMPILER_DEPLOY_HOSTS");
        std::env::remove_var("COMPILER_DEPLOY_SYSTEMCTL");
    }
}
