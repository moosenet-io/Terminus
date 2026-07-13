//! BLD-06 — the compiler SCHEDULER.
//!
//! Reads the durable [`queue`](super::queue) and makes runs happen *gracefully*:
//! small/capped builds go now on the primary; heavy builds (that need the heavy
//! host's idle-mode) are held for a configured build **window** or a fleet-quiet
//! signal. Concurrency is bounded per host, and no two conflicting builds of one
//! module ever run at once (the queue's per-module lock, enforced atomically in
//! [`QueueStore::claim`]).
//!
//! ## Dispatch rule (per tick)
//! Walk the queue in dispatch order (priority, then FIFO). For each job:
//!   1. Pick its host (`heavy` ⇒ heavy, else primary — decided at request time).
//!   2. A HEAVY job is dispatchable only when [`heavy_dispatch_allowed`] holds
//!      (inside a configured window OR a fleet-quiet signal). Otherwise it stays
//!      queued (surfaced in `compiler_status`) — a window closing mid-build never
//!      cancels the in-flight build (no preemption), it only stops NEW heavy
//!      dispatch.
//!   3. Try to [`QueueStore::claim`] it under the host's concurrency cap + the
//!      module lock. On success, spawn the build; on `ModuleBusy`/`HostFull`,
//!      leave it queued for a later tick.
//! Priority orders the QUEUE only — it never preempts a running build.
//!
//! ## Idle-mode seam (BLD-11)
//! A heavy build must acquire the heavy host's idle-mode lease before it runs and
//! release it right after. That coordination is BLD-11's job; here it is a clean
//! trait seam ([`IdleCoordinator`]) whose default is an explicit no-op, called
//! ONLY around a heavy build actually being dispatched. BLD-11 swaps in the real
//! coordinator without touching the scheduler.
//!
//! ## Discipline (S1)
//! Every knob — per-host caps, the build windows, the poll interval — is a config
//! env var with a safe SERIALIZE-everything fallback (cap 1, no heavy window); no
//! hostnames/paths/thresholds are baked in. The real resource caps (Plex-safe
//! memory/CPU) live in [`super::host`] and remain required there.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::compiler::host::HostRole;
use crate::compiler::idle_lease::{IdleModeLease, LeaseError, LeaseGuard};
use crate::compiler::queue::{
    ClaimOutcome, FinalizeOutcome, JobState, QueueError, QueueStore, QueuedJob,
};

// ─────────────────────────────────────────────────────────────────────────────
// Config (S1 — every value from env, with a safe conservative fallback)
// ─────────────────────────────────────────────────────────────────────────────

/// Per-host concurrency cap env vars. Default `1` = fully serialized per host
/// (the safe floor — never overloads a host); an operator raises it per host.
const CAP_PRIMARY_ENV: &str = "BUILD_HOST_CAP_PRIMARY";
const CAP_HEAVY_ENV: &str = "BUILD_HOST_CAP_HEAVY";
/// Heavy-build windows (local hour ranges), e.g. `"22-24,0-6"`. Empty ⇒ NO
/// window configured ⇒ heavy builds wait for a fleet-quiet signal (never a baked
/// default window).
const WINDOW_ENV: &str = "BUILD_WINDOW_HOURS";
/// A fleet-quiet override signal (`1`/`true`/`yes`). The injectable seam for a
/// real fleet-quiet detector; when set, heavy builds may dispatch regardless of
/// the window.
const FLEET_QUIET_ENV: &str = "BUILD_FLEET_QUIET";
/// Scheduler poll interval (secs). Default modest so the queue drains promptly.
const INTERVAL_ENV: &str = "BUILD_SCHED_INTERVAL_SECS";
/// How many queued jobs to consider per tick.
const PEEK_ENV: &str = "BUILD_SCHED_PEEK";
/// Reconcile lease (secs): a `building` job whose claim is older than this with
/// no completion is treated as a crashed worker and requeued. Default an hour —
/// safely longer than any build + the complete-retry window, so a live build is
/// never wrongly requeued.
const STALE_BUILDING_ENV: &str = "BUILD_STALE_BUILDING_SECS";
/// Base backoff (ms) for retrying a completion after a Redis-down failure.
const COMPLETE_RETRY_BASE_ENV: &str = "BUILD_COMPLETE_RETRY_BASE_MS";
/// Max completion retry attempts before falling back to the reconcile backstop.
const COMPLETE_RETRY_MAX_ENV: &str = "BUILD_COMPLETE_RETRY_MAX";

const DEFAULT_CAP: u32 = 1;
const DEFAULT_INTERVAL_SECS: u64 = 15;
const DEFAULT_PEEK: usize = 64;
const DEFAULT_COMPLETE_RETRY_BASE_MS: u64 = 500;
const DEFAULT_COMPLETE_RETRY_MAX: u32 = 20;

/// Headroom (secs) above the max build timeout for the completion-retry window
/// (finalize + release, each ~5min worst-case at the default backoff schedule)
/// before a build is considered stale.
const STALE_RETRY_HEADROOM_SECS: u64 = 900;
/// The SAFE MINIMUM reconcile lease: no `building` job may be reconciled until it
/// has run longer than a full build PLUS the completion-retry window — otherwise
/// a misconfigured tiny value could requeue a genuinely-live long build and start
/// a second concurrent one. Any configured `BUILD_STALE_BUILDING_SECS` below this
/// is clamped UP to it (loudly).
const MIN_STALE_BUILDING_SECS: u64 = super::MAX_BUILD_TIMEOUT_SECS + STALE_RETRY_HEADROOM_SECS;
/// Default reconcile lease: comfortably above the floor.
const DEFAULT_STALE_BUILDING_SECS: u64 = MIN_STALE_BUILDING_SECS * 2;

/// Clamp a configured stale-lease (secs) UP to the safe floor. Returns the
/// effective value and whether it had to be clamped (for a warning). Pure.
fn clamp_stale_secs(configured: u64) -> (u64, bool) {
    if configured < MIN_STALE_BUILDING_SECS {
        (MIN_STALE_BUILDING_SECS, true)
    } else {
        (configured, false)
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(default)
}

/// Is the fleet-quiet override currently set?
pub fn fleet_quiet_from_env() -> bool {
    matches!(
        std::env::var(FLEET_QUIET_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// A single heavy-build window as an hour range `[start, end)` on a 0..=24 clock.
/// `start > end` means the window WRAPS past midnight (e.g. `22-6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: u8,
    pub end: u8,
}

impl Window {
    /// Does `hour` (0..=23) fall in this window? Handles the wrap case.
    pub fn contains(&self, hour: u8) -> bool {
        if self.start == self.end {
            // Degenerate range → empty (never; an operator writes `0-24` for all-day).
            false
        } else if self.start < self.end {
            hour >= self.start && hour < self.end
        } else {
            // Wraps midnight: [start, 24) ∪ [0, end).
            hour >= self.start || hour < self.end
        }
    }
}

/// Parse `BUILD_WINDOW_HOURS` (e.g. `"22-24, 0-6"`) into windows. Bad/empty
/// tokens are skipped; the whole thing empty ⇒ `[]` (no window).
pub fn parse_windows(raw: &str) -> Vec<Window> {
    parse_windows_checked(raw).0
}

/// Like [`parse_windows`] but also reports whether any NON-EMPTY token was
/// invalid (so the caller can warn — a typo that silently drops a window would
/// otherwise make all heavy builds wait indefinitely). Returns `(windows,
/// had_invalid_token)`.
pub fn parse_windows_checked(raw: &str) -> (Vec<Window>, bool) {
    let mut windows = Vec::new();
    let mut had_invalid = false;
    for tok in raw.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let parsed = tok.split_once('-').and_then(|(a, b)| {
            let start: u8 = a.trim().parse().ok()?;
            let end: u8 = b.trim().parse().ok()?;
            // `24` is only meaningful as an END (an all-day/`0-24` upper bound).
            // A START of 24 (e.g. `24-6`) can never be an active hour (`contains`
            // needs `hour >= 24`), so it is a nonsensical never-active window —
            // reject it as invalid so it hits the loud warn path rather than
            // silently stranding every heavy build.
            (start < 24 && end <= 24).then_some(Window { start, end })
        });
        match parsed {
            Some(w) => windows.push(w),
            None => had_invalid = true,
        }
    }
    (windows, had_invalid)
}

/// The pure heavy-dispatch decision: a heavy build may dispatch iff the fleet is
/// signalled quiet OR the current hour is inside a configured window. With no
/// window configured and no quiet signal, heavy builds WAIT (safe default) —
/// surfaced as queued in `compiler_status`.
pub fn heavy_dispatch_allowed(hour: u8, windows: &[Window], fleet_quiet: bool) -> bool {
    fleet_quiet || windows.iter().any(|w| w.contains(hour))
}

/// Resolved scheduler tunables.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub primary_cap: u32,
    pub heavy_cap: u32,
    pub windows: Vec<Window>,
    pub interval: Duration,
    pub peek_limit: usize,
    /// Reconcile lease: a building job older than this (no completion) is requeued.
    pub stale_after: Duration,
    /// Backoff base for retrying a completion after a Redis-down failure.
    pub complete_retry_base: Duration,
    /// Max completion retry attempts (then the reconcile backstop takes over).
    pub complete_retry_max: u32,
}

impl SchedulerConfig {
    pub fn from_env() -> Self {
        let secs = |key: &str, default: u64| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(default)
        };
        let peek = std::env::var(PEEK_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(DEFAULT_PEEK);
        let raw_windows = std::env::var(WINDOW_ENV).unwrap_or_default();
        let (windows, had_invalid) = parse_windows_checked(&raw_windows);
        // Degrade LOUDLY: a typo that drops windows would silently make every
        // heavy build wait forever (absent a fleet-quiet signal).
        if !raw_windows.trim().is_empty() && (had_invalid || windows.is_empty()) {
            tracing::warn!(
                "BUILD_WINDOW_HOURS ({:?}) contains invalid tokens or yielded no valid \
                 windows; heavy builds will wait for a fleet-quiet signal until it is fixed",
                raw_windows
            );
        }
        Self {
            primary_cap: env_u32(CAP_PRIMARY_ENV, DEFAULT_CAP),
            heavy_cap: env_u32(CAP_HEAVY_ENV, DEFAULT_CAP),
            windows,
            interval: Duration::from_secs(secs(INTERVAL_ENV, DEFAULT_INTERVAL_SECS)),
            peek_limit: peek,
            stale_after: {
                let (v, clamped) =
                    clamp_stale_secs(secs(STALE_BUILDING_ENV, DEFAULT_STALE_BUILDING_SECS));
                if clamped {
                    tracing::warn!(
                        "BUILD_STALE_BUILDING_SECS below the safe floor ({}s = max build \
                         timeout + retry window); clamping up to protect live builds",
                        MIN_STALE_BUILDING_SECS
                    );
                }
                Duration::from_secs(v)
            },
            complete_retry_base: Duration::from_millis(std::env::var(COMPLETE_RETRY_BASE_ENV)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(DEFAULT_COMPLETE_RETRY_BASE_MS)),
            complete_retry_max: env_u32(COMPLETE_RETRY_MAX_ENV, DEFAULT_COMPLETE_RETRY_MAX),
        }
    }

    fn cap_for(&self, host: HostRole) -> u32 {
        match host {
            HostRole::Primary => self.primary_cap,
            HostRole::Heavy => self.heavy_cap,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Seams: the build executor and the idle-mode coordinator (BLD-11)
// ─────────────────────────────────────────────────────────────────────────────

/// Runs the actual build for a claimed job. The production impl calls
/// `compiler_build`; the tests use a fake.
#[async_trait]
pub trait BuildExecutor: Send + Sync {
    async fn build(&self, job: &QueuedJob) -> Result<(), String>;
}

/// Coordinates the heavy host's idle-mode lease around a heavy build (BLD-11). This
/// seam is invoked ONLY around a heavy build actually being dispatched. [`acquire`]
/// idles Chord + MINT and WAITS for the freed-RAM budget:
/// - `Ok(guard)` ⇒ enough RAM was freed; run the build while holding the guard. Its
///   drop/watchdog guarantee reactivation even on a crash/hang (see [`LeaseGuard`]).
/// - `Err(`[`LeaseError`]`)` ⇒ the budget could not be freed; the heavy build MUST
///   NOT run — the scheduler requeues it. Both services are already reactivated.
///
/// [`acquire`]: IdleCoordinator::acquire
#[async_trait]
pub trait IdleCoordinator: Send + Sync {
    async fn acquire(&self, job: &QueuedJob) -> Result<LeaseGuard, LeaseError>;
}

/// A no-op idle coordinator (used when no idle-mode wiring is desired): acquires a
/// do-nothing lease so heavy builds run uncoordinated. The production wiring uses
/// [`IdleModeLease`] (see [`Scheduler::from_env`]).
pub struct NoopIdle;

#[async_trait]
impl IdleCoordinator for NoopIdle {
    async fn acquire(&self, job: &QueuedJob) -> Result<LeaseGuard, LeaseError> {
        tracing::debug!(
            module = %job.module,
            "compiler scheduler: heavy build dispatched with the no-op idle coordinator \
             (no Chord/MINT idle-mode wiring)"
        );
        Ok(LeaseGuard::noop())
    }
}

/// The production executor: dispatches to the `compiler_build` tool with the
/// host the scheduler selected.
pub struct CompilerBuildExecutor;

#[async_trait]
impl BuildExecutor for CompilerBuildExecutor {
    async fn build(&self, job: &QueuedJob) -> Result<(), String> {
        super::invoke_build(&job.module, &job.git_ref, job.heavy)
            .await
            .map_err(|e| e.to_string())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The scheduler
// ─────────────────────────────────────────────────────────────────────────────

/// What one [`Scheduler::tick_once`] did (counts are ids for test assertions).
pub struct TickReport {
    /// Crashed-mid-build jobs requeued by the reconcile backstop this tick.
    pub reconciled: Vec<String>,
    /// Finished-but-unreleased jobs released by reconcile WITHOUT a rebuild.
    pub self_healed: Vec<String>,
    /// Jobs claimed + dispatched to a build this tick.
    pub dispatched: Vec<String>,
    /// Heavy jobs held because they're outside a window and the fleet isn't quiet.
    pub held_window: Vec<String>,
    /// Jobs left queued by a module lock / host cap (they retry next tick).
    pub contended: Vec<String>,
    /// The queue was unreachable this tick (degrade; retry next tick).
    pub unavailable: bool,
    /// Join handles for the dispatched build tasks (production detaches them;
    /// tests await them to observe the fake executor).
    pub handles: Vec<tokio::task::JoinHandle<()>>,
}

/// The compiler scheduler: turns queued readiness into gracefully-serialized
/// builds. Cheap to clone-share via `Arc`.
pub struct Scheduler {
    queue: Arc<dyn QueueStore>,
    executor: Arc<dyn BuildExecutor>,
    idle: Arc<dyn IdleCoordinator>,
    config: SchedulerConfig,
}

impl Scheduler {
    pub fn new(
        queue: Arc<dyn QueueStore>,
        executor: Arc<dyn BuildExecutor>,
        idle: Arc<dyn IdleCoordinator>,
        config: SchedulerConfig,
    ) -> Self {
        Self {
            queue,
            executor,
            idle,
            config,
        }
    }

    /// The production scheduler over the shared durable queue, the real
    /// `compiler_build` executor, and the real BLD-11 idle-mode coordinator
    /// ([`IdleModeLease`], which idles Chord + MINT around a heavy build).
    /// `None` when Redis is not configured (nothing to schedule).
    pub fn from_env() -> Option<Self> {
        let queue = super::queue::RedisQueue::from_env()?;
        Some(Self::new(
            Arc::new(queue),
            Arc::new(CompilerBuildExecutor),
            Arc::new(IdleModeLease::from_env()),
            SchedulerConfig::from_env(),
        ))
    }

    /// One scheduling pass. Pure w.r.t. the clock — `hour` (0..=23, local) and
    /// `fleet_quiet` are passed in so it is deterministically testable; the run
    /// loop supplies the live values.
    pub async fn tick_once(&self, hour: u8, fleet_quiet: bool) -> TickReport {
        let mut report = TickReport {
            reconciled: Vec::new(),
            self_healed: Vec::new(),
            dispatched: Vec::new(),
            held_window: Vec::new(),
            contended: Vec::new(),
            unavailable: false,
            handles: Vec::new(),
        };
        // Crash/restart backstop FIRST: free the module lock + host slot of stale
        // building jobs before we try to dispatch. A FINISHED-but-unreleased job is
        // released only (self-healed, NOT rebuilt); a CRASHED one is requeued.
        match self.queue.reconcile(self.config.stale_after).await {
            Ok(mut rep) => {
                report.reconciled.append(&mut rep.requeued);
                report.self_healed.append(&mut rep.released);
            }
            // reconcile only ever fails Unavailable; any error degrades this tick.
            Err(_) => {
                report.unavailable = true;
                return report;
            }
        }
        let jobs = match self.queue.peek(self.config.peek_limit).await {
            Ok(j) => j,
            Err(_) => {
                report.unavailable = true;
                return report;
            }
        };
        for job in jobs {
            let host = if job.heavy {
                HostRole::Heavy
            } else {
                HostRole::Primary
            };
            // Heavy builds are window/quiet gated; small builds go straight to
            // the claim (which enforces the host cap + module lock).
            if job.heavy && !heavy_dispatch_allowed(hour, &self.config.windows, fleet_quiet) {
                report.held_window.push(job.job_id.clone());
                continue;
            }
            match self
                .queue
                .claim(&job.job_id, &job.module, host, self.config.cap_for(host))
                .await
            {
                Ok(ClaimOutcome::Claimed { token }) => {
                    report.dispatched.push(job.job_id.clone());
                    report.handles.push(self.spawn_build(job, host, token));
                }
                Ok(ClaimOutcome::ModuleBusy) | Ok(ClaimOutcome::HostFull) => {
                    report.contended.push(job.job_id.clone());
                }
                Ok(ClaimOutcome::NotQueued) => {}
                Ok(ClaimOutcome::Rejected) => {
                    // A module-arg mismatch is a bug, not a normal contention;
                    // skip it (never retried into a wrong lock) and surface it.
                    tracing::error!(
                        module = %job.module, job = %job.job_id,
                        "compiler scheduler: claim rejected (module arg mismatched the job's \
                         stored module) — skipping"
                    );
                }
                Err(_) => {
                    report.unavailable = true;
                    break;
                }
            }
        }
        report
    }

    /// Spawn the build for a claimed job: acquire idle-mode (heavy only), run the
    /// build, release idle-mode, then complete in two durable steps.
    ///
    /// Completion is `finalize` (durably record the terminal outcome) THEN
    /// `release` (free the lock/slot). Both are token-fenced + idempotent and are
    /// RETRIED with bounded backoff on a Redis-down failure. Recording the outcome
    /// FIRST means that if `release` never lands, the reconcile backstop finds the
    /// marker and RELEASES the job (never rebuilds it) — a finished build is never
    /// re-run. If `finalize` itself never lands (Redis down since the build
    /// finished), reconcile has no marker and treats it as a crash (requeue) — the
    /// safe fallback. Either way the lock/slot can never be permanently wedged.
    fn spawn_build(
        &self,
        job: QueuedJob,
        host: HostRole,
        token: String,
    ) -> tokio::task::JoinHandle<()> {
        let queue = self.queue.clone();
        let executor = self.executor.clone();
        let idle = self.idle.clone();
        let (base, max_attempts) =
            (self.config.complete_retry_base, self.config.complete_retry_max);
        tokio::spawn(async move {
            // HEAVY builds take the idle-mode lease FIRST (idle Chord+MINT, wait for
            // the freed-RAM budget). If it can't be acquired (budget never freed),
            // the build MUST NOT run: requeue the job (token-fenced) so a later tick
            // retries, and STOP — never build under budget. The `LeaseGuard` is held
            // across the whole build; on a normal return we release it explicitly
            // below, and on an early return / PANIC its `Drop` reactivates both
            // services (a crashed build never leaves them stuck idle).
            let lease = if job.heavy {
                match idle.acquire(&job).await {
                    Ok(guard) => Some(guard),
                    Err(e) => {
                        tracing::warn!(
                            module = %job.module, job = %job.job_id,
                            "compiler scheduler: heavy build could not acquire the idle-mode \
                             lease ({e}); requeueing (NOT building under budget)"
                        );
                        // Token-fenced requeue: free the module lock + host slot and
                        // return the job to the queue (clearing stale error state). A
                        // stale token (reconciled/re-claimed) is a safe no-op.
                        if let Err(re) = queue
                            .requeue(&job.job_id, &job.module, host, &token, "idle-lease-unavailable")
                            .await
                        {
                            tracing::error!(
                                module = %job.module, job = %job.job_id,
                                "compiler scheduler: requeue after idle-lease failure did not \
                                 land ({re}); the reconcile backstop will requeue it"
                            );
                        }
                        return;
                    }
                }
            } else {
                None
            };
            // PANIC BOUNDARY: run the build in a nested task so a panicking build
            // surfaces here as a `JoinError` instead of unwinding THIS scheduler task.
            // That makes crash handling DETERMINISTIC: we always fall through to
            // release the idle lease AND finalize+release the queue job (Failed),
            // rather than leaking the claim to the (slow) reconcile backstop.
            let build_outcome = {
                let job_for_build = job.clone();
                let exec = executor.clone();
                tokio::spawn(async move { exec.build(&job_for_build).await }).await
            };
            let result: Result<(), String> = match build_outcome {
                Ok(r) => r,
                Err(join_err) => {
                    tracing::error!(
                        module = %job.module, job = %job.job_id,
                        "compiler scheduler: build task PANICKED ({join_err}); releasing the \
                         idle lease and finalizing the job as Failed (deterministic, not via \
                         the reconcile backstop)"
                    );
                    Err(format!("build panicked: {join_err}"))
                }
            };
            // Release the lease (awaited, deterministic) whether the build returned
            // Ok, Err, or PANICKED — the panic was caught above so we ALWAYS reach
            // here. The guard's `Drop` remains a backstop for any other early return.
            if let Some(guard) = lease {
                guard.release().await;
            }
            let state = if result.is_ok() {
                JobState::Done
            } else {
                JobState::Failed
            };
            // A bounded-backoff retry helper for a single idempotent completion
            // step; returns false if every attempt failed (reconcile then covers it).
            async fn retry<F, Fut>(base: Duration, max: u32, mut op: F) -> bool
            where
                F: FnMut() -> Fut,
                Fut: std::future::Future<Output = Result<(), QueueError>>,
            {
                for attempt in 0..max.max(1) {
                    match op().await {
                        Ok(()) => return true,
                        Err(_) => {
                            let backoff = base.saturating_mul(1u32 << attempt.min(5));
                            tokio::time::sleep(backoff).await;
                        }
                    }
                }
                false
            }

            // STEP 1: durably record the terminal outcome FIRST (so reconcile can
            // release, not rebuild, a finished job). A STALE token means this job
            // was reconciled + re-claimed (or already released) — we no longer own
            // it, so STOP (do not retry, do not release the new claim's slot).
            let mut finalized = false;
            for attempt in 0..max_attempts.max(1) {
                match queue.finalize(&job.job_id, state, &token).await {
                    Ok(FinalizeOutcome::Finalized) => {
                        finalized = true;
                        break;
                    }
                    Ok(FinalizeOutcome::StaleToken) => {
                        tracing::warn!(
                            module = %job.module, job = %job.job_id,
                            "compiler scheduler: completion token is stale (job reconciled/\
                             re-claimed); yielding without releasing the current claim"
                        );
                        return;
                    }
                    Err(_) => {
                        tokio::time::sleep(base.saturating_mul(1u32 << attempt.min(5))).await;
                    }
                }
            }
            if !finalized {
                tracing::error!(
                    module = %job.module, job = %job.job_id,
                    "compiler scheduler: could not record build outcome after retries; \
                     reconcile will treat it as a crash and requeue"
                );
                return;
            }
            // STEP 2: free the lock/slot.
            let released = retry(base, max_attempts, || {
                queue.release(&job.job_id, &job.module, host, &token)
            })
            .await;
            if !released {
                tracing::error!(
                    module = %job.module, job = %job.job_id,
                    "compiler scheduler: build finished + recorded but release still \
                     failing after retries; the reconcile backstop will release it (no rebuild)"
                );
            }
        })
    }

    /// Run the scheduler forever, polling at the configured interval and driving
    /// each tick off the live local hour + fleet-quiet signal. Dispatched build
    /// tasks are detached (they release their own queue slot on completion).
    pub async fn run_forever(self: Arc<Self>) {
        use chrono::Timelike;
        let interval = self.config.interval;
        loop {
            let hour = chrono::Local::now().hour() as u8;
            let report = self.tick_once(hour, fleet_quiet_from_env()).await;
            // Detach the build tasks; they self-release the queue slot.
            drop(report.handles);
            tokio::time::sleep(interval).await;
        }
    }

    /// Spawn the run loop as a background task. Called once at startup when Redis
    /// is configured (see [`super::register`]).
    pub fn spawn(self) {
        let this = Arc::new(self);
        tokio::spawn(this.run_forever());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::queue::{fake::InMemoryQueue, JobRequest, Priority};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A fake executor that records which jobs it built (and optionally fails).
    struct RecordingExecutor {
        built: Mutex<Vec<String>>,
        fail: bool,
    }
    impl RecordingExecutor {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                built: Mutex::new(Vec::new()),
                fail,
            })
        }
    }
    #[async_trait]
    impl BuildExecutor for RecordingExecutor {
        async fn build(&self, job: &QueuedJob) -> Result<(), String> {
            self.built.lock().unwrap().push(job.module.clone());
            if self.fail {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
    }

    /// An executor that PANICS mid-build, to prove the scheduler's panic boundary
    /// releases the lease AND finalizes+releases the queue job deterministically.
    struct PanickingExecutor;
    #[async_trait]
    impl BuildExecutor for PanickingExecutor {
        async fn build(&self, _job: &QueuedJob) -> Result<(), String> {
            panic!("simulated build crash");
        }
    }

    /// A fake idle coordinator counting acquires (to prove idle-mode is touched
    /// ONLY for heavy builds) and releases (via a real counting `LeaseGuard`), and
    /// optionally failing the acquire (to exercise the insufficient-RAM abort +
    /// requeue path — the build must NOT run).
    struct CountingIdle {
        acquires: AtomicUsize,
        releases: Arc<AtomicUsize>,
        /// When set, `acquire` returns `InsufficientRam` (never a lease).
        fail_acquire: bool,
    }
    impl CountingIdle {
        fn new(fail_acquire: bool) -> Self {
            Self {
                acquires: AtomicUsize::new(0),
                releases: Arc::new(AtomicUsize::new(0)),
                fail_acquire,
            }
        }
    }
    #[async_trait]
    impl IdleCoordinator for CountingIdle {
        async fn acquire(&self, _job: &QueuedJob) -> Result<LeaseGuard, LeaseError> {
            self.acquires.fetch_add(1, Ordering::SeqCst);
            if self.fail_acquire {
                Err(LeaseError::InsufficientRam {
                    available_gb: 0.0,
                    budget_gb: 100.0,
                })
            } else {
                Ok(crate::compiler::idle_lease::test_guard(self.releases.clone()))
            }
        }
    }

    fn req(module: &str, git_ref: &str, heavy: bool) -> JobRequest {
        JobRequest {
            module: module.into(),
            git_ref: git_ref.into(),
            priority: Priority::Normal,
            heavy,
            ready: true,
        }
    }

    fn sched(
        q: Arc<InMemoryQueue>,
        ex: Arc<dyn BuildExecutor>,
        idle: Arc<dyn IdleCoordinator>,
        cfg: SchedulerConfig,
    ) -> Scheduler {
        Scheduler::new(q, ex, idle, cfg)
    }

    fn cfg(primary_cap: u32, heavy_cap: u32, windows: Vec<Window>) -> SchedulerConfig {
        SchedulerConfig {
            primary_cap,
            heavy_cap,
            windows,
            interval: Duration::from_secs(1),
            peek_limit: 64,
            // Long lease by default so tests that don't exercise reconcile never
            // trip it; fast retry so the retry test doesn't sleep long.
            stale_after: Duration::from_secs(3600),
            complete_retry_base: Duration::from_millis(1),
            complete_retry_max: 50,
        }
    }

    #[test]
    fn malformed_window_tokens_are_flagged_for_a_loud_warning() {
        // B5: a typo in BUILD_WINDOW_HOURS must be detectable (the operator is
        // warned) — silently dropping windows would make heavy builds wait forever.
        let (ws, had_invalid) = parse_windows_checked("22-24, junk, 30-40");
        assert_eq!(ws, vec![Window { start: 22, end: 24 }]);
        assert!(had_invalid, "invalid tokens must be reported");
        // An all-invalid string yields zero windows AND flags invalid.
        let (ws, had_invalid) = parse_windows_checked("nope, 99-100");
        assert!(ws.is_empty() && had_invalid);
        // A clean string flags nothing.
        let (ws, had_invalid) = parse_windows_checked("22-24, 0-6");
        assert_eq!(ws.len(), 2);
        assert!(!had_invalid);
        // Empty / whitespace-only is not "invalid" (just no window configured).
        let (ws, had_invalid) = parse_windows_checked("  ,  ");
        assert!(ws.is_empty() && !had_invalid);
    }

    #[test]
    fn window_start_of_24_is_rejected_as_invalid() {
        // A START of 24 is a never-active window (e.g. `24-6`); it must be flagged
        // invalid (loud warn), not silently accepted (which would strand heavy builds).
        let (ws, had_invalid) = parse_windows_checked("24-6");
        assert!(ws.is_empty() && had_invalid, "start=24 must be rejected as invalid");
        let (ws, had_invalid) = parse_windows_checked("24-24");
        assert!(ws.is_empty() && had_invalid);
        // A mix: the bad start=24 token is dropped + flagged; the good one parses.
        let (ws, had_invalid) = parse_windows_checked("24-6, 0-6");
        assert_eq!(ws, vec![Window { start: 0, end: 6 }]);
        assert!(had_invalid);
        // Legitimate ranges still parse: a wrap (22-6), an all-day (0-24), and a
        // normal range — none flagged.
        let (ws, had_invalid) = parse_windows_checked("22-6");
        assert_eq!(ws, vec![Window { start: 22, end: 6 }]);
        assert!(!had_invalid);
        assert!(ws[0].contains(23) && ws[0].contains(3) && !ws[0].contains(12));
        let (ws, had_invalid) = parse_windows_checked("0-24");
        assert_eq!(ws, vec![Window { start: 0, end: 24 }]);
        assert!(!had_invalid);
        assert!(ws[0].contains(0) && ws[0].contains(23), "0-24 is all-day");
    }

    #[test]
    fn window_parse_and_contains_with_wrap() {
        let ws = parse_windows("22-24, 0-6, junk, 30-40");
        assert_eq!(ws, vec![Window { start: 22, end: 24 }, Window { start: 0, end: 6 }]);
        assert!(Window { start: 22, end: 24 }.contains(23));
        assert!(!Window { start: 22, end: 24 }.contains(21));
        // Wrap window 22-6 covers 23 and 3, not 12.
        let w = Window { start: 22, end: 6 };
        assert!(w.contains(23) && w.contains(3) && !w.contains(12));
    }

    #[test]
    fn heavy_gate_needs_window_or_quiet() {
        let windows = vec![Window { start: 0, end: 6 }];
        assert!(heavy_dispatch_allowed(3, &windows, false)); // inside window
        assert!(!heavy_dispatch_allowed(12, &windows, false)); // outside, not quiet
        assert!(heavy_dispatch_allowed(12, &windows, true)); // fleet-quiet override
        assert!(!heavy_dispatch_allowed(12, &[], false)); // no window, not quiet → wait
    }

    #[tokio::test]
    async fn two_agents_same_module_ref_yields_one_build() {
        let q = Arc::new(InMemoryQueue::new());
        // Two agents mark the same module@ref ready.
        q.enqueue(&req("chord", "abc", false)).await.unwrap();
        q.enqueue(&req("chord", "abc", false)).await.unwrap();
        let ex = RecordingExecutor::new(false);
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), cfg(2, 1, vec![]));
        let report = s.tick_once(12, false).await;
        for h in report.handles {
            h.await.unwrap();
        }
        assert_eq!(ex.built.lock().unwrap().len(), 1, "coalesced to exactly one build");
    }

    #[tokio::test]
    async fn heavy_build_held_outside_window_then_dispatched_inside() {
        let q = Arc::new(InMemoryQueue::new());
        q.enqueue(&req("harmony", "big", true)).await.unwrap();
        let ex = RecordingExecutor::new(false);
        let idle = Arc::new(CountingIdle::new(false));
        let s = sched(
            q.clone(),
            ex.clone(),
            idle.clone(),
            cfg(1, 1, vec![Window { start: 0, end: 6 }]),
        );
        // Outside the window, not quiet → held, no build, no idle acquire.
        let r1 = s.tick_once(12, false).await;
        assert_eq!(r1.held_window.len(), 1);
        assert!(r1.dispatched.is_empty());
        assert_eq!(idle.acquires.load(Ordering::SeqCst), 0);
        // Inside the window → dispatched, idle-mode acquired+released exactly once.
        let r2 = s.tick_once(3, false).await;
        assert_eq!(r2.dispatched.len(), 1);
        for h in r2.handles {
            h.await.unwrap();
        }
        assert_eq!(*ex.built.lock().unwrap(), vec!["harmony".to_string()]);
        assert_eq!(idle.acquires.load(Ordering::SeqCst), 1);
        assert_eq!(idle.releases.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn heavy_build_under_budget_is_requeued_and_never_built() {
        // BLD-11: when the idle lease can't be acquired (insufficient freed RAM),
        // the heavy build must NOT run and must NOT be lost — it is requeued for a
        // later tick. `fail_acquire=true` makes the coordinator return InsufficientRam.
        let q = Arc::new(InMemoryQueue::new());
        let enq = q.enqueue(&req("harmony", "big", true)).await.unwrap();
        let ex = RecordingExecutor::new(false);
        let idle = Arc::new(CountingIdle::new(true));
        let s = sched(q.clone(), ex.clone(), idle.clone(), cfg(1, 1, vec![]));
        // Fleet-quiet so the heavy build is dispatch-eligible (window gate passes),
        // isolating the idle-lease gate as the reason it doesn't run.
        let r = s.tick_once(12, true).await;
        assert_eq!(r.dispatched.len(), 1, "claimed + spawned this tick");
        for h in r.handles {
            h.await.unwrap();
        }
        // Acquire was attempted, but the build NEVER ran (never under budget)...
        assert_eq!(idle.acquires.load(Ordering::SeqCst), 1);
        assert!(
            ex.built.lock().unwrap().is_empty(),
            "under-budget heavy build must never execute"
        );
        // ...and the job was requeued (back to `queued`, no terminal outcome), so a
        // later tick can retry it once the host is quiet enough to free the budget.
        assert_eq!(q.state_of(&enq.job_id).as_deref(), Some("queued"));
        assert!(!q.has_outcome(&enq.job_id), "no terminal outcome recorded");
        assert_eq!(
            q.inflight_count(HostRole::Heavy),
            0,
            "host slot freed by the requeue"
        );
        // A subsequent tick with a coordinator that CAN acquire builds it exactly once.
        let idle_ok = Arc::new(CountingIdle::new(false));
        let s2 = sched(q.clone(), ex.clone(), idle_ok.clone(), cfg(1, 1, vec![]));
        let r2 = s2.tick_once(12, true).await;
        assert_eq!(r2.dispatched.len(), 1, "the requeued job is dispatchable again");
        for h in r2.handles {
            h.await.unwrap();
        }
        assert_eq!(*ex.built.lock().unwrap(), vec!["harmony".to_string()]);
        assert_eq!(idle_ok.releases.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn panicking_heavy_build_releases_lease_and_finalizes_through_dispatch() {
        // FINDING 3: a build that PANICS must, END-TO-END THROUGH DISPATCH (not just a
        // simulated guard drop), (a) release the idle lease and (b) finalize+release
        // the queue job as Failed — deterministically, not left to reconcile.
        let q = Arc::new(InMemoryQueue::new());
        let enq = q.enqueue(&req("harmony", "big", true)).await.unwrap();
        let ex: Arc<dyn BuildExecutor> = Arc::new(PanickingExecutor);
        let idle = Arc::new(CountingIdle::new(false));
        let s = sched(q.clone(), ex, idle.clone(), cfg(1, 1, vec![]));
        let r = s.tick_once(12, true).await; // fleet-quiet ⇒ heavy dispatchable
        assert_eq!(r.dispatched.len(), 1);
        // The scheduler task itself must NOT panic (the boundary caught it).
        for h in r.handles {
            h.await.expect("scheduler task survived the build panic");
        }
        // (a) idle lease released despite the panic.
        assert_eq!(
            idle.releases.load(Ordering::SeqCst),
            1,
            "idle lease released after a panicking build"
        );
        // (b) job finalized as Failed + released: terminal state, slot freed.
        assert_eq!(q.state_of(&enq.job_id).as_deref(), Some("failed"));
        assert!(q.has_outcome(&enq.job_id), "terminal outcome recorded");
        assert_eq!(q.inflight_count(HostRole::Heavy), 0, "host slot freed");
    }

    #[tokio::test]
    async fn small_job_coalesced_to_heavy_becomes_window_gated() {
        let q = Arc::new(InMemoryQueue::new());
        // First request is small (would dispatch immediately on primary)...
        q.enqueue(&req("harmony", "r", false)).await.unwrap();
        // ...but a later heavy request for the same module@ref upgrades it.
        q.enqueue(&req("harmony", "r", true)).await.unwrap();
        let ex = RecordingExecutor::new(false);
        let idle = Arc::new(CountingIdle::new(false));
        let s = sched(
            q.clone(),
            ex.clone(),
            idle.clone(),
            cfg(1, 1, vec![Window { start: 0, end: 6 }]),
        );
        // Outside the window: the now-heavy job is HELD, not dispatched on primary.
        let r = s.tick_once(12, false).await;
        assert_eq!(r.held_window.len(), 1, "coalesced-to-heavy job is window-gated");
        assert!(r.dispatched.is_empty());
        assert!(ex.built.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn idle_mode_untouched_for_small_builds() {
        let q = Arc::new(InMemoryQueue::new());
        q.enqueue(&req("terminus", "r", false)).await.unwrap();
        let ex = RecordingExecutor::new(false);
        let idle = Arc::new(CountingIdle::new(false));
        let s = sched(q.clone(), ex.clone(), idle.clone(), cfg(1, 1, vec![]));
        let r = s.tick_once(12, false).await;
        for h in r.handles {
            h.await.unwrap();
        }
        assert_eq!(idle.acquires.load(Ordering::SeqCst), 0, "small build never acquires idle-mode");
    }

    #[tokio::test]
    async fn per_host_cap_holds_second_build() {
        let q = Arc::new(InMemoryQueue::new());
        q.enqueue(&req("m1", "r", false)).await.unwrap();
        q.enqueue(&req("m2", "r", false)).await.unwrap();
        let ex = RecordingExecutor::new(false);
        // cap=1 on primary: only one dispatches this tick; the other is contended.
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), cfg(1, 1, vec![]));
        let r = s.tick_once(12, false).await;
        assert_eq!(r.dispatched.len(), 1);
        assert_eq!(r.contended.len(), 1);
        // Do NOT await the build (leave the slot busy); a fresh tick still can't
        // dispatch the second while the first holds the only slot.
        let r2 = s.tick_once(12, false).await;
        assert_eq!(r2.dispatched.len(), 0, "host cap still bounds concurrency");
        assert_eq!(r2.contended.len(), 1);
        for h in r.handles.into_iter().chain(r2.handles) {
            h.await.unwrap();
        }
        // After the first build releases, a subsequent tick dispatches the second.
        let r3 = s.tick_once(12, false).await;
        assert_eq!(r3.dispatched.len(), 1);
        for h in r3.handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn failed_build_still_releases_the_slot() {
        let q = Arc::new(InMemoryQueue::new());
        q.enqueue(&req("m1", "r1", false)).await.unwrap();
        let ex = RecordingExecutor::new(true); // build fails
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), cfg(1, 1, vec![]));
        let r = s.tick_once(12, false).await;
        for h in r.handles {
            h.await.unwrap();
        }
        // The failed build released its host slot + module lock, so a new job of
        // the SAME module can now be claimed (no wedged lock).
        let j2 = q.enqueue(&req("m1", "r2", false)).await.unwrap();
        assert!(matches!(
            q.claim(&j2.job_id, "m1", HostRole::Primary, 1).await.unwrap(),
            ClaimOutcome::Claimed { .. }
        ));
    }

    #[tokio::test]
    async fn queue_unavailable_degrades_without_panic() {
        let q = Arc::new(InMemoryQueue::new());
        q.set_down(true);
        let ex = RecordingExecutor::new(false);
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), cfg(1, 1, vec![]));
        let r = s.tick_once(12, false).await;
        assert!(r.unavailable);
        assert!(r.dispatched.is_empty());
    }

    #[tokio::test]
    async fn completion_retried_after_redis_down_eventually_releases_the_slot() {
        let q = Arc::new(InMemoryQueue::new());
        q.enqueue(&req("m1", "r1", false)).await.unwrap();
        // The first 3 RELEASE attempts fail (Redis-down after the build finished
        // + outcome was recorded), then succeed — the retry loop self-heals.
        q.fail_releases(3);
        let ex = RecordingExecutor::new(false);
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), cfg(1, 1, vec![]));
        let r = s.tick_once(12, false).await;
        assert_eq!(r.dispatched.len(), 1);
        // Await the build task: it retries release until it lands.
        for h in r.handles {
            h.await.unwrap();
        }
        // The slot + module lock were released once Redis came back — a new job
        // of the same module can claim (the wedge self-healed).
        assert_eq!(q.inflight_count(HostRole::Primary), 0, "host slot released after retries");
        let j2 = q.enqueue(&req("m1", "r2", false)).await.unwrap();
        assert!(matches!(
            q.claim(&j2.job_id, "m1", HostRole::Primary, 1).await.unwrap(),
            ClaimOutcome::Claimed { .. }
        ));
    }

    #[tokio::test]
    async fn tick_reconciles_a_stale_building_job_and_redispatches_it() {
        let q = Arc::new(InMemoryQueue::new());
        let j = q.enqueue(&req("m1", "r1", false)).await.unwrap();
        // Simulate a crashed worker: the job is claimed (building) but never
        // completes, and its claim is old.
        let _ = q.claim(&j.job_id, "m1", HostRole::Primary, 1).await.unwrap();
        q.backdate_started(&j.job_id, 60_000);
        assert_eq!(q.inflight_count(HostRole::Primary), 1);
        let ex = RecordingExecutor::new(false);
        // Zero-length lease so the tick's reconcile treats the stale job as
        // reclaimable, frees its lock/slot, and re-dispatches it the same tick.
        let mut c = cfg(1, 1, vec![]);
        c.stale_after = Duration::ZERO;
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), c);
        let r = s.tick_once(12, false).await;
        assert_eq!(r.reconciled, vec![j.job_id.clone()], "stale job requeued by tick");
        assert_eq!(r.dispatched, vec![j.job_id.clone()], "and re-dispatched");
        for h in r.handles {
            h.await.unwrap();
        }
        assert_eq!(*ex.built.lock().unwrap(), vec!["m1".to_string()]);
        assert_eq!(q.inflight_count(HostRole::Primary), 0, "slot freed after the rebuild");
    }

    #[tokio::test]
    async fn finished_but_unreleased_job_is_released_not_rebuilt_by_tick() {
        // C: the worker FINISHED (outcome recorded) but its release never landed;
        // the tick's reconcile must RELEASE it, never rebuild it.
        let q = Arc::new(InMemoryQueue::new());
        let j = q.enqueue(&req("m1", "r1", false)).await.unwrap();
        let tok = match q.claim(&j.job_id, "m1", HostRole::Primary, 1).await.unwrap() {
            ClaimOutcome::Claimed { token } => token,
            other => panic!("{other:?}"),
        };
        // Build finished + outcome durably recorded, but release is stuck + stale.
        q.finalize(&j.job_id, JobState::Done, &tok).await.unwrap();
        q.backdate_started(&j.job_id, 60_000);
        let ex = RecordingExecutor::new(false);
        let mut c = cfg(1, 1, vec![]);
        c.stale_after = Duration::ZERO;
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), c);
        let r = s.tick_once(12, false).await;
        assert_eq!(r.self_healed, vec![j.job_id.clone()], "released, not rebuilt");
        assert!(r.reconciled.is_empty(), "a finished job is never requeued");
        assert!(r.dispatched.is_empty(), "and never re-dispatched");
        for h in r.handles {
            h.await.unwrap();
        }
        // The executor was NEVER invoked for a rebuild.
        assert!(ex.built.lock().unwrap().is_empty(), "no rebuild of a finished job");
        assert_eq!(q.inflight_count(HostRole::Primary), 0);
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("done"));
    }

    #[test]
    fn stale_floor_clamps_below_minimum_and_leaves_safe_values() {
        // D: a below-floor lease is clamped UP; a genuinely-safe one is unchanged.
        let (v, clamped) = clamp_stale_secs(1);
        assert!(clamped && v == MIN_STALE_BUILDING_SECS);
        let (v, clamped) = clamp_stale_secs(MIN_STALE_BUILDING_SECS - 1);
        assert!(clamped && v == MIN_STALE_BUILDING_SECS);
        let (v, clamped) = clamp_stale_secs(MIN_STALE_BUILDING_SECS);
        assert!(!clamped && v == MIN_STALE_BUILDING_SECS);
        let (v, clamped) = clamp_stale_secs(99_999);
        assert!(!clamped && v == 99_999);
        // The floor must exceed the longest build so a live build is never reconciled.
        assert!(MIN_STALE_BUILDING_SECS > super::super::MAX_BUILD_TIMEOUT_SECS);
        assert!(DEFAULT_STALE_BUILDING_SECS >= MIN_STALE_BUILDING_SECS);
    }

    #[tokio::test]
    async fn a_live_build_is_never_reconciled_at_the_floor_lease() {
        // D: with the safe-floor lease, a fresh (just-claimed) build is untouched.
        let q = Arc::new(InMemoryQueue::new());
        let j = q.enqueue(&req("m1", "r1", false)).await.unwrap();
        let _ = q.claim(&j.job_id, "m1", HostRole::Primary, 1).await.unwrap();
        let ex = RecordingExecutor::new(false);
        let mut c = cfg(1, 1, vec![]);
        c.stale_after = Duration::from_secs(MIN_STALE_BUILDING_SECS);
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), c);
        let r = s.tick_once(12, false).await;
        assert!(r.reconciled.is_empty() && r.self_healed.is_empty());
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("building"));
        assert_eq!(q.inflight_count(HostRole::Primary), 1);
        for h in r.handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn reconcile_releases_a_finished_job_immediately_not_after_the_stale_floor() {
        // codex cycle-4: a job whose finalize SUCCEEDED but whose release keeps
        // failing past the retry max sits FINISHED-but-unreleased holding its
        // lock+slot. The next reconcile tick must release it PROMPTLY — the stale
        // floor gates ONLY the crashed (no-marker) requeue path, NOT the finished
        // release. Proven with a FRESH claim (age far under the stale floor).
        let q = Arc::new(InMemoryQueue::new());
        let j_fin = q.enqueue(&req("m1", "r1", false)).await.unwrap();
        // A second, genuinely-CRASHED build (claimed, no marker, fresh) that must
        // stay untouched — the floor still gates the crashed path.
        let j_crash = q.enqueue(&req("m2", "r2", false)).await.unwrap();
        let _ = q.claim(&j_crash.job_id, "m2", HostRole::Primary, 2).await.unwrap();

        // Make every release fail (a release outage that outlasts the retry max).
        q.fail_releases(1_000);
        let ex = RecordingExecutor::new(false);
        let mut c = cfg(2, 1, vec![]); // primary cap 2 so both can build
        c.complete_retry_max = 3; // give up quickly after finalize succeeds
        c.complete_retry_base = Duration::from_millis(1);
        // A LONG stale floor: if the finished-release were (wrongly) age-gated it
        // would NOT fire on a fresh claim.
        c.stale_after = Duration::from_secs(MIN_STALE_BUILDING_SECS);
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), c);

        // Tick 1: dispatches j_fin; its build finishes + finalizes, but release
        // fails past the retry max → finished-but-unreleased (slot still held).
        let r1 = s.tick_once(12, false).await;
        assert_eq!(r1.dispatched, vec![j_fin.job_id.clone()]);
        for h in r1.handles {
            h.await.unwrap();
        }
        assert!(q.has_outcome(&j_fin.job_id), "finalize recorded the terminal marker");
        assert_eq!(q.state_of(&j_fin.job_id).as_deref(), Some("building"), "not yet released");
        assert_eq!(q.inflight_count(HostRole::Primary), 2, "both slots held");

        // Tick 2 (claims are FRESH, far under the stale floor): reconcile releases
        // the FINISHED job immediately — no age wait — and leaves the CRASHED one.
        let r2 = s.tick_once(12, false).await;
        assert_eq!(r2.self_healed, vec![j_fin.job_id.clone()], "finished job released now");
        assert!(r2.reconciled.is_empty(), "crashed fresh-claim NOT requeued under the floor");
        for h in r2.handles {
            h.await.unwrap();
        }
        // Finished job: released, terminal, NEVER rebuilt (executor ran once).
        assert_eq!(q.state_of(&j_fin.job_id).as_deref(), Some("done"));
        assert_eq!(ex.built.lock().unwrap().as_slice(), ["m1".to_string()]);
        // Crashed fresh-claim job: still building, still holding its slot.
        assert_eq!(q.state_of(&j_crash.job_id).as_deref(), Some("building"));
        // The finished job's slot + module lock were freed → a same-module (m1)
        // build can now claim.
        let j_next = q.enqueue(&req("m1", "r3", false)).await.unwrap();
        assert!(matches!(
            q.claim(&j_next.job_id, "m1", HostRole::Primary, 2).await.unwrap(),
            ClaimOutcome::Claimed { .. }
        ));
    }
}
