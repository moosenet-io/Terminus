//! BLD-11 — compiler ↔ idle-mode LEASE wiring.
//!
//! A HEAVY constellation build runs on the big-RAM/GPU host, which normally also
//! serves Chord (the LLM proxy) and hosts MINT's GPU-heavy profiling sweeps. To
//! hand that host to a build without permanently tearing either down, the scheduler
//! takes an **idle-mode lease** around the heavy build:
//!
//!   1. [`acquire`](IdleModeLease::acquire) — signal Chord (BLD-09) and MINT (BLD-10)
//!      to go *idle* (drain, release their GPU/RAM), then **WAIT** until the freed
//!      RAM reaches the build's configured budget. Only then does the heavy build
//!      start — it never runs under budget.
//!   2. The build runs while the [`LeaseGuard`] is held.
//!   3. Release — [`activate`] both services again. Release is **guaranteed**: the
//!      guard's `Drop` reactivates on a normal return, an early return, OR a panic
//!      (a crashed build never leaves Chord/MINT stuck idle), and a **max-lease
//!      watchdog** force-activates if the build hangs past a hard bound.
//!
//! ## Sanctioned reach paths (no new door invented)
//! - **Chord** is a separate process/host: idle/activate go over the SAME control
//!   channel the serving tools already use — an HTTP POST to `CHORD_CONTROL_URL`
//!   (see [`crate::config::chord_control_url`]), exactly like
//!   `serving_profile_refresh` POSTs to `{base}/serving/reload`. Here it is
//!   `{base}/idle` and `{base}/activate` (Chord's BLD-09 control endpoints).
//! - **MINT** runs in *this* process (the intake harness embedded in Terminus), so
//!   idle/activate are the IN-PROCESS calls MINT already exposes:
//!   [`crate::mint::idle::enter_idle`] / [`crate::mint::idle::activate`], driving the
//!   process-global MINT idle controller. No new IPC is introduced.
//!
//! ## S1 discipline
//! Every knob — the freed-RAM budget, the acquire timeout, the max-lease timeout,
//! the poll interval, the Chord HTTP timeout — is a `BUILD_IDLE_*` env var with a
//! safe default; the Chord endpoint comes from `CHORD_CONTROL_URL`. No host, IP,
//! port, path, or RAM threshold is baked into source. The freed-RAM budget has NO
//! infra default: unset ⇒ the hard RAM GATE is OFF (idle is still signalled, but the
//! build proceeds without a budget check); a POSITIVE value is enforced strictly, so
//! a configured budget is never under-run.
//!
//! ## Testability
//! All side effects (Chord HTTP, MINT in-process calls, `/proc/meminfo`) live behind
//! the [`IdleBackend`] trait, so [`acquire_lease`] — the whole drain/wait/gate/abort
//! decision — is exercised offline with a mock backend, with no network, no MINT
//! runtime, and no sleeping beyond the tiny injected poll.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::compiler::queue::QueuedJob;
use crate::compiler::scheduler::IdleCoordinator;

// ─────────────────────────────────────────────────────────────────────────────
// Config (S1 — every value from env, with a safe default)
// ─────────────────────────────────────────────────────────────────────────────

/// Freed-RAM budget (whole GiB) a heavy build requires before it may start. From
/// `BUILD_IDLE_FREED_RAM_BUDGET_GB`. NO infra default: unset/blank/`0` ⇒ the hard
/// RAM gate is OFF (idle is still signalled). A positive value is enforced strictly.
const FREED_RAM_BUDGET_ENV: &str = "BUILD_IDLE_FREED_RAM_BUDGET_GB";
/// How long to WAIT for the freed RAM to reach the budget before aborting the heavy
/// build (and requeueing it). From `BUILD_IDLE_ACQUIRE_TIMEOUT_SECS`.
const ACQUIRE_TIMEOUT_ENV: &str = "BUILD_IDLE_ACQUIRE_TIMEOUT_SECS";
/// Hard max-lease bound: after this long the watchdog force-activates Chord+MINT so
/// a hung/forgotten build can NEVER leave them idle indefinitely. From
/// `BUILD_IDLE_MAX_LEASE_SECS`; defaults comfortably above a full build timeout.
const MAX_LEASE_ENV: &str = "BUILD_IDLE_MAX_LEASE_SECS";
/// Poll interval (ms) while waiting for freed RAM. From `BUILD_IDLE_POLL_MS`.
const POLL_MS_ENV: &str = "BUILD_IDLE_POLL_MS";
/// Per-request HTTP timeout (secs) for a Chord idle/activate control call. From
/// `BUILD_IDLE_CHORD_TIMEOUT_SECS`.
const CHORD_TIMEOUT_ENV: &str = "BUILD_IDLE_CHORD_TIMEOUT_SECS";
/// Backoff (ms) between release/reactivation retry rounds — so a transient partial
/// activation failure self-heals. From `BUILD_IDLE_ACTIVATE_RETRY_MS`.
const ACTIVATE_RETRY_MS_ENV: &str = "BUILD_IDLE_ACTIVATE_RETRY_MS";

const DEFAULT_ACQUIRE_TIMEOUT_SECS: u64 = 120;
/// Default max-lease: a full build timeout plus generous headroom, so the watchdog
/// only ever fires on a genuinely stuck build — never on a legitimately long one.
const DEFAULT_MAX_LEASE_SECS: u64 = super::MAX_BUILD_TIMEOUT_SECS + 1800;
const DEFAULT_POLL_MS: u64 = 1000;
const DEFAULT_CHORD_TIMEOUT_SECS: u64 = 30;
const DEFAULT_ACTIVATE_RETRY_MS: u64 = 500;
/// Bounded rounds of reactivation retry per release call (backstopped by later
/// release calls, the max-lease watchdog, and each service's own idle watchdog).
const RELEASE_MAX_ATTEMPTS: u32 = 5;

/// The idle-lease reason label (diagnostic only; recorded in MINT's resume
/// manifest). Not an infra identifier.
const LEASE_REASON: &str = "compiler-heavy-build";

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// The freed-RAM budget (GiB) — `0.0` when unset/blank/unparsable/≤0 (gate OFF).
fn freed_ram_budget_gb_from_env() -> f64 {
    std::env::var(FREED_RAM_BUDGET_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(0.0)
}

/// Resolved idle-lease tunables.
#[derive(Debug, Clone)]
pub struct IdleLeaseConfig {
    /// Freed RAM (GiB) required before a heavy build starts. `0.0` ⇒ gate OFF.
    pub freed_ram_budget_gb: f64,
    /// How long to wait for the budget before aborting + requeueing the build.
    pub acquire_timeout: Duration,
    /// Hard bound after which the watchdog force-activates Chord+MINT.
    pub max_lease: Duration,
    /// Poll cadence while waiting for freed RAM.
    pub poll: Duration,
    /// Per-request HTTP timeout for a Chord idle/activate call.
    pub chord_timeout: Duration,
    /// Backoff between reactivation retry rounds (partial-failure self-heal).
    pub activate_retry: Duration,
}

impl IdleLeaseConfig {
    pub fn from_env() -> Self {
        Self {
            freed_ram_budget_gb: freed_ram_budget_gb_from_env(),
            acquire_timeout: Duration::from_secs(env_u64(
                ACQUIRE_TIMEOUT_ENV,
                DEFAULT_ACQUIRE_TIMEOUT_SECS,
            )),
            max_lease: Duration::from_secs(env_u64(MAX_LEASE_ENV, DEFAULT_MAX_LEASE_SECS)),
            poll: Duration::from_millis(env_u64(POLL_MS_ENV, DEFAULT_POLL_MS)),
            chord_timeout: Duration::from_secs(env_u64(
                CHORD_TIMEOUT_ENV,
                DEFAULT_CHORD_TIMEOUT_SECS,
            )),
            activate_retry: Duration::from_millis(env_u64(
                ACTIVATE_RETRY_MS_ENV,
                DEFAULT_ACTIVATE_RETRY_MS,
            )),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backend seam: the two services' idle/activate + a freed-RAM sample
// ─────────────────────────────────────────────────────────────────────────────

/// The side-effecting operations the idle lease performs, factored behind a trait
/// so the acquire/wait/gate/abort logic is fully unit-testable offline. Each idle
/// call returns the freed RAM (GiB) that service reports, best-effort (`None` if it
/// didn't report a figure).
#[async_trait]
pub trait IdleBackend: Send + Sync {
    /// Signal Chord to enter idle-mode (drain + release its GPU/RAM). `Ok(freed_gb)`
    /// on success; `Err` if Chord could not be reached/idled.
    async fn chord_idle(&self) -> Result<Option<f64>, String>;
    /// Signal Chord to resume (activate). Idempotent on Chord's side.
    async fn chord_activate(&self) -> Result<(), String>;
    /// Signal MINT (in-process) to enter idle-mode. `Ok(freed_gb)` on success.
    async fn mint_idle(&self) -> Result<Option<f64>, String>;
    /// Signal MINT (in-process) to activate. Idempotent.
    async fn mint_activate(&self) -> Result<(), String>;
    /// Current best-effort `MemAvailable` (GiB); `None` if unreadable.
    fn mem_available_gb(&self) -> Option<f64>;
}

/// The production backend: Chord over its HTTP control endpoint (the sanctioned
/// `CHORD_CONTROL_URL` channel), MINT via its in-process idle API.
pub struct ProdIdleBackend {
    chord_timeout: Duration,
}

impl ProdIdleBackend {
    pub fn new(chord_timeout: Duration) -> Self {
        Self { chord_timeout }
    }

    /// POST to a Chord control sub-path (`idle`/`activate`), returning the parsed
    /// JSON body on 2xx. A missing `CHORD_CONTROL_URL`, an unreachable endpoint, or
    /// a non-2xx status is an `Err` with a GENERICIZED message (no host echoed — the
    /// same discipline as `serving_profile_refresh`).
    async fn chord_post(&self, sub_path: &str) -> Result<serde_json::Value, String> {
        let base = crate::config::chord_control_url()
            .ok_or_else(|| "CHORD_CONTROL_URL not set — cannot reach Chord idle control".to_string())?;
        let url = format!("{}/{}", base.trim_end_matches('/'), sub_path);
        let client = reqwest::Client::builder()
            .timeout(self.chord_timeout)
            .build()
            .map_err(|_| "could not build Chord control client".to_string())?;
        let resp = client
            .post(&url)
            .send()
            .await
            .map_err(|_| "Chord control endpoint unreachable".to_string())?;
        if !resp.status().is_success() {
            return Err(format!(
                "Chord rejected the idle control call (status {})",
                resp.status().as_u16()
            ));
        }
        // The body is advisory (a freed-RAM figure); a missing/unparsable body is
        // fine — treat as "no figure reported".
        Ok(resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null))
    }
}

/// Pull a freed-RAM (GiB) figure out of a Chord control response, tolerating a few
/// likely field names; `None` if none is present.
fn freed_from_body(body: &serde_json::Value) -> Option<f64> {
    for key in ["freed_gb", "freed_ram_gb", "freed"] {
        if let Some(v) = body.get(key).and_then(serde_json::Value::as_f64) {
            return Some(v);
        }
    }
    None
}

#[async_trait]
impl IdleBackend for ProdIdleBackend {
    async fn chord_idle(&self) -> Result<Option<f64>, String> {
        let body = self.chord_post("idle").await?;
        Ok(freed_from_body(&body))
    }

    async fn chord_activate(&self) -> Result<(), String> {
        self.chord_post("activate").await.map(|_| ())
    }

    async fn mint_idle(&self) -> Result<Option<f64>, String> {
        use crate::mint::idle::{enter_idle, EnterOutcome};
        let (outcome, report) = enter_idle(LEASE_REASON).await;
        match outcome {
            // Entered now, or already idle from a prior lease — either way MINT's
            // RAM is released. Surface whatever freed figure we have.
            EnterOutcome::Entered(_) | EnterOutcome::AlreadyIdle(_) => {
                Ok(report.and_then(|r| r.freed_gb))
            }
            // A concurrent transition means we didn't get a clean idle this call;
            // report no figure but do NOT hard-fail (the wait loop + budget gate
            // still governs whether the build may start).
            EnterOutcome::InTransition => Ok(None),
        }
    }

    async fn mint_activate(&self) -> Result<(), String> {
        crate::mint::idle::activate(LEASE_REASON).await;
        Ok(())
    }

    fn mem_available_gb(&self) -> Option<f64> {
        crate::mint::idle::read_mem_available_gb()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The lease + its guaranteed release
// ─────────────────────────────────────────────────────────────────────────────

/// Why a heavy build could not take its idle lease. In BOTH cases the heavy build
/// MUST NOT run and the scheduler requeues it; any service that was idled is
/// reactivated (best-effort) before the error is returned, so nothing is left idle.
#[derive(Debug)]
pub enum LeaseError {
    /// The freed RAM never reached the configured budget within the acquire timeout.
    InsufficientRam { freed_gb: f64, budget_gb: f64 },
    /// Idle coordination itself FAILED with the RAM gate ON (a service could not be
    /// idled), so we cannot guarantee the host was freed — degrade SAFELY by
    /// aborting + requeueing rather than building uncoordinated.
    IdleFailed { reason: String },
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::InsufficientRam {
                freed_gb,
                budget_gb,
            } => write!(
                f,
                "idle-mode freed only {freed_gb:.1} GiB (< {budget_gb:.1} GiB budget) \
                 within the acquire timeout — refusing to build under budget"
            ),
            LeaseError::IdleFailed { reason } => write!(
                f,
                "idle coordination failed ({reason}) with a RAM budget configured — \
                 refusing to build uncoordinated"
            ),
        }
    }
}

/// The shared release core behind a lease. Reactivation is tracked PER SERVICE so a
/// PARTIAL failure never leaves a service stuck idle: a single `released` once-flag
/// (set before activation) was wrong — if Chord activation failed but the flag was
/// already burned, every later attempt no-oped and Chord stayed idle forever.
/// Instead, [`release`](Self::release) only ever marks a service done once ITS OWN
/// activate SUCCEEDS, retries the not-yet-confirmed service(s) with bounded backoff,
/// and is safe to call repeatedly (explicit release, guard drop, watchdog) — each
/// call re-attempts only the services still not confirmed active. Every `activate`
/// is idempotent on its service's side, so re-attempting one that already succeeded
/// (in a rare concurrent double-fire) is harmless.
struct LeaseInner {
    backend: Arc<dyn IdleBackend>,
    /// Confirmed-active flags — set ONLY after that service's `activate` returns Ok.
    chord_active: AtomicBool,
    mint_active: AtomicBool,
    /// Backoff between release retry rounds.
    retry_backoff: Duration,
    /// Max release retry rounds before giving up this call (a later call, or the
    /// per-service idle watchdogs, remain the backstop).
    max_attempts: u32,
}

impl LeaseInner {
    /// Both services confirmed reactivated?
    fn fully_released(&self) -> bool {
        self.chord_active.load(Ordering::SeqCst) && self.mint_active.load(Ordering::SeqCst)
    }

    /// One activation pass: attempt each service NOT yet confirmed active; mark it
    /// done only on a successful `activate`. Returns whether both are now confirmed.
    async fn try_activate_pending(&self) -> bool {
        if !self.chord_active.load(Ordering::SeqCst) {
            match self.backend.chord_activate().await {
                Ok(()) => self.chord_active.store(true, Ordering::SeqCst),
                Err(e) => warn!(error = %e, "idle lease: Chord activate failed — will retry (not marking released)"),
            }
        }
        if !self.mint_active.load(Ordering::SeqCst) {
            match self.backend.mint_activate().await {
                Ok(()) => self.mint_active.store(true, Ordering::SeqCst),
                Err(e) => warn!(error = %e, "idle lease: MINT activate failed — will retry (not marking released)"),
            }
        }
        self.fully_released()
    }

    /// Reactivate both services, retrying any that fail with bounded backoff so a
    /// transient partial failure self-heals instead of leaving a service stuck idle.
    /// Idempotent + re-entrant: a service already confirmed active is never touched
    /// again by this call, and re-invoking after a partial failure resumes ONLY the
    /// still-pending service.
    async fn release(&self) {
        if self.fully_released() {
            return;
        }
        for attempt in 0..self.max_attempts.max(1) {
            if self.try_activate_pending().await {
                info!("idle lease released — Chord + MINT reactivated");
                return;
            }
            if attempt + 1 < self.max_attempts.max(1) {
                tokio::time::sleep(self.retry_backoff).await;
            }
        }
        warn!(
            chord_active = self.chord_active.load(Ordering::SeqCst),
            mint_active = self.mint_active.load(Ordering::SeqCst),
            "idle lease: a service is still not confirmed active after release retries — \
             a later release/watchdog or the per-service idle watchdog remains the backstop"
        );
    }
}

/// An RAII handle to a held idle-mode lease. Dropping it (normal completion, an
/// early return, or a PANIC unwind) reactivates Chord + MINT, so a crashed heavy
/// build can never leave them stuck idle. A [`noop`](Self::noop) guard (a
/// non-heavy build) holds nothing and does nothing on drop.
///
/// The scheduler should prefer the explicit, awaited [`release`](Self::release) on
/// the normal path so reactivation is deterministic; the `Drop` is the safety net
/// for the panic / early-return paths.
#[must_use = "hold the lease for the duration of the heavy build; dropping it releases idle-mode"]
pub struct LeaseGuard {
    inner: Option<Arc<LeaseInner>>,
    /// The max-lease watchdog task; aborted on an explicit/dropped release so it
    /// doesn't linger for the full max-lease after the build already finished.
    watchdog: Option<tokio::task::JoinHandle<()>>,
}

impl LeaseGuard {
    /// A do-nothing guard for a non-heavy build (no lease was taken).
    pub fn noop() -> Self {
        Self {
            inner: None,
            watchdog: None,
        }
    }

    /// Explicit, AWAITED release for the normal completion path. Idempotent with the
    /// `Drop` and the watchdog (the inner once-guard means reactivation runs once).
    pub async fn release(mut self) {
        if let Some(w) = self.watchdog.take() {
            w.abort();
        }
        if let Some(inner) = self.inner.take() {
            inner.release().await;
        }
    }
}

// Manual `Debug` (the held `Arc<LeaseInner>` / `JoinHandle` aren't meaningfully
// printable; expose only whether a lease is held) — needed so `Result<LeaseGuard, _>`
// works with `expect_err`/assertions.
impl std::fmt::Debug for LeaseGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseGuard")
            .field("held", &self.inner.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Some(w) = self.watchdog.take() {
            w.abort();
        }
        if let Some(inner) = self.inner.take() {
            // The build returned early or PANICKED without an explicit release.
            // Reactivate on a detached task (Drop can't await). The once-guard makes
            // this safe even if the watchdog already fired.
            if tokio::runtime::Handle::try_current().is_ok() {
                warn!("idle lease guard dropped without explicit release (early return/panic) — reactivating Chord + MINT");
                tokio::spawn(async move { inner.release().await });
            } else {
                // No runtime (e.g. a synchronous test drop): reactivation can't be
                // spawned; the per-service watchdogs remain the backstop.
                warn!("idle lease guard dropped outside a runtime — relying on per-service idle watchdogs to reactivate");
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Acquire: idle both, wait for the budget, gate the build, or abort
// ─────────────────────────────────────────────────────────────────────────────

/// Estimate the RAM freed so far: the greater of the measured `MemAvailable` delta
/// (ground truth, but only when both samples are readable) and the sum of the
/// figures the two services reported at idle time. Pure.
fn estimate_freed(
    mem_before: Option<f64>,
    mem_now: Option<f64>,
    chord_freed: Option<f64>,
    mint_freed: Option<f64>,
) -> f64 {
    let measured = match (mem_before, mem_now) {
        (Some(b), Some(a)) => (a - b).max(0.0),
        _ => 0.0,
    };
    let reported = chord_freed.unwrap_or(0.0).max(0.0) + mint_freed.unwrap_or(0.0).max(0.0);
    measured.max(reported)
}

/// Acquire the idle-mode lease for a build whose freed-RAM budget is `budget_gb`.
///
/// ## Gate OFF (`budget_gb <= 0.0`) — idle coordination DISABLED
/// No idle call is made at all: the build runs directly, exactly as the pre-BLD-11
/// no-op seam did. This removes any "idle failed but we proceeded uncoordinated"
/// path — with no budget there is nothing to coordinate, so there is nothing to
/// fail. A [`LeaseGuard::noop`] is returned (nothing to release).
///
/// ## Gate ON (`budget_gb > 0.0`) — idle coordination REQUIRED
/// Idle Chord + MINT; if EITHER idle call FAILS we cannot guarantee the host was
/// freed, so we degrade SAFELY: reactivate whatever we idled and return
/// [`LeaseError::IdleFailed`] (the scheduler requeues — never builds uncoordinated).
/// On success, WAIT for the freed RAM to reach `budget_gb`, bounded by the acquire
/// timeout; on timeout reactivate both and return [`LeaseError::InsufficientRam`]
/// (requeue — never builds under budget). On success, hand back a [`LeaseGuard`]
/// whose drop/watchdog guarantee reactivation. Generic over [`IdleBackend`] so it is
/// fully testable offline.
pub async fn acquire_lease(
    backend: Arc<dyn IdleBackend>,
    cfg: &IdleLeaseConfig,
    budget_gb: f64,
) -> Result<LeaseGuard, LeaseError> {
    // Gate OFF ⇒ do NOT attempt idle coordination at all (no failed-idle-then-proceed
    // path): build directly, holding a no-op lease.
    if budget_gb <= 0.0 {
        info!("idle lease: no freed-RAM budget for this build (gate off) — building directly, no idle coordination attempted");
        return Ok(LeaseGuard::noop());
    }

    let mem_before = backend.mem_available_gb();
    info!(budget_gb, "idle lease: idling Chord + MINT for a heavy build");

    // Gate ON: a FAILED idle call means we can't guarantee the host was freed. Do NOT
    // proceed uncoordinated — reactivate whatever we touched and abort (→ requeue).
    let chord_freed = match backend.chord_idle().await {
        Ok(f) => f,
        Err(e) => return abort_on_idle_failure(backend, "Chord idle failed", e).await,
    };
    let mint_freed = match backend.mint_idle().await {
        Ok(f) => f,
        Err(e) => return abort_on_idle_failure(backend, "MINT idle failed", e).await,
    };

    // WAIT for the freed RAM to reach the budget, bounded by the timeout.
    let deadline = tokio::time::Instant::now() + cfg.acquire_timeout;
    loop {
        let freed = estimate_freed(
            mem_before,
            backend.mem_available_gb(),
            chord_freed,
            mint_freed,
        );
        if freed >= budget_gb {
            info!(
                freed_gb = freed,
                budget_gb, "idle lease acquired — freed RAM meets the budget; heavy build may start"
            );
            return Ok(arm_guard(backend, cfg.max_lease, cfg.activate_retry));
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                freed_gb = freed,
                budget_gb,
                "idle lease: freed RAM below budget after the acquire timeout — \
                 reactivating Chord + MINT and refusing the heavy build (it will be requeued)"
            );
            // Never leave services idle for a build we're not going to run.
            reactivate_best_effort(&backend).await;
            return Err(LeaseError::InsufficientRam {
                freed_gb: freed,
                budget_gb,
            });
        }
        tokio::time::sleep(cfg.poll).await;
    }
}

/// Reactivate both services best-effort (used on an abort — the build won't run).
async fn reactivate_best_effort(backend: &Arc<dyn IdleBackend>) {
    if let Err(e) = backend.chord_activate().await {
        warn!(error = %e, "idle lease: Chord reactivate after abort failed (best-effort)");
    }
    if let Err(e) = backend.mint_activate().await {
        warn!(error = %e, "idle lease: MINT reactivate after abort failed (best-effort)");
    }
}

/// Gate-ON idle-coordination failure: reactivate anything we idled, then abort so the
/// scheduler requeues (never build uncoordinated after a failed idle call).
async fn abort_on_idle_failure(
    backend: Arc<dyn IdleBackend>,
    what: &str,
    err: String,
) -> Result<LeaseGuard, LeaseError> {
    warn!(error = %err, "idle lease: {what} with a RAM budget configured — aborting (safe degrade), the build is requeued");
    reactivate_best_effort(&backend).await;
    Err(LeaseError::IdleFailed {
        reason: format!("{what}: {err}"),
    })
}

/// Build a `LeaseGuard` and arm its max-lease watchdog (force-activate after the
/// hard bound so a hung build can never wedge Chord/MINT idle).
fn arm_guard(
    backend: Arc<dyn IdleBackend>,
    max_lease: Duration,
    activate_retry: Duration,
) -> LeaseGuard {
    let inner = Arc::new(LeaseInner {
        backend,
        chord_active: AtomicBool::new(false),
        mint_active: AtomicBool::new(false),
        retry_backoff: activate_retry,
        max_attempts: RELEASE_MAX_ATTEMPTS,
    });
    let wd_inner = inner.clone();
    let watchdog = tokio::spawn(async move {
        tokio::time::sleep(max_lease).await;
        warn!(
            max_lease_secs = max_lease.as_secs(),
            "idle lease: MAX-LEASE timeout reached — force-activating Chord + MINT (a build hung or was forgotten)"
        );
        wd_inner.release().await;
    });
    LeaseGuard {
        inner: Some(inner),
        watchdog: Some(watchdog),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The production IdleCoordinator the scheduler uses
// ─────────────────────────────────────────────────────────────────────────────

/// The real heavy-host idle-mode coordinator: acquires the lease (idle Chord+MINT,
/// wait for the freed-RAM budget) before a heavy build and guarantees release after.
/// Swapped into the scheduler in place of the `NoopIdle` seam.
pub struct IdleModeLease {
    backend: Arc<dyn IdleBackend>,
    cfg: IdleLeaseConfig,
}

impl IdleModeLease {
    pub fn new(backend: Arc<dyn IdleBackend>, cfg: IdleLeaseConfig) -> Self {
        Self { backend, cfg }
    }

    /// Production wiring from the environment: the HTTP-Chord + in-process-MINT
    /// backend, all knobs from `BUILD_IDLE_*` / `CHORD_CONTROL_URL`.
    pub fn from_env() -> Self {
        let cfg = IdleLeaseConfig::from_env();
        let backend = Arc::new(ProdIdleBackend::new(cfg.chord_timeout));
        Self::new(backend, cfg)
    }
}

impl IdleModeLease {
    /// The freed-RAM budget (GiB) for THIS build: the module's own known peak build
    /// RSS (`BUILD_MODULE_PEAK_MB_<MODULE>`, the same per-build config host selection
    /// already uses) when configured — so the gate reflects what the build actually
    /// needs — falling back to the process-wide `BUILD_IDLE_FREED_RAM_BUDGET_GB`
    /// default only when the job has no per-build figure. A present-but-unparsable
    /// module peak degrades to the global default rather than erroring the build path.
    fn budget_for(&self, job: &QueuedJob) -> f64 {
        match super::host::module_peak_mb(&job.module) {
            Ok(Some(mb)) => mb as f64 / 1024.0,
            _ => self.cfg.freed_ram_budget_gb,
        }
    }
}

#[async_trait]
impl IdleCoordinator for IdleModeLease {
    async fn acquire(&self, job: &QueuedJob) -> Result<LeaseGuard, LeaseError> {
        let budget = self.budget_for(job);
        acquire_lease(self.backend.clone(), &self.cfg, budget).await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Cross-module test support
// ─────────────────────────────────────────────────────────────────────────────

/// Test-only: a real [`LeaseGuard`] that increments `counter` exactly once when it
/// is released (via an explicit release, a drop, or its watchdog). Lets the
/// scheduler's tests assert that a heavy build's lease is released without standing
/// up a full backend. The watchdog bound is long so it never fires during a test.
#[cfg(test)]
pub fn test_guard(counter: Arc<std::sync::atomic::AtomicUsize>) -> LeaseGuard {
    struct CountingReleaseBackend {
        counter: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl IdleBackend for CountingReleaseBackend {
        async fn chord_idle(&self) -> Result<Option<f64>, String> {
            Ok(None)
        }
        async fn chord_activate(&self) -> Result<(), String> {
            // Always succeeds ⇒ counted exactly once per full release (a re-invoked
            // release sees both services confirmed active and no-ops).
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn mint_idle(&self) -> Result<Option<f64>, String> {
            Ok(None)
        }
        async fn mint_activate(&self) -> Result<(), String> {
            Ok(())
        }
        fn mem_available_gb(&self) -> Option<f64> {
            None
        }
    }
    arm_guard(
        Arc::new(CountingReleaseBackend { counter }),
        Duration::from_secs(3600),
        Duration::from_millis(1),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// A mock backend recording every call and returning scripted freed figures /
    /// a scripted `MemAvailable` sequence. `chord_activates`/`mint_activates` count
    /// only SUCCESSFUL activations, so a count of 1 proves that service ended active.
    struct MockBackend {
        chord_idles: AtomicUsize,
        chord_activates: AtomicUsize,
        mint_idles: AtomicUsize,
        mint_activates: AtomicUsize,
        chord_freed: Option<f64>,
        mint_freed: Option<f64>,
        /// A queue of `MemAvailable` samples; the last value repeats once drained.
        mem_samples: Mutex<Vec<f64>>,
        /// If true, chord_idle/mint_idle return Err.
        fail_idle: bool,
        /// The next N `chord_activate` calls fail (transient); after that, succeed.
        chord_activate_fail_times: AtomicUsize,
    }

    impl MockBackend {
        fn new(chord_freed: Option<f64>, mint_freed: Option<f64>) -> Arc<Self> {
            Arc::new(Self {
                chord_idles: AtomicUsize::new(0),
                chord_activates: AtomicUsize::new(0),
                mint_idles: AtomicUsize::new(0),
                mint_activates: AtomicUsize::new(0),
                chord_freed,
                mint_freed,
                mem_samples: Mutex::new(Vec::new()),
                fail_idle: false,
                chord_activate_fail_times: AtomicUsize::new(0),
            })
        }
        fn failing_idle() -> Arc<Self> {
            Arc::new(Self {
                chord_idles: AtomicUsize::new(0),
                chord_activates: AtomicUsize::new(0),
                mint_idles: AtomicUsize::new(0),
                mint_activates: AtomicUsize::new(0),
                chord_freed: None,
                mint_freed: None,
                mem_samples: Mutex::new(Vec::new()),
                fail_idle: true,
                chord_activate_fail_times: AtomicUsize::new(0),
            })
        }
        fn with_mem(self: Arc<Self>, samples: Vec<f64>) -> Arc<Self> {
            *self.mem_samples.lock().unwrap() = samples;
            self
        }
        fn with_chord_activate_fails(self: Arc<Self>, n: usize) -> Arc<Self> {
            self.chord_activate_fail_times.store(n, Ordering::SeqCst);
            self
        }
        /// (successful chord activations, successful mint activations)
        fn activates(&self) -> (usize, usize) {
            (
                self.chord_activates.load(Ordering::SeqCst),
                self.mint_activates.load(Ordering::SeqCst),
            )
        }
    }

    #[async_trait]
    impl IdleBackend for MockBackend {
        async fn chord_idle(&self) -> Result<Option<f64>, String> {
            self.chord_idles.fetch_add(1, Ordering::SeqCst);
            if self.fail_idle {
                Err("chord down".into())
            } else {
                Ok(self.chord_freed)
            }
        }
        async fn chord_activate(&self) -> Result<(), String> {
            if self.chord_activate_fail_times.load(Ordering::SeqCst) > 0 {
                self.chord_activate_fail_times.fetch_sub(1, Ordering::SeqCst);
                return Err("chord activate transient failure".into());
            }
            self.chord_activates.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn mint_idle(&self) -> Result<Option<f64>, String> {
            self.mint_idles.fetch_add(1, Ordering::SeqCst);
            if self.fail_idle {
                Err("mint down".into())
            } else {
                Ok(self.mint_freed)
            }
        }
        async fn mint_activate(&self) -> Result<(), String> {
            self.mint_activates.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn mem_available_gb(&self) -> Option<f64> {
            let mut s = self.mem_samples.lock().unwrap();
            if s.is_empty() {
                return None;
            }
            if s.len() == 1 {
                Some(s[0])
            } else {
                Some(s.remove(0))
            }
        }
    }

    /// Config with the RAM budget passed SEPARATELY to `acquire_lease` (per-build).
    fn cfg(acquire_secs: u64, max_lease_secs: u64) -> IdleLeaseConfig {
        IdleLeaseConfig {
            freed_ram_budget_gb: 0.0, // global default; per-call budget is explicit
            acquire_timeout: Duration::from_secs(acquire_secs),
            max_lease: Duration::from_secs(max_lease_secs),
            poll: Duration::from_millis(1),
            chord_timeout: Duration::from_secs(1),
            activate_retry: Duration::from_millis(1),
        }
    }

    #[test]
    fn estimate_freed_uses_the_greater_of_measured_and_reported() {
        // Measured delta wins when it's larger.
        assert_eq!(
            estimate_freed(Some(10.0), Some(140.0), Some(20.0), Some(30.0)),
            130.0
        );
        // Reported sum wins when the measurement isn't available.
        assert_eq!(estimate_freed(None, None, Some(60.0), Some(70.0)), 130.0);
        // A negative measured delta (other activity) clamps to the reported sum.
        assert_eq!(estimate_freed(Some(100.0), Some(90.0), Some(40.0), None), 40.0);
        // Nothing known ⇒ 0 (never fabricates freed RAM).
        assert_eq!(estimate_freed(None, None, None, None), 0.0);
    }

    #[tokio::test]
    async fn gate_off_makes_no_idle_calls_and_builds_directly() {
        // FINDING 1: with NO budget (gate off), idle coordination is NOT attempted at
        // all — no chord_idle/mint_idle call is made, so there is no "idle failed but
        // proceeded" path. The guard is a no-op (nothing to release).
        let be = MockBackend::new(None, None);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), 0.0)
            .await
            .expect("gate off ⇒ builds directly");
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 0, "no chord idle attempted");
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 0, "no mint idle attempted");
        guard.release().await;
        assert_eq!(be.activates(), (0, 0), "nothing was idled ⇒ nothing to reactivate");
    }

    #[tokio::test]
    async fn gate_on_idle_failure_aborts_and_requeues_never_uncoordinated() {
        // FINDING 4 (+1): with a budget configured (gate ON), a FAILED idle call must
        // abort + requeue — NEVER proceed to build uncoordinated. Anything idled is
        // reactivated best-effort.
        let be = MockBackend::failing_idle();
        let err = acquire_lease(be.clone(), &cfg(5, 3600), 120.0)
            .await
            .expect_err("gate on + idle failure ⇒ abort");
        assert!(
            matches!(err, LeaseError::IdleFailed { .. }),
            "must be IdleFailed, got {err:?}"
        );
        // chord_idle was attempted (and failed); reactivation was attempted for both.
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 1);
        assert_eq!(be.activates(), (1, 1), "reactivated best-effort on abort");
    }

    #[tokio::test]
    async fn happy_path_acquires_when_reported_freed_meets_budget() {
        // Chord+MINT report 60+70=130 GiB freed ≥ 120 budget ⇒ acquire immediately.
        let be = MockBackend::new(Some(60.0), Some(70.0));
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), 120.0)
            .await
            .expect("budget met ⇒ acquired");
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 1);
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 1);
        // No premature reactivation while the lease is held.
        assert_eq!(be.activates(), (0, 0));
        guard.release().await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn waits_for_measured_ram_to_climb_to_budget() {
        // No reported figures; MemAvailable climbs 10 → 40 → 135 GiB across polls.
        // Delta from the first sample (10) reaches 125 ≥ 120 on the third sample.
        let be = MockBackend::new(None, None).with_mem(vec![10.0, 40.0, 135.0]);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), 120.0)
            .await
            .expect("measured RAM reaches budget ⇒ acquired");
        guard.release().await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn insufficient_ram_aborts_and_reactivates_never_building_under_budget() {
        // Freed (10+20=30) never reaches the 120 budget; the acquire times out,
        // BOTH services are reactivated, and InsufficientRam is returned so the
        // scheduler requeues instead of building.
        let be = MockBackend::new(Some(10.0), Some(20.0));
        let err = acquire_lease(be.clone(), &cfg(0, 3600), 120.0)
            .await
            .expect_err("under budget ⇒ InsufficientRam");
        match err {
            LeaseError::InsufficientRam {
                freed_gb,
                budget_gb,
            } => {
                assert_eq!(freed_gb, 30.0);
                assert_eq!(budget_gb, 120.0);
            }
            other => panic!("expected InsufficientRam, got {other:?}"),
        }
        // Reactivated on abort — never left idle for a build that won't run.
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn partial_activation_failure_retries_until_both_active_no_stuck_idle() {
        // FINDING 2: chord_activate fails ONCE then succeeds; MINT (which succeeds
        // first try) must not be double-fired, and Chord must end ACTIVE (not stuck
        // idle) after the retry — the once-flag is never burned on a partial failure.
        let be = MockBackend::new(Some(200.0), Some(0.0)).with_chord_activate_fails(1);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), 120.0)
            .await
            .expect("budget met ⇒ acquired");
        guard.release().await;
        // Chord ended active (exactly one SUCCESSFUL activate, after one failure), and
        // MINT was activated exactly once (not double-fired on the retry round).
        assert_eq!(
            be.activates(),
            (1, 1),
            "Chord retried to ACTIVE; MINT not double-fired"
        );
        assert_eq!(
            be.chord_activate_fail_times.load(Ordering::SeqCst),
            0,
            "the forced failure was consumed by the first attempt"
        );
    }

    #[tokio::test]
    async fn max_lease_watchdog_force_activates_when_the_build_hangs() {
        // A tiny max-lease: the guard is held (build "hangs") and never released
        // explicitly; the watchdog must reactivate both on its own.
        let be = MockBackend::new(Some(200.0), Some(0.0));
        let guard = acquire_lease(be.clone(), &cfg(5, 0), 120.0)
            .await
            .expect("budget met ⇒ acquired");
        // Hold the guard (do NOT release). max_lease=0 ⇒ the watchdog fires promptly.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            be.activates(),
            (1, 1),
            "watchdog force-activated Chord + MINT after the max-lease timeout"
        );
        // A later explicit release is a harmless no-op (both already active).
        guard.release().await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn dropping_the_guard_reactivates_even_on_a_crash_path() {
        // Simulate a crashed/early-returning build: acquire, then DROP the guard
        // without an explicit release. Reactivation must still happen (detached).
        let be = MockBackend::new(Some(200.0), Some(0.0));
        {
            let guard = acquire_lease(be.clone(), &cfg(5, 3600), 120.0)
                .await
                .expect("acquired");
            // No explicit release: drop it here (as a panic unwind would).
            drop(guard);
        }
        // The Drop spawns a detached reactivation; give it a tick to run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            be.activates(),
            (1, 1),
            "guard drop reactivated Chord + MINT (crash-safe release)"
        );
    }

    #[tokio::test]
    async fn noop_guard_does_nothing() {
        // A non-heavy build's guard holds no lease and touches no backend.
        let g = LeaseGuard::noop();
        g.release().await; // no panic, nothing to release
    }

    #[test]
    fn per_job_budget_prefers_module_peak_over_the_global_default() {
        // FINDING 5: the build's OWN budget (its module peak RSS, MB→GiB) is used when
        // configured; the process-wide default is only the fallback.
        use crate::compiler::queue::Priority;
        let be = MockBackend::new(None, None);
        let mut c = cfg(5, 3600);
        c.freed_ram_budget_gb = 7.0; // global default
        let lease = IdleModeLease::new(be, c);
        let job = QueuedJob {
            job_id: "j".into(),
            module: "budgetprobe_uniqzz".into(),
            git_ref: "r".into(),
            priority: Priority::Normal,
            heavy: true,
        };
        let key = "BUILD_MODULE_PEAK_MB_BUDGETPROBE_UNIQZZ";
        // No per-module peak ⇒ falls back to the global default (7 GiB).
        std::env::remove_var(key);
        assert_eq!(lease.budget_for(&job), 7.0);
        // A per-module peak (20480 MB) drives the budget (20 GiB), overriding default.
        std::env::set_var(key, "20480");
        assert_eq!(lease.budget_for(&job), 20.0);
        // An unparsable peak degrades to the global default (never errors the build).
        std::env::set_var(key, "notanumber");
        assert_eq!(lease.budget_for(&job), 7.0);
        std::env::remove_var(key);
    }
}
