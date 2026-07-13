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
use crate::compiler::queue::{ClaimOutcome, JobState, QueueError, QueueStore, QueuedJob};

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
const DEFAULT_STALE_BUILDING_SECS: u64 = 3600;
const DEFAULT_COMPLETE_RETRY_BASE_MS: u64 = 500;
const DEFAULT_COMPLETE_RETRY_MAX: u32 = 20;

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
    raw.split(',')
        .filter_map(|tok| {
            let tok = tok.trim();
            if tok.is_empty() {
                return None;
            }
            let (a, b) = tok.split_once('-')?;
            let start: u8 = a.trim().parse().ok()?;
            let end: u8 = b.trim().parse().ok()?;
            if start > 24 || end > 24 {
                return None;
            }
            Some(Window { start, end })
        })
        .collect()
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
        Self {
            primary_cap: env_u32(CAP_PRIMARY_ENV, DEFAULT_CAP),
            heavy_cap: env_u32(CAP_HEAVY_ENV, DEFAULT_CAP),
            windows: parse_windows(&std::env::var(WINDOW_ENV).unwrap_or_default()),
            interval: Duration::from_secs(secs(INTERVAL_ENV, DEFAULT_INTERVAL_SECS)),
            peek_limit: peek,
            stale_after: Duration::from_secs(secs(STALE_BUILDING_ENV, DEFAULT_STALE_BUILDING_SECS)),
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

/// Coordinates the heavy host's idle-mode lease around a heavy build. **BLD-11
/// owns the real implementation**; this seam is called ONLY around a heavy build
/// actually being dispatched. The default is an explicit, logged no-op so heavy
/// builds work (uncoordinated) until BLD-11 lands.
#[async_trait]
pub trait IdleCoordinator: Send + Sync {
    async fn acquire(&self, job: &QueuedJob);
    async fn release(&self, job: &QueuedJob);
}

/// The default idle coordinator: a no-op that records the seam. BLD-11 replaces
/// it with the real heavy-host idle-mode acquire/release.
pub struct NoopIdle;

#[async_trait]
impl IdleCoordinator for NoopIdle {
    async fn acquire(&self, job: &QueuedJob) {
        tracing::debug!(
            module = %job.module,
            "compiler scheduler: heavy build dispatched; idle-mode acquire is a no-op \
             until BLD-11 wires the real coordinator"
        );
    }
    async fn release(&self, _job: &QueuedJob) {}
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
    /// Stale `building` jobs requeued by the reconcile backstop this tick.
    pub reconciled: Vec<String>,
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
    /// `compiler_build` executor, and the BLD-11 idle seam (no-op default).
    /// `None` when Redis is not configured (nothing to schedule).
    pub fn from_env() -> Option<Self> {
        let queue = super::queue::RedisQueue::from_env()?;
        Some(Self::new(
            Arc::new(queue),
            Arc::new(CompilerBuildExecutor),
            Arc::new(NoopIdle),
            SchedulerConfig::from_env(),
        ))
    }

    /// One scheduling pass. Pure w.r.t. the clock — `hour` (0..=23, local) and
    /// `fleet_quiet` are passed in so it is deterministically testable; the run
    /// loop supplies the live values.
    pub async fn tick_once(&self, hour: u8, fleet_quiet: bool) -> TickReport {
        let mut report = TickReport {
            reconciled: Vec::new(),
            dispatched: Vec::new(),
            held_window: Vec::new(),
            contended: Vec::new(),
            unavailable: false,
            handles: Vec::new(),
        };
        // Crash/restart backstop FIRST: requeue any stale building jobs so their
        // module lock + host slot free up before we try to dispatch this tick.
        match self.queue.reconcile(self.config.stale_after).await {
            Ok(mut ids) => report.reconciled.append(&mut ids),
            Err(QueueError::Unavailable) => {
                report.unavailable = true;
                return report;
            }
        }
        let jobs = match self.queue.peek(self.config.peek_limit).await {
            Ok(j) => j,
            Err(QueueError::Unavailable) => {
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
                Err(QueueError::Unavailable) => {
                    report.unavailable = true;
                    break;
                }
            }
        }
        report
    }

    /// Spawn the build for a claimed job: acquire idle-mode (heavy only), run the
    /// build, release idle-mode, then release the queue slot with the outcome.
    ///
    /// The release (`complete`) is RETRIED with bounded backoff on a Redis-down
    /// failure — the build outcome is already known and `complete` is idempotent
    /// (fenced by `token`), so retrying eventually frees the module lock + host
    /// slot once Redis is back. If every retry fails, the tick's reconcile
    /// backstop requeues the (now stale) job — so a completion outage can never
    /// permanently wedge the lock/slot.
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
            if job.heavy {
                idle.acquire(&job).await;
            }
            let result = executor.build(&job).await;
            if job.heavy {
                idle.release(&job).await;
            }
            let state = if result.is_ok() {
                JobState::Done
            } else {
                JobState::Failed
            };
            // Retry the release until it lands (or the bounded attempts run out;
            // the reconcile backstop covers the extreme case).
            for attempt in 0..max_attempts.max(1) {
                match queue
                    .complete(&job.job_id, &job.module, host, state, &token)
                    .await
                {
                    Ok(()) => return,
                    Err(e) => {
                        // Exponential backoff, capped, so a long outage does not
                        // spin hot; the last attempt logs and yields to reconcile.
                        let backoff = base.saturating_mul(1u32 << attempt.min(5));
                        tracing::warn!(
                            module = %job.module, job = %job.job_id, attempt,
                            "compiler scheduler: completion failed ({e}); retrying release"
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
            tracing::error!(
                module = %job.module, job = %job.job_id,
                "compiler scheduler: completion still failing after {max_attempts} retries; \
                 the reconcile backstop will requeue the stale job"
            );
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

    /// A fake idle coordinator counting acquire/release so we can prove it's
    /// only touched for heavy builds.
    #[derive(Default)]
    struct CountingIdle {
        acquires: AtomicUsize,
        releases: AtomicUsize,
    }
    #[async_trait]
    impl IdleCoordinator for CountingIdle {
        async fn acquire(&self, _job: &QueuedJob) {
            self.acquires.fetch_add(1, Ordering::SeqCst);
        }
        async fn release(&self, _job: &QueuedJob) {
            self.releases.fetch_add(1, Ordering::SeqCst);
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
        let idle = Arc::new(CountingIdle::default());
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
    async fn small_job_coalesced_to_heavy_becomes_window_gated() {
        let q = Arc::new(InMemoryQueue::new());
        // First request is small (would dispatch immediately on primary)...
        q.enqueue(&req("harmony", "r", false)).await.unwrap();
        // ...but a later heavy request for the same module@ref upgrades it.
        q.enqueue(&req("harmony", "r", true)).await.unwrap();
        let ex = RecordingExecutor::new(false);
        let idle = Arc::new(CountingIdle::default());
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
        let idle = Arc::new(CountingIdle::default());
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
        // The first 3 completion attempts fail (Redis-down at completion time),
        // then succeed — the retry loop must self-heal the wedge.
        q.fail_completes(3);
        let ex = RecordingExecutor::new(false);
        let s = sched(q.clone(), ex.clone(), Arc::new(NoopIdle), cfg(1, 1, vec![]));
        let r = s.tick_once(12, false).await;
        assert_eq!(r.dispatched.len(), 1);
        // Await the build task: it retries complete until it lands.
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
}
