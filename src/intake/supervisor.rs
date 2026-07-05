//! MINT Phase 3: the permanent sweep-supervisor daemon.
//!
//! ## Why this exists
//! Two interim stopgaps have been doing this job: a bash watchdog
//! (`/opt/intake/sweep-watchdog.sh`, driven by `sweep-watchdog.timer` on
//! <host>) and a session-scoped Claude cron job that auto-expires. Both detect a
//! JAMMED coder/assistant sweep — the GPU pegged busy while NO new
//! `code_profile_runs` rows have landed for a long time — and auto-recover by
//! restarting `ollama.service` plus whichever sweep unit(s) are active. This
//! module is the real, permanent Rust replacement: a long-running tokio daemon
//! (`mint supervisor run`) plus `install`/`uninstall` for its systemd unit.
//!
//! ## What it does NOT do
//! It OBSERVES and RESTARTS services. It does NOT itself hold or release the
//! GPU-authority exclusive lock — that is the sweep binaries' own job (see
//! [`crate::intake::gpu_authority`]). These concerns stay strictly separate: a
//! recovery restart bounces `ollama` + the sweep unit, and the sweep unit's
//! own `ExclusiveGuard` re-acquires the lock on its next start. The supervisor
//! never touches `/run/gpu-authority.lock`.
//!
//! ## Jam-detection algorithm (ported faithfully from the bash watchdog)
//! Every ~90s tick:
//!   - `last_row_epoch` = `max(created_at)` from `code_profile_runs`;
//!   - `gpu_busy` = `/sys/class/drm/card*/device/gpu_busy_percent`;
//!   - `sweep_active` / `assistant_active` = `systemctl is-active` of the two
//!     sweep units;
//!   - `age = now - last_row_epoch`;
//!   - verdict = `idle` if neither unit active; `stuck` if
//!     `gpu_busy >= 70 AND age > 2700`; else `working`.
//!   - On `stuck`, with a 1-hour cooldown between recoveries: restart
//!     `ollama.service`, wait 5s, then restart whichever sweep unit(s) were
//!     active. The thresholds ([`STUCK_THRESHOLD_SEC`], [`GPU_BUSY_MIN`],
//!     [`RECOVERY_COOLDOWN_SEC`]) are the already-tuned bash values, ported
//!     verbatim — this is proven detection logic, NOT redesigned here.
//!
//! ## NEW in the Rust port: repeat-stuck escalation
//! The daemon tracks a rolling 1-hour window of stuck-recoveries per
//! `(model, backend, mem_config)` combo (identified from the most-recent
//! `code_profile_runs` row at recovery time). If the SAME combo produces
//! [`REPEAT_STUCK_THRESHOLD`]+ recoveries within [`REPEAT_STUCK_WINDOW_SEC`],
//! that is "repeat-stuck": a plain restart clearly is not fixing it, so this is
//! where Phase 4 (breakfix — NOT YET BUILT) will plug in. For now the
//! [`BreakfixHandler`] seam has a logging-only default
//! ([`LoggingBreakfixHandler`]) that records a structured `ESCALATION` line and
//! defers, so the daemon still performs the normal restart-recovery as a safe
//! fallback. Phase 4 supplies a real `BreakfixHandler` impl without touching
//! this loop.
//!
//! ## Log-line compatibility (load-bearing)
//! Tick lines are written to the SAME log file (`/var/log/sweep-watchdog.log`)
//! in the SAME shape as the bash script —
//! `TIMESTAMP verdict=X gpu_busy=Y% row_age=Zs sweep=A assistant=B` — because
//! the operator's monitoring routine already parses exactly this format. See
//! [`format_tick_line`]. Escalation events add NEW, clearly-distinguishable
//! `ESCALATION` lines (see [`format_escalation_line`]).
//!
//! ## Pure/IO separation
//! Following this crate's convention (see `gpu_authority::is_blocked` /
//! `is_idempotent_reacquire`), every decision is a pure, unit-tested function
//! ([`compute_verdict`], [`should_recover`], [`is_repeat_stuck`],
//! [`format_tick_line`], [`format_escalation_line`], [`SupervisorState`]). The
//! async loop ([`tick`]) wires those to real I/O through the [`SupervisorEnv`]
//! trait, whose live impl ([`LiveEnv`]) does the DB query / `/sys` read /
//! `systemctl` shell-outs, and whose test impls script the environment so the
//! loop's decision-making is testable without a live DB or systemd.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::PgPool;

// ── Tuned thresholds — ported VERBATIM from sweep-watchdog.sh ────────────────

/// A jammed run's row-age floor. A full model suite can legitimately take
/// 20-40+ min (batched DB writes: rows only land after a whole suite finishes),
/// so a shorter threshold falsely flagged an in-progress suite as stuck and the
/// watchdog restarted mid-suite, discarding all its in-progress work. Widened
/// to 2700s (45 min) on 2026-07-04. `STUCK_THRESHOLD_SEC` in the bash script.
pub const STUCK_THRESHOLD_SEC: i64 = 2700;

/// GPU-busy floor (percent) for a "stuck" verdict: the GPU must be actively
/// pegged, not merely idle-between-models. `GPU_BUSY_MIN` in the bash script.
pub const GPU_BUSY_MIN: u64 = 70;

/// Minimum spacing between recoveries — never thrash-restart. Widened to 3600s
/// (1 hour) on 2026-07-04 alongside `STUCK_THRESHOLD_SEC`. The bash script's
/// `now_epoch - last_recovery > 3600` cooldown.
pub const RECOVERY_COOLDOWN_SEC: u64 = 3600;

/// Tick cadence — matches `sweep-watchdog.timer`'s `OnUnitActiveSec=90`.
pub const TICK_INTERVAL_SEC: u64 = 90;

// ── NEW: repeat-stuck escalation window ──────────────────────────────────────

/// Rolling window over which repeat-stuck recoveries for one combo are counted.
///
/// Caught in review: this MUST be meaningfully larger than
/// [`RECOVERY_COOLDOWN_SEC`], not equal to it. `RECOVERY_COOLDOWN_SEC` forces
/// at least a 1-hour gap between any two recoveries, so getting
/// [`REPEAT_STUCK_THRESHOLD`] (3) recoveries for the SAME combo takes at
/// least 2 full cooldown gaps end-to-end (~2h) before the 3rd fires — if the
/// window were also 1h, the 1st recovery would already have aged out of the
/// window by the time the 2nd is even allowed to happen, making escalation
/// mathematically unreachable. Four hours gives a full cooldown's worth of
/// slack beyond the ~2h physical minimum.
pub const REPEAT_STUCK_WINDOW_SEC: u64 = 4 * RECOVERY_COOLDOWN_SEC;

/// How many stuck-recoveries of the SAME combo within
/// [`REPEAT_STUCK_WINDOW_SEC`] constitute "repeat-stuck" (escalation).
pub const REPEAT_STUCK_THRESHOLD: usize = 3;

/// The log file the bash watchdog wrote and the operator's monitoring routine
/// parses. The Rust daemon appends to the SAME file, in the SAME tick-line
/// shape, so that monitoring keeps working unchanged.
pub const LOG_PATH: &str = "/var/log/sweep-watchdog.log";

/// Where the recovery-cooldown timestamp persists across daemon restarts (a
/// `Restart=on-failure` bounce must not reset the 1-hour cooldown and let the
/// daemon immediately re-recover). Mirrors the bash script's
/// `/var/lib/sweep-watchdog/last_recovery_epoch`, under the daemon's own dir.
pub const STATE_DIR: &str = "/var/lib/mint-supervisor";

/// The two sweep systemd units the supervisor observes and restarts.
pub const CODER_SWEEP_UNIT: &str = "intake-coder-sweep.service";
pub const ASSISTANT_SWEEP_UNIT: &str = "intake-assistant-sweep.service";
pub const OLLAMA_UNIT: &str = "ollama.service";

// ── Verdict (pure) ───────────────────────────────────────────────────────────

/// The three states the bash script computes each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Neither sweep unit is active — nothing to supervise.
    Idle,
    /// A sweep is running and producing rows (or between models) — healthy.
    Working,
    /// A sweep is active, the GPU is pegged, and no rows have landed for longer
    /// than [`STUCK_THRESHOLD_SEC`] — jammed; a recovery is warranted.
    Stuck,
}

impl Verdict {
    /// The exact lowercase token the bash script wrote (`idle`/`working`/
    /// `stuck`) — load-bearing for the operator's log parser.
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Idle => "idle",
            Verdict::Working => "working",
            Verdict::Stuck => "stuck",
        }
    }
}

/// Compute the tick verdict — the bash script's decision, ported verbatim:
///   - neither unit active            ⇒ `idle` (checked FIRST, so an idle host
///     is never mislabelled "stuck" just because the GPU happens to be busy
///     with unrelated work);
///   - `gpu_busy >= GPU_BUSY_MIN` AND `age_secs > STUCK_THRESHOLD_SEC` ⇒ `stuck`;
///   - otherwise                       ⇒ `working`.
/// Pure — no clock, no DB, no `/sys`.
pub fn compute_verdict(gpu_busy: u64, age_secs: i64, sweep_active: bool, assistant_active: bool) -> Verdict {
    if !sweep_active && !assistant_active {
        return Verdict::Idle;
    }
    if gpu_busy >= GPU_BUSY_MIN && age_secs > STUCK_THRESHOLD_SEC {
        return Verdict::Stuck;
    }
    Verdict::Working
}

/// Whether enough time has elapsed since the last recovery to recover again —
/// the bash script's `now_epoch - last_recovery > RECOVERY_COOLDOWN_SEC`
/// anti-thrash cooldown. A `last_recovery` of 0 (never recovered) always
/// permits recovery. Pure.
pub fn should_recover(now: u64, last_recovery: u64) -> bool {
    now.saturating_sub(last_recovery) > RECOVERY_COOLDOWN_SEC
}

/// Whether `recoveries_for_combo` (recovery timestamps for ONE combo) crosses
/// the repeat-stuck escalation threshold: at least [`REPEAT_STUCK_THRESHOLD`]
/// of them fall within [`REPEAT_STUCK_WINDOW_SEC`] of `now`. This is the SOLE
/// authority on the rolling window, so callers can pass an unfiltered per-combo
/// history and let this decide. Pure.
pub fn is_repeat_stuck(recoveries_for_combo: &[u64], now: u64) -> bool {
    let within = recoveries_for_combo
        .iter()
        .filter(|&&t| now.saturating_sub(t) < REPEAT_STUCK_WINDOW_SEC)
        .count();
    within >= REPEAT_STUCK_THRESHOLD
}

// ── Combo identity ───────────────────────────────────────────────────────────

/// The `(model, backend, mem_config)` combination a stuck event is attributed
/// to — identified from the most-recent `code_profile_runs` row at recovery
/// time. `mem_config` is `Option` because rows written before that column
/// existed (the preserved baseline) carry SQL NULL — never assume `None` means
/// a specific config (same discipline as `storage::CodeRunRowV2::mem_config`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComboKey {
    pub model: String,
    pub backend: String,
    pub mem_config: Option<String>,
}

impl ComboKey {
    /// A stable, human-readable label for logs: `model:backend:mem_config`,
    /// with a NULL `mem_config` rendered as the literal `NULL` (never an empty
    /// segment that would make `a::gpu:` ambiguous with `a:gpu`).
    pub fn label(&self) -> String {
        format!(
            "{}:{}:{}",
            self.model,
            self.backend,
            self.mem_config.as_deref().unwrap_or("NULL")
        )
    }
}

// ── Rolling recovery ledger (pure, in-memory) ────────────────────────────────

/// The daemon's mutable state between ticks: the last recovery instant (drives
/// the cooldown, persisted across restarts) and a rolling ledger of
/// per-combo recovery timestamps (drives repeat-stuck escalation, in-memory
/// only — a daemon restart resets it, which is acceptable given the 1-hour
/// window). Pure — no I/O; the loop owns persistence.
#[derive(Debug, Clone)]
pub struct SupervisorState {
    /// Epoch of the most recent recovery restart (0 = never). Seeded at startup
    /// from the persisted state file so a daemon bounce keeps the cooldown.
    pub last_recovery: u64,
    recoveries: Vec<(ComboKey, u64)>,
}

impl SupervisorState {
    /// A fresh state seeded with a known `last_recovery` (0 when the state file
    /// is absent).
    pub fn new(last_recovery: u64) -> Self {
        SupervisorState {
            last_recovery,
            recoveries: Vec::new(),
        }
    }

    /// Record a recovery for `combo` at `now`, pruning entries older than
    /// [`REPEAT_STUCK_WINDOW_SEC`] so the ledger stays bounded regardless of
    /// how long the daemon runs. Does NOT itself set `last_recovery` — the loop
    /// does that (and persists it) after the restart actually issues.
    pub fn record_recovery(&mut self, combo: ComboKey, now: u64) {
        self.recoveries.push((combo, now));
        self.recoveries
            .retain(|(_, t)| now.saturating_sub(*t) < REPEAT_STUCK_WINDOW_SEC);
    }

    /// All recovery timestamps recorded for `combo` (unfiltered — the caller
    /// applies the window via [`is_repeat_stuck`]).
    pub fn recoveries_for_combo(&self, combo: &ComboKey, _now: u64) -> Vec<u64> {
        self.recoveries
            .iter()
            .filter(|(c, _)| c == combo)
            .map(|(_, t)| *t)
            .collect()
    }
}

// ── Breakfix extension point (Phase 4 seam) ──────────────────────────────────

/// What a [`BreakfixHandler`] decided to do about a repeat-stuck combo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakfixOutcome {
    /// Handler took no corrective action of its own; the daemon must fall back
    /// to its normal restart-recovery. This is the Phase-3 default (Phase 4 not
    /// built) — the loop ALWAYS restart-recovers after a `Deferred`.
    Deferred,
    /// Handler performed its own remediation (Phase 4). The daemon still
    /// restart-recovers as a safe backstop — a handler signalling `Handled`
    /// changes the LOGS, not (yet) the fallback, so a buggy future handler can
    /// never leave a jammed sweep un-restarted.
    Handled,
}

/// The Phase-4 plug point. Phase 3 ships only [`LoggingBreakfixHandler`]; Phase
/// 4 supplies a real impl (invoke breakfix) WITHOUT editing [`tick`] — the loop
/// depends on this trait object, never a concrete handler.
pub trait BreakfixHandler: Send + Sync {
    /// Called when `combo` has crossed the repeat-stuck threshold
    /// (`recovery_count` recoveries within the window). Returns what it did so
    /// the loop can log it; the loop restart-recovers regardless.
    fn handle_repeat_stuck(&self, combo: &ComboKey, recovery_count: usize) -> BreakfixOutcome;
}

/// Phase-3 default: log a structured escalation and defer. No-op remediation —
/// the clean seam Phase 4 replaces.
pub struct LoggingBreakfixHandler;

impl BreakfixHandler for LoggingBreakfixHandler {
    fn handle_repeat_stuck(&self, combo: &ComboKey, recovery_count: usize) -> BreakfixOutcome {
        tracing::warn!(
            "ESCALATION: repeat-stuck combo {} ({recovery_count} recoveries within {}s) — \
             would invoke breakfix here (Phase 4 not yet built); falling back to restart-recovery",
            combo.label(),
            REPEAT_STUCK_WINDOW_SEC
        );
        BreakfixOutcome::Deferred
    }
}

// ── Log-line formatting (pure) ───────────────────────────────────────────────

/// Format an epoch as the bash script's `ts()` did: `date -u
/// +"%Y-%m-%dT%H:%M:%SZ"`. Pure and deterministic (unlike the shell's
/// call-time `date`), so tick lines are reproducible in tests.
pub fn format_epoch(epoch: u64) -> String {
    chrono::DateTime::from_timestamp(epoch as i64, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap())
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Build a tick log line in the EXACT shape the bash watchdog wrote — the
/// operator's monitoring routine parses this verbatim, so the token order,
/// `key=value` spacing, and the `%`/`s` suffixes must not drift:
/// `TIMESTAMP verdict=X gpu_busy=Y% row_age=Zs sweep=A assistant=B`.
/// `sweep`/`assistant` are the RAW `systemctl is-active` strings (e.g.
/// `active`/`inactive`), matching the bash script (which logged the raw
/// `$sweep_active`/`$assistant_active`), NOT a re-derived bool. Pure.
pub fn format_tick_line(
    ts: &str,
    verdict: Verdict,
    gpu_busy: u64,
    age_secs: i64,
    sweep: &ServiceStatus,
    assistant: &ServiceStatus,
) -> String {
    format!(
        "{ts} verdict={} gpu_busy={gpu_busy}% row_age={age_secs}s sweep={} assistant={}",
        verdict.as_str(),
        sweep.as_str(),
        assistant.as_str()
    )
}

/// Build a NEW, clearly-distinguishable escalation log line (a leading
/// `ESCALATION` token after the timestamp, so it can never be confused with a
/// tick line by the operator's parser). Pure.
pub fn format_escalation_line(ts: &str, combo: &ComboKey, recovery_count: usize) -> String {
    format!(
        "{ts} ESCALATION repeat-stuck combo={} recoveries={recovery_count} window={REPEAT_STUCK_WINDOW_SEC}s \
         action=would-invoke-breakfix(phase4) fallback=restart-recovery",
        combo.label()
    )
}

/// The raw result of `systemctl is-active <unit>` — a thin newtype so the raw
/// token (`active`/`inactive`/`failed`/…) is preserved for the log line AND the
/// `is_active()` predicate feeding [`compute_verdict`] can't drift from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus(pub String);

impl ServiceStatus {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The bash script's `[[ "$x" == "active" ]]` predicate.
    pub fn is_active(&self) -> bool {
        self.0 == "active"
    }
}

// ── The environment the loop talks to (trait — makes the loop testable) ──────

/// Everything [`tick`] needs from the outside world. The live impl
/// ([`LiveEnv`]) does real DB/`/sys`/`systemctl`/file I/O; tests inject a
/// scripted fake (mirroring `coder_sweep`'s `CoderSuiteDriver`/`ScriptDriver`
/// pattern) so the loop's decision-making is exercised without a live DB or
/// systemd.
#[async_trait::async_trait]
pub trait SupervisorEnv: Send + Sync {
    /// Wall-clock now, epoch seconds.
    fn now(&self) -> u64;
    /// `/sys/class/drm/card*/device/gpu_busy_percent` (first readable), or 0.
    fn gpu_busy(&self) -> u64;
    /// `systemctl is-active intake-coder-sweep.service`.
    fn sweep_status(&self) -> ServiceStatus;
    /// `systemctl is-active intake-assistant-sweep.service`.
    fn assistant_status(&self) -> ServiceStatus;
    /// `max(created_at)` epoch from `code_profile_runs`; `None` when the query
    /// fails or the table is empty (tick is skipped, matching the bash script's
    /// "could not read last_row_epoch ⇒ skip this tick").
    async fn last_row_epoch(&self) -> Option<i64>;
    /// The `(model, backend, mem_config)` of the most-recent `code_profile_runs`
    /// row — the combo a stuck event is attributed to. `None` when unknown.
    async fn current_combo(&self) -> Option<ComboKey>;
    /// `systemctl restart ollama.service`.
    fn restart_ollama(&self) -> Result<(), String>;
    /// `systemctl restart intake-coder-sweep.service`.
    fn restart_sweep(&self) -> Result<(), String>;
    /// `systemctl restart intake-assistant-sweep.service`.
    fn restart_assistant(&self) -> Result<(), String>;
    /// The bash script's `sleep 5` between the ollama restart and the sweep-unit
    /// restart (let ollama come back up first). No-op in tests.
    async fn settle(&self);
    /// Append one line (a trailing newline is added) to the shared log file.
    fn log_line(&self, line: &str);
    /// Persist `epoch` as the new recovery cooldown anchor (survives a daemon
    /// restart).
    fn persist_last_recovery(&self, epoch: u64);
}

// ── The tick (decision logic wired to the env) ───────────────────────────────

/// Run ONE supervisor tick against `env`, mutating `state` and consulting
/// `breakfix` on a repeat-stuck escalation. This is the faithful port of the
/// bash script's per-tick body, plus the new escalation branch. Structured so
/// the whole decision path is exercised by tests through a scripted
/// [`SupervisorEnv`], with no live DB/systemd.
pub async fn tick(env: &dyn SupervisorEnv, state: &mut SupervisorState, breakfix: &dyn BreakfixHandler) {
    let now = env.now();
    let ts = format_epoch(now);

    // Signals. `last_row_epoch` missing ⇒ skip this tick (bash parity).
    let Some(last_row_epoch) = env.last_row_epoch().await else {
        env.log_line(&format!(
            "{ts} WARN: could not read last_row_epoch from DB, skipping this tick"
        ));
        return;
    };
    let gpu_busy = env.gpu_busy();
    let sweep = env.sweep_status();
    let assistant = env.assistant_status();
    let age = now as i64 - last_row_epoch;

    let verdict = compute_verdict(gpu_busy, age, sweep.is_active(), assistant.is_active());
    env.log_line(&format_tick_line(&ts, verdict, gpu_busy, age, &sweep, &assistant));

    if verdict != Verdict::Stuck {
        return;
    }

    // Anti-thrash cooldown (bash parity).
    if !should_recover(now, state.last_recovery) {
        env.log_line(&format!(
            "{ts} STUCK but recovered {}s ago -- holding off (cooldown)",
            now.saturating_sub(state.last_recovery)
        ));
        return;
    }

    // NEW: attribute this stuck event to a combo and check for repeat-stuck.
    if let Some(combo) = env.current_combo().await {
        state.record_recovery(combo.clone(), now);
        let history = state.recoveries_for_combo(&combo, now);
        if is_repeat_stuck(&history, now) {
            // Phase-4 seam: the handler logs/remediates; we ALSO write the
            // structured escalation line to the shared log file (so it is
            // captured even with no tracing subscriber installed), then fall
            // through to the normal restart-recovery as the safe fallback.
            let _outcome = breakfix.handle_repeat_stuck(&combo, history.len());
            env.log_line(&format_escalation_line(&ts, &combo, history.len()));
        }
    }

    // Recovery restart (bash parity): ollama first, settle, then whichever
    // sweep unit(s) were active this tick.
    env.log_line(&format!(
        "{ts} STUCK detected -- restarting ollama.service then sweep unit(s)"
    ));
    if let Err(e) = env.restart_ollama() {
        env.log_line(&format!("{ts} WARN: ollama restart failed: {e}"));
    }
    env.settle().await;
    if sweep.is_active() {
        if let Err(e) = env.restart_sweep() {
            env.log_line(&format!("{ts} WARN: coder-sweep restart failed: {e}"));
        }
    }
    if assistant.is_active() {
        if let Err(e) = env.restart_assistant() {
            env.log_line(&format!("{ts} WARN: assistant-sweep restart failed: {e}"));
        }
    }
    state.last_recovery = now;
    env.persist_last_recovery(now);
    env.log_line(&format!("{ts} recovery restart issued"));
}

// ── Live I/O impls ───────────────────────────────────────────────────────────

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read the first readable `/sys/class/drm/card*/device/gpu_busy_percent` —
/// the Rust equivalent of the bash `cat .../card*/... | head -1`, defaulting to
/// 0 when nothing is readable (headless/no-amdgpu). Free function so it can be
/// reasoned about independent of [`LiveEnv`].
fn read_gpu_busy() -> u64 {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Only `cardN` roots (not `cardN-CONNECTOR` outputs), matching the glob.
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let path = entry.path().join("device/gpu_busy_percent");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(v) = raw.trim().parse::<u64>() {
                return v;
            }
        }
    }
    0
}

/// `systemctl is-active <unit>` → its raw stdout token. Mirrors the bash
/// `systemctl is-active ... || true`: a non-zero exit (inactive/failed/unknown)
/// still yields the printed token rather than erroring.
fn systemctl_is_active(unit: &str) -> ServiceStatus {
    let out = Command::new("systemctl").args(["is-active", unit]).output();
    let token = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => String::new(),
    };
    // An empty token (spawn failure / no output) reads as not-active, exactly
    // like the bash script's `!= "active"` comparison.
    ServiceStatus(if token.is_empty() { "unknown".to_string() } else { token })
}

/// `systemctl restart <unit>`. Same shell-out idiom as
/// `gpu_authority::run` (spawn, check status, stderr into the error).
fn systemctl_restart(unit: &str) -> Result<(), String> {
    let out = Command::new("systemctl")
        .args(["restart", unit])
        .output()
        .map_err(|e| format!("spawn systemctl restart {unit}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl restart {unit} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The live environment: a held Postgres pool plus the real `/sys`/`systemctl`/
/// file I/O.
pub struct LiveEnv {
    pool: PgPool,
}

impl LiveEnv {
    pub fn new(pool: PgPool) -> Self {
        LiveEnv { pool }
    }

    fn state_file() -> String {
        format!("{STATE_DIR}/last_recovery_epoch")
    }

    /// Read the persisted recovery-cooldown anchor (0 when absent/unparseable) —
    /// used to seed [`SupervisorState`] at startup so a daemon restart keeps the
    /// cooldown.
    pub fn load_last_recovery() -> u64 {
        std::fs::read_to_string(Self::state_file())
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl SupervisorEnv for LiveEnv {
    fn now(&self) -> u64 {
        now_epoch()
    }

    fn gpu_busy(&self) -> u64 {
        read_gpu_busy()
    }

    fn sweep_status(&self) -> ServiceStatus {
        systemctl_is_active(CODER_SWEEP_UNIT)
    }

    fn assistant_status(&self) -> ServiceStatus {
        systemctl_is_active(ASSISTANT_SWEEP_UNIT)
    }

    async fn last_row_epoch(&self) -> Option<i64> {
        // `extract(epoch from max(created_at))::bigint` — the bash query,
        // verbatim. `max(...)` over an empty table is NULL ⇒ inner Option None.
        let res = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT extract(epoch from max(created_at))::bigint FROM code_profile_runs",
        )
        .fetch_one(&self.pool)
        .await;
        match res {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("supervisor: last_row_epoch query failed: {e}");
                None
            }
        }
    }

    async fn current_combo(&self) -> Option<ComboKey> {
        let res = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
            "SELECT p.model_name, r.backend_tag, r.mem_config \
             FROM code_profile_runs r JOIN model_profiles p ON r.profile_id = p.id \
             ORDER BY r.created_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await;
        match res {
            Ok(Some((model, backend, mem_config))) => Some(ComboKey {
                model,
                // A row with no backend_tag (legacy) reads as "unknown" rather
                // than dropping the attribution entirely.
                backend: backend.unwrap_or_else(|| "unknown".to_string()),
                mem_config,
            }),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("supervisor: current_combo query failed: {e}");
                None
            }
        }
    }

    fn restart_ollama(&self) -> Result<(), String> {
        systemctl_restart(OLLAMA_UNIT)
    }

    fn restart_sweep(&self) -> Result<(), String> {
        systemctl_restart(CODER_SWEEP_UNIT)
    }

    fn restart_assistant(&self) -> Result<(), String> {
        systemctl_restart(ASSISTANT_SWEEP_UNIT)
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    fn log_line(&self, line: &str) {
        use std::io::Write;
        // Best-effort append; a logging failure must never crash the daemon.
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(LOG_PATH) {
            let _ = writeln!(f, "{line}");
        }
        // Also emit to the tracing pipeline (journal) for operators watching
        // `journalctl -u mint-supervisor`.
        tracing::info!("{line}");
    }

    fn persist_last_recovery(&self, epoch: u64) {
        let _ = std::fs::create_dir_all(STATE_DIR);
        let _ = std::fs::write(LiveEnv::state_file(), epoch.to_string());
    }
}

// ── Daemon entry point + systemd unit management ─────────────────────────────

/// Run the permanent supervisor daemon: connect the pool once, seed state from
/// the persisted cooldown anchor, then tick every [`TICK_INTERVAL_SEC`] until
/// SIGTERM (graceful shutdown). Returns `FAILURE` only if the initial pool
/// connect fails — a transient per-tick DB error is handled inside the tick
/// (skip), never fatal.
pub async fn run() -> std::process::ExitCode {
    let pool = match super::storage::get_pool().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mint supervisor: cannot connect to intake DB: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let env = LiveEnv::new(pool);
    let mut state = SupervisorState::new(LiveEnv::load_last_recovery());
    let breakfix = LoggingBreakfixHandler;

    env.log_line(&format!(
        "{} mint-supervisor started (tick={TICK_INTERVAL_SEC}s, stuck_threshold={STUCK_THRESHOLD_SEC}s, \
         gpu_busy_min={GPU_BUSY_MIN}%, cooldown={RECOVERY_COOLDOWN_SEC}s)",
        format_epoch(env.now())
    ));

    let mut ticker = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SEC));
    // Don't fire a burst of catch-up ticks if one runs long.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Graceful shutdown on SIGTERM (systemd stop) or Ctrl-C.
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mint supervisor: cannot install SIGTERM handler: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                tick(&env, &mut state, &breakfix).await;
            }
            _ = sigterm.recv() => {
                env.log_line(&format!("{} mint-supervisor received SIGTERM, shutting down", format_epoch(env.now())));
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                env.log_line(&format!("{} mint-supervisor received SIGINT, shutting down", format_epoch(env.now())));
                break;
            }
        }
    }
    std::process::ExitCode::SUCCESS
}

/// Where the daemon's systemd unit lives — the same directory the sweep units
/// occupy on <host>.
pub const UNIT_PATH: &str = "/etc/systemd/system/mint-supervisor.service";

/// Render the `mint-supervisor.service` unit, mirroring the sweep units'
/// convention (`User=root`, `WorkingDirectory=/opt/intake`, `Restart=on-failure`,
/// `RestartSec=10`, `RUST_LOG=info`, `After=... ollama.service`). `exec_path`
/// is the absolute path to the `mint` binary. Pure so the unit's shape is
/// unit-testable without writing any file. The `EnvironmentFile=-` line is
/// optional (leading `-`): the daemon reads its DB URL from the same
/// export-stripped intake env the sweep units use when present, but starts fine
/// without it (e.g. when `INTAKE_DATABASE_URL` is set some other way).
pub fn supervisor_unit_content(exec_path: &str) -> String {
    format!(
        "# /etc/systemd/system/mint-supervisor.service\n\
         # MINT Phase 3: permanent jam-detect + auto-recover for the coder/assistant\n\
         # model sweeps. Replaces the interim sweep-watchdog.timer/.sh stopgap.\n\
         # Service-level restarts only: never touches carveout, never reboots.\n\
         [Unit]\n\
         Description=MINT supervisor (permanent jam-detect + auto-recover for model sweeps)\n\
         Documentation=https://git.example.com/moosenet/terminus\n\
         After=network-online.target ollama.service\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User=root\n\
         WorkingDirectory=/opt/intake\n\
         ExecStart={exec_path} supervisor run\n\
         EnvironmentFile=-/root/intake-staging/intake.env.systemd\n\
         Environment=\"RUST_LOG=info\"\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         SyslogIdentifier=mint-supervisor\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Resolve the running executable's absolute path (for the unit's `ExecStart`),
/// falling back to a plain `mint` on the `PATH` if it can't be determined.
fn current_exe_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "mint".to_string())
}

/// `mint supervisor install`: write the unit, reload systemd, and enable+start
/// it. NOTE: not exercised in this build per the Phase-3 brief (build/test
/// only, no live systemd changes) — but implemented for the eventual deploy.
pub fn install() -> std::process::ExitCode {
    let content = supervisor_unit_content(&current_exe_path());
    if let Err(e) = std::fs::write(UNIT_PATH, content) {
        eprintln!("mint supervisor install: cannot write {UNIT_PATH}: {e}");
        return std::process::ExitCode::FAILURE;
    }
    for args in [
        vec!["daemon-reload"],
        vec!["enable", "mint-supervisor.service"],
        vec!["start", "mint-supervisor.service"],
    ] {
        if let Err(e) = systemctl(&args) {
            eprintln!("mint supervisor install: systemctl {} failed: {e}", args.join(" "));
            return std::process::ExitCode::FAILURE;
        }
    }
    println!("mint-supervisor installed and started ({UNIT_PATH})");
    std::process::ExitCode::SUCCESS
}

/// `mint supervisor uninstall`: stop+disable the unit and remove its file.
/// Best-effort on the stop/disable (a not-installed unit is not an error);
/// removing the file is the load-bearing step.
pub fn uninstall() -> std::process::ExitCode {
    let _ = systemctl(&["stop", "mint-supervisor.service"]);
    let _ = systemctl(&["disable", "mint-supervisor.service"]);
    match std::fs::remove_file(UNIT_PATH) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!("mint supervisor uninstall: cannot remove {UNIT_PATH}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    let _ = systemctl(&["daemon-reload"]);
    println!("mint-supervisor uninstalled");
    std::process::ExitCode::SUCCESS
}

/// Thin `systemctl` shell-out for install/uninstall (distinct from the
/// unit-observing helpers above, which return richer types).
fn systemctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| format!("spawn systemctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ===========================================================================
// Tests — the module's PURE decision logic, plus a hermetic scripted-env test
// of the async `tick` loop's decision-making (no live DB / systemd), mirroring
// coder_sweep's `ScriptDriver` convention.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ---- compute_verdict (bash parity) ----

    #[test]
    fn verdict_idle_when_neither_unit_active_even_if_gpu_pegged() {
        // Idle is checked FIRST: an idle host with a busy GPU (unrelated work)
        // must never read as "stuck".
        assert_eq!(compute_verdict(100, 99_999, false, false), Verdict::Idle);
    }

    #[test]
    fn verdict_stuck_requires_busy_gpu_and_stale_rows() {
        // Both conditions must hold.
        assert_eq!(
            compute_verdict(GPU_BUSY_MIN, STUCK_THRESHOLD_SEC + 1, true, false),
            Verdict::Stuck
        );
        assert_eq!(
            compute_verdict(GPU_BUSY_MIN, STUCK_THRESHOLD_SEC + 1, false, true),
            Verdict::Stuck
        );
    }

    #[test]
    fn verdict_working_when_gpu_idle_or_rows_fresh() {
        // GPU below the floor ⇒ working even with very stale rows.
        assert_eq!(compute_verdict(GPU_BUSY_MIN - 1, 999_999, true, false), Verdict::Working);
        // Rows fresh (age not past the threshold) ⇒ working even with a pegged GPU.
        assert_eq!(compute_verdict(100, STUCK_THRESHOLD_SEC, true, false), Verdict::Working);
    }

    #[test]
    fn verdict_boundary_is_strictly_greater_than_threshold() {
        // Exactly AT the threshold is NOT stuck (bash used `age > THRESHOLD`).
        assert_eq!(compute_verdict(100, STUCK_THRESHOLD_SEC, true, true), Verdict::Working);
        assert_eq!(compute_verdict(100, STUCK_THRESHOLD_SEC + 1, true, true), Verdict::Stuck);
    }

    #[test]
    fn verdict_gpu_busy_boundary_is_inclusive() {
        // bash used `gpu_busy >= GPU_BUSY_MIN`.
        assert_eq!(compute_verdict(GPU_BUSY_MIN, STUCK_THRESHOLD_SEC + 1, true, false), Verdict::Stuck);
        assert_eq!(compute_verdict(GPU_BUSY_MIN - 1, STUCK_THRESHOLD_SEC + 1, true, false), Verdict::Working);
    }

    // ---- should_recover (cooldown) ----

    #[test]
    fn should_recover_true_when_never_recovered() {
        assert!(should_recover(10_000, 0));
    }

    #[test]
    fn should_recover_respects_one_hour_cooldown() {
        let last = 1_000_000;
        assert!(!should_recover(last + RECOVERY_COOLDOWN_SEC, last)); // exactly at: not yet
        assert!(should_recover(last + RECOVERY_COOLDOWN_SEC + 1, last)); // one past: ok
    }

    #[test]
    fn should_recover_saturates_on_backwards_clock() {
        // now < last (clock skew) must never panic or wrongly permit recovery.
        assert!(!should_recover(500, 1000));
    }

    // ---- is_repeat_stuck (escalation window) ----

    #[test]
    fn repeat_stuck_needs_three_within_window() {
        let now = 100_000;
        // Two recent recoveries: not yet repeat-stuck.
        assert!(!is_repeat_stuck(&[now - 10, now - 20], now));
        // Three recent recoveries: repeat-stuck.
        assert!(is_repeat_stuck(&[now - 10, now - 20, now - 30], now));
    }

    #[test]
    fn repeat_stuck_ignores_recoveries_outside_window() {
        let now = 100_000;
        // Three recoveries but two are older than the window ⇒ only one counts.
        let old = now - REPEAT_STUCK_WINDOW_SEC - 1;
        assert!(!is_repeat_stuck(&[old, old - 5, now - 10], now));
    }

    // ---- ComboKey ----

    #[test]
    fn combo_label_renders_null_mem_config_explicitly() {
        let c = ComboKey { model: "qwen3-coder:30b".into(), backend: "gpu".into(), mem_config: None };
        assert_eq!(c.label(), "qwen3-coder:30b:gpu:NULL");
        let c2 = ComboKey {
            model: "qwen3-coder:30b".into(),
            backend: "gpu".into(),
            mem_config: Some("dynamic_gtt".into()),
        };
        assert_eq!(c2.label(), "qwen3-coder:30b:gpu:dynamic_gtt");
    }

    #[test]
    fn combos_differ_by_backend_and_mem_config() {
        let base = ComboKey { model: "m".into(), backend: "gpu".into(), mem_config: Some("dynamic_gtt".into()) };
        assert_ne!(base, ComboKey { backend: "cpu".into(), ..base.clone() });
        assert_ne!(base, ComboKey { mem_config: Some("carveout".into()), ..base.clone() });
        assert_ne!(base, ComboKey { mem_config: None, ..base.clone() });
    }

    // ---- SupervisorState ledger ----

    #[test]
    fn state_records_and_windows_recoveries_per_combo() {
        let a = ComboKey { model: "a".into(), backend: "gpu".into(), mem_config: None };
        let b = ComboKey { model: "b".into(), backend: "gpu".into(), mem_config: None };
        let mut s = SupervisorState::new(0);
        let now = 100_000;
        s.record_recovery(a.clone(), now - 30);
        s.record_recovery(a.clone(), now - 20);
        s.record_recovery(b.clone(), now - 10);
        assert_eq!(s.recoveries_for_combo(&a, now).len(), 2);
        assert_eq!(s.recoveries_for_combo(&b, now).len(), 1);
        // Cross-combo isolation: three total recoveries, but neither combo alone
        // reaches the repeat-stuck threshold.
        assert!(!is_repeat_stuck(&s.recoveries_for_combo(&a, now), now));
        assert!(!is_repeat_stuck(&s.recoveries_for_combo(&b, now), now));
    }

    #[test]
    fn state_prunes_entries_older_than_window() {
        let a = ComboKey { model: "a".into(), backend: "gpu".into(), mem_config: None };
        let mut s = SupervisorState::new(0);
        // An ancient recovery, then a recent one far enough ahead that the
        // ancient one is now outside the window: recording prunes it.
        s.record_recovery(a.clone(), 0);
        s.record_recovery(a.clone(), REPEAT_STUCK_WINDOW_SEC + 100);
        let now = REPEAT_STUCK_WINDOW_SEC + 100;
        assert_eq!(s.recoveries_for_combo(&a, now).len(), 1);
    }

    // ---- log-line format compatibility (load-bearing) ----

    #[test]
    fn tick_line_matches_bash_format_exactly() {
        // The operator's monitor parses this EXACT shape.
        let line = format_tick_line(
            "2026-07-05T00:17:00Z",
            Verdict::Working,
            85,
            42,
            &ServiceStatus("active".into()),
            &ServiceStatus("inactive".into()),
        );
        assert_eq!(
            line,
            "2026-07-05T00:17:00Z verdict=working gpu_busy=85% row_age=42s sweep=active assistant=inactive"
        );
    }

    #[test]
    fn tick_line_stuck_verdict_and_raw_status_tokens() {
        let line = format_tick_line(
            "2026-07-05T01:00:00Z",
            Verdict::Stuck,
            98,
            3600,
            &ServiceStatus("active".into()),
            &ServiceStatus("failed".into()),
        );
        assert_eq!(
            line,
            "2026-07-05T01:00:00Z verdict=stuck gpu_busy=98% row_age=3600s sweep=active assistant=failed"
        );
    }

    #[test]
    fn escalation_line_is_distinguishable_from_tick_lines() {
        let c = ComboKey { model: "qwen3-coder:30b".into(), backend: "gpu".into(), mem_config: Some("dynamic_gtt".into()) };
        let line = format_escalation_line("2026-07-05T02:00:00Z", &c, 3);
        assert!(line.contains("ESCALATION"));
        assert!(line.contains("combo=qwen3-coder:30b:gpu:dynamic_gtt"));
        assert!(line.contains("recoveries=3"));
        // Must NOT look like a tick line (no `verdict=` token to confuse the parser).
        assert!(!line.contains("verdict="));
    }

    #[test]
    fn format_epoch_is_utc_iso8601_z() {
        // 0 = the Unix epoch.
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn service_status_is_active_only_for_active_token() {
        assert!(ServiceStatus("active".into()).is_active());
        assert!(!ServiceStatus("inactive".into()).is_active());
        assert!(!ServiceStatus("failed".into()).is_active());
        assert!(!ServiceStatus("unknown".into()).is_active());
    }

    // ---- systemd unit content ----

    #[test]
    fn unit_content_mirrors_sweep_unit_conventions() {
        let u = supervisor_unit_content("/opt/intake/mint");
        assert!(u.contains("ExecStart=/opt/intake/mint supervisor run"));
        assert!(u.contains("User=root"));
        assert!(u.contains("WorkingDirectory=/opt/intake"));
        assert!(u.contains("Restart=on-failure"));
        assert!(u.contains("RestartSec=10"));
        assert!(u.contains("After=network-online.target ollama.service"));
        assert!(u.contains("WantedBy=multi-user.target"));
        // Optional env file (leading `-`) so a missing file doesn't fail start.
        assert!(u.contains("EnvironmentFile=-/root/intake-staging/intake.env.systemd"));
    }

    // ---- hermetic tick() loop test (scripted env, no DB/systemd) ----

    #[derive(Default)]
    struct Recorder {
        logs: Vec<String>,
        restarted_ollama: usize,
        restarted_sweep: usize,
        restarted_assistant: usize,
        persisted_recovery: Option<u64>,
    }

    /// A fully scripted [`SupervisorEnv`] — mirrors coder_sweep's `ScriptDriver`.
    struct ScriptEnv {
        now: u64,
        gpu_busy: u64,
        sweep: ServiceStatus,
        assistant: ServiceStatus,
        last_row_epoch: Option<i64>,
        combo: Option<ComboKey>,
        rec: Mutex<Recorder>,
    }

    impl ScriptEnv {
        fn stuck_scene() -> Self {
            // GPU pegged, rows very stale, coder sweep active ⇒ stuck.
            ScriptEnv {
                now: 1_000_000,
                gpu_busy: 95,
                sweep: ServiceStatus("active".into()),
                assistant: ServiceStatus("inactive".into()),
                last_row_epoch: Some(1_000_000 - (STUCK_THRESHOLD_SEC + 500)),
                combo: Some(ComboKey { model: "qwen3-coder:30b".into(), backend: "gpu".into(), mem_config: Some("dynamic_gtt".into()) }),
                rec: Mutex::new(Recorder::default()),
            }
        }
    }

    #[async_trait::async_trait]
    impl SupervisorEnv for ScriptEnv {
        fn now(&self) -> u64 { self.now }
        fn gpu_busy(&self) -> u64 { self.gpu_busy }
        fn sweep_status(&self) -> ServiceStatus { self.sweep.clone() }
        fn assistant_status(&self) -> ServiceStatus { self.assistant.clone() }
        async fn last_row_epoch(&self) -> Option<i64> { self.last_row_epoch }
        async fn current_combo(&self) -> Option<ComboKey> { self.combo.clone() }
        fn restart_ollama(&self) -> Result<(), String> { self.rec.lock().unwrap().restarted_ollama += 1; Ok(()) }
        fn restart_sweep(&self) -> Result<(), String> { self.rec.lock().unwrap().restarted_sweep += 1; Ok(()) }
        fn restart_assistant(&self) -> Result<(), String> { self.rec.lock().unwrap().restarted_assistant += 1; Ok(()) }
        async fn settle(&self) {}
        fn log_line(&self, line: &str) { self.rec.lock().unwrap().logs.push(line.to_string()); }
        fn persist_last_recovery(&self, epoch: u64) { self.rec.lock().unwrap().persisted_recovery = Some(epoch); }
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    #[test]
    fn tick_working_scene_logs_but_never_restarts() {
        let env = ScriptEnv {
            gpu_busy: 10, // GPU idle ⇒ working
            ..ScriptEnv::stuck_scene()
        };
        let mut state = SupervisorState::new(0);
        block(tick(&env, &mut state, &LoggingBreakfixHandler));
        let rec = env.rec.lock().unwrap();
        assert_eq!(rec.restarted_ollama, 0);
        assert!(rec.logs.iter().any(|l| l.contains("verdict=working")));
        assert_eq!(rec.persisted_recovery, None);
    }

    #[test]
    fn tick_missing_row_epoch_skips_the_tick() {
        let env = ScriptEnv { last_row_epoch: None, ..ScriptEnv::stuck_scene() };
        let mut state = SupervisorState::new(0);
        block(tick(&env, &mut state, &LoggingBreakfixHandler));
        let rec = env.rec.lock().unwrap();
        assert_eq!(rec.restarted_ollama, 0);
        assert!(rec.logs.iter().any(|l| l.contains("skipping this tick")));
        // No verdict line at all when the tick is skipped.
        assert!(!rec.logs.iter().any(|l| l.contains("verdict=")));
    }

    #[test]
    fn tick_stuck_scene_recovers_only_active_units_and_persists_cooldown() {
        let env = ScriptEnv::stuck_scene(); // coder active, assistant inactive
        let mut state = SupervisorState::new(0);
        block(tick(&env, &mut state, &LoggingBreakfixHandler));
        let rec = env.rec.lock().unwrap();
        assert_eq!(rec.restarted_ollama, 1);
        assert_eq!(rec.restarted_sweep, 1, "active coder sweep restarted");
        assert_eq!(rec.restarted_assistant, 0, "inactive assistant not restarted");
        assert_eq!(rec.persisted_recovery, Some(env.now));
        assert_eq!(state.last_recovery, env.now);
        assert!(rec.logs.iter().any(|l| l.contains("verdict=stuck")));
        assert!(rec.logs.iter().any(|l| l.contains("recovery restart issued")));
    }

    #[test]
    fn tick_stuck_within_cooldown_holds_off() {
        let env = ScriptEnv::stuck_scene();
        // Last recovery was very recent (well within the cooldown).
        let mut state = SupervisorState::new(env.now - 10);
        block(tick(&env, &mut state, &LoggingBreakfixHandler));
        let rec = env.rec.lock().unwrap();
        assert_eq!(rec.restarted_ollama, 0, "cooldown suppresses the restart");
        assert!(rec.logs.iter().any(|l| l.contains("holding off (cooldown)")));
    }

    #[test]
    fn repeat_stuck_scene_emits_escalation_and_still_restarts() {
        // A handler that records that it was consulted.
        struct SpyBreakfix { hits: Mutex<Vec<(String, usize)>> }
        impl BreakfixHandler for SpyBreakfix {
            fn handle_repeat_stuck(&self, combo: &ComboKey, n: usize) -> BreakfixOutcome {
                self.hits.lock().unwrap().push((combo.label(), n));
                BreakfixOutcome::Deferred
            }
        }
        let spy = SpyBreakfix { hits: Mutex::new(Vec::new()) };

        let env = ScriptEnv::stuck_scene();
        let mut state = SupervisorState::new(0);
        // Pre-seed two prior recoveries of the SAME combo within the window, so
        // THIS tick's recovery is the third ⇒ repeat-stuck.
        let combo = env.combo.clone().unwrap();
        state.record_recovery(combo.clone(), env.now - 100);
        state.record_recovery(combo.clone(), env.now - 50);

        block(tick(&env, &mut state, &spy));

        // The breakfix handler was consulted with the right combo and count (3).
        let hits = spy.hits.lock().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "qwen3-coder:30b:gpu:dynamic_gtt");
        assert_eq!(hits[0].1, 3);

        let rec = env.rec.lock().unwrap();
        // Structured escalation line written to the shared log.
        assert!(rec.logs.iter().any(|l| l.contains("ESCALATION") && l.contains("recoveries=3")));
        // AND the safe-fallback restart-recovery still happened.
        assert_eq!(rec.restarted_ollama, 1);
        assert_eq!(rec.restarted_sweep, 1);
    }

    #[test]
    fn repeat_stuck_is_reachable_via_realistic_cooldown_spaced_recoveries() {
        // Caught in review: the OTHER repeat-stuck test above seeds recoveries
        // only 50-100s apart, a sequence `should_recover`'s cooldown gate would
        // never actually produce (recoveries for a combo are always >=
        // RECOVERY_COOLDOWN_SEC apart in real ticks). That let the window
        // constant regress to a value (equal to the cooldown) that made
        // 3-in-a-row escalation mathematically unreachable in the real daemon
        // even though the isolated unit test still passed. This test instead
        // seeds two PRIOR recoveries at the minimum spacing `should_recover`
        // actually permits (just over `RECOVERY_COOLDOWN_SEC` apart, folded
        // back from `now`) and asserts they are BOTH still inside
        // `REPEAT_STUCK_WINDOW_SEC` — i.e. that the window is wide enough for
        // a real, cooldown-respecting sequence to reach the 3rd recovery
        // before the 1st ages out.
        let now = 10_000_000u64;
        let recovery_2 = now - (RECOVERY_COOLDOWN_SEC + 1); // just past cooldown before now
        let recovery_1 = recovery_2 - (RECOVERY_COOLDOWN_SEC + 1); // just past cooldown before recovery_2
        assert!(
            should_recover(recovery_2, recovery_1),
            "recovery_2 must be a real, cooldown-permitted recovery after recovery_1"
        );
        assert!(
            should_recover(now, recovery_2),
            "the 3rd (current) recovery must be cooldown-permitted after recovery_2"
        );
        // With realistically-spaced history, the 3rd recovery at `now` must
        // actually be classified repeat-stuck — this is the exact scenario
        // that was unreachable before REPEAT_STUCK_WINDOW_SEC was widened.
        // All three recoveries (the two prior plus the current one at `now`)
        // must fall within REPEAT_STUCK_WINDOW_SEC for this to trip.
        assert!(
            is_repeat_stuck(&[recovery_1, recovery_2, now], now),
            "REPEAT_STUCK_WINDOW_SEC ({REPEAT_STUCK_WINDOW_SEC}s) must be wide enough to still \
             count a recovery from ~2 cooldown-periods ago"
        );
    }
}
