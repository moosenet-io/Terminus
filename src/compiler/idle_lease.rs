//! BLD-11 — compiler ↔ idle-mode LEASE wiring.
//!
//! A HEAVY constellation build runs on the big-RAM/GPU host, which normally also
//! serves Chord (the LLM proxy) and hosts MINT's GPU-heavy profiling sweeps. To
//! hand that host to a build without permanently tearing either down, the scheduler
//! takes an **idle-mode lease** around the heavy build:
//!
//!   1. [`acquire`](IdleModeLease::acquire) — when idle coordination is ENABLED,
//!      signal Chord (BLD-09) and MINT (BLD-10) to go *idle* (drain, release their
//!      GPU/RAM) around EVERY heavy build. If a freed-RAM budget is configured, then
//!      additionally **WAIT** until the freed RAM reaches it before the build starts
//!      (never under budget); with no budget, proceed as soon as idle is confirmed.
//!   2. The build runs while the [`LeaseGuard`] is held.
//!   3. Release — [`activate`] both services again. Release is **guaranteed**: the
//!      guard's `Drop` reactivates on a normal return, an early return, OR a panic
//!      (a crashed build never leaves Chord/MINT stuck idle), and a **max-lease
//!      watchdog** force-activates if the build hangs past a hard bound.
//!
//! **Idle coordination is ENABLED by backend availability (or `BUILD_IDLE_ENABLED`),
//! NOT by whether a RAM budget is set.** A no-budget heavy build still idles+releases;
//! the budget is only the optional freed-RAM wait-gate. A NO-OP lease (build directly,
//! no idle) is used ONLY when coordination is genuinely disabled/unavailable. Every
//! idle/activate call is bounded by a per-operation timeout, so a hung backend can
//! never hang dispatch or queue finalization — it degrades (abort+requeue, or
//! retry-on-release) instead.
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
/// Per-OPERATION timeout (secs) bounding EACH individual idle/activate call — Chord
/// AND MINT — so a hung backend can never hang dispatch or queue finalization. From
/// `BUILD_IDLE_OP_TIMEOUT_SECS`.
const OP_TIMEOUT_ENV: &str = "BUILD_IDLE_OP_TIMEOUT_SECS";
/// Explicit on/off switch for idle coordination. `0`/`false`/`no`/`off` ⇒ OFF (build
/// directly, no idle). Unset ⇒ AUTO (enabled when the backend is available). A truthy
/// value forces ON. From `BUILD_IDLE_ENABLED`.
const ENABLED_ENV: &str = "BUILD_IDLE_ENABLED";
/// Backoff (secs) between rounds of the PERSISTENT reactivation backstop — the
/// never-give-up loop that keeps retrying activation after the bounded immediate
/// attempts exhaust, so a service is never stranded idle. From
/// `BUILD_IDLE_PERSISTENT_RETRY_SECS`.
const PERSISTENT_RETRY_SECS_ENV: &str = "BUILD_IDLE_PERSISTENT_RETRY_SECS";

const DEFAULT_ACQUIRE_TIMEOUT_SECS: u64 = 120;
/// Default max-lease: a full build timeout plus generous headroom, so the watchdog
/// only ever fires on a genuinely stuck build — never on a legitimately long one.
const DEFAULT_MAX_LEASE_SECS: u64 = super::MAX_BUILD_TIMEOUT_SECS + 1800;
const DEFAULT_POLL_MS: u64 = 1000;
const DEFAULT_CHORD_TIMEOUT_SECS: u64 = 30;
const DEFAULT_ACTIVATE_RETRY_MS: u64 = 500;
/// Default per-operation timeout: bounds a single idle/activate call. Comfortably
/// above the Chord HTTP timeout and a normal MINT drain, short enough that a hung
/// backend degrades promptly instead of stalling dispatch/finalization.
const DEFAULT_OP_TIMEOUT_SECS: u64 = 60;
/// Bounded rounds of the IMMEDIATE reactivation retry per release call. After these
/// exhaust with a service still idle, a PERSISTENT background backstop takes over and
/// never gives up (see `spawn_persistent_backstop`).
const RELEASE_MAX_ATTEMPTS: u32 = 5;
/// Default backoff between PERSISTENT reactivation-backstop rounds.
const DEFAULT_PERSISTENT_RETRY_SECS: u64 = 30;

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
    /// Per-OPERATION timeout bounding EACH idle/activate call (Chord and MINT) so a
    /// hung backend can never hang dispatch or finalization.
    pub op_timeout: Duration,
    /// Backoff between PERSISTENT reactivation-backstop rounds (never-give-up loop).
    pub persistent_retry: Duration,
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
            op_timeout: Duration::from_secs(env_u64(OP_TIMEOUT_ENV, DEFAULT_OP_TIMEOUT_SECS)),
            persistent_retry: Duration::from_secs(env_u64(
                PERSISTENT_RETRY_SECS_ENV,
                DEFAULT_PERSISTENT_RETRY_SECS,
            )),
        }
    }
}

/// Is idle coordination ENABLED? `BUILD_IDLE_ENABLED` is the explicit switch:
/// falsey (`0`/`false`/`no`/`off`) ⇒ OFF (build directly, no idle); truthy ⇒ ON;
/// unset/blank ⇒ AUTO (enabled when the backend reports itself available). Decoupled
/// from whether a RAM budget is configured — a heavy build idles whenever coordination
/// is enabled, budget or not.
pub fn idle_coordination_enabled(backend: &dyn IdleBackend) -> bool {
    match std::env::var(ENABLED_ENV).ok().map(|s| s.trim().to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => false,
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => true,
        _ => backend.available(),
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
    /// Can this backend coordinate idle-mode at all right now? MINT is in-process so
    /// coordination is available whenever MINT is (Chord is an ADDITIONAL, optional
    /// target). Used by [`idle_coordination_enabled`] for the AUTO decision — NOT tied
    /// to a RAM budget.
    fn available(&self) -> bool;
    /// Is CHORD specifically configured/reachable right now? When `false`, Chord is NOT
    /// part of the lease — it is never idled or activated (MINT-only coordination), so
    /// an UNCONFIGURED Chord can never fail an idle call and requeue-forever (F1).
    fn chord_available(&self) -> bool;
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

/// Poll interval while waiting for a mid-transition MINT to settle to a CONFIRMED idle.
const MINT_SETTLE_POLL: Duration = Duration::from_millis(100);

/// Turn a MINT `enter_idle` outcome into a clean idle result. A CONFIRMED idle
/// (`Entered`/`AlreadyIdle`) yields the freed figure. An `InTransition` did NOT reach a
/// clean idle this call (a concurrent enter/activate was in flight), so — instead of
/// falsely reporting success — this WAITS (polling `is_idle` every `poll`) until MINT is
/// CONFIRMED idle, then returns `Ok(None)`. It never returns on its own while MINT is
/// still mid-transition: the caller's per-op `timeout` bounds the wait and, on expiry,
/// cancels this future so `acquire_lease` treats it as a FAILED idle (abort + requeue).
/// This guarantees acquire never proceeds to a build while MINT is merely mid-transition.
/// Generic over the idle observer so it is unit-testable without the global controller.
async fn settle_mint_idle<F>(
    enter: (crate::mint::idle::EnterOutcome, Option<crate::mint::idle::IdleReport>),
    is_idle: F,
    poll: Duration,
) -> Result<Option<f64>, String>
where
    F: Fn() -> bool,
{
    use crate::mint::idle::EnterOutcome;
    let (outcome, report) = enter;
    match outcome {
        // Confirmed idle now (or already idle from a prior lease) — surface the freed figure.
        EnterOutcome::Entered(_) | EnterOutcome::AlreadyIdle(_) => Ok(report.and_then(|r| r.freed_gb)),
        // NOT a clean idle: wait until CONFIRMED idle (bounded by the caller's op timeout,
        // which cancels this if MINT never settles → treated as a failed idle).
        EnterOutcome::InTransition => loop {
            if is_idle() {
                return Ok(None);
            }
            tokio::time::sleep(poll).await;
        },
    }
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
        use crate::mint::idle::{enter_idle, mint_idle as mint_controller};
        let enter = enter_idle(LEASE_REASON).await;
        // On `InTransition`, poll the process-global MINT controller's settled state.
        // Bounded by the caller's per-op timeout (which cancels this if it never settles).
        settle_mint_idle(
            enter,
            || mint_controller().is_idle(),
            MINT_SETTLE_POLL,
        )
        .await
    }

    async fn mint_activate(&self) -> Result<(), String> {
        crate::mint::idle::activate(LEASE_REASON).await;
        Ok(())
    }

    fn mem_available_gb(&self) -> Option<f64> {
        crate::mint::idle::read_mem_available_gb()
    }

    fn available(&self) -> bool {
        // MINT idle-mode is ALWAYS available in-process (the intake harness is
        // embedded in this binary), so idle coordination can always at least idle
        // MINT. Chord is an additional, optional target (its control URL may be
        // unset); its absence degrades coordination but does not disable it.
        true
    }

    fn chord_available(&self) -> bool {
        // Chord is only reachable when its control endpoint is configured. If it isn't,
        // it is left OUT of the lease entirely (MINT-only) — never idled/failed (F1).
        crate::config::chord_control_url().is_some()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The lease + its guaranteed release
// ─────────────────────────────────────────────────────────────────────────────

/// Why a heavy build could not take its idle lease. In BOTH cases the heavy build MUST
/// NOT run and the scheduler requeues it; any service that was idled is GUARANTEED
/// reactivation (retry + watchdog) before/after the error, so nothing is left idle.
#[derive(Debug)]
pub enum LeaseError {
    /// The host's TOTAL AVAILABLE RAM never reached the build's budget within the
    /// acquire timeout (a build needs at least `budget_gb` available to run).
    InsufficientRam { available_gb: f64, budget_gb: f64 },
    /// Idle coordination itself FAILED/timed out (a service could not be idled), so we
    /// cannot guarantee the host is free — degrade SAFELY by aborting + requeueing
    /// rather than building uncoordinated.
    IdleFailed { reason: String },
    /// The lease was RELEASED/EXPIRED (e.g. the max-lease watchdog fired) WHILE acquire
    /// was still waiting for RAM — the services are (being) reactivated, so the build
    /// must NOT start under a dead lease. Abort + requeue.
    LeaseExpired { reason: String },
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::InsufficientRam {
                available_gb,
                budget_gb,
            } => write!(
                f,
                "only {available_gb:.1} GiB available (< {budget_gb:.1} GiB budget) after \
                 idling within the acquire timeout — refusing to build under budget"
            ),
            LeaseError::IdleFailed { reason } => write!(
                f,
                "idle coordination failed ({reason}) — refusing to build uncoordinated"
            ),
            LeaseError::LeaseExpired { reason } => write!(
                f,
                "idle lease expired mid-acquire ({reason}) — refusing to build under a dead lease"
            ),
        }
    }
}

/// Bound an idle/activate backend call by a per-operation timeout, flattening a
/// timeout into the same `Err(String)` shape as a call failure — so a HUNG backend
/// is treated identically to a failing one (safe degradation) and can never hang the
/// awaiting path (dispatch or finalization).
async fn with_op_timeout<T, F>(op_timeout: Duration, what: &str, fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    match tokio::time::timeout(op_timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!("{what} timed out after {}s", op_timeout.as_secs())),
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
/// (in a rare concurrent double-fire) is harmless. Each activate call is bounded by
/// `op_timeout`, so a HUNG activate is treated as a (retryable) failure, never a hang.
struct LeaseInner {
    backend: Arc<dyn IdleBackend>,
    /// Confirmed-active flags — set ONLY after that service's `activate` returns Ok.
    /// `chord_active` is initialised `true` when Chord is NOT part of the lease (not
    /// configured), so release never waits on / touches an absent Chord (F1).
    chord_active: AtomicBool,
    mint_active: AtomicBool,
    /// Backoff between release retry rounds.
    retry_backoff: Duration,
    /// Max release retry rounds before giving up this call (a later call, or the
    /// per-service idle watchdogs, remain the backstop).
    max_attempts: u32,
    /// Per-operation timeout bounding each `activate` call.
    op_timeout: Duration,
    /// Serializes reactivation (F5): only ONE activation sequence runs at a time, so
    /// concurrent `release()` calls (explicit release racing the watchdog / a drop)
    /// COALESCE instead of firing redundant concurrent activate calls.
    release_lock: tokio::sync::Mutex<()>,
    /// Set the instant ANY release begins (watchdog, explicit, drop). Signals the lease
    /// is dead — `acquire_lease`'s wait loop checks this and REFUSES to hand back a lease
    /// whose services are being reactivated (F1: never build under an expired lease).
    expired: AtomicBool,
    /// Woken the instant `expired` is set, so `acquire_lease`'s budget wait is
    /// INTERRUPTIBLE — it returns `LeaseExpired` immediately on expiry instead of
    /// sleeping out the rest of a (possibly long) `BUILD_IDLE_POLL_MS` interval.
    expired_notify: tokio::sync::Notify,
    /// Backoff between rounds of the PERSISTENT reactivation backstop.
    persistent_backoff: Duration,
    /// Set while a persistent reactivation backstop task is running, so at most ONE
    /// runs (a later release that finds a service still idle does not spawn a second).
    persistent_running: AtomicBool,
}

impl LeaseInner {
    /// Both services confirmed reactivated?
    fn fully_released(&self) -> bool {
        self.chord_active.load(Ordering::SeqCst) && self.mint_active.load(Ordering::SeqCst)
    }

    /// Has release begun (watchdog fired / explicit release / drop)? Once true, the
    /// lease is dead and must never be handed to a build.
    fn is_expired(&self) -> bool {
        self.expired.load(Ordering::SeqCst)
    }

    /// One activation pass: attempt each service NOT yet confirmed active (bounded by
    /// the per-op timeout); mark it done only on a successful `activate`. Returns
    /// whether both are now confirmed.
    async fn try_activate_pending(&self) -> bool {
        if !self.chord_active.load(Ordering::SeqCst) {
            match with_op_timeout(self.op_timeout, "Chord activate", self.backend.chord_activate())
                .await
            {
                Ok(()) => self.chord_active.store(true, Ordering::SeqCst),
                Err(e) => warn!(error = %e, "idle lease: Chord activate failed/timed out — will retry (not marking released)"),
            }
        }
        if !self.mint_active.load(Ordering::SeqCst) {
            match with_op_timeout(self.op_timeout, "MINT activate", self.backend.mint_activate())
                .await
            {
                Ok(()) => self.mint_active.store(true, Ordering::SeqCst),
                Err(e) => warn!(error = %e, "idle lease: MINT activate failed/timed out — will retry (not marking released)"),
            }
        }
        self.fully_released()
    }

    /// Reactivate both services, retrying any that fail with bounded backoff so a
    /// transient partial failure self-heals instead of leaving a service stuck idle.
    /// Idempotent + re-entrant: a service already confirmed active is never touched
    /// again by this call, and re-invoking after a partial failure resumes ONLY the
    /// still-pending service. SERIALIZED under `release_lock`: concurrent callers
    /// coalesce — the second waits, then finds the work already done (or resumes the
    /// still-pending service) rather than racing a duplicate concurrent activation.
    ///
    /// NEVER GIVES UP (F1): if the bounded IMMEDIATE attempts exhaust with a service
    /// still idle, a PERSISTENT background backstop is started that keeps retrying until
    /// BOTH services are ACTIVE. `release` itself does NOT block on that backstop (so the
    /// scheduler's completion path never hangs on a down backend), but a service is never
    /// stranded idle: the backstop runs until fully released.
    async fn release(self: Arc<Self>) {
        // Mark the lease dead the instant release begins, so a concurrent
        // `acquire_lease` wait loop observes the expiry and refuses to hand back a lease
        // whose services are being reactivated (F1). Set BEFORE the fast-path return so
        // even an already-fully-released lease reads as expired. Wake any interruptible
        // acquire wait immediately (F2) so it returns without sleeping out its poll.
        self.expired.store(true, Ordering::SeqCst);
        self.expired_notify.notify_waiters();
        // Fast path: already done (lock-free).
        if self.fully_released() {
            return;
        }
        {
            // Serialize: only one activation sequence at a time.
            let _seq = self.release_lock.lock().await;
            // Another caller may have finished while we waited for the lock.
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
        } // drop the lock BEFORE spawning the persistent backstop (it re-acquires it)
        // Bounded immediate attempts exhausted with a service still idle → NEVER give up:
        // hand off to a persistent background backstop that retries until fully active.
        warn!(
            chord_active = self.chord_active.load(Ordering::SeqCst),
            mint_active = self.mint_active.load(Ordering::SeqCst),
            "idle lease: a service is still idle after the bounded retries — starting the \
             PERSISTENT reactivation backstop (never gives up until active)"
        );
        self.spawn_persistent_backstop();
    }

    /// Ensure a PERSISTENT reactivation backstop is running: a detached task that keeps
    /// retrying activation (with `persistent_backoff` between rounds) until BOTH services
    /// are ACTIVE, then stops. At most ONE runs at a time (a later release that still
    /// finds a service idle relies on the existing one). A service is thus NEVER stranded
    /// idle after the bounded immediate attempts (F1). Requires an ambient runtime to
    /// spawn; without one (a rare no-runtime crash drop) the immediate blocking attempts
    /// already ran and there is nothing further to do without a runtime.
    fn spawn_persistent_backstop(self: &Arc<Self>) {
        if self.fully_released() {
            return;
        }
        // Claim the single backstop slot; if one is already running it will converge.
        if self.persistent_running.swap(true, Ordering::SeqCst) {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            self.persistent_running.store(false, Ordering::SeqCst);
            warn!("idle lease: no ambient runtime for a persistent reactivation backstop (blocking attempts already ran)");
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                {
                    let _seq = this.release_lock.lock().await;
                    if this.try_activate_pending().await {
                        info!("idle lease: persistent backstop reactivated all services — stopping");
                        break;
                    }
                }
                warn!("idle lease: a service is STILL idle — persistent reactivation backstop will retry (never gives up)");
                tokio::time::sleep(this.persistent_backoff).await;
            }
            this.persistent_running.store(false, Ordering::SeqCst);
        });
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

    /// Has this lease been released/expired (watchdog fired, or a release began)? A
    /// no-op guard is never expired. `acquire_lease` checks this during its wait.
    fn is_expired(&self) -> bool {
        self.inner.as_ref().map(|i| i.is_expired()).unwrap_or(false)
    }

    /// A clone of the shared inner (present for a real, non-noop guard) so
    /// `acquire_lease` can await the expiry notifier for an interruptible wait (F2).
    fn inner_handle(&self) -> Option<Arc<LeaseInner>> {
        self.inner.clone()
    }

    /// A guard HOLDING `inner` but with NO max-lease watchdog armed yet. Used during the
    /// idle calls; [`arm_watchdog`](Self::arm_watchdog) arms it only AFTER idle completes
    /// (F1), so the watchdog can never fire against an in-flight idle.
    fn held(inner: Arc<LeaseInner>) -> Self {
        Self {
            inner: Some(inner),
            watchdog: None,
        }
    }

    /// Arm (or replace) the max-lease watchdog: after `max_lease`, force a release that
    /// reactivates the (genuinely-idled) services — the fail-safe against a hung/forgotten
    /// build. Called only once both idle calls have COMPLETED.
    fn arm_watchdog(&mut self, max_lease: Duration) {
        if let Some(inner) = self.inner.clone() {
            let handle = tokio::spawn(async move {
                tokio::time::sleep(max_lease).await;
                warn!(
                    max_lease_secs = max_lease.as_secs(),
                    "idle lease: MAX-LEASE timeout reached — force-activating (a build hung or was forgotten)"
                );
                inner.release().await;
            });
            self.watchdog = Some(handle);
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

    /// Abort path (F3): the build is being ABANDONED (insufficient RAM / idle failure /
    /// expired lease) but services were already idled — GUARANTEE they return to ACTIVE.
    /// Kicks off a retrying reactivation NOW and LEAVES the max-lease watchdog armed as
    /// the ultimate backstop (it is NOT aborted). Consumes the guard; caller returns the
    /// abort error.
    fn reactivate_detached(mut self) {
        // Detach (but DO NOT abort) the watchdog so it survives as a backstop; it holds
        // its own Arc<LeaseInner>, so dropping this handle keeps the task running.
        let _ = self.watchdog.take();
        if let Some(inner) = self.inner.take() {
            drive_release(inner);
        }
    }
}

/// Drive `inner.release()` to completion, GUARANTEED even without an ambient Tokio
/// runtime (F3). In a runtime we detach it (spawn); with NO runtime we block on a
/// short-lived current-thread runtime so services are never left idle after a
/// crash-drop that happens outside async context.
fn drive_release(inner: Arc<LeaseInner>) {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(async move { inner.release().await });
    } else {
        // No ambient runtime: block on a private current-thread runtime so reactivation
        // still runs to completion (the `release()` internals use tokio time/sync/timeout).
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(inner.release()),
            Err(e) => warn!(error = %e,
                "idle lease: no ambient runtime and could not build one — cannot guarantee reactivation"),
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
        // In an ambient runtime the watchdog is aborted (the release below covers it);
        // with NO runtime the watchdog task can't run here anyway, so we leave its handle
        // and GUARANTEE reactivation via a blocking release in `drive_release`.
        let in_runtime = tokio::runtime::Handle::try_current().is_ok();
        if in_runtime {
            if let Some(w) = self.watchdog.take() {
                w.abort();
            }
        }
        if let Some(inner) = self.inner.take() {
            // The build returned early or PANICKED without an explicit release — GUARANTEE
            // reactivation (detached in a runtime; blocking otherwise, F3).
            warn!("idle lease guard dropped without explicit release (early return/panic) — reactivating Chord + MINT");
            drive_release(inner);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Acquire: idle the available backends, arm the guard, gate on available RAM
// ─────────────────────────────────────────────────────────────────────────────

/// Acquire the idle-mode lease for a heavy build. **Idle coordination (whether we idle
/// at all) is decided by `enabled`, NOT by the RAM budget** — the feature's whole point
/// is to free the host around EVERY heavy build. The RAM budget is an OPTIONAL,
/// additional wait-gate.
///
/// ## `enabled == false` — coordination unavailable/disabled
/// No idle call is made; the build runs directly ([`LeaseGuard::noop`]). The ONLY
/// no-idle path — chosen because the backend is genuinely unavailable or
/// `BUILD_IDLE_ENABLED=0`, NOT merely because no budget is set.
///
/// ## `enabled == true` — coordination on
/// Idle the AVAILABLE backends: always MINT (in-process); Chord ONLY when configured
/// ([`IdleBackend::chord_available`]) — an unconfigured Chord is left out of the lease
/// entirely, never called/failed (F1). Each idle call is bounded by the per-op timeout;
/// a FAILED/HUNG idle ⇒ degrade safely (guaranteed reactivation + requeue, F3). The
/// max-lease watchdog is armed AS SOON AS services are idled, so it bounds the whole
/// idle window INCLUDING the budget wait (F4). Then:
/// - **`budget_gb <= 0.0`** — no threshold to wait on: proceed once idle is confirmed.
/// - **`budget_gb > 0.0`** — WAIT until the host's TOTAL AVAILABLE RAM (`MemAvailable`)
///   reaches `budget_gb` (F2 — a build needs that much available, whether just freed or
///   already free), bounded by the acquire timeout; on timeout, guaranteed-reactivate
///   and return [`LeaseError::InsufficientRam`] (requeue — never builds under budget).
///
/// On success returns a [`LeaseGuard`] whose drop/watchdog guarantee reactivation.
/// Generic over [`IdleBackend`] so it is fully testable offline.
pub async fn acquire_lease(
    backend: Arc<dyn IdleBackend>,
    cfg: &IdleLeaseConfig,
    enabled: bool,
    budget_gb: f64,
) -> Result<LeaseGuard, LeaseError> {
    // Coordination disabled/unavailable ⇒ no idle call is made. BUT disabling
    // COORDINATION must NOT disable the RAM GATE (F2): if a budget is configured, still
    // enforce it before building — WITHOUT idling (we can't free RAM with coordination
    // off), so we can only wait for the host to already have enough available, else
    // abort + requeue (never build under budget). No lease/watchdog is needed (nothing
    // is idled), so a `noop` guard is returned on success.
    if !enabled {
        if budget_gb <= 0.0 {
            info!("idle lease: coordination disabled/unavailable + no budget — building directly (no idle)");
            return Ok(LeaseGuard::noop());
        }
        info!(
            budget_gb,
            "idle lease: coordination disabled — enforcing the RAM gate WITHOUT idling"
        );
        let deadline = tokio::time::Instant::now() + cfg.acquire_timeout;
        loop {
            let available = backend.mem_available_gb();
            if available.map(|a| a >= budget_gb).unwrap_or(false) {
                info!(
                    available_gb = available,
                    budget_gb,
                    "idle lease: available RAM already meets the budget — building directly (noop lease, no idle)"
                );
                return Ok(LeaseGuard::noop());
            }
            if tokio::time::Instant::now() >= deadline {
                warn!(
                    available_gb = available,
                    budget_gb,
                    "idle lease: available RAM below budget with idle coordination DISABLED \
                     (cannot free RAM) — refusing the heavy build (it will be requeued)"
                );
                return Err(LeaseError::InsufficientRam {
                    available_gb: available.unwrap_or(0.0),
                    budget_gb,
                });
            }
            tokio::time::sleep(cfg.poll).await;
        }
    }

    // Chord is part of the lease ONLY when configured (F1: never call an absent Chord).
    let chord_in_lease = backend.chord_available();
    info!(
        budget_gb,
        chord_in_lease, "idle lease: idling for a heavy build (MINT always; Chord iff configured)"
    );

    // Build the release machinery (inner) but DO NOT arm the max-lease watchdog yet
    // (F1): the watchdog must never fire against an IN-FLIGHT idle call — if it did, a
    // premature release would mark services active and the idle call would then land,
    // stranding the service idle. So we arm the watchdog ONLY AFTER both idle calls
    // COMPLETE. A Chord left out of the lease starts already-"active" (release skips it).
    let mut guard = LeaseGuard::held(build_lease_inner(
        backend.clone(),
        cfg.activate_retry,
        cfg.op_timeout,
        cfg.persistent_retry,
        chord_in_lease,
    ));

    // Idle the available backends (bounded by the per-op timeout). Capture the freed-RAM
    // each reports, used ONLY as a fallback when `/proc/meminfo` is unreadable (F2). A
    // FAILED or HUNG idle ⇒ degrade safely: reactivate anything that landed + abort
    // (requeue, F3). No watchdog is armed yet, so none can race an in-flight idle.
    let mut chord_freed = None;
    if chord_in_lease {
        match with_op_timeout(cfg.op_timeout, "Chord idle", backend.chord_idle()).await {
            Ok(f) => chord_freed = f,
            Err(e) => return abort_with_guard(guard, "Chord idle", e),
        }
    }
    let mint_freed = match with_op_timeout(cfg.op_timeout, "MINT idle", backend.mint_idle()).await {
        Ok(f) => f,
        Err(e) => return abort_with_guard(guard, "MINT idle", e),
    };
    // Sum the reported freed (fallback budget estimate when MemAvailable is unreadable).
    let reported_freed = chord_freed.unwrap_or(0.0).max(0.0) + mint_freed.unwrap_or(0.0).max(0.0);

    // Both idle calls have COMPLETED (services actually idled) → NOW arm the max-lease
    // watchdog. It still bounds the ENTIRE budget wait below (F4), but can only ever fire
    // against genuinely-idled services, so its release always correctly reactivates them.
    guard.arm_watchdog(cfg.max_lease);

    // No budget ⇒ idle confirmed; proceed WITHOUT waiting on an available-RAM threshold.
    if budget_gb <= 0.0 {
        // Even here, if the lease already expired (a max_lease≈0 watchdog fired the
        // instant we armed it) do NOT hand back a dead lease.
        if guard.is_expired() {
            return expired_abort(guard, "lease expired during idle");
        }
        info!("idle lease acquired — idled (no available-RAM budget to wait on)");
        return Ok(guard);
    }

    // Budget configured ⇒ WAIT until enough RAM is established, bounded by the acquire
    // timeout. PRIMARY gate: total AVAILABLE RAM (`MemAvailable`) ≥ budget (a build
    // needs that much available). FALLBACK when MemAvailable is unreadable: the services'
    // REPORTED freed-RAM ≥ budget (never abort-forever just because /proc is blind).
    // The wait is INTERRUPTIBLE by expiry (F2): the max-lease watchdog wakes it at once
    // via `expired_notify`, so it never sleeps out a large poll interval under a dead lease.
    let inner = guard
        .inner_handle()
        .expect("a real (armed) guard always has inner");
    let deadline = tokio::time::Instant::now() + cfg.acquire_timeout;
    loop {
        // F1: if the lease was released/expired (watchdog fired) mid-wait, the services
        // are being reactivated — NEVER hand back a dead lease; abort + requeue.
        if guard.is_expired() {
            return expired_abort(guard, "max-lease watchdog fired during the budget wait");
        }
        let available = backend.mem_available_gb();
        let estimate = available.unwrap_or(reported_freed); // available if measurable, else reported
        if estimate >= budget_gb {
            // Re-check expiry immediately before returning Ok (watchdog could fire right
            // as the budget is met) — never return Ok(guard) after expiry (F1).
            if guard.is_expired() {
                return expired_abort(guard, "max-lease watchdog fired as the budget was met");
            }
            info!(
                available_gb = available,
                reported_freed,
                budget_gb,
                "idle lease acquired — RAM budget established (available RAM, or reported-freed fallback)"
            );
            return Ok(guard);
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                available_gb = available,
                reported_freed,
                budget_gb,
                "idle lease: RAM below budget after the acquire timeout — guaranteeing \
                 reactivation and refusing the heavy build (it will be requeued)"
            );
            // Guaranteed reactivation (retry + watchdog) — never leave services idle for
            // a build we're not going to run (F3).
            guard.reactivate_detached();
            return Err(LeaseError::InsufficientRam {
                available_gb: estimate,
                budget_gb,
            });
        }
        // INTERRUPTIBLE poll (F2): register for the expiry notification and enable it
        // BEFORE re-checking `is_expired`, so an expiry that races the registration is
        // never missed — then wait for the poll tick OR an immediate expiry wake.
        let notified = inner.expired_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if guard.is_expired() {
            return expired_abort(guard, "lease expired just before the interruptible sleep");
        }
        tokio::select! {
            _ = &mut notified => { /* woken by expiry — loop re-checks and aborts */ }
            _ = tokio::time::sleep(cfg.poll) => { /* normal poll tick */ }
        }
    }
}

/// The lease released/expired while acquire was still waiting (F1): guarantee
/// reactivation of the (being-torn-down) services and return a requeue error — the
/// heavy build must NEVER start under a dead lease.
fn expired_abort(guard: LeaseGuard, reason: &str) -> Result<LeaseGuard, LeaseError> {
    warn!(reason, "idle lease: expired mid-acquire — refusing to build under a dead lease; requeueing");
    guard.reactivate_detached();
    Err(LeaseError::LeaseExpired {
        reason: reason.to_string(),
    })
}

/// Idle-coordination failure/timeout while enabled: GUARANTEE reactivation of anything
/// idled (retry + the guard's still-armed watchdog, F3), then abort so the scheduler
/// requeues (never build uncoordinated after a failed idle call).
fn abort_with_guard(
    guard: LeaseGuard,
    what: &str,
    err: String,
) -> Result<LeaseGuard, LeaseError> {
    warn!(error = %err, "idle lease: {what} failed/timed out — aborting (safe degrade), guaranteeing reactivation, the build is requeued");
    guard.reactivate_detached();
    Err(LeaseError::IdleFailed {
        reason: format!("{what}: {err}"),
    })
}

/// Build the shared release machinery (`LeaseInner`) for a lease, WITHOUT a watchdog.
/// `chord_in_lease` false ⇒ Chord starts already-"active" (not part of the lease, never
/// idled/activated).
fn build_lease_inner(
    backend: Arc<dyn IdleBackend>,
    activate_retry: Duration,
    op_timeout: Duration,
    persistent_retry: Duration,
    chord_in_lease: bool,
) -> Arc<LeaseInner> {
    Arc::new(LeaseInner {
        backend,
        chord_active: AtomicBool::new(!chord_in_lease),
        mint_active: AtomicBool::new(false),
        retry_backoff: activate_retry,
        max_attempts: RELEASE_MAX_ATTEMPTS,
        op_timeout,
        release_lock: tokio::sync::Mutex::new(()),
        expired: AtomicBool::new(false),
        expired_notify: tokio::sync::Notify::new(),
        persistent_backoff: persistent_retry,
        persistent_running: AtomicBool::new(false),
    })
}

/// Build a fully-armed `LeaseGuard` (inner + max-lease watchdog). Used by tests; the
/// production `acquire_lease` builds the inner first and arms the watchdog only AFTER the
/// idle calls complete (F1). `chord_in_lease` false ⇒ Chord is not part of the lease.
#[cfg(test)]
fn arm_guard(
    backend: Arc<dyn IdleBackend>,
    max_lease: Duration,
    activate_retry: Duration,
    op_timeout: Duration,
    chord_in_lease: bool,
) -> LeaseGuard {
    let mut guard = LeaseGuard::held(build_lease_inner(
        backend,
        activate_retry,
        op_timeout,
        Duration::from_millis(1), // fast persistent backstop in tests
        chord_in_lease,
    ));
    guard.arm_watchdog(max_lease);
    guard
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
        // Coordination-enabled is decided by backend availability / the explicit knob
        // — NOT by whether this build has a RAM budget. The budget is only the
        // optional freed-RAM wait-gate applied AFTER idling.
        let enabled = idle_coordination_enabled(self.backend.as_ref());
        let budget = self.budget_for(job);
        acquire_lease(self.backend.clone(), &self.cfg, enabled, budget).await
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
        fn available(&self) -> bool {
            true
        }
        fn chord_available(&self) -> bool {
            true
        }
    }
    arm_guard(
        Arc::new(CountingReleaseBackend { counter }),
        Duration::from_secs(3600),
        Duration::from_millis(1),
        Duration::from_secs(5),
        true,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::Mutex;

    /// A long "hang" used by hung-call tests: longer than any test's per-op timeout,
    /// so the `with_op_timeout` wrapper always cancels it (the call never returns).
    const HANG: Duration = Duration::from_secs(3600);

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
        /// If set, chord_idle/mint_idle return Err.
        fail_idle: AtomicBool,
        /// If set, chord_idle HANGS (never returns) — the per-op timeout must fire.
        chord_idle_hangs: AtomicBool,
        /// The next N `chord_activate` calls fail (transient); after that, succeed.
        chord_activate_fail_times: AtomicUsize,
        /// The next N `chord_activate` calls HANG (per-op timeout must fire).
        chord_activate_hang_times: AtomicUsize,
        /// Whether the backend reports itself available for coordination.
        available: AtomicBool,
        /// Whether CHORD specifically is configured/part of the lease.
        chord_available: AtomicBool,
        /// If >0, `mint_idle` sleeps this long BEFORE it actually idles — an in-flight
        /// window used to prove the watchdog can't fire mid-idle (F1).
        mint_idle_delay_ms: AtomicU64,
        /// REAL current idle state per service: set true when its idle lands, false when
        /// its activate lands. Lets a test detect a service left STUCK idle (unlike the
        /// call counters, which can't distinguish a premature activate from a real one).
        chord_is_idle: AtomicBool,
        mint_is_idle: AtomicBool,
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
                fail_idle: AtomicBool::new(false),
                chord_idle_hangs: AtomicBool::new(false),
                chord_activate_fail_times: AtomicUsize::new(0),
                chord_activate_hang_times: AtomicUsize::new(0),
                available: AtomicBool::new(true),
                chord_available: AtomicBool::new(true),
                mint_idle_delay_ms: AtomicU64::new(0),
                chord_is_idle: AtomicBool::new(false),
                mint_is_idle: AtomicBool::new(false),
            })
        }
        fn with_mint_idle_delay_ms(self: Arc<Self>, ms: u64) -> Arc<Self> {
            self.mint_idle_delay_ms.store(ms, Ordering::SeqCst);
            self
        }
        fn is_stuck_idle(&self) -> bool {
            self.chord_is_idle.load(Ordering::SeqCst) || self.mint_is_idle.load(Ordering::SeqCst)
        }
        fn with_fail_idle(self: Arc<Self>) -> Arc<Self> {
            self.fail_idle.store(true, Ordering::SeqCst);
            self
        }
        fn with_chord_unavailable(self: Arc<Self>) -> Arc<Self> {
            self.chord_available.store(false, Ordering::SeqCst);
            self
        }
        fn with_chord_idle_hang(self: Arc<Self>) -> Arc<Self> {
            self.chord_idle_hangs.store(true, Ordering::SeqCst);
            self
        }
        fn with_mem(self: Arc<Self>, samples: Vec<f64>) -> Arc<Self> {
            *self.mem_samples.lock().unwrap() = samples;
            self
        }
        fn with_chord_activate_fails(self: Arc<Self>, n: usize) -> Arc<Self> {
            self.chord_activate_fail_times.store(n, Ordering::SeqCst);
            self
        }
        fn with_chord_activate_hangs(self: Arc<Self>, n: usize) -> Arc<Self> {
            self.chord_activate_hang_times.store(n, Ordering::SeqCst);
            self
        }
        fn with_unavailable(self: Arc<Self>) -> Arc<Self> {
            self.available.store(false, Ordering::SeqCst);
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
            if self.chord_idle_hangs.load(Ordering::SeqCst) {
                tokio::time::sleep(HANG).await; // cancelled by the per-op timeout
            }
            if self.fail_idle.load(Ordering::SeqCst) {
                Err("chord down".into())
            } else {
                self.chord_is_idle.store(true, Ordering::SeqCst); // actually idled now
                Ok(self.chord_freed)
            }
        }
        async fn chord_activate(&self) -> Result<(), String> {
            if self.chord_activate_fail_times.load(Ordering::SeqCst) > 0 {
                self.chord_activate_fail_times.fetch_sub(1, Ordering::SeqCst);
                return Err("chord activate transient failure".into());
            }
            if self.chord_activate_hang_times.load(Ordering::SeqCst) > 0 {
                self.chord_activate_hang_times.fetch_sub(1, Ordering::SeqCst);
                tokio::time::sleep(HANG).await; // cancelled by the per-op timeout
            }
            self.chord_activates.fetch_add(1, Ordering::SeqCst);
            self.chord_is_idle.store(false, Ordering::SeqCst); // reactivated
            Ok(())
        }
        async fn mint_idle(&self) -> Result<Option<f64>, String> {
            // Optional in-flight delay BEFORE the service actually idles (F1 test).
            let delay = self.mint_idle_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            self.mint_idles.fetch_add(1, Ordering::SeqCst);
            if self.fail_idle.load(Ordering::SeqCst) {
                Err("mint down".into())
            } else {
                self.mint_is_idle.store(true, Ordering::SeqCst); // actually idled now
                Ok(self.mint_freed)
            }
        }
        async fn mint_activate(&self) -> Result<(), String> {
            self.mint_activates.fetch_add(1, Ordering::SeqCst);
            self.mint_is_idle.store(false, Ordering::SeqCst); // reactivated
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
        fn available(&self) -> bool {
            self.available.load(Ordering::SeqCst)
        }
        fn chord_available(&self) -> bool {
            self.chord_available.load(Ordering::SeqCst)
        }
    }

    /// Config with the RAM budget + `enabled` passed SEPARATELY to `acquire_lease`.
    /// A generous per-op timeout by default; hung-call tests shorten `op_timeout`.
    fn cfg(acquire_secs: u64, max_lease_secs: u64) -> IdleLeaseConfig {
        IdleLeaseConfig {
            freed_ram_budget_gb: 0.0, // global default; per-call budget is explicit
            acquire_timeout: Duration::from_secs(acquire_secs),
            max_lease: Duration::from_secs(max_lease_secs),
            poll: Duration::from_millis(1),
            chord_timeout: Duration::from_secs(1),
            activate_retry: Duration::from_millis(1),
            op_timeout: Duration::from_secs(5),
            persistent_retry: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn coordination_disabled_no_budget_builds_directly_with_no_idle_calls() {
        // Coordination DISABLED (enabled=false) + NO budget ⇒ NO idle call at all,
        // builds directly, no-op guard.
        let be = MockBackend::new(None, None);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), false, 0.0)
            .await
            .expect("disabled + no budget ⇒ builds directly");
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 0, "no chord idle attempted");
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 0, "no mint idle attempted");
        guard.release().await;
        assert_eq!(be.activates(), (0, 0), "nothing idled ⇒ nothing to reactivate");
    }

    #[tokio::test]
    async fn coordination_disabled_still_enforces_budget_aborts_when_under() {
        // FINDING 2: disabling COORDINATION must NOT bypass the RAM GATE. Disabled +
        // budget set + MemAvailable < budget ⇒ abort + requeue (never build under
        // budget), and NO idle call is made (coordination is off).
        let be = MockBackend::new(None, None).with_mem(vec![50.0]);
        let err = acquire_lease(be.clone(), &cfg(0, 3600), false, 120.0)
            .await
            .expect_err("disabled + under budget ⇒ InsufficientRam");
        assert!(
            matches!(err, LeaseError::InsufficientRam { .. }),
            "must refuse to build under budget even with coordination off, got {err:?}"
        );
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 0, "no idle attempted (coordination off)");
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 0, "no idle attempted (coordination off)");
    }

    #[tokio::test]
    async fn coordination_disabled_proceeds_when_available_ram_meets_budget_no_idle() {
        // FINDING 2: disabled + budget set + MemAvailable >= budget ⇒ proceed with a
        // NOOP lease (the host already has the room; no idle calls are made).
        let be = MockBackend::new(None, None).with_mem(vec![200.0]);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), false, 120.0)
            .await
            .expect("disabled + ample available RAM ⇒ proceeds");
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 0, "no chord idle attempted");
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 0, "no mint idle attempted");
        guard.release().await;
        assert_eq!(be.activates(), (0, 0), "nothing idled ⇒ nothing to reactivate");
    }

    #[tokio::test]
    async fn enabled_no_budget_still_idles_and_releases_without_waiting() {
        // FINDING 2 (b): coordination ENABLED with NO budget ⇒ STILL idles Chord+MINT
        // (the feature's whole point), does NOT wait on a freed-RAM threshold, and
        // releases after. Fixes the earlier bug where no-budget skipped idling.
        let be = MockBackend::new(None, None); // no freed figures, no mem samples
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), true, 0.0)
            .await
            .expect("enabled + no budget ⇒ idles, no wait");
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 1, "chord WAS idled");
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 1, "mint WAS idled");
        assert_eq!(be.activates(), (0, 0), "not released while held");
        guard.release().await;
        assert_eq!(be.activates(), (1, 1), "released after the build");
    }

    #[test]
    fn idle_coordination_enabled_honors_the_knob_and_availability() {
        // FINDING 1/2: enabled is decided by the knob / backend availability, NOT by a
        // budget. AUTO (unset) follows availability; the explicit knob overrides both.
        let key = ENABLED_ENV;
        let avail = MockBackend::new(None, None);
        let unavail = MockBackend::new(None, None).with_unavailable();
        // AUTO (unset) ⇒ follows backend.available().
        std::env::remove_var(key);
        assert!(idle_coordination_enabled(avail.as_ref()));
        assert!(!idle_coordination_enabled(unavail.as_ref()));
        // Explicit OFF wins even when the backend is available.
        std::env::set_var(key, "0");
        assert!(!idle_coordination_enabled(avail.as_ref()));
        // Explicit ON wins even when the backend reports unavailable.
        std::env::set_var(key, "true");
        assert!(idle_coordination_enabled(unavail.as_ref()));
        std::env::remove_var(key);
    }

    #[tokio::test]
    async fn mint_only_when_chord_unconfigured_never_calls_chord_no_requeue_forever() {
        // FINDING 1: AUTO mode with Chord NOT configured must coordinate MINT-only —
        // NEVER call an unconfigured chord_idle (which would fail and requeue-forever).
        // With ample available RAM it acquires; on release only MINT is reactivated.
        let be = MockBackend::new(None, None)
            .with_chord_unavailable()
            .with_mem(vec![200.0]);
        // available() is still true (MINT in-process) ⇒ AUTO enables coordination.
        assert!(idle_coordination_enabled(be.as_ref()));
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), true, 120.0)
            .await
            .expect("MINT-only coordination + ample RAM ⇒ acquired (no requeue-forever)");
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 0, "chord NEVER idled");
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 1, "mint WAS idled");
        guard.release().await;
        assert_eq!(be.activates(), (0, 1), "only MINT reactivated; Chord untouched");
    }

    #[tokio::test]
    async fn enabled_idle_failure_aborts_and_requeues_never_uncoordinated() {
        // With coordination ENABLED, a FAILED idle call must abort + requeue — NEVER
        // proceed to build uncoordinated. Reactivation is GUARANTEED (detached retry).
        let be = MockBackend::new(None, None).with_fail_idle();
        let err = acquire_lease(be.clone(), &cfg(5, 3600), true, 120.0)
            .await
            .expect_err("enabled + idle failure ⇒ abort");
        assert!(
            matches!(err, LeaseError::IdleFailed { .. }),
            "must be IdleFailed, got {err:?}"
        );
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(be.activates(), (1, 1), "guaranteed reactivation on abort");
    }

    #[tokio::test]
    async fn hung_idle_call_times_out_and_aborts_requeue_never_hangs() {
        // FINDING 3: an idle backend that HANGS must trip the per-op timeout and
        // degrade to abort+requeue (never builds), and the acquire must RETURN (not
        // hang the caller). A short op_timeout makes the hang trip promptly.
        let be = MockBackend::new(None, None)
            .with_chord_idle_hang()
            .with_mem(vec![200.0]);
        let mut c = cfg(5, 3600);
        c.op_timeout = Duration::from_millis(20);
        let err = tokio::time::timeout(
            Duration::from_secs(5), // the test itself must not hang
            acquire_lease(be.clone(), &c, true, 120.0),
        )
        .await
        .expect("acquire returned (did not hang)")
        .expect_err("hung idle ⇒ IdleFailed abort");
        assert!(
            matches!(err, LeaseError::IdleFailed { .. }),
            "hung idle degrades to IdleFailed, got {err:?}"
        );
        // Guaranteed (detached) reactivation on abort — nothing left idle.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(be.activates(), (1, 1));
    }

    // ── MINT `InTransition` must not count as a clean idle (rev-10 finding) ──────────

    fn manifest() -> crate::mint::idle::ResumeManifest {
        crate::mint::idle::ResumeManifest {
            reason: "compiler-heavy-build".into(),
            entered_at: 0,
            watchdog_deadline: 0,
            released_holders: vec![],
            mem_available_before_gb: 0.0,
        }
    }
    fn idle_report(freed: Option<f64>) -> crate::mint::idle::IdleReport {
        crate::mint::idle::IdleReport {
            mem_available_before_gb: None,
            mem_available_after_gb: None,
            freed_gb: freed,
            holders_released: vec![],
            inflight_remaining: 0,
            foreign_gpu_lock_holder: None,
        }
    }

    #[tokio::test]
    async fn settle_confirmed_idle_returns_freed_without_polling() {
        // A CONFIRMED idle (Entered/AlreadyIdle) returns the freed figure immediately and
        // never polls the settled-state observer.
        use crate::mint::idle::EnterOutcome;
        let polled = std::sync::Arc::new(AtomicBool::new(false));
        let p = polled.clone();
        let out = settle_mint_idle(
            (EnterOutcome::Entered(manifest()), Some(idle_report(Some(42.0)))),
            move || {
                p.store(true, Ordering::SeqCst);
                true
            },
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(out, Ok(Some(42.0)));
        assert!(!polled.load(Ordering::SeqCst), "confirmed idle must not poll the observer");

        // AlreadyIdle behaves the same (idle from a prior lease).
        let out = settle_mint_idle(
            (EnterOutcome::AlreadyIdle(manifest()), None),
            || true,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(out, Ok(None));
    }

    #[tokio::test]
    async fn settle_in_transition_waits_until_confirmed_idle() {
        // An `InTransition` is NOT a clean idle — it must WAIT until the observer reports
        // a settled idle, THEN return Ok (a transient in-transition converges, no requeue).
        use crate::mint::idle::EnterOutcome;
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let out = settle_mint_idle(
            (EnterOutcome::InTransition, None),
            move || c.fetch_add(1, Ordering::SeqCst) >= 2, // idle only from the 3rd poll on
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(out, Ok(None), "settled to idle ⇒ Ok");
        assert!(calls.load(Ordering::SeqCst) >= 3, "polled until confirmed idle");
    }

    #[tokio::test]
    async fn settle_in_transition_that_never_settles_never_returns_ok() {
        // An `InTransition` that NEVER settles must NOT return Ok (never a false idle). It
        // loops until the caller's per-op timeout cancels it — modelled here by an outer
        // timeout. On expiry the (real) acquire path treats it as a FAILED idle → abort.
        use crate::mint::idle::EnterOutcome;
        let res = tokio::time::timeout(
            Duration::from_millis(50),
            settle_mint_idle(
                (EnterOutcome::InTransition, None),
                || false, // never settles
                Duration::from_millis(1),
            ),
        )
        .await;
        assert!(
            res.is_err(),
            "unsettled InTransition must never return Ok — it waits (until cancelled)"
        );
    }

    #[tokio::test]
    async fn mint_mid_transition_that_never_settles_aborts_requeue_never_builds() {
        // End-to-end: when MINT never reaches a clean idle (modelled as a mint_idle that
        // does not return within the per-op timeout — exactly how `settle_mint_idle`
        // behaves on an unsettled InTransition), acquire must ABORT + requeue (IdleFailed),
        // NEVER proceed to a build, and both services are reactivated.
        let be = MockBackend::new(None, None)
            .with_mem(vec![200.0]) // ample RAM — proves the abort is due to MINT, not budget
            .with_mint_idle_delay_ms(3_600_000); // mint_idle never returns in time
        let mut c = cfg(5, 3600);
        c.op_timeout = Duration::from_millis(20);
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            acquire_lease(be.clone(), &c, true, 120.0),
        )
        .await
        .expect("acquire returned (did not hang)")
        .expect_err("unsettled MINT ⇒ IdleFailed abort (never builds)");
        assert!(
            matches!(err, LeaseError::IdleFailed { .. }),
            "MINT never confirmed idle ⇒ IdleFailed, got {err:?}"
        );
        // Reactivated on abort (chord was idled first, then the MINT idle timed out).
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn hung_activate_call_times_out_then_retries_no_stuck_idle() {
        // FINDING 3 (release side): a HUNG chord_activate must trip the per-op timeout
        // (treated as a retryable failure, NOT marking Chord done), and a retry round
        // brings Chord ACTIVE — never stuck idle, never hanging release.
        let be = MockBackend::new(None, None)
            .with_mem(vec![200.0])
            .with_chord_activate_hangs(1);
        let mut c = cfg(5, 3600);
        c.op_timeout = Duration::from_millis(20);
        c.activate_retry = Duration::from_millis(1);
        let guard = acquire_lease(be.clone(), &c, true, 120.0)
            .await
            .expect("budget met ⇒ acquired");
        tokio::time::timeout(Duration::from_secs(5), guard.release())
            .await
            .expect("release returned (did not hang)");
        // Chord ended ACTIVE after the hung attempt timed out + one retry; MINT once.
        assert_eq!(
            be.activates(),
            (1, 1),
            "Chord retried past the hang to ACTIVE; MINT not double-fired"
        );
    }

    #[tokio::test]
    async fn acquires_when_available_ram_meets_budget() {
        // FINDING 2: gate on TOTAL AVAILABLE RAM (MemAvailable ≥ budget), not a
        // freed-delta. A host with 130 GiB available meets a 120 budget immediately.
        let be = MockBackend::new(None, None).with_mem(vec![130.0]);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), true, 120.0)
            .await
            .expect("available RAM ≥ budget ⇒ acquired");
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 1);
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 1);
        assert_eq!(be.activates(), (0, 0));
        guard.release().await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn ample_available_ram_with_little_to_free_still_proceeds() {
        // FINDING 2 (regression): a host with ample AVAILABLE RAM but nothing to free
        // must PROCEED, not time out. (Under the old freed-delta gate this aborted.)
        let be = MockBackend::new(None, None).with_mem(vec![200.0]); // already-idle host
        let guard = acquire_lease(be.clone(), &cfg(1, 3600), true, 120.0)
            .await
            .expect("ample available RAM proceeds without timing out");
        guard.release().await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn waits_for_available_ram_to_climb_to_budget() {
        // MemAvailable climbs 10 → 40 → 135 GiB across polls (e.g. models unloading);
        // the AVAILABLE-RAM gate is met on the third sample (135 ≥ 120).
        let be = MockBackend::new(None, None).with_mem(vec![10.0, 40.0, 135.0]);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), true, 120.0)
            .await
            .expect("available RAM reaches budget ⇒ acquired");
        guard.release().await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn insufficient_ram_aborts_and_reactivates_never_building_under_budget() {
        // Available (50) never reaches the 120 budget; the acquire times out, services
        // are GUARANTEED reactivation, and InsufficientRam is returned so the scheduler
        // requeues instead of building.
        let be = MockBackend::new(None, None).with_mem(vec![50.0]);
        let err = acquire_lease(be.clone(), &cfg(0, 3600), true, 120.0)
            .await
            .expect_err("under budget ⇒ InsufficientRam");
        match err {
            LeaseError::InsufficientRam {
                available_gb,
                budget_gb,
            } => {
                assert_eq!(available_gb, 50.0);
                assert_eq!(budget_gb, 120.0);
            }
            other => panic!("expected InsufficientRam, got {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(be.activates(), (1, 1), "guaranteed reactivation on abort");
    }

    #[tokio::test]
    async fn abort_after_idle_guarantees_reactivation_even_if_first_activate_fails() {
        // FINDING 3: idle succeeds, the build aborts (insufficient RAM), and the FIRST
        // chord_activate transiently fails — the guaranteed reactivation (retry) must
        // still converge, leaving services ACTIVE, not stuck idle.
        let be = MockBackend::new(None, None)
            .with_mem(vec![50.0]) // below the 120 budget ⇒ abort
            .with_chord_activate_fails(1); // first reactivation attempt fails
        let err = acquire_lease(be.clone(), &cfg(0, 3600), true, 120.0)
            .await
            .expect_err("under budget ⇒ abort");
        assert!(matches!(err, LeaseError::InsufficientRam { .. }), "got {err:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            be.activates(),
            (1, 1),
            "reactivation retried to ACTIVE after a transient failure — not stuck idle"
        );
    }

    #[tokio::test]
    async fn partial_activation_failure_retries_until_both_active_no_stuck_idle() {
        // chord_activate fails ONCE then succeeds; MINT (which succeeds first try) must
        // not be double-fired, and Chord must end ACTIVE.
        let be = MockBackend::new(None, None)
            .with_mem(vec![200.0])
            .with_chord_activate_fails(1);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), true, 120.0)
            .await
            .expect("budget met ⇒ acquired");
        guard.release().await;
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
    async fn persistent_backstop_keeps_retrying_past_bounded_attempts_until_active() {
        // FINDING 1: activation fails MORE than RELEASE_MAX_ATTEMPTS times, then succeeds.
        // The bounded immediate attempts exhaust, but the PERSISTENT reactivation backstop
        // keeps retrying until the service ends ACTIVE — never stranded idle after N.
        let fails = RELEASE_MAX_ATTEMPTS as usize + 2; // strictly more than the bounded attempts
        let be = MockBackend::new(None, None)
            .with_mem(vec![200.0])
            .with_chord_activate_fails(fails);
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), true, 120.0)
            .await
            .expect("acquired");
        // Immediate release exhausts its bounded attempts (all fail) → hands off to the
        // persistent backstop, then RETURNS promptly (never blocks on a down backend).
        guard.release().await;
        // The persistent backstop consumes the remaining failures and finally succeeds.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !be.is_stuck_idle(),
            "persistent backstop reactivated the service (not stranded after bounded attempts)"
        );
        assert_eq!(
            be.chord_activates.load(Ordering::SeqCst),
            1,
            "exactly one SUCCESSFUL chord activate (after all the forced failures)"
        );
        assert_eq!(
            be.chord_activate_fail_times.load(Ordering::SeqCst),
            0,
            "every forced failure was consumed — the backstop never gave up"
        );
    }

    #[tokio::test]
    async fn concurrent_release_calls_coalesce_to_a_single_activation_set() {
        // FINDING 5: two concurrent release() calls (e.g. the watchdog waking exactly
        // as the build finishes) must COALESCE — a single set of activation attempts,
        // no duplicate concurrent activations — and services end ACTIVE.
        let be = MockBackend::new(None, None);
        let inner = Arc::new(LeaseInner {
            backend: be.clone(),
            chord_active: AtomicBool::new(false),
            mint_active: AtomicBool::new(false),
            retry_backoff: Duration::from_millis(1),
            max_attempts: 5,
            op_timeout: Duration::from_secs(5),
            release_lock: tokio::sync::Mutex::new(()),
            expired: AtomicBool::new(false),
            expired_notify: tokio::sync::Notify::new(),
            persistent_backoff: Duration::from_millis(1),
            persistent_running: AtomicBool::new(false),
        });
        let (i1, i2) = (inner.clone(), inner.clone());
        tokio::join!(async { i1.release().await }, async { i2.release().await });
        assert_eq!(
            be.activates(),
            (1, 1),
            "coalesced: each service activated exactly once, no concurrent duplicates"
        );
    }

    #[tokio::test]
    async fn max_lease_watchdog_force_activates_when_the_build_hangs() {
        // A tiny max-lease: the guard is held (build "hangs") and never released
        // explicitly; the watchdog must reactivate both on its own.
        let be = MockBackend::new(None, None).with_mem(vec![200.0]);
        let guard = acquire_lease(be.clone(), &cfg(5, 0), true, 120.0)
            .await
            .expect("budget met ⇒ acquired");
        // Hold the guard (do NOT release). max_lease=0 ⇒ the watchdog fires promptly.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            be.activates(),
            (1, 1),
            "watchdog force-activated Chord + MINT after the max-lease timeout"
        );
        guard.release().await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn watchdog_never_fires_against_an_in_flight_idle_no_stuck_service() {
        // FINDING 1: the max-lease watchdog is armed ONLY AFTER the idle calls COMPLETE,
        // so it can never fire mid-idle and strand a service. mint_idle takes 40ms (an
        // in-flight window); max_lease=0 makes the watchdog fire the instant it's armed.
        // The service must end ACTIVE (never stuck idle) — with the old ordering the
        // watchdog would fire DURING mint_idle, prematurely reactivate, then mint_idle
        // would land and leave MINT stuck idle.
        let be = MockBackend::new(None, None)
            .with_mem(vec![200.0])
            .with_mint_idle_delay_ms(40);
        let res = acquire_lease(be.clone(), &cfg(5, 0), true, 120.0).await; // max_lease=0
        // acquire may return Ok (watchdog fires while holding) or Err(LeaseExpired) (it
        // fired during the budget re-check) — either way, drive reactivation and observe.
        if let Ok(guard) = res {
            guard.release().await;
        }
        // MINT was genuinely idled during acquire...
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 1, "MINT actually idled");
        // ...and after the watchdog/release fired, NO service is left stuck idle.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            !be.is_stuck_idle(),
            "no service left stuck idle after a watchdog that could only fire post-idle"
        );
    }

    #[tokio::test]
    async fn watchdog_covers_the_budget_wait_even_when_acquire_timeout_is_longer() {
        // FINDING 4 + FINDING 2: the max-lease watchdog is armed as soon as services are
        // IDLED, so it bounds the WHOLE idle window including the budget wait — and the
        // wait is INTERRUPTIBLE, so acquire returns Err(LeaseExpired) PROMPTLY when the
        // watchdog fires, NOT bounded by the (5s) acquire_timeout nor the (60s) poll.
        // A huge poll proves interruption; we do NOT abort the task to sidestep it.
        let be = MockBackend::new(None, None).with_mem(vec![50.0]); // stays below budget
        let mut c = cfg(5, 0); // acquire_timeout = 5s >> max_lease = 0
        c.poll = Duration::from_secs(60); // huge: acquire must NOT be poll-bound
        let res = tokio::time::timeout(Duration::from_secs(1), acquire_lease(be.clone(), &c, true, 120.0))
            .await
            .expect("acquire returned PROMPTLY (interrupted by the watchdog, not poll/acquire-timeout-bound)");
        assert!(
            matches!(res, Err(LeaseError::LeaseExpired { .. })),
            "watchdog fired mid-wait ⇒ Err(LeaseExpired), never Ok — got {res:?}"
        );
        assert_eq!(
            be.activates(),
            (1, 1),
            "watchdog force-activated DURING the wait (bounded by max_lease, not acquire_timeout)"
        );
    }

    #[tokio::test]
    async fn dropping_the_guard_reactivates_even_on_a_crash_path() {
        // Simulate a crashed/early-returning build: acquire, then DROP the guard
        // without an explicit release. Reactivation must still happen (detached).
        let be = MockBackend::new(None, None).with_mem(vec![200.0]);
        {
            let guard = acquire_lease(be.clone(), &cfg(5, 3600), true, 120.0)
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

    #[tokio::test]
    async fn watchdog_release_mid_wait_makes_acquire_fail_not_build_under_dead_lease() {
        // FINDING 1 (BLOCKER) + FINDING 2 (interruptible): if the max-lease watchdog
        // RELEASES services while acquire is still waiting for RAM, acquire must FAIL
        // (LeaseExpired → requeue) PROMPTLY — never Ok, and never sleeping out a big poll
        // interval. A HUGE poll (60s) proves the wait is interrupted by expiry, not
        // poll-bound: acquire returns well within a second of the 30ms watchdog firing.
        let be = MockBackend::new(None, None).with_mem(vec![50.0]); // stays below budget
        let mut c = cfg(5, 3600); // acquire_timeout = 5s (won't be hit)
        c.max_lease = Duration::from_millis(30); // watchdog fires DURING the wait
        c.poll = Duration::from_secs(60); // huge: without interruption, acquire would hang here
        let res = tokio::time::timeout(Duration::from_secs(1), acquire_lease(be.clone(), &c, true, 120.0))
            .await
            .expect("acquire returned PROMPTLY after expiry (interruptible wait, not poll-bound)");
        assert!(
            matches!(res, Err(LeaseError::LeaseExpired { .. })),
            "expired lease ⇒ acquire must Err(LeaseExpired), never Ok — got {res:?}"
        );
        // Services ended ACTIVE (watchdog reactivated them; the build never started).
        assert_eq!(be.activates(), (1, 1));
    }

    #[tokio::test]
    async fn reported_freed_fallback_satisfies_budget_when_meminfo_unreadable() {
        // FINDING 2: MemAvailable is the PRIMARY gate, but when it is UNREADABLE
        // (no /proc), fall back to the services' reported freed-RAM rather than
        // aborting forever. chord+mint report 70+60=130 ≥ 120 ⇒ acquire.
        let be = MockBackend::new(Some(70.0), Some(60.0)); // reports freed; NO mem samples ⇒ None
        let guard = acquire_lease(be.clone(), &cfg(5, 3600), true, 120.0)
            .await
            .expect("reported-freed fallback satisfies the budget when meminfo is unreadable");
        assert_eq!(be.chord_idles.load(Ordering::SeqCst), 1);
        assert_eq!(be.mint_idles.load(Ordering::SeqCst), 1);
        guard.release().await;
        assert_eq!(be.activates(), (1, 1));
    }

    #[test]
    fn drop_without_an_ambient_runtime_still_activates() {
        // FINDING 3: a crash-drop of the guard OUTSIDE any Tokio runtime must STILL
        // reactivate (blocking best-effort), never leaving services idle. This is a
        // plain #[test] ⇒ no ambient runtime.
        let be = MockBackend::new(None, None);
        let inner = Arc::new(LeaseInner {
            backend: be.clone(),
            chord_active: AtomicBool::new(false),
            mint_active: AtomicBool::new(false),
            retry_backoff: Duration::from_millis(1),
            max_attempts: 5,
            op_timeout: Duration::from_secs(5),
            release_lock: tokio::sync::Mutex::new(()),
            expired: AtomicBool::new(false),
            expired_notify: tokio::sync::Notify::new(),
            persistent_backoff: Duration::from_millis(1),
            persistent_running: AtomicBool::new(false),
        });
        // Manually build a guard with NO watchdog (simulating a guard whose ambient
        // runtime is gone), then drop it with no ambient runtime.
        let guard = LeaseGuard {
            inner: Some(inner),
            watchdog: None,
        };
        drop(guard);
        assert_eq!(
            be.activates(),
            (1, 1),
            "no-runtime drop guaranteed reactivation (blocking release)"
        );
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
