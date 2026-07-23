//! BLD-10: MINT test-harness idle-mode — release the sweep harness's GPU lock
//! and the RAM/VRAM its resident models hold, on demand, for a CI/CD compiler run.
//!
//! ## Why this exists
//! The constellation CI/CD compiler (S117) builds on the heavy GPU/big-RAM host.
//! That same host runs MINT's GPU-heavy profiling sweeps (`bin/mint`:
//! `coder`/`assistant` sweeps, ad hoc `case` reruns, `breakfix`), each of which
//! takes the shared [`gpu_authority`](crate::intake::gpu_authority) exclusive lock
//! and loads a model into VRAM. To hand the host to a build WITHOUT permanently
//! tearing MINT down, the compiler asks MINT to go *idle*: stop admitting new
//! sweep/case runs, drain what is in flight, RELEASE MINT's own GPU-authority
//! lock (handing the shared GPU back), and enter a low-footprint wait — freeing
//! the RAM/VRAM the harness held. When the build finishes — or lazily, on the
//! next MINT run — MINT *activates* and resumes: the next sweep re-acquires the
//! GPU-authority lock on its own exactly as from a cold start.
//!
//! This is the exact hardened parallel of Chord's BLD-09 idle-mode. The
//! concurrency design below was proven necessary by five review cycles on
//! BLD-09; MINT reuses it verbatim rather than re-deriving (and re-breaking) it.
//!
//! ## Transition state machine (closed-world drain)
//! Idle-mode is a real state machine, not a snapshot-then-act flag, so it is
//! correct under CONCURRENT control calls and concurrent MINT runs:
//!
//! ```text
//!   Active ──begin_enter (CAS)──▶ EnteringIdle ──finish_enter──▶ Idle
//!     ▲                                                            │
//!     └──────────── finish_activate ◀── Activating ◀──begin_activate (CAS)┘
//! ```
//!
//! - The `EnteringIdle`/`Activating` markers are installed ATOMICALLY (compare-and-swap
//!   under the state lock) *before* any side-effect work, so a second concurrent
//!   `enter`/`activate` sees the in-flight transition and returns a no-op instead of
//!   re-running drain/release.
//! - A new MINT run is admitted ([`IdleController::try_admit`]) only while the state is
//!   `Active`, and the admission increment happens *under the same lock* that flips the
//!   state. Once we flip to `EnteringIdle`, no further run can join the in-flight set, so
//!   the subsequent drain is a genuine CLOSED-WORLD drain — nothing slips in after the
//!   drain window opens.
//!
//! ## Compiler-lease awareness
//! Lazy activation and the watchdog distinguish a *compiler build lease* (see
//! [`is_compiler_lease`]) from MINT's own GPU holders (`intake_coder_sweep`, …) or any
//! other GPU-exclusive holder. While a compiler build lease is held, a stray MINT run
//! does NOT tear down the idle manifest, and the watchdog does NOT auto-activate — the
//! build window stays protected. A non-compiler holder does not extend the idle window.
//!
//! ## Contract (see `README.md`)
//! - [`enter_idle`]  → drain, release MINT's GPU lock, free RAM; reports freed RAM. Idempotent.
//! - [`activate`]    → resume; MINT runs re-acquire the GPU lock on demand. Idempotent. Also
//!                     happens lazily on the next admitted run ([`admit_run`]).
//! - A watchdog ([`watchdog_loop`]) re-activates on timeout so MINT is never left silently
//!   idle; it holds off only while a COMPILER GPU-exclusive lease is actively held.
//!
//! ## Testability
//! The pure decision logic ([`decide_enter`], [`decide_activate`], [`is_compiler_lease`],
//! [`lazy_action`], [`watchdog_should_activate`], [`ResumeManifest::watchdog_expired`], and
//! the in-memory [`IdleController`] transitions) is separated from the clock, the
//! filesystem, and `gpu_authority`'s side effects so it is exhaustively unit-testable
//! offline with no global state and no sleeping. The release/restore *side effects*
//! (releasing the GPU-authority lock, reading `/proc/meminfo`) live in the async
//! orchestration functions and are best-effort.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Default hard-timeout (seconds) after which the watchdog re-activates an idle
/// harness if no compiler GPU-exclusive lease is held. 1 hour: comfortably longer
/// than a heavy fleet build, short enough that a crashed/forgotten compiler never
/// wedges MINT idle indefinitely. Override with `MINT_IDLE_WATCHDOG_SECS`.
pub const DEFAULT_WATCHDOG_SECS: u64 = 3600;

/// Default bound (seconds) on draining in-flight MINT runs before releasing
/// resources. A sweep case that overruns this bound is left to complete on its own
/// while release proceeds (the report flags `inflight_remaining > 0`). Override
/// `MINT_IDLE_DRAIN_SECS`.
pub const DEFAULT_DRAIN_SECS: u64 = 30;

/// Default substrings (case-insensitive) that identify a GPU-exclusive holder as a
/// *compiler build* lease, as opposed to some other GPU job (e.g. MINT's own
/// `intake_coder_sweep`). Role/label conventions, NOT infra identifiers — override
/// with `MINT_IDLE_COMPILER_LEASE_HOLDERS` (comma-separated) if the compiler adopts a
/// different holder label. Kept identical to Chord's BLD-09 default so one compiler
/// lease label is recognised by BOTH idle-modes.
pub const DEFAULT_COMPILER_LEASE_HOLDERS: &str = "compiler,build,bld";

/// Default hard budget (seconds) for the ENTIRE `enter_idle` release sequence
/// (drain + release GPU lock + sample RAM). Bounding release under an explicit
/// timeout — kept STRICTLY BELOW the stale-recovery threshold — makes it structurally
/// impossible for the watchdog to reopen admission while release is still running: the
/// release either completes (→ `Idle`) or self-aborts via this budget (→ `Active`
/// through the guard, with admission having been closed the whole `EnteringIdle`
/// window). Override `MINT_IDLE_RELEASE_BUDGET_SECS`.
pub const DEFAULT_RELEASE_BUDGET_SECS: u64 = 90;

/// Default bound (seconds) after which the watchdog force-resolves a controller stuck
/// in a TRANSIENT phase (`EnteringIdle`/`Activating`) back to `Active`. MUST be
/// strictly greater than [`DEFAULT_RELEASE_BUDGET_SECS`] so the watchdog can only ever
/// recover a transition whose release future has ALREADY self-aborted or vanished —
/// never one still doing live release work (see [`stale_transition_secs_from_env`],
/// which clamps this ordering at runtime). Backstop only: the RAII [`EnterTransition`]
/// guard already rolls a dropped/panicked enter back to `Active` immediately.
/// Override with `MINT_IDLE_STALE_TRANSITION_SECS`.
pub const DEFAULT_STALE_TRANSITION_SECS: u64 = 300;

/// Resolve the watchdog timeout from `MINT_IDLE_WATCHDOG_SECS` (seconds); a
/// missing/blank/zero/unparseable value falls back to [`DEFAULT_WATCHDOG_SECS`].
pub fn watchdog_secs_from_env() -> u64 {
    parse_positive_env("MINT_IDLE_WATCHDOG_SECS", DEFAULT_WATCHDOG_SECS)
}

/// Resolve the in-flight drain bound from `MINT_IDLE_DRAIN_SECS` (seconds); a
/// missing/blank/zero/unparseable value falls back to [`DEFAULT_DRAIN_SECS`].
pub fn drain_secs_from_env() -> u64 {
    parse_positive_env("MINT_IDLE_DRAIN_SECS", DEFAULT_DRAIN_SECS)
}

/// Resolve the whole-release budget from `MINT_IDLE_RELEASE_BUDGET_SECS` (seconds); a
/// missing/blank/zero/unparseable value falls back to [`DEFAULT_RELEASE_BUDGET_SECS`].
pub fn release_budget_secs_from_env() -> u64 {
    parse_positive_env("MINT_IDLE_RELEASE_BUDGET_SECS", DEFAULT_RELEASE_BUDGET_SECS)
}

/// Resolve the stale-transition backstop bound from `MINT_IDLE_STALE_TRANSITION_SECS`
/// (seconds), CLAMPED so it is always strictly greater than the release budget. This
/// preserves the core invariant: the release future self-aborts (via its budget) BEFORE
/// the watchdog is ever allowed to force-recover the transition, so stale-recovery can
/// only fire once release is already gone — never concurrently with live release. A
/// misconfiguration (stale ≤ budget) is logged and clamped up to a safe value.
pub fn stale_transition_secs_from_env() -> u64 {
    let stale = parse_positive_env(
        "MINT_IDLE_STALE_TRANSITION_SECS",
        DEFAULT_STALE_TRANSITION_SECS,
    );
    let budget = release_budget_secs_from_env();
    if stale > budget {
        return stale;
    }
    // Misconfigured: clamp strictly above the budget (≥ 1.5× budget, and never below
    // the safe default), so the ordering invariant always holds.
    let safe = budget
        .saturating_add(budget / 2)
        .max(DEFAULT_STALE_TRANSITION_SECS);
    warn!(
        stale,
        budget,
        clamped_to = safe,
        "MINT_IDLE_STALE_TRANSITION_SECS ≤ release budget — clamping up to preserve the \
         no-mid-release-admission invariant"
    );
    safe
}

fn parse_positive_env(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Epoch seconds now (best-effort; never panics — a clock before the epoch yields 0).
/// Local mirror of `gpu_authority`'s private helper so this module has no clock global.
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The configured set of compiler-lease holder substrings (lowercased), from
/// `MINT_IDLE_COMPILER_LEASE_HOLDERS` or [`DEFAULT_COMPILER_LEASE_HOLDERS`]. Not a
/// secret and not an infra identifier — a list of role labels.
pub fn compiler_lease_holders_from_env() -> Vec<String> {
    let raw = std::env::var("MINT_IDLE_COMPILER_LEASE_HOLDERS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_COMPILER_LEASE_HOLDERS.to_string());
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// MINT's OWN GPU-authority holder labels — the labels its sweeps/cases/breakfix
/// acquire the shared exclusive lock under. Reuses the existing `pub const GPU_HOLDER`
/// labels (no new literals) so idle-mode releases exactly the locks MINT itself takes;
/// override with `MINT_GPU_HOLDERS` (comma-separated) if new harness front doors are
/// added. These are role labels, not infra identifiers.
pub fn mint_gpu_holders_from_env() -> Vec<String> {
    use crate::intake::{assistant::runner as assistant_runner, breakfix, coder_case, coder_sweep};
    match std::env::var("MINT_GPU_HOLDERS")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(raw) => raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => vec![
            coder_sweep::GPU_HOLDER.to_string(),
            assistant_runner::GPU_HOLDER.to_string(),
            coder_case::GPU_HOLDER.to_string(),
            breakfix::BREAKFIX_GPU_HOLDER.to_string(),
        ],
    }
}

/// Does `holder` name a COMPILER build lease (per `patterns`)? Case-insensitive
/// substring match. Pure — the caller supplies the patterns and the holder label,
/// so this is fully unit-testable without the global lock.
pub fn is_compiler_lease(holder: &str, patterns: &[String]) -> bool {
    let h = holder.to_ascii_lowercase();
    patterns
        .iter()
        .any(|p| !p.is_empty() && h.contains(p.as_str()))
}

/// Is `holder` one of MINT's OWN GPU-authority holder labels (case-insensitive)?
/// Pure — used to tell MINT's own lock apart from a foreign holder (a compiler lease or
/// some other job) when deciding what to release vs merely report.
pub fn is_mint_holder(holder: &str, mint_holders: &[String]) -> bool {
    let h = holder.to_ascii_lowercase();
    mint_holders
        .iter()
        .any(|m| !m.is_empty() && h == m.to_ascii_lowercase())
}

/// Is a COMPILER build lease currently held on the shared GPU by a LIVE process?
/// Reads `gpu_authority`'s lock and applies [`is_compiler_lease`] to the live holder.
/// A dead/abandoned holder, a non-compiler holder, or no holder ⇒ `false` — only a
/// live build lease protects idle. Bridges the pure decision logic to the real
/// GPU-authority gate; the side-effect read lives here, not in the decision functions.
pub fn compiler_lease_held(_now: u64) -> bool {
    match crate::intake::gpu_authority::status().lock {
        // (holder, mode, pid, pid_alive)
        Some((holder, _mode, _pid, pid_alive)) => {
            pid_alive && is_compiler_lease(&holder, &compiler_lease_holders_from_env())
        }
        None => false,
    }
}

// ── Freed-RAM sampling ────────────────────────────────────────────────────────

/// Best-effort `MemAvailable` (GiB) from `/proc/meminfo`. `None` on any error /
/// unexpected format — reading local `/proc`, no infra host involved. Kept here (not
/// in `config`) so idle-mode owns its own measurement and stays self-contained.
pub fn read_mem_available_gb() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            // Format: "MemAvailable:   12345678 kB"
            let kb: f64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / (1024.0 * 1024.0));
        }
    }
    None
}

// ── Resume manifest ───────────────────────────────────────────────────────────

/// What to restore when leaving idle, plus the bookkeeping the idle report and
/// the watchdog need. Persisted (when a state path is configured) so a crash mid-idle
/// leaves a record the watchdog can act on after restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResumeManifest {
    /// Who/what requested idle (e.g. `"compiler"`), for diagnostics. Never a secret.
    pub reason: String,
    /// Epoch seconds idle was entered.
    pub entered_at: u64,
    /// Epoch seconds after which the watchdog will auto-activate (unless a
    /// compiler GPU-exclusive lease is still held).
    pub watchdog_deadline: u64,
    /// MINT GPU-authority holder labels whose lock was released on entering idle.
    /// Restoration is LAZY — the next sweep/case re-acquires on its own — so this list
    /// is informational, not a force-reacquire instruction.
    pub released_holders: Vec<String>,
    /// `MemAvailable` (GiB) sampled just before release, for the freed-RAM delta.
    pub mem_available_before_gb: f64,
}

impl ResumeManifest {
    /// Has the watchdog deadline passed at `now`? (Pure — the lease-held override
    /// is applied by the watchdog, not here.)
    pub fn watchdog_expired(&self, now: u64) -> bool {
        now >= self.watchdog_deadline
    }
}

// ── Pure decisions ────────────────────────────────────────────────────────────

/// Pure decision for a request to enter idle, given the current state.
#[derive(Debug, PartialEq, Eq)]
pub enum EnterDecision {
    /// Currently active ⇒ run the release side effects and enter idle.
    Enter,
    /// Already idle ⇒ idempotent no-op (do NOT re-run release).
    AlreadyIdle,
}

pub fn decide_enter(current: Option<&ResumeManifest>) -> EnterDecision {
    match current {
        None => EnterDecision::Enter,
        Some(_) => EnterDecision::AlreadyIdle,
    }
}

/// Pure decision for a request to activate, given the current state.
#[derive(Debug, PartialEq, Eq)]
pub enum ActivateDecision {
    /// Currently idle ⇒ restore.
    Restore,
    /// Already active ⇒ idempotent no-op.
    AlreadyActive,
}

pub fn decide_activate(current: Option<&ResumeManifest>) -> ActivateDecision {
    match current {
        Some(_) => ActivateDecision::Restore,
        None => ActivateDecision::AlreadyActive,
    }
}

/// Pure decision for the lazy-restore hook: when a real MINT run arrives while idle,
/// should we restore, or preserve idle because a compiler build is still running?
#[derive(Debug, PartialEq, Eq)]
pub enum LazyAction {
    /// No compiler lease ⇒ restore, then let the run proceed.
    Restore,
    /// A compiler build lease is still held ⇒ keep the idle manifest + watchdog
    /// protection intact; the run is refused (retryable) rather than allowed to tear
    /// the build window down.
    PreserveIdle,
}

pub fn lazy_action(compiler_lease_held: bool) -> LazyAction {
    if compiler_lease_held {
        LazyAction::PreserveIdle
    } else {
        LazyAction::Restore
    }
}

/// Pure decision for the watchdog: given whether the deadline has passed and the
/// current GPU holder (if any), should the watchdog auto-activate now? Defers ONLY
/// for a live compiler build lease; a non-compiler holder does not extend idle.
pub fn watchdog_should_activate(expired: bool, holder: Option<&str>, patterns: &[String]) -> bool {
    if !expired {
        return false;
    }
    match holder {
        Some(h) if is_compiler_lease(h, patterns) => false, // compiler build in progress → defer
        _ => true,                                          // no/other holder → auto-activate
    }
}

// ── In-memory controller + durable persistence ───────────────────────────────

/// The lifecycle phase of idle-mode. `EnteringIdle`/`Activating` are transient
/// transition markers held only for the duration of the (short) release/restore
/// work; they are never persisted (a crash mid-transition reloads as `Active`, and
/// the GPU-authority lock + watchdog keep things safe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Active,
    EnteringIdle,
    Idle,
    Activating,
}

/// Internal state cell. Owns the manifest only in the idle/activating phases. The
/// transient phases carry the epoch second they began (`since`) so the watchdog can
/// detect and force-resolve a wedged transition, plus a unique `generation` token so
/// a stale transition guard can prove it still owns the CURRENT transition before
/// finalizing it — preventing a dropped/committed stale guard from clobbering a newer
/// transition (the ABA hazard).
enum IdleState {
    Active,
    EnteringIdle {
        since: u64,
        generation: u64,
    },
    Idle(ResumeManifest),
    Activating {
        since: u64,
        generation: u64,
        manifest: ResumeManifest,
    },
}

impl IdleState {
    fn phase(&self) -> Phase {
        match self {
            IdleState::Active => Phase::Active,
            IdleState::EnteringIdle { .. } => Phase::EnteringIdle,
            IdleState::Idle(_) => Phase::Idle,
            IdleState::Activating { .. } => Phase::Activating,
        }
    }
    /// The manifest to persist for this phase: only a fully-`Idle` harness persists a
    /// manifest; every other phase (including the transients) persists "not idle".
    fn to_persisted(&self) -> Option<&ResumeManifest> {
        match self {
            IdleState::Idle(m) => Some(m),
            _ => None,
        }
    }
    /// The epoch second a TRANSIENT phase began, or `None` for a steady phase.
    fn transition_since(&self) -> Option<u64> {
        match self {
            IdleState::EnteringIdle { since, .. } | IdleState::Activating { since, .. } => {
                Some(*since)
            }
            _ => None,
        }
    }
}

/// Result of trying to BEGIN entering idle (CAS `Active → EnteringIdle`).
#[derive(Debug, PartialEq)]
pub enum BeginEnter {
    /// Won the CAS: caller MUST run release work then commit the transition (via the
    /// RAII [`EnterTransition`] guard's `commit`).
    Begin,
    /// Already fully idle ⇒ idempotent no-op (carries the existing manifest).
    AlreadyIdle(ResumeManifest),
    /// Another enter/activate transition is in flight ⇒ no-op, do NOT run release.
    InTransition,
}

/// Result of trying to BEGIN activating (CAS `Idle → Activating`).
#[derive(Debug, PartialEq)]
pub enum BeginActivate {
    /// Won the CAS: caller finishes the activate transition.
    Begin(ResumeManifest),
    /// Already active ⇒ idempotent no-op.
    AlreadyActive,
    /// An enter/activate transition is in flight ⇒ no-op.
    InTransition,
    /// A state path is configured but clearing the persisted manifest to `Active`
    /// FAILED, so the activate was ABORTED before touching memory — the controller
    /// stays `Idle` (recoverable; a crash would reload `Idle`, consistent with memory).
    /// The caller should surface a retryable error and try again.
    PersistFailed,
}

/// Outcome of a full enter (begin+release+finish) against the live state.
#[derive(Debug, PartialEq)]
pub enum EnterOutcome {
    /// Transitioned active → idle; carries the stored manifest.
    Entered(ResumeManifest),
    /// Already idle; carries the existing manifest (idempotent).
    AlreadyIdle(ResumeManifest),
    /// A concurrent transition was already in flight; nothing was re-run.
    InTransition,
}

/// Outcome of a full activate against the live state.
#[derive(Debug, PartialEq)]
pub enum ActivateOutcome {
    /// Transitioned idle → active; carries the manifest that was cleared.
    Activated(ResumeManifest),
    /// Already active (idempotent no-op).
    AlreadyActive,
    /// A concurrent transition was in flight; nothing was re-run.
    InTransition,
    /// Activate aborted because the persist-Active-before-restore hard gate failed;
    /// the controller stays `Idle` and the caller should retry.
    PersistFailed,
}

/// Outcome of trying to admit one new MINT run.
pub enum AdmitOutcome {
    /// Admitted while `Active`; holds the in-flight guard (already counted).
    Admitted(InflightGuard),
    /// Steady `Idle`: caller decides restore-vs-preserve (see [`lazy_action`]).
    Idle,
    /// Mid-transition (`EnteringIdle`/`Activating`): brief, retryable — do NOT admit.
    Transitioning,
}

/// Process-global idle-mode state machine. One MINT harness serves one host, so
/// this is a singleton, like `gpu_authority`'s lock.
pub struct IdleController {
    inner: RwLock<IdleState>,
    /// Count of admitted in-flight MINT runs. Owned per-controller (not a module
    /// global) so unit tests are fully isolated. Shared with each [`InflightGuard`]
    /// via an `Arc` so the guard decrements the RIGHT counter on drop, no matter how
    /// long the run outlives the admission call.
    inflight: Arc<AtomicUsize>,
    /// Monotonic generation counter. Each `begin_enter`/`begin_activate` mints a fresh
    /// generation (under the `inner` write lock) that is stamped into the transient
    /// phase and captured by the transition's guard. `finish`/`abort` only act while
    /// that same generation is still the live one, so a stale guard (whose transition
    /// was force-recovered by the watchdog and superseded by a newer one) becomes a
    /// no-op instead of clobbering the newer transition (the ABA hazard).
    next_gen: AtomicU64,
    /// The POINT-OF-NO-RETURN latch: set for the exact span the (non-abortable)
    /// blocking GPU release is executing, and cleared only once that release has
    /// actually finished. While it is set, NO path is allowed to flip the controller
    /// out of `EnteringIdle` back to `Active` — not the dropped-guard rollback
    /// ([`abort_enter`](Self::abort_enter)) and not the watchdog's stale-recovery
    /// ([`recover_stale_transition`](Self::recover_stale_transition)). This is what
    /// actually ENFORCES "admission never reopens mid-release": a `tokio::spawn_blocking`
    /// release cannot be cancelled once started, so bounding only the *async wrapper*
    /// (as the first cut did) would reopen admission while the blocking release was
    /// still running. The latch closes that hole — admission (which is gated on the
    /// `Active` phase) can only reopen after the release has cleared it.
    releasing: AtomicBool,
    /// Where the manifest is persisted across restarts. `None` ⇒ persistence
    /// disabled (in-memory only) — behaviourally fine, the watchdog still bounds it.
    state_path: Option<PathBuf>,
}

impl Default for IdleController {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleController {
    /// In-memory-only controller (no persistence). Used by unit tests.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(IdleState::Active),
            inflight: Arc::new(AtomicUsize::new(0)),
            next_gen: AtomicU64::new(0),
            releasing: AtomicBool::new(false),
            state_path: None,
        }
    }

    /// Construct with durable persistence at `state_path`, seeding any persisted
    /// manifest. A missing/corrupt file seeds `Active` and never panics.
    pub fn with_state(state_path: Option<PathBuf>) -> Self {
        let seed = match state_path.as_deref().and_then(load_persisted) {
            Some(m) => {
                info!(
                    reason = %m.reason,
                    entered_at = m.entered_at,
                    "mint idle-mode: reloaded persisted idle state across restart (watchdog will bound it)"
                );
                IdleState::Idle(m)
            }
            None => IdleState::Active,
        };
        Self {
            inner: RwLock::new(seed),
            inflight: Arc::new(AtomicUsize::new(0)),
            next_gen: AtomicU64::new(0),
            releasing: AtomicBool::new(false),
            state_path,
        }
    }

    /// Mark the start of the point-of-no-return blocking GPU release. From here until
    /// [`clear_releasing`](Self::clear_releasing), no path may flip out of
    /// `EnteringIdle` back to `Active`, so admission stays closed for the whole release.
    fn set_releasing(&self) {
        self.releasing.store(true, Ordering::SeqCst);
    }

    /// Mark the blocking GPU release as finished — only now may the phase leave
    /// `EnteringIdle` (via commit → `Idle`, or via a rollback/recovery → `Active`).
    fn clear_releasing(&self) {
        self.releasing.store(false, Ordering::SeqCst);
    }

    /// Is a blocking GPU release currently in progress (point of no return)?
    pub fn is_releasing(&self) -> bool {
        self.releasing.load(Ordering::SeqCst)
    }

    /// From `MINT_IDLE_STATE_PATH` (unset ⇒ in-memory only, no infra path guessed).
    pub fn from_env() -> Self {
        let path = std::env::var("MINT_IDLE_STATE_PATH")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        Self::with_state(path)
    }

    /// Best-effort persist for the non-critical transitions (finish/abort/recover):
    /// a failure here is logged and swallowed because those all move TOWARD a resting
    /// phase and a stale on-disk state is bounded by the watchdog. The one persist that
    /// is NOT best-effort — clearing to `Active` before the activate restore — is
    /// hard-gated directly in [`begin_activate_inner`](Self::begin_activate_inner).
    fn persist_locked(&self, current: &IdleState) {
        if let Some(path) = self.state_path.as_deref() {
            if let Err(e) = persist_state(path, &current.to_persisted().cloned()) {
                warn!(path = %path.display(), error = %e,
                    "mint idle-mode: state persist failed (best-effort — watchdog still bounds it)");
            }
        }
    }

    /// Current lifecycle phase (cheap snapshot).
    pub fn phase(&self) -> Phase {
        self.inner.read().expect("mint idle lock poisoned").phase()
    }

    /// Is MINT fully idle right now? (Transitions do NOT count as idle.)
    pub fn is_idle(&self) -> bool {
        matches!(
            &*self.inner.read().expect("mint idle lock poisoned"),
            IdleState::Idle(_)
        )
    }

    /// A snapshot of the current manifest (present while idle or activating).
    pub fn snapshot(&self) -> Option<ResumeManifest> {
        match &*self.inner.read().expect("mint idle lock poisoned") {
            IdleState::Idle(m) | IdleState::Activating { manifest: m, .. } => Some(m.clone()),
            _ => None,
        }
    }

    /// S125 IDLE-WATCHDOG: extend the auto-reactivate deadline to at least `new_deadline`
    /// (never shorten). Used when a fresh compiler lease enters while MINT is ALREADY idle
    /// from an earlier lease, so a longer build window is honored instead of the first
    /// lease's (possibly shorter) deadline. No-op unless fully `Idle`.
    pub fn bump_watchdog(&self, new_deadline: u64) {
        let mut guard = self.inner.write().expect("mint idle lock poisoned");
        let changed = if let IdleState::Idle(m) = &mut *guard {
            if new_deadline > m.watchdog_deadline {
                m.watchdog_deadline = new_deadline;
                true
            } else {
                false
            }
        } else {
            false
        };
        // Persist the extended deadline so a restart mid-build reloads the LONGER window,
        // not the first lease's stale (shorter) one, and cannot reactivate MINT early.
        if changed {
            self.persist_locked(&guard);
        }
    }

    /// Try to admit ONE new MINT run. The in-flight increment happens under the SAME
    /// write lock that flips the phase, so once a concurrent
    /// [`begin_enter`](Self::begin_enter) has installed `EnteringIdle`, this can never
    /// return `Admitted` — the drain that follows is closed-world.
    pub fn try_admit(&self) -> AdmitOutcome {
        let guard = self.inner.write().expect("mint idle lock poisoned");
        match &*guard {
            IdleState::Active => {
                AdmitOutcome::Admitted(InflightGuard::admit(self.inflight.clone()))
            }
            IdleState::Idle(_) => AdmitOutcome::Idle,
            IdleState::EnteringIdle { .. } | IdleState::Activating { .. } => {
                AdmitOutcome::Transitioning
            }
        }
    }

    /// Current number of admitted in-flight MINT runs.
    pub fn inflight_count(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// Wait (bounded by `timeout`) for in-flight runs to drain to zero. Returns the
    /// number still in flight when it returned (0 = fully drained; >0 = the bound was
    /// hit and release proceeds anyway). Polls at 100ms. Because admission is closed
    /// once the phase left `Active`, the count is monotonically non-increasing here —
    /// a genuine closed-world drain.
    pub async fn drain_inflight(&self, timeout: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let n = self.inflight_count();
            if n == 0 {
                return 0;
            }
            if tokio::time::Instant::now() >= deadline {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Internal CAS `Active → EnteringIdle{generation}`. Mints a fresh generation
    /// under the write lock and stamps it into the transient phase. `Ok(gen)` on a
    /// real transition (caller must eventually finish/abort with that `gen`); `Err`
    /// carries the non-transition outcome.
    fn begin_enter_inner(&self) -> Result<u64, BeginEnter> {
        let mut guard = self.inner.write().expect("mint idle lock poisoned");
        match &*guard {
            IdleState::Active => {
                let generation = self.next_gen.fetch_add(1, Ordering::SeqCst);
                *guard = IdleState::EnteringIdle {
                    since: now_epoch(),
                    generation,
                };
                Ok(generation)
            }
            IdleState::Idle(m) => Err(BeginEnter::AlreadyIdle(m.clone())),
            IdleState::EnteringIdle { .. } | IdleState::Activating { .. } => {
                Err(BeginEnter::InTransition)
            }
        }
    }

    /// CAS `Active → EnteringIdle`. Installs the transition marker atomically BEFORE
    /// any release work, so exactly one caller ever runs the release side effects.
    /// Prefer [`try_begin_enter`](Self::try_begin_enter), whose RAII guard guarantees
    /// the phase is finalized even if the enter future is dropped mid-transition.
    pub fn begin_enter(&self) -> BeginEnter {
        match self.begin_enter_inner() {
            Ok(_gen) => BeginEnter::Begin,
            Err(e) => e,
        }
    }

    /// CAS into `EnteringIdle` and return an RAII [`EnterTransition`] guard carrying
    /// this transition's generation. The guard MUST be finalized with
    /// [`EnterTransition::commit`]; if it is instead dropped (the enter future is
    /// cancelled, panics, or returns early), its `Drop` deterministically rolls
    /// `EnteringIdle → Active` — BUT only if its generation is still the live one, so a
    /// stale guard can never roll back a newer transition. `Err` carries the non-`Begin`
    /// CAS result.
    pub fn try_begin_enter(&self) -> Result<EnterTransition<'_>, BeginEnter> {
        match self.begin_enter_inner() {
            Ok(generation) => Ok(EnterTransition {
                ctl: self,
                generation,
                committed: false,
            }),
            Err(e) => Err(e),
        }
    }

    /// Complete an enter: `EnteringIdle{gen} → Idle(manifest)`, persisting the manifest.
    /// GENERATION-GUARDED: only installs the manifest if the controller is STILL in the
    /// same-generation `EnteringIdle` this transition began — otherwise (watchdog
    /// recovered it, or a newer transition superseded it) it is a NO-OP returning
    /// `None`, so a stale commit can never clobber a newer phase.
    fn finish_enter(&self, generation: u64, manifest: ResumeManifest) -> Option<ResumeManifest> {
        let mut guard = self.inner.write().expect("mint idle lock poisoned");
        match &*guard {
            IdleState::EnteringIdle {
                generation: cur, ..
            } if *cur == generation => {
                *guard = IdleState::Idle(manifest.clone());
                self.persist_locked(&guard);
                Some(manifest)
            }
            _ => None,
        }
    }

    /// Roll an in-progress `EnteringIdle` back to `Active` (the safe resting phase).
    /// Used by the [`EnterTransition`] guard when an enter is dropped before commit.
    /// GENERATION-GUARDED: only acts while the controller is STILL in the same-generation
    /// `EnteringIdle` this transition installed. If the phase advanced (committed), was
    /// recovered by the watchdog, or was superseded by a newer transition, it is a
    /// NO-OP — so a stale/late drop can never clobber a newer transition's state.
    ///
    /// RELEASE-GUARDED: it is ALSO a no-op while [`is_releasing`](Self::is_releasing) is
    /// set (the blocking GPU release is mid-flight). A dropped/cancelled enter future
    /// whose blocking release is still running must NOT reopen admission — the detached
    /// blocking task will clear the latch when it finishes, and the watchdog then
    /// recovers the (now safe) `EnteringIdle` to `Active`. Returns whether it rolled back.
    fn abort_enter(&self, generation: u64) -> bool {
        if self.is_releasing() {
            return false; // point of no return — release still running, keep admission closed
        }
        let mut guard = self.inner.write().expect("mint idle lock poisoned");
        if let IdleState::EnteringIdle {
            generation: cur, ..
        } = &*guard
        {
            if *cur == generation {
                *guard = IdleState::Active;
                self.persist_locked(&guard);
                return true;
            }
        }
        false
    }

    /// Internal CAS `Idle → Activating{generation}`, returning the manifest to clear
    /// plus the transition's generation. Clearing the persisted manifest to the RESTING
    /// `Active`/none form is a HARD PREREQUISITE done BEFORE mutating memory — while the
    /// controller is still `Idle`. When a state path is configured and that persist
    /// FAILS, the activate is ABORTED (`Err(PersistFailed)`) with memory left `Idle`, so
    /// on-disk and memory stay consistent (a crash reloads `Idle`, recoverable) — we
    /// never proceed into restore with the disk still saying `Idle` while memory says
    /// `Activating`. When no state path is configured, this gate is skipped (best-effort).
    /// Only after the disk reads `Active` do we flip memory to `Activating`.
    fn begin_activate_inner(&self) -> Result<(ResumeManifest, u64), BeginActivate> {
        let mut guard = self.inner.write().expect("mint idle lock poisoned");
        match &*guard {
            IdleState::Idle(_) => {
                // HARD GATE: clear disk → Active BEFORE touching memory (still `Idle`).
                if let Some(path) = self.state_path.as_deref() {
                    if let Err(e) = persist_state(path, &None) {
                        warn!(path = %path.display(), error = %e,
                            "mint idle-mode: could not clear persisted idle state before activate — \
                             aborting activate (staying Idle, retryable)");
                        return Err(BeginActivate::PersistFailed);
                    }
                }
                // Disk now reads `Active`; safe to flip memory to `Activating`.
                let m = match std::mem::replace(&mut *guard, IdleState::Active) {
                    IdleState::Idle(m) => m,
                    _ => unreachable!("matched Idle above"),
                };
                let generation = self.next_gen.fetch_add(1, Ordering::SeqCst);
                *guard = IdleState::Activating {
                    since: now_epoch(),
                    generation,
                    manifest: m.clone(),
                };
                Ok((m, generation))
            }
            IdleState::Active => Err(BeginActivate::AlreadyActive),
            IdleState::EnteringIdle { .. } | IdleState::Activating { .. } => {
                Err(BeginActivate::InTransition)
            }
        }
    }

    /// CAS `Idle → Activating`, returning the manifest to clear. Concurrent activates:
    /// exactly one wins `Begin`; the rest see `InTransition`/`AlreadyActive`.
    pub fn begin_activate(&self) -> BeginActivate {
        match self.begin_activate_inner() {
            Ok((m, _gen)) => BeginActivate::Begin(m),
            Err(e) => e,
        }
    }

    /// Complete an activate: `Activating{gen} → Active`. GENERATION-GUARDED: only acts
    /// while the controller is STILL in the same-generation `Activating` this transition
    /// began; otherwise a NO-OP. Returns whether it finalized. (Disk already reads
    /// `Active` from [`begin_activate_inner`], so this is a memory-only resolution.)
    fn finish_activate(&self, generation: u64) -> bool {
        let mut guard = self.inner.write().expect("mint idle lock poisoned");
        if let IdleState::Activating {
            generation: cur, ..
        } = &*guard
        {
            if *cur == generation {
                *guard = IdleState::Active;
                self.persist_locked(&guard);
                return true;
            }
        }
        false
    }

    /// Backstop: if the controller has been stuck in a TRANSIENT phase
    /// (`EnteringIdle`/`Activating`) since before `now - max_age`, force-resolve it to
    /// `Active` and return `true`. Never touches a steady `Active`/`Idle` phase. Bumps
    /// the generation so any outstanding guard for the recovered transition is
    /// invalidated (its finish/abort become no-ops). Insurance behind the RAII guard.
    ///
    /// RELEASE-GUARDED: it is a no-op while [`is_releasing`](Self::is_releasing) is set —
    /// the watchdog must never force a stuck `EnteringIdle` back to `Active` while the
    /// (non-abortable) blocking GPU release is still executing, or it would reopen
    /// admission mid-release. The blocking task clears the latch when it finishes, and
    /// only a subsequent tick recovers the transition.
    pub fn recover_stale_transition(&self, now: u64, max_age: u64) -> bool {
        if self.is_releasing() {
            return false; // point of no return — release still running, keep admission closed
        }
        let mut guard = self.inner.write().expect("mint idle lock poisoned");
        let Some(since) = guard.transition_since() else {
            return false;
        };
        if now.saturating_sub(since) >= max_age {
            // Bump the generation: the abandoned transition's guard now holds a stale
            // generation, so its Drop/commit can't clobber whatever comes next.
            self.next_gen.fetch_add(1, Ordering::SeqCst);
            *guard = IdleState::Active;
            self.persist_locked(&guard);
            true
        } else {
            false
        }
    }

    /// Convenience full enter used by unit tests: atomically `Active → Idle` (begin +
    /// finish with no release work in between, via the RAII guard). Idempotent.
    pub fn enter(&self, manifest: ResumeManifest) -> EnterOutcome {
        match self.try_begin_enter() {
            Ok(t) => match t.commit(manifest) {
                Some(m) => EnterOutcome::Entered(m),
                None => EnterOutcome::InTransition,
            },
            Err(BeginEnter::AlreadyIdle(m)) => EnterOutcome::AlreadyIdle(m),
            Err(_) => EnterOutcome::InTransition,
        }
    }

    /// Full leave idle (begin + finish). Idempotent: already active ⇒ `AlreadyActive`.
    pub fn exit(&self) -> ActivateOutcome {
        match self.begin_activate_inner() {
            Ok((m, generation)) => {
                self.finish_activate(generation);
                ActivateOutcome::Activated(m)
            }
            Err(BeginActivate::AlreadyActive) => ActivateOutcome::AlreadyActive,
            Err(BeginActivate::PersistFailed) => ActivateOutcome::PersistFailed,
            Err(_) => ActivateOutcome::InTransition,
        }
    }
}

/// RAII guard for an in-progress `EnteringIdle` transition. Obtained from
/// [`IdleController::try_begin_enter`]. The transition spans several `.await` points
/// (drain, GPU-lock release); if the enclosing future is dropped/cancelled/panics
/// before [`commit`](Self::commit), this guard's `Drop` deterministically rolls the
/// phase back to `Active`, so a cancelled enter can never leave the controller wedged
/// in `EnteringIdle` (which would refuse all runs and block admin enter/activate).
#[must_use = "commit the transition, or it will roll back to Active on drop"]
pub struct EnterTransition<'a> {
    ctl: &'a IdleController,
    /// The generation minted for THIS transition. `commit`/`Drop` only act while this
    /// is still the controller's live `EnteringIdle` generation (ABA guard).
    generation: u64,
    committed: bool,
}

impl EnterTransition<'_> {
    /// Complete the transition: `EnteringIdle{gen} → Idle(manifest)`. Consumes the
    /// guard so its `Drop` becomes a no-op. Returns `Some(manifest)` if this transition
    /// still owned the live generation, or `None` if it had been superseded/recovered
    /// (in which case nothing was installed — the caller must not report success).
    pub fn commit(mut self, manifest: ResumeManifest) -> Option<ResumeManifest> {
        self.committed = true;
        self.ctl.finish_enter(self.generation, manifest)
    }
}

impl Drop for EnterTransition<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Generation-guarded AND release-guarded inside `abort_enter`: a stale guard
            // (recovered by the watchdog, superseded by a newer transition) is a no-op,
            // and — critically — so is a drop while the blocking GPU release is still
            // running (`is_releasing`). In that case admission stays closed until the
            // detached release finishes and the watchdog recovers the transition; we do
            // NOT reopen admission mid-release.
            if self.ctl.abort_enter(self.generation) {
                warn!(
                    "mint idle-mode: enter transition dropped before commit (future cancelled/panicked) \
                     — rolled EnteringIdle back to Active"
                );
            } else {
                warn!(
                    "mint idle-mode: enter transition dropped before commit but rollback withheld \
                     (superseded, or a blocking release is still running — admission stays closed)"
                );
            }
        }
    }
}

// Manual `Debug` (the held `&IdleController` isn't `Debug`, so we can't derive).
impl std::fmt::Debug for EnterTransition<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnterTransition")
            .field("generation", &self.generation)
            .field("committed", &self.committed)
            .finish_non_exhaustive()
    }
}

/// The process-global MINT idle-mode controller, lazily initialised from the
/// environment on first use (Terminus's `OnceLock` idiom — see `pki::ca`). Held in an
/// `Arc` so the (non-abortable) blocking release task in [`enter_idle_on`] can own a
/// clone that outlives a cancelled enter future and still clear the release latch when
/// it finishes. The admission hook and the watchdog reference this via [`mint_idle`];
/// unit tests use isolated [`IdleController::new`] instances so they never touch it.
static MINT_IDLE_CELL: std::sync::OnceLock<Arc<IdleController>> = std::sync::OnceLock::new();

/// Accessor for the process-global MINT idle-mode controller (initialised once, from
/// the environment). Returns a cheap `Arc` clone of the single shared controller.
pub fn mint_idle() -> Arc<IdleController> {
    MINT_IDLE_CELL
        .get_or_init(|| Arc::new(IdleController::from_env()))
        .clone()
}

/// Load a persisted manifest from `path`. Missing/unreadable/malformed ⇒ `None`
/// with a warn (never a panic). The file stores `Option<ResumeManifest>`; a stored
/// `null` (last write was an activate) also yields `None`.
fn load_persisted(path: &Path) -> Option<ResumeManifest> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = %path.display(), error = %e,
                "mint idle-mode: could not read persisted state (starting active)");
            return None;
        }
    };
    match serde_json::from_str::<Option<ResumeManifest>>(&data) {
        Ok(m) => m,
        Err(e) => {
            warn!(path = %path.display(), error = %e,
                "mint idle-mode: persisted state is corrupt/unrecognized (starting active)");
            None
        }
    }
}

/// Atomically persist the current state (tempfile + rename). Returns the IO error on
/// failure so the CALLER can decide whether it is fatal: most callers treat it as
/// best-effort, but the activate path hard-gates on it (see `begin_activate_inner`).
fn persist_state(path: &Path, state: &Option<ResumeManifest>) -> std::io::Result<()> {
    let json = serde_json::to_string(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes())?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

// ── In-flight gauge ───────────────────────────────────────────────────────────

/// RAII guard for one admitted in-flight MINT run. Constructed only via
/// [`IdleController::try_admit`] (which increments under the state lock, and only while
/// `Active`); the decrement on drop is lock-free and can never leak (fires on panic /
/// `?` / early return alike). Holds an `Arc` to its owning controller's counter so it
/// always decrements the counter it incremented.
#[must_use = "hold the guard for the duration of the run"]
pub struct InflightGuard {
    counter: Arc<AtomicUsize>,
}

impl InflightGuard {
    /// Increment `counter` and hand back the guard. Private: callers go through
    /// [`IdleController::try_admit`] so the increment stays under the state lock.
    fn admit(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        InflightGuard { counter }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

// ── Freed-RAM report ──────────────────────────────────────────────────────────

/// The observable result of entering idle, surfaced so the compiler knows how much
/// headroom it just gained.
#[derive(Debug, Clone, Serialize)]
pub struct IdleReport {
    /// `MemAvailable` (GiB) sampled before release; `null` if unreadable.
    pub mem_available_before_gb: Option<f64>,
    /// `MemAvailable` (GiB) sampled after release; `null` if unreadable.
    pub mem_available_after_gb: Option<f64>,
    /// `after - before`, clamped at 0 (a transient negative from other activity is
    /// reported as 0 freed). `null` if either sample was unreadable.
    pub freed_gb: Option<f64>,
    /// MINT's own GPU-authority holder labels whose lock this idle released.
    pub holders_released: Vec<String>,
    /// In-flight MINT runs still running when release proceeded (0 = clean drain).
    pub inflight_remaining: usize,
    /// If the GPU-authority lock is held by a NON-MINT holder (a compiler lease or
    /// some other job), its label — reported, NOT force-released. `None` otherwise.
    pub foreign_gpu_lock_holder: Option<String>,
}

fn freed_gb(before: Option<f64>, after: Option<f64>) -> Option<f64> {
    match (before, after) {
        (Some(b), Some(a)) => Some((a - b).max(0.0)),
        _ => None,
    }
}

// ── GPU-lock release (best-effort side effect) ────────────────────────────────

/// The outcome of releasing MINT's GPU-authority lock during idle: which of MINT's
/// own holder labels were released, and any FOREIGN holder (non-MINT) that currently
/// owns the lock and was left untouched (reported, never force-released).
pub struct GpuReleaseResult {
    pub holders_released: Vec<String>,
    pub foreign_holder: Option<String>,
}

/// Release whichever of MINT's OWN GPU-authority holder labels currently holds the
/// shared exclusive lock, handing the GPU back for the compiler. Best-effort:
/// - For each MINT holder label, `gpu_authority::release(label)` is a safe no-op when
///   no lock exists or MINT doesn't own it, and clears it when MINT does — so we call
///   it for each of MINT's labels and record which one actually owned the lock.
/// - A FOREIGN holder (a compiler lease or another job) is reported via
///   `foreign_holder` and NEVER force-released (`release` refuses another holder's lock).
///
/// Pulled out of [`enter_idle`] so the pure state machine stays independently testable;
/// this function is the only place idle-mode reaches into `gpu_authority`.
fn release_mint_gpu_lock() -> GpuReleaseResult {
    use crate::intake::gpu_authority;
    let mint_holders = mint_gpu_holders_from_env();

    // Who holds the lock right now (if anyone), and is it foreign to MINT?
    let foreign_holder = match gpu_authority::status().lock {
        Some((holder, _mode, _pid, _alive)) if !is_mint_holder(&holder, &mint_holders) => {
            warn!(
                holder = %holder,
                "mint idle-mode: GPU-authority lock held by a non-MINT holder — reporting, not force-releasing"
            );
            Some(holder)
        }
        _ => None,
    };

    // Release each of MINT's own labels. `release` returns Ok when MINT owned it (now
    // cleared) or when no/foreign lock exists it doesn't own; Err only when a DIFFERENT
    // holder owns it (the foreign case above), which we log and skip.
    let mut holders_released = Vec::new();
    for label in &mint_holders {
        // Only attempt a release that will actually own-and-clear: check the live lock.
        match gpu_authority::status().lock {
            Some((holder, _mode, _pid, _alive)) if holder == *label => {
                match gpu_authority::release(label) {
                    Ok(()) => {
                        info!(holder = %label, "mint idle-mode: released MINT GPU-authority lock");
                        holders_released.push(label.clone());
                    }
                    Err(e) => warn!(holder = %label, error = %e,
                        "mint idle-mode: failed to release MINT GPU-authority lock (best-effort)"),
                }
            }
            _ => {} // not held by this MINT label right now — nothing to release
        }
    }

    GpuReleaseResult {
        holders_released,
        foreign_holder,
    }
}

// ── Orchestration (async, best-effort side effects) ──────────────────────────

/// RAII guard that clears the controller's `releasing` latch on drop. Lives INSIDE the
/// `spawn_blocking` release closure so the latch is cleared on every exit path of the
/// blocking thread — including an unwinding panic from the release function. This is the
/// panic-safety net behind the point-of-no-return latch: without it, a panicking release
/// on a cancelled enter future (whose `JoinHandle` was dropped, so the async-side clear
/// never runs) would leave the latch stuck, permanently closing admission and blocking
/// `recover_stale_transition`. `clear_releasing` is idempotent, so a later defensive
/// clear on the async side is harmless.
struct ReleasingLatchGuard {
    ctl: Arc<IdleController>,
}

impl Drop for ReleasingLatchGuard {
    fn drop(&mut self) {
        self.ctl.clear_releasing();
    }
}

/// Enter idle: drain in-flight MINT runs, release MINT's GPU-authority lock, sample
/// freed RAM, record the resume manifest. Delegates to [`enter_idle_on`] against the
/// process-global controller with the real GPU-lock release.
pub async fn enter_idle(reason: &str) -> (EnterOutcome, Option<IdleReport>) {
    enter_idle_on(mint_idle(), reason, release_mint_gpu_lock, None).await
}

/// Like [`enter_idle`] but pins the auto-reactivate watchdog window (seconds). The
/// compiler idle lease (S125 IDLE-WATCHDOG) passes its own max-lease so the MINT
/// fail-safe never reactivates MINT mid-build for a build longer than the 3600s default.
pub async fn enter_idle_with_watchdog(
    reason: &str,
    watchdog_secs: u64,
) -> (EnterOutcome, Option<IdleReport>) {
    enter_idle_on(mint_idle(), reason, release_mint_gpu_lock, Some(watchdog_secs)).await
}

/// The generic enter-idle orchestration, parameterised over the controller and the
/// (blocking) GPU-release function so it is unit-testable offline with an injected
/// fake slow release (see `admission_stays_closed_until_blocking_release_completes`).
///
/// ## Why the release budget does NOT wrap the blocking release (codex review fix)
/// A `tokio::spawn_blocking` task cannot be cancelled once it has started. The first
/// cut wrapped the WHOLE release (drain + `spawn_blocking`) in a `timeout`; on timeout
/// the wrapper future was dropped and the [`EnterTransition`] guard rolled the phase
/// back to `Active`, reopening admission WHILE the blocking release was still running —
/// exactly the "admission never reopens mid-release" violation. The fix splits release
/// into two parts:
///
/// 1. **Drain + async prep** (cancellable, no external side effects): bounded by the
///    release budget. On timeout the guard drops and rolls back to `Active` cleanly —
///    safe, because the blocking release has NOT started yet (the latch is not set).
/// 2. **Point of no return** — the blocking GPU release: guarded by
///    [`IdleController::set_releasing`]/[`clear_releasing`](IdleController::clear_releasing)
///    and awaited UNCONDITIONALLY (no timeout can abandon it). While the latch is set,
///    neither the dropped-guard rollback nor the watchdog's stale-recovery may flip the
///    phase back to `Active`, so admission (which is gated on the `Active` phase) stays
///    closed until the release has actually completed. The blocking closure clears the
///    latch as its final act, so even if THIS future is cancelled mid-release the
///    detached task still completes the release and reopens the door — never before.
pub async fn enter_idle_on<F>(
    ctl: Arc<IdleController>,
    reason: &str,
    release_fn: F,
    watchdog_secs: Option<u64>,
) -> (EnterOutcome, Option<IdleReport>)
where
    F: FnOnce() -> GpuReleaseResult + Send + 'static,
{
    // S125 IDLE-WATCHDOG: the caller may override the auto-reactivate window. The
    // compiler lease passes its own (longer) max-lease so the fail-safe watchdog never
    // fires BEFORE the lease's own cap during a legitimately long build; `None` uses the
    // env default (`MINT_IDLE_WATCHDOG_SECS`, 3600s).
    let watchdog_secs = watchdog_secs.unwrap_or_else(watchdog_secs_from_env);
    // Atomically CLAIM the transition FIRST (fixes the TOCTOU: two concurrent enters
    // can no longer both observe "active" and both run release). Only the `Begin`
    // winner proceeds; everyone else returns a no-op with no side effects. The RAII
    // `transition` guard makes the pre-release phase cancellation-safe.
    let transition = match ctl.try_begin_enter() {
        Ok(t) => t,
        Err(BeginEnter::AlreadyIdle(m)) => {
            // Already idle from an earlier lease: honor a longer watchdog window from this
            // (overlapping) lease so the fail-safe never fires before the longest lease's
            // cap. Never shortens an existing deadline.
            ctl.bump_watchdog(now_epoch().saturating_add(watchdog_secs));
            return (EnterOutcome::AlreadyIdle(m), None);
        }
        Err(_) => return (EnterOutcome::InTransition, None),
    };
    // Phase is now `EnteringIdle`: `try_admit` rejects all new runs, so the drain below
    // is a genuine closed-world drain.

    info!(
        reason,
        "mint idle-mode: entering — draining runs and releasing GPU lock"
    );

    // ── Part 1: drain + prep, bounded by the release budget (CANCELLABLE) ──────────
    // If this overruns, the timeout fires, we do NOT commit, and the RAII `transition`
    // guard drops → clean rollback to `Active`. This is safe to abandon precisely
    // because the blocking release has NOT started (the latch is still clear), so no
    // point-of-no-return work is left running.
    let release_budget = Duration::from_secs(release_budget_secs_from_env());
    let prep = tokio::time::timeout(release_budget, async {
        let inflight_remaining = ctl
            .drain_inflight(Duration::from_secs(drain_secs_from_env()))
            .await;
        if inflight_remaining > 0 {
            warn!(
                inflight_remaining,
                "mint idle-mode: drain bound hit — releasing anyway (overrunning runs finish on their own)"
            );
        }
        let mem_before = read_mem_available_gb();
        (inflight_remaining, mem_before)
    })
    .await;

    let (inflight_remaining, mem_before) = match prep {
        Ok(v) => v,
        Err(_elapsed) => {
            warn!(
                reason,
                budget_secs = release_budget.as_secs(),
                "mint idle-mode: drain/prep exceeded its budget — aborting enter BEFORE releasing (guard rolls EnteringIdle back to Active; no blocking release was started)"
            );
            // `transition` drops here → clean rollback to Active (latch clear, so the
            // rollback is allowed); admission reopens only after this consistent rollback.
            return (EnterOutcome::InTransition, None);
        }
    };

    // ── Part 2: POINT OF NO RETURN — the non-abortable blocking GPU release ─────────
    // Set the latch BEFORE spawning so any concurrent drop/watchdog already sees
    // "releasing" and refuses to reopen admission. The blocking closure clears the
    // latch as its FINAL act (so a cancelled enter future still reopens the door only
    // after the detached release finished). We await the join UNCONDITIONALLY — no
    // timeout may abandon it.
    ctl.set_releasing();
    let ctl_for_task = ctl.clone();
    let handle = tokio::task::spawn_blocking(move || {
        // PANIC-SAFE latch clear: construct the RAII guard BEFORE calling release_fn(),
        // so the latch is cleared on EVERY exit path of the blocking thread — normal
        // return, early return, OR an unwinding panic in release_fn(). Without this, a
        // release_fn() panic would skip a bare `clear_releasing()` call and, since the
        // enter future may have been cancelled (its JoinHandle dropped, so the async-side
        // clear never runs either), leave `releasing` set forever — permanently wedging
        // admission and defeating recover_stale_transition (which early-returns while
        // releasing). Clear is idempotent, so the async-side fallback below is harmless.
        let _latch = ReleasingLatchGuard { ctl: ctl_for_task };
        release_fn()
    });
    // A FAILED release (the blocking task panicked ⇒ `JoinError`) must NOT be reported as
    // a successful idle: the GPU/RAM may not have been released, so committing
    // `EnteringIdle → Idle` and persisting an idle manifest would be a lie the compiler
    // (and a restart) would trust. On failure we therefore do NOT commit — we return a
    // non-`Entered` outcome and let the `transition` guard's `Drop` roll the phase back to
    // `Active` (the SAFE direction when release may not have happened). The in-task RAII
    // guard has already cleared the latch on the panic unwind, so the rollback is allowed
    // and the transition is never wedged. Only a SUCCESSFUL release (`Ok`) proceeds to
    // commit. (If `release_fn` ever becomes fallible and returns an error value, that
    // error path must be handled here identically — never commit Idle after a failed
    // release.)
    let gpu = match handle.await {
        Ok(r) => r,
        Err(_join_err) => {
            // Defensive, idempotent re-clear (the in-task RAII guard already cleared it).
            ctl.clear_releasing();
            warn!(
                reason,
                "mint idle-mode: blocking GPU release FAILED (task panicked) — NOT committing Idle; \
                 dropping the transition rolls EnteringIdle back to Active (resources may not be released)"
            );
            // `transition` drops on return → generation- and (now-clear) latch-guarded
            // `abort_enter` rolls EnteringIdle back to Active. No idle manifest is
            // installed or persisted.
            return (EnterOutcome::InTransition, None);
        }
    };

    // Release SUCCEEDED and the latch is clear; it is safe for the phase to leave
    // `EnteringIdle` and commit to `Idle`.
    let mem_after = read_mem_available_gb();
    let report = IdleReport {
        mem_available_before_gb: mem_before,
        mem_available_after_gb: mem_after,
        freed_gb: freed_gb(mem_before, mem_after),
        holders_released: gpu.holders_released.clone(),
        inflight_remaining,
        foreign_gpu_lock_holder: gpu.foreign_holder,
    };

    let now = now_epoch();
    let manifest = ResumeManifest {
        reason: reason.to_string(),
        entered_at: now,
        watchdog_deadline: now.saturating_add(watchdog_secs),
        released_holders: report.holders_released.clone(),
        mem_available_before_gb: report.mem_available_before_gb.unwrap_or(0.0),
    };
    // Complete the transition: `EnteringIdle{gen} → Idle` (consumes the guard so its
    // Drop rollback becomes a no-op). Generation-guarded: if our transition was
    // force-recovered while releasing (only possible AFTER the latch cleared, i.e. the
    // release already finished), `commit` returns `None` — our enter did NOT take
    // effect, so report `InTransition` with no report.
    let Some(stored) = transition.commit(manifest) else {
        warn!(
            reason,
            "mint idle-mode: enter superseded (transition recovered after release finished) — not reporting Entered"
        );
        return (EnterOutcome::InTransition, None);
    };

    info!(
        reason,
        holders_released = report.holders_released.len(),
        freed_gb = report.freed_gb.unwrap_or(0.0),
        "mint idle-mode: entered — host resources released for the compiler"
    );
    (EnterOutcome::Entered(stored), Some(report))
}

/// Leave idle and resume normal harness operation. Idempotent, CAS-guarded. MINT runs
/// re-acquire the GPU-authority lock LAZILY on their next sweep/case exactly as from a
/// cold start, so there is no async restore work on this path — hence `Activating` is a
/// nanosecond window and activate is effectively a single atomic transition.
pub async fn activate(reason: &str) -> ActivateOutcome {
    match mint_idle().begin_activate_inner() {
        Ok((m, generation)) => {
            // (restore side effects would go here; lazy re-acquire means none today.)
            // Disk already reads `Active` from begin_activate_inner; finish_activate
            // resolves memory `Activating{gen} → Active`. Generation-guarded.
            mint_idle().finish_activate(generation);
            info!(
                reason,
                released_holders = m.released_holders.len(),
                "mint idle-mode: activated — normal harness operation resumed (runs re-acquire the GPU lock on demand)"
            );
            ActivateOutcome::Activated(m)
        }
        Err(BeginActivate::AlreadyActive) => ActivateOutcome::AlreadyActive,
        Err(BeginActivate::PersistFailed) => ActivateOutcome::PersistFailed,
        Err(_) => ActivateOutcome::InTransition,
    }
}

/// The result of asking to admit one MINT run through idle-mode.
pub enum RunAdmission {
    /// Admitted — hold this guard for the duration of the run.
    Admitted(InflightGuard),
    /// Refused: idle-mode is in a transient window or a compiler build lease is being
    /// preserved. Retryable; carries a short human reason for logs.
    Refused(&'static str),
}

/// Admission hook for the MINT harness: a sweep/case run calls this before starting.
/// - `Active`        ⇒ admitted (guard already counted under the state lock).
/// - `EnteringIdle`/`Activating` ⇒ refused (a brief, bounded transition window).
/// - `Idle` + no compiler lease ⇒ lazily activate, then admit.
/// - `Idle` + compiler build lease held ⇒ refused, PRESERVING idle + watchdog protection
///   (the build window is not torn down by a stray MINT run).
pub async fn admit_run() -> RunAdmission {
    // Bounded attempts: at most one lazy restore, then a re-admit. A pathological re-idle
    // between the two just yields a retryable refusal rather than spinning.
    for _ in 0..3 {
        match mint_idle().try_admit() {
            AdmitOutcome::Admitted(guard) => return RunAdmission::Admitted(guard),
            AdmitOutcome::Transitioning => {
                return RunAdmission::Refused("mint idle transition in progress")
            }
            AdmitOutcome::Idle => match lazy_action(compiler_lease_held(now_epoch())) {
                LazyAction::PreserveIdle => {
                    info!(
                        "mint idle-mode: run requested while idle but a compiler build lease is held — \
                         preserving idle (refused, watchdog still protecting the build window)"
                    );
                    return RunAdmission::Refused("compiler build active — mint idle preserved");
                }
                LazyAction::Restore => {
                    info!("mint idle-mode: lazy activate — a real run arrived while idle");
                    if activate("lazy-on-run").await == ActivateOutcome::PersistFailed {
                        // Couldn't clear persisted idle safely → stay Idle; refuse rather
                        // than loop on a persist that keeps failing.
                        return RunAdmission::Refused("mint idle activate persist failed");
                    }
                    // loop: re-admit now that we should be Active
                }
            },
        }
    }
    RunAdmission::Refused("mint idle transition in progress")
}

// ── GPU-acquisition admission gate (makes idle-mode AUTHORITATIVE) ─────────────

/// The refusal reason surfaced when a MINT GPU acquisition is declined because the
/// harness is idling for a compiler build. Deliberately does NOT contain
/// `"held exclusively by"`, so `gpu_authority::is_live_holder_refusal` treats it as
/// NON-retryable — a sweep's bounded-backoff acquire that observes idling aborts the
/// wait promptly instead of spinning until its `max_wait` cap.
pub const MINT_IDLE_GPU_REFUSAL: &str =
    "MINT is idling for a compiler build — refusing GPU work (retry later)";

/// The single choke point that ties MINT's REAL GPU work to the idle admission gate,
/// evaluated at the moment the shared `gpu_authority` exclusive lock is ACTUALLY held
/// (not merely at entry, before any backoff wait — see the cycle-5 TOCTOU note on
/// [`gpu_authority::LiveGpuLock`](crate::intake::gpu_authority::LiveGpuLock)). Called at
/// each place MINT takes the lock ([`gpu_authority::ExclusiveGuard`](crate::intake::gpu_authority::ExclusiveGuard)
/// and `LiveGpuLock`):
///
/// - **`holder` is NOT a MINT holder** (a compiler build lease, or an operator's ad-hoc
///   `mint gpu acquire`): returns `Ok(None)` — NOT gated. Critically, this is why the
///   compiler can still acquire the GPU while MINT is idle (gating it would deadlock the
///   very build MINT idled for).
/// - **`holder` IS a MINT holder** and the harness is `Active`: returns `Ok(Some(guard))`.
///   The caller holds the [`InflightGuard`] for the whole unit of GPU work, so the
///   in-flight counter reflects a thread ACTUALLY holding the GPU, and a concurrent
///   [`enter_idle`] drains it (not zero). The admit increment is atomic with the phase
///   check (see [`IdleController::try_admit`]), so a unit can only ever be admitted while
///   the phase is still `Active` — never after `enter_idle` has flipped to `EnteringIdle`.
/// - **`holder` IS a MINT holder** but the harness is `EnteringIdle`/`Idle`/`Activating`:
///   returns `Err(_)` — the MINT GPU work MUST NOT start. A waiter that was admissible
///   while `Active` but only WINS the lock after idle began is refused HERE (and must hand
///   the lock back), closing the TOCTOU window.
///
/// Because the guard is taken only at true lock-acquisition, a queued waiter that never
/// wins the lock holds NO guard and so never inflates the drain count (never stalling
/// `enter_idle`). Controller-scoped for offline testing; [`try_admit_mint_gpu`] wraps it
/// against the process-global controller + configured MINT holders.
pub fn try_admit_gpu_on(
    ctl: &IdleController,
    holder: &str,
    mint_holders: &[String],
) -> Result<Option<InflightGuard>, String> {
    if !is_mint_holder(holder, mint_holders) {
        return Ok(None); // not MINT's work — never gated (e.g. the compiler lease itself)
    }
    match ctl.try_admit() {
        AdmitOutcome::Admitted(guard) => Ok(Some(guard)),
        AdmitOutcome::Idle | AdmitOutcome::Transitioning => Err(MINT_IDLE_GPU_REFUSAL.to_string()),
    }
}

/// Process-global wrapper for [`try_admit_gpu_on`]: validate a MINT GPU acquisition at the
/// moment the lock is held, against the live idle controller and configured MINT holders.
/// `Ok(None)` ⇒ ungated (non-MINT holder); `Ok(Some)` ⇒ admitted, hold the guard for the
/// GPU span; `Err` ⇒ refused (MINT is idling) — hand the lock back / do NOT begin work.
pub fn try_admit_mint_gpu(holder: &str) -> Result<Option<InflightGuard>, String> {
    try_admit_gpu_on(&mint_idle(), holder, &mint_gpu_holders_from_env())
}

/// Non-committing peek: would a MINT GPU acquisition for `holder` currently be admissible?
/// `true` for a non-MINT holder (never gated) or when the harness is `Active`; `false`
/// while `EnteringIdle`/`Idle`/`Activating` for a MINT holder. Takes NO in-flight guard
/// and changes no state — used to fast-abort a bounded-backoff acquire's WAIT the instant
/// idle begins (so a queued waiter gives up promptly), and to skip the acquire side
/// effects entirely when already idling. The AUTHORITATIVE, atomic decision is still
/// [`try_admit_gpu_on`] at true lock-acquisition; this is only a cheap early-out.
pub fn mint_gpu_admission_open_on(
    ctl: &IdleController,
    holder: &str,
    mint_holders: &[String],
) -> bool {
    !is_mint_holder(holder, mint_holders) || ctl.phase() == Phase::Active
}

/// Process-global wrapper for [`mint_gpu_admission_open_on`].
pub fn mint_gpu_admission_open(holder: &str) -> bool {
    mint_gpu_admission_open_on(&mint_idle(), holder, &mint_gpu_holders_from_env())
}

// ── Watchdog ──────────────────────────────────────────────────────────────────

/// Background fail-safe: every `interval`, if idle and the watchdog deadline has passed
/// AND no COMPILER build lease is currently held, auto-activate so MINT is never left
/// silently idle (a crashed/forgotten compiler, or a stale idle state reloaded after a
/// restart). While a compiler build lease IS held the deadline is deferred — a
/// legitimately long build keeps MINT idle as long as it holds the GPU. A NON-compiler
/// GPU holder does NOT extend the idle window. Also force-resolves a controller wedged
/// in a transient phase past the stale bound (backstop behind the RAII guard).
pub async fn watchdog_loop(interval: Duration) {
    info!(
        interval_secs = interval.as_secs(),
        "mint idle-mode watchdog started"
    );
    let patterns = compiler_lease_holders_from_env();
    let stale_secs = stale_transition_secs_from_env();
    loop {
        tokio::time::sleep(interval).await;
        let now = now_epoch();
        // Backstop: force-resolve a controller wedged in a transient phase past the
        // stale bound. The RAII EnterTransition guard normally prevents this; this only
        // fires for a pathological wedge that escaped the guard.
        if mint_idle().recover_stale_transition(now, stale_secs) {
            warn!(
                stale_secs,
                "mint idle-mode watchdog: force-resolved a stale idle transition back to Active"
            );
            continue;
        }
        let Some(m) = mint_idle().snapshot() else {
            continue;
        };
        // Live GPU holder (if any), from gpu_authority — only a LIVE holder counts.
        let holder = match crate::intake::gpu_authority::status().lock {
            Some((h, _mode, _pid, alive)) if alive => Some(h),
            _ => None,
        };
        let holder_label = holder.as_deref();
        if !watchdog_should_activate(m.watchdog_expired(now), holder_label, &patterns) {
            continue;
        }
        warn!(
            reason = %m.reason,
            "mint idle-mode watchdog: deadline passed with no active compiler lease — auto-activating (fail-safe)"
        );
        let _ = activate("watchdog-timeout").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(reason: &str, entered_at: u64, deadline: u64) -> ResumeManifest {
        ResumeManifest {
            reason: reason.into(),
            entered_at,
            watchdog_deadline: deadline,
            released_holders: vec!["intake_coder_sweep".into()],
            mem_available_before_gb: 12.0,
        }
    }

    fn holders() -> Vec<String> {
        vec!["compiler".into(), "build".into(), "bld".into()]
    }

    // ── pure decisions ───────────────────────────────────────────────────────

    #[test]
    fn enter_when_active_enters_when_idle_is_noop() {
        assert_eq!(decide_enter(None), EnterDecision::Enter);
        let m = manifest("compiler", 100, 3700);
        assert_eq!(decide_enter(Some(&m)), EnterDecision::AlreadyIdle);
    }

    #[test]
    fn activate_when_idle_restores_when_active_is_noop() {
        let m = manifest("compiler", 100, 3700);
        assert_eq!(decide_activate(Some(&m)), ActivateDecision::Restore);
        assert_eq!(decide_activate(None), ActivateDecision::AlreadyActive);
    }

    #[test]
    fn watchdog_expiry_is_deadline_relative() {
        let m = manifest("compiler", 100, 3700);
        assert!(!m.watchdog_expired(3699));
        assert!(m.watchdog_expired(3700)); // exactly at the deadline ⇒ expired
        assert!(m.watchdog_expired(9999));
    }

    // ── compiler-lease vs MINT-own holder matching ────────────────────────────

    #[test]
    fn compiler_lease_matches_only_build_holders() {
        let p = holders();
        assert!(is_compiler_lease("compiler", &p));
        assert!(is_compiler_lease("bld-05-compiler", &p));
        assert!(is_compiler_lease("constellation-build", &p));
        assert!(is_compiler_lease("COMPILER", &p)); // case-insensitive
                                                    // MINT's own GPU jobs must NOT read as a compiler lease:
        assert!(!is_compiler_lease("intake_coder_sweep", &p));
        assert!(!is_compiler_lease("intake_assistant_sweep", &p));
        assert!(!is_compiler_lease("mint_breakfix", &p));
        assert!(!is_compiler_lease("", &p));
    }

    #[test]
    fn mint_holder_matches_only_mints_own_labels() {
        let mints = vec![
            "intake_coder_sweep".to_string(),
            "intake_assistant_sweep".to_string(),
            "intake_coder_case".to_string(),
            "mint_breakfix".to_string(),
        ];
        assert!(is_mint_holder("intake_coder_sweep", &mints));
        assert!(is_mint_holder("INTAKE_CODER_SWEEP", &mints)); // case-insensitive
        assert!(is_mint_holder("mint_breakfix", &mints));
        // A compiler lease or some other job is NOT a MINT holder → not force-released.
        assert!(!is_mint_holder("bld-05-compiler", &mints));
        assert!(!is_mint_holder("some_other_job", &mints));
        assert!(!is_mint_holder("", &mints));
    }

    #[test]
    fn lazy_action_preserves_idle_only_under_compiler_lease() {
        assert_eq!(lazy_action(true), LazyAction::PreserveIdle);
        assert_eq!(lazy_action(false), LazyAction::Restore);
    }

    #[test]
    fn watchdog_defers_only_for_compiler_lease() {
        let p = holders();
        // Not expired ⇒ never activate, regardless of holder.
        assert!(!watchdog_should_activate(false, Some("compiler"), &p));
        assert!(!watchdog_should_activate(false, None, &p));
        // Expired + compiler lease held ⇒ defer.
        assert!(!watchdog_should_activate(true, Some("bld-05-compiler"), &p));
        // Expired + a NON-compiler GPU holder (e.g. MINT's own sweep) ⇒ auto-activate.
        assert!(watchdog_should_activate(
            true,
            Some("intake_coder_sweep"),
            &p
        ));
        // Expired + no holder ⇒ auto-activate.
        assert!(watchdog_should_activate(true, None, &p));
    }

    // ── controller transitions (isolated instance, no globals) ───────────────

    #[test]
    fn enter_then_exit_cycle() {
        let ctl = IdleController::new();
        assert_eq!(ctl.phase(), Phase::Active);
        assert!(!ctl.is_idle());
        assert!(ctl.snapshot().is_none());

        let m = manifest("compiler", 100, 3700);
        match ctl.enter(m.clone()) {
            EnterOutcome::Entered(got) => assert_eq!(got, m),
            other => panic!("expected Entered, got {other:?}"),
        }
        assert!(ctl.is_idle());
        assert_eq!(ctl.phase(), Phase::Idle);
        assert_eq!(ctl.snapshot().unwrap(), m);

        match ctl.exit() {
            ActivateOutcome::Activated(got) => assert_eq!(got, m),
            other => panic!("expected Activated, got {other:?}"),
        }
        assert!(!ctl.is_idle());
        assert_eq!(ctl.phase(), Phase::Active);
    }

    #[test]
    fn enter_is_idempotent_and_does_not_clobber() {
        let ctl = IdleController::new();
        let first = manifest("compiler", 100, 3700);
        let second = manifest("someone-else", 999, 9999);
        assert!(matches!(ctl.enter(first.clone()), EnterOutcome::Entered(_)));

        // A second enter must NOT overwrite the original manifest.
        match ctl.enter(second) {
            EnterOutcome::AlreadyIdle(got) => assert_eq!(got, first),
            other => panic!("expected AlreadyIdle, got {other:?}"),
        }
        assert_eq!(ctl.snapshot().unwrap(), first);
    }

    #[test]
    fn exit_is_idempotent() {
        let ctl = IdleController::new();
        assert!(matches!(ctl.exit(), ActivateOutcome::AlreadyActive));
        ctl.enter(manifest("compiler", 1, 2));
        assert!(matches!(ctl.exit(), ActivateOutcome::Activated(_)));
        assert!(matches!(ctl.exit(), ActivateOutcome::AlreadyActive));
    }

    // ── concurrency-safety: CAS + closed-world drain ─────────────────────────

    #[test]
    fn begin_enter_is_exclusive_cas_release_runs_once() {
        // Only ONE caller may run release. The first begin wins; a second while
        // EnteringIdle must NOT also get a transition.
        let ctl = IdleController::new();
        let t = ctl.try_begin_enter().expect("first begins");
        assert!(matches!(
            ctl.try_begin_enter(),
            Err(BeginEnter::InTransition)
        ));
        assert_eq!(ctl.begin_enter(), BeginEnter::InTransition);
        // commit and confirm a later begin sees AlreadyIdle, never a second Begin.
        let m = manifest("compiler", 1, 2);
        assert_eq!(t.commit(m.clone()), Some(m.clone()));
        match ctl.begin_enter() {
            BeginEnter::AlreadyIdle(got) => assert_eq!(got, m),
            other => panic!("expected AlreadyIdle, got {other:?}"),
        }
    }

    #[test]
    fn begin_activate_is_exclusive_cas() {
        let ctl = IdleController::new();
        ctl.enter(manifest("compiler", 1, 2));
        let (_m, generation) = ctl.begin_activate_inner().expect("Idle ⇒ activate begins");
        // While Activating, a second begin must not also win.
        assert_eq!(ctl.begin_activate(), BeginActivate::InTransition);
        assert!(ctl.finish_activate(generation));
        assert_eq!(ctl.begin_activate(), BeginActivate::AlreadyActive);
    }

    // ── cancellation-safety of the EnteringIdle transition ───────────────────

    #[test]
    fn dropped_enter_transition_rolls_back_to_active() {
        // A transition guard dropped WITHOUT commit (future cancelled/panicked) must
        // leave the controller recoverable (Active), never wedged in EnteringIdle.
        let ctl = IdleController::new();
        {
            let _t = ctl.try_begin_enter().expect("Active ⇒ transition begins");
            assert_eq!(ctl.phase(), Phase::EnteringIdle);
            // fall out of scope WITHOUT calling commit → Drop rolls back
        }
        assert_eq!(
            ctl.phase(),
            Phase::Active,
            "dropped transition must roll back to Active"
        );
        assert!(!ctl.is_idle());
        // Controller is fully usable afterwards.
        assert!(matches!(
            ctl.enter(manifest("compiler", 1, 2)),
            EnterOutcome::Entered(_)
        ));
    }

    #[test]
    fn committed_enter_transition_reaches_idle() {
        let ctl = IdleController::new();
        let t = ctl.try_begin_enter().expect("Active ⇒ transition begins");
        let m = manifest("compiler", 5, 10);
        assert_eq!(t.commit(m.clone()), Some(m.clone()));
        assert!(ctl.is_idle());
        assert_eq!(ctl.snapshot().unwrap(), m);
    }

    #[test]
    fn try_begin_enter_errors_when_not_active() {
        let ctl = IdleController::new();
        ctl.enter(manifest("compiler", 1, 2));
        // Already idle ⇒ Err(AlreadyIdle), and NO transition guard handed out (so no
        // spurious rollback of the live idle state when that Err is dropped).
        match ctl.try_begin_enter() {
            Err(BeginEnter::AlreadyIdle(_)) => {}
            other => panic!("expected Err(AlreadyIdle), got {other:?}"),
        }
        assert!(
            ctl.is_idle(),
            "a rejected try_begin_enter must not disturb idle"
        );
    }

    #[test]
    fn recover_stale_transition_only_when_stale() {
        let ctl = IdleController::new();
        assert_eq!(ctl.begin_enter(), BeginEnter::Begin);
        let now = now_epoch();
        // Fresh transition ⇒ NOT recovered.
        assert!(!ctl.recover_stale_transition(now, 120));
        assert_eq!(ctl.phase(), Phase::EnteringIdle);
        // Well past the bound ⇒ force-resolved to Active.
        assert!(ctl.recover_stale_transition(now + 1_000, 120));
        assert_eq!(ctl.phase(), Phase::Active);
        // A steady phase is never touched.
        assert!(!ctl.recover_stale_transition(now + 1_000, 120));
    }

    #[test]
    fn stale_guard_drop_does_not_clobber_newer_transition() {
        // ABA: begin (gen N) → watchdog recovers to Active (gen bumped) → begin again
        // (gen N+2) → drop the FIRST guard. The stale guard's rollback must be a NO-OP.
        let ctl = IdleController::new();
        let first = ctl.try_begin_enter().expect("first transition begins");
        assert_eq!(ctl.phase(), Phase::EnteringIdle);

        let now = now_epoch();
        assert!(ctl.recover_stale_transition(now + 10_000, 120));
        assert_eq!(ctl.phase(), Phase::Active);

        let second = ctl.try_begin_enter().expect("second transition begins");
        assert_eq!(ctl.phase(), Phase::EnteringIdle);

        // Drop the FIRST (stale) guard: its Drop→abort_enter must NOT touch the second
        // transition (generation mismatch), so we stay in EnteringIdle.
        drop(first);
        assert_eq!(
            ctl.phase(),
            Phase::EnteringIdle,
            "stale guard drop must not roll back the newer transition"
        );

        // The second transition can still commit normally.
        let m = manifest("compiler", 7, 9);
        assert_eq!(second.commit(m.clone()), Some(m.clone()));
        assert!(ctl.is_idle());
        assert_eq!(ctl.snapshot().unwrap(), m);
    }

    #[test]
    fn stale_guard_commit_does_not_clobber_newer_phase() {
        // A stale guard's COMMIT must also no-op (return None) rather than install its
        // manifest over whatever phase now exists.
        let ctl = IdleController::new();
        let stale = ctl.try_begin_enter().expect("first transition begins");
        let now = now_epoch();
        assert!(ctl.recover_stale_transition(now + 10_000, 120)); // → Active, gen bumped
        let fresh = manifest("compiler", 1, 2);
        assert!(matches!(ctl.enter(fresh.clone()), EnterOutcome::Entered(_)));
        let stale_m = manifest("stale", 99, 100);
        assert_eq!(stale.commit(stale_m), None);
        assert_eq!(
            ctl.snapshot().unwrap(),
            fresh,
            "stale commit must not overwrite the newer manifest"
        );
    }

    #[test]
    fn no_inflight_admitted_after_entering_idle() {
        // Once we flip to EnteringIdle, try_admit must reject — no new run can join the
        // in-flight set, so the drain is closed-world. The counter is per-controller.
        let ctl = IdleController::new();
        assert_eq!(ctl.inflight_count(), 0);
        let guard = match ctl.try_admit() {
            AdmitOutcome::Admitted(g) => {
                assert_eq!(ctl.inflight_count(), 1);
                g
            }
            _ => panic!("Active must admit"),
        };
        drop(guard);
        assert_eq!(ctl.inflight_count(), 0);

        // Enter the transition; now admission must be refused with NO increment.
        let t = ctl.try_begin_enter().expect("Active ⇒ transition begins");
        assert!(matches!(ctl.try_admit(), AdmitOutcome::Transitioning));
        assert_eq!(
            ctl.inflight_count(),
            0,
            "no run admitted after EnteringIdle"
        );

        // Fully idle also refuses admission (caller lazy-activates instead).
        let _ = t.commit(manifest("compiler", 1, 2));
        assert!(matches!(ctl.try_admit(), AdmitOutcome::Idle));
        assert_eq!(ctl.inflight_count(), 0);
    }

    #[test]
    fn admit_guard_increments_and_decrements() {
        let ctl = IdleController::new();
        assert_eq!(ctl.inflight_count(), 0);
        match ctl.try_admit() {
            AdmitOutcome::Admitted(g) => {
                assert_eq!(ctl.inflight_count(), 1);
                drop(g);
                assert_eq!(ctl.inflight_count(), 0);
            }
            _ => panic!("Active must admit"),
        }
    }

    // ── freed-RAM arithmetic ─────────────────────────────────────────────────

    #[test]
    fn freed_gb_clamps_and_handles_missing() {
        assert_eq!(freed_gb(Some(10.0), Some(25.0)), Some(15.0));
        assert_eq!(freed_gb(Some(25.0), Some(24.0)), Some(0.0));
        assert_eq!(freed_gb(None, Some(25.0)), None);
        assert_eq!(freed_gb(Some(10.0), None), None);
    }

    // ── durable persistence ──────────────────────────────────────────────────

    #[test]
    fn with_state_reloads_idle_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mint_idle_state.json");

        let ctl = IdleController::with_state(Some(path.clone()));
        let m = manifest("compiler", 100, 3700);
        assert!(matches!(ctl.enter(m.clone()), EnterOutcome::Entered(_)));
        assert!(path.exists(), "state file should be written on enter");

        // Simulate a restart: a fresh controller reloads the same file.
        let restarted = IdleController::with_state(Some(path.clone()));
        assert!(restarted.is_idle());
        assert_eq!(restarted.snapshot().unwrap(), m);
    }

    #[test]
    fn exit_clears_persisted_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mint_idle_state.json");

        let ctl = IdleController::with_state(Some(path.clone()));
        ctl.enter(manifest("compiler", 1, 2));
        assert!(matches!(ctl.exit(), ActivateOutcome::Activated(_)));

        let restarted = IdleController::with_state(Some(path.clone()));
        assert!(!restarted.is_idle());
    }

    #[test]
    fn entering_idle_is_not_persisted() {
        // The transient marker must not persist: a crash mid-enter reloads Active.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mint_idle_state.json");
        let ctl = IdleController::with_state(Some(path.clone()));
        assert_eq!(ctl.begin_enter(), BeginEnter::Begin); // EnteringIdle, no finish
        let restarted = IdleController::with_state(Some(path));
        assert!(
            !restarted.is_idle(),
            "EnteringIdle must not persist as idle"
        );
    }

    #[test]
    fn crash_during_activating_reloads_active_not_idle() {
        // begin_activate clears the persisted manifest to Active BEFORE the (async)
        // restore work. A crash while memory is `Activating` must reload as Active.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mint_idle_state.json");

        let ctl = IdleController::with_state(Some(path.clone()));
        ctl.enter(manifest("compiler", 1, 2)); // → Idle, disk = Some(manifest)

        let (_m, _gen) = ctl.begin_activate_inner().expect("Idle ⇒ activate begins");
        assert_eq!(ctl.phase(), Phase::Activating);

        let reloaded = IdleController::with_state(Some(path));
        assert!(
            !reloaded.is_idle(),
            "crash during Activating must reload Active, not Idle"
        );
        assert_eq!(reloaded.phase(), Phase::Active);
    }

    #[test]
    fn with_state_corrupt_file_starts_active_no_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mint_idle_state.json");
        std::fs::write(&path, b"{ not valid json ").unwrap();

        let ctl = IdleController::with_state(Some(path));
        assert!(!ctl.is_idle());
        assert!(matches!(
            ctl.enter(manifest("compiler", 1, 2)),
            EnterOutcome::Entered(_)
        ));
    }

    #[test]
    fn with_state_missing_file_starts_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let ctl = IdleController::with_state(Some(path));
        assert!(!ctl.is_idle());
    }

    #[test]
    fn no_state_path_writes_nothing_and_still_works() {
        let ctl = IdleController::new(); // in-memory only
        assert!(matches!(
            ctl.enter(manifest("compiler", 1, 2)),
            EnterOutcome::Entered(_)
        ));
        assert!(ctl.is_idle());
        assert!(matches!(ctl.exit(), ActivateOutcome::Activated(_)));
    }

    #[test]
    fn begin_activate_persist_failure_stays_recoverable() {
        // When a state path is configured and clearing the manifest to Active FAILS,
        // begin_activate must ABORT (PersistFailed) and leave the controller Idle — so
        // on-disk and memory stay consistent, never Activating-with-disk-still-Idle.
        let dir = tempfile::tempdir().unwrap();
        // Make the state file's PARENT a regular file, so create_dir_all/write fails.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("mint_idle_state.json"); // parent `blocker` is a file

        let ctl = IdleController::with_state(Some(path));
        // Reach Idle in memory (finish_enter's persist is best-effort, fails silently).
        assert!(matches!(
            ctl.enter(manifest("compiler", 1, 2)),
            EnterOutcome::Entered(_)
        ));
        assert!(ctl.is_idle());

        // The hard-gated persist(None) MUST fail → PersistFailed, memory stays Idle.
        match ctl.begin_activate_inner() {
            Err(BeginActivate::PersistFailed) => {}
            other => panic!("expected PersistFailed, got {other:?}"),
        }
        assert!(
            ctl.is_idle(),
            "activate must abort and remain Idle when the persist gate fails"
        );
        assert_eq!(ctl.phase(), Phase::Idle);
        // The full exit() path surfaces it as a retryable ActivateOutcome too.
        assert_eq!(ctl.exit(), ActivateOutcome::PersistFailed);
        assert!(ctl.is_idle());
    }

    #[tokio::test]
    async fn prep_timeout_before_release_rolls_back_with_admission_closed() {
        // The drain/prep phase (BEFORE the point of no return) is cancellable: on a
        // budget timeout the guard rollback reopens admission — but only because the
        // blocking release has NOT started (the latch is clear), so this rollback is
        // consistent, not a mid-release reopen.
        let ctl = IdleController::new();
        let transition = ctl.try_begin_enter().expect("Active ⇒ transition begins");

        assert!(matches!(ctl.try_admit(), AdmitOutcome::Transitioning));
        assert!(!ctl.is_releasing(), "no blocking release started yet");

        let budget = Duration::from_millis(20);
        let prep = tokio::time::timeout(budget, async {
            tokio::time::sleep(Duration::from_millis(500)).await;
        })
        .await;
        assert!(prep.is_err(), "prep must exceed its budget");

        // STILL EnteringIdle and STILL closed — not committed or rolled back.
        assert_eq!(ctl.phase(), Phase::EnteringIdle);
        assert!(matches!(ctl.try_admit(), AdmitOutcome::Transitioning));

        // Latch clear ⇒ dropping the guard rolls back to Active (safe: nothing running).
        drop(transition);
        assert_eq!(ctl.phase(), Phase::Active);
        assert!(matches!(ctl.try_admit(), AdmitOutcome::Admitted(_)));
    }

    #[test]
    fn abort_enter_withheld_while_releasing() {
        // The dropped-guard rollback (abort_enter) must be a NO-OP while a blocking
        // release is in progress — otherwise a cancelled enter would reopen admission
        // mid-release. Once the latch clears, the rollback is allowed again.
        let ctl = IdleController::new();
        let t = ctl.try_begin_enter().expect("Active ⇒ transition begins");
        assert_eq!(ctl.phase(), Phase::EnteringIdle);

        ctl.set_releasing();
        // Drop the guard WHILE releasing: abort must be withheld, phase stays EnteringIdle,
        // admission stays closed.
        drop(t);
        assert_eq!(
            ctl.phase(),
            Phase::EnteringIdle,
            "rollback must be withheld while releasing"
        );
        assert!(matches!(ctl.try_admit(), AdmitOutcome::Transitioning));

        // After the release finishes (latch clear), the watchdog can recover it.
        ctl.clear_releasing();
        let now = now_epoch();
        assert!(ctl.recover_stale_transition(now + 10_000, 1));
        assert_eq!(ctl.phase(), Phase::Active);
    }

    #[test]
    fn recover_stale_transition_withheld_while_releasing() {
        // The watchdog's stale-recovery must ALSO be a no-op while releasing — it must
        // never force a stuck EnteringIdle back to Active mid-release.
        let ctl = IdleController::new();
        assert_eq!(ctl.begin_enter(), BeginEnter::Begin);
        let now = now_epoch();
        ctl.set_releasing();
        // Even a wildly-past deadline must NOT recover while releasing.
        assert!(!ctl.recover_stale_transition(now + 10_000, 1));
        assert_eq!(ctl.phase(), Phase::EnteringIdle);
        // Once release finishes, recovery proceeds.
        ctl.clear_releasing();
        assert!(ctl.recover_stale_transition(now + 10_000, 1));
        assert_eq!(ctl.phase(), Phase::Active);
    }

    #[tokio::test]
    async fn admission_stays_closed_until_blocking_release_completes() {
        // THE codex-review invariant: a spawn_blocking release cannot be cancelled once
        // started, so admission must stay CLOSED for the ENTIRE blocking release — not
        // just the async wrapper — and reopen only after the release actually completes.
        // Drive the real `enter_idle_on` with an injected fake release that blocks until
        // the test signals it, and prove no admission ever succeeds until then.
        use std::sync::mpsc;

        let ctl = Arc::new(IdleController::new());
        let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
        let started = Arc::new(AtomicBool::new(false));

        let started_in = started.clone();
        let slow_release = move || {
            started_in.store(true, Ordering::SeqCst);
            // Block the blocking thread until the test lets it finish — simulating a
            // release that runs far longer than any async budget.
            let _ = unblock_rx.recv();
            GpuReleaseResult {
                holders_released: vec!["intake_coder_sweep".into()],
                foreign_holder: None,
            }
        };

        let ctl_task = ctl.clone();
        let enter =
            tokio::spawn(async move { enter_idle_on(ctl_task, "test", slow_release, None).await });

        // Wait until the blocking release has actually started.
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Release is mid-flight: admission MUST be closed (Transitioning), never Admitted,
        // across repeated attempts and real elapsed time — the whole release, not a wrapper.
        assert!(
            ctl.is_releasing(),
            "latch must be set during the blocking release"
        );
        for _ in 0..10 {
            match ctl.try_admit() {
                AdmitOutcome::Transitioning => {}
                AdmitOutcome::Admitted(_) => {
                    panic!("admission reopened MID-RELEASE — invariant violated")
                }
                AdmitOutcome::Idle => panic!("must not be Idle until release completes"),
            }
            assert_eq!(ctl.phase(), Phase::EnteringIdle);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Let the blocking release complete.
        unblock_tx.send(()).unwrap();
        let (outcome, report) = enter.await.expect("enter task joins");

        // Only now does the phase resolve — to Idle (never Active/Admitted mid-release).
        assert!(matches!(outcome, EnterOutcome::Entered(_)));
        assert!(report.is_some());
        assert!(
            !ctl.is_releasing(),
            "latch cleared once the release finished"
        );
        assert!(ctl.is_idle());
        assert!(
            matches!(ctl.try_admit(), AdmitOutcome::Idle),
            "post-release admission is the lazy Idle path, never a stale Admitted"
        );
    }

    #[tokio::test]
    async fn cancelled_enter_mid_release_keeps_admission_closed_until_release_done() {
        // Even if the enter FUTURE is cancelled mid-release, the detached blocking task
        // keeps running and admission must stay closed until it finishes; only then may
        // the phase leave EnteringIdle.
        use std::sync::mpsc;

        let ctl = Arc::new(IdleController::new());
        let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
        let started = Arc::new(AtomicBool::new(false));

        let started_in = started.clone();
        let slow_release = move || {
            started_in.store(true, Ordering::SeqCst);
            let _ = unblock_rx.recv();
            GpuReleaseResult {
                holders_released: Vec::new(),
                foreign_holder: None,
            }
        };

        let ctl_task = ctl.clone();
        let enter =
            tokio::spawn(async move { enter_idle_on(ctl_task, "test", slow_release, None).await });

        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(ctl.is_releasing());

        // Cancel the enter future mid-release (client disconnect). The guard drops, but
        // its rollback is WITHHELD because the latch is set — admission stays closed.
        enter.abort();
        let _ = enter.await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(
            ctl.phase(),
            Phase::EnteringIdle,
            "cancelled enter must not reopen admission while the release runs"
        );
        assert!(matches!(ctl.try_admit(), AdmitOutcome::Transitioning));

        // Let the detached release finish — it clears the latch as its final act.
        unblock_tx.send(()).unwrap();
        // Wait for the latch to clear (the detached blocking task ran to completion).
        for _ in 0..500 {
            if !ctl.is_releasing() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            !ctl.is_releasing(),
            "detached release must clear the latch when done"
        );

        // Now (release finished) the watchdog may recover the abandoned transition.
        let now = now_epoch();
        assert!(ctl.recover_stale_transition(now + 10_000, 1));
        assert_eq!(ctl.phase(), Phase::Active);
    }

    #[tokio::test]
    async fn panicking_release_on_cancelled_enter_clears_latch_and_is_recoverable() {
        // codex cycle-2 hole: if the enter future is cancelled (JoinHandle dropped, so
        // the async-side clear never runs) AND the blocking release then PANICS before a
        // bare clear could run, a naive impl leaves `releasing` set forever — permanently
        // wedging admission and defeating recover_stale_transition. The in-task RAII
        // ReleasingLatchGuard must clear the latch on the panic unwind regardless.
        use std::sync::mpsc;

        let ctl = Arc::new(IdleController::new());
        let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
        let started = Arc::new(AtomicBool::new(false));

        let started_in = started.clone();
        let panicking_release = move || -> GpuReleaseResult {
            started_in.store(true, Ordering::SeqCst);
            let _ = unblock_rx.recv(); // block until the test lets it run
            panic!("simulated GPU-release failure mid-release");
        };

        let ctl_task = ctl.clone();
        let enter =
            tokio::spawn(async move { enter_idle_on(ctl_task, "test", panicking_release, None).await });

        // Wait until the blocking release has started (latch set, blocked on recv).
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(ctl.is_releasing());

        // Cancel the enter future mid-release: JoinHandle dropped, guard rollback withheld.
        enter.abort();
        let _ = enter.await;
        assert_eq!(
            ctl.phase(),
            Phase::EnteringIdle,
            "cancelled enter keeps admission closed while the release runs"
        );
        assert!(ctl.is_releasing());

        // Let the detached release run — it PANICS. The in-task RAII guard must STILL
        // clear the latch on the unwind (the panic is contained at the spawn_blocking
        // boundary, so the test process does not abort).
        unblock_tx.send(()).unwrap();
        for _ in 0..500 {
            if !ctl.is_releasing() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            !ctl.is_releasing(),
            "a panicking release must still clear the latch via the RAII drop-guard"
        );

        // The stuck EnteringIdle transition is now recoverable (not permanently wedged).
        let now = now_epoch();
        assert!(ctl.recover_stale_transition(now + 10_000, 1));
        assert_eq!(ctl.phase(), Phase::Active);

        // And a subsequent enter proceeds normally — admission is not permanently closed.
        match ctl.enter(manifest("compiler", 1, 2)) {
            EnterOutcome::Entered(_) => {}
            other => panic!("expected a fresh enter to succeed after recovery, got {other:?}"),
        }
        assert!(ctl.is_idle());
    }

    #[tokio::test]
    async fn awaited_failed_release_does_not_commit_idle_and_ends_active() {
        // codex cycle-3: when the caller is STILL awaiting the blocking release and it
        // FAILS (panics), enter_idle_on must NOT fabricate success — it must not commit
        // EnteringIdle→Idle nor persist an idle manifest (the GPU/RAM may not be freed).
        // It ends Active (rolled back) with a non-Entered outcome and no idle persisted.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mint_idle_state.json");
        let ctl = Arc::new(IdleController::with_state(Some(path.clone())));

        let failing_release = || -> GpuReleaseResult {
            panic!("simulated GPU-release failure");
        };

        // Await to completion (NOT cancelled): the JoinError path must decline to commit.
        let (outcome, report) = enter_idle_on(ctl.clone(), "test", failing_release, None).await;

        assert!(
            matches!(outcome, EnterOutcome::InTransition),
            "a failed release must yield a non-Entered outcome, got {outcome:?}"
        );
        assert!(report.is_none(), "a failed release reports no freed-RAM");
        assert!(
            !ctl.is_releasing(),
            "latch cleared by the in-task RAII guard"
        );
        assert!(
            !ctl.is_idle(),
            "a failed release must not leave the controller Idle"
        );
        assert_eq!(
            ctl.phase(),
            Phase::Active,
            "a failed release rolls the transition back to Active (safe direction)"
        );

        // No idle manifest was persisted: a fresh reload from the same path is not idle.
        let reloaded = IdleController::with_state(Some(path));
        assert!(
            !reloaded.is_idle(),
            "no idle manifest may be persisted after a failed release"
        );

        // The controller is fully usable afterwards (a real enter still works).
        assert!(matches!(
            ctl.enter(manifest("compiler", 1, 2)),
            EnterOutcome::Entered(_)
        ));
        assert!(ctl.is_idle());
    }

    // ── env parsing + holder config ──────────────────────────────────────────

    #[test]
    fn positive_env_falls_back_on_junk() {
        std::env::set_var("MINT_IDLE_TEST_KEY", "not-a-number");
        assert_eq!(parse_positive_env("MINT_IDLE_TEST_KEY", 42), 42);
        std::env::set_var("MINT_IDLE_TEST_KEY", "0");
        assert_eq!(parse_positive_env("MINT_IDLE_TEST_KEY", 42), 42);
        std::env::set_var("MINT_IDLE_TEST_KEY", "900");
        assert_eq!(parse_positive_env("MINT_IDLE_TEST_KEY", 42), 900);
        std::env::remove_var("MINT_IDLE_TEST_KEY");
    }

    #[test]
    fn compiler_lease_holders_default_when_unset() {
        std::env::remove_var("MINT_IDLE_COMPILER_LEASE_HOLDERS");
        let p = compiler_lease_holders_from_env();
        assert!(p.contains(&"compiler".to_string()));
        assert!(is_compiler_lease("compiler", &p));
    }

    #[test]
    fn mint_holders_default_to_the_harness_labels() {
        std::env::remove_var("MINT_GPU_HOLDERS");
        let h = mint_gpu_holders_from_env();
        // The real GPU_HOLDER consts the harness acquires under.
        assert!(h.contains(&"intake_coder_sweep".to_string()));
        assert!(h.contains(&"intake_coder_case".to_string()));
        assert!(h.contains(&"intake_assistant_sweep".to_string()));
        assert!(h.contains(&"mint_breakfix".to_string()));
        assert!(is_mint_holder("intake_coder_sweep", &h));
        // Overridable without touching code.
        std::env::set_var("MINT_GPU_HOLDERS", "foo, bar ");
        let o = mint_gpu_holders_from_env();
        assert_eq!(o, vec!["foo".to_string(), "bar".to_string()]);
        std::env::remove_var("MINT_GPU_HOLDERS");
    }

    #[test]
    fn stale_threshold_always_exceeds_release_budget() {
        // Invariant: the watchdog stale bound must be strictly greater than the release
        // budget, so stale-recovery can never fire during live release.
        std::env::remove_var("MINT_IDLE_STALE_TRANSITION_SECS");
        std::env::remove_var("MINT_IDLE_RELEASE_BUDGET_SECS");
        assert!(stale_transition_secs_from_env() > release_budget_secs_from_env());

        // Misconfigured so stale ≤ budget ⇒ clamped strictly above the budget.
        std::env::set_var("MINT_IDLE_RELEASE_BUDGET_SECS", "90");
        std::env::set_var("MINT_IDLE_STALE_TRANSITION_SECS", "30"); // below budget
        assert!(
            stale_transition_secs_from_env() > release_budget_secs_from_env(),
            "misconfigured stale ≤ budget must be clamped above the budget"
        );

        std::env::remove_var("MINT_IDLE_STALE_TRANSITION_SECS");
        std::env::remove_var("MINT_IDLE_RELEASE_BUDGET_SECS");
    }

    // ── GPU-acquisition admission gate (authoritative drain wiring) ───────────

    fn mint_labels() -> Vec<String> {
        vec![
            "intake_coder_sweep".to_string(),
            "intake_assistant_sweep".to_string(),
            "intake_coder_case".to_string(),
            "mint_breakfix".to_string(),
        ]
    }

    #[test]
    fn mint_gpu_admission_refused_while_idling_but_compiler_never_gated() {
        // Proves the gate MINT's real entry points call (ExclusiveGuard / LiveGpuLock
        // via try_admit_mint_gpu) refuses MINT GPU work while EnteringIdle/Idle, and
        // never gates a non-MINT (compiler) holder.
        let mints = mint_labels();

        // Active: a MINT holder is admitted (Some guard); a non-MINT holder is ungated.
        let ctl = IdleController::new();
        let g = try_admit_gpu_on(&ctl, "intake_coder_sweep", &mints)
            .expect("Active admits MINT work")
            .expect("MINT holder yields a counted guard");
        assert_eq!(ctl.inflight_count(), 1, "an admitted MINT unit is counted");
        assert!(
            try_admit_gpu_on(&ctl, "bld-05-compiler", &mints)
                .expect("compiler is never gated")
                .is_none(),
            "a non-MINT holder is ungated (no in-flight guard)"
        );
        drop(g);
        assert_eq!(ctl.inflight_count(), 0);

        // Idle: MINT GPU work is REFUSED; the compiler is STILL ungated (must be able to
        // acquire the GPU it idled MINT for).
        let idle_ctl = IdleController::new();
        idle_ctl.enter(manifest("compiler", 1, 2));
        assert!(idle_ctl.is_idle());
        assert!(
            try_admit_gpu_on(&idle_ctl, "intake_coder_sweep", &mints).is_err(),
            "MINT GPU work must be refused while idle"
        );
        assert!(
            try_admit_gpu_on(&idle_ctl, "mint_breakfix", &mints).is_err(),
            "every MINT holder is refused while idle"
        );
        assert!(
            try_admit_gpu_on(&idle_ctl, "bld-05-compiler", &mints)
                .expect("compiler never gated, even while MINT idle")
                .is_none(),
            "the compiler must still be able to acquire the GPU while MINT is idle"
        );
        assert_eq!(idle_ctl.inflight_count(), 0, "no refused work is counted");

        // EnteringIdle (transient): MINT GPU work is likewise refused.
        let entering = IdleController::new();
        assert_eq!(entering.begin_enter(), BeginEnter::Begin);
        assert!(
            try_admit_gpu_on(&entering, "intake_coder_case", &mints).is_err(),
            "MINT GPU work must be refused while EnteringIdle"
        );
    }

    #[tokio::test]
    async fn admitted_mint_unit_is_counted_so_a_concurrent_enter_drains_it_not_zero() {
        // Proves the in-flight counter reflects REAL held MINT work: a concurrent enter's
        // closed-world drain sees a non-zero count until the unit's guard is dropped.
        let mints = mint_labels();
        let ctl = IdleController::new();

        // A MINT unit acquires the GPU (holds the admission guard for the unit).
        let unit = try_admit_gpu_on(&ctl, "intake_coder_sweep", &mints)
            .unwrap()
            .unwrap();
        assert_eq!(ctl.inflight_count(), 1);

        // A concurrent enter would drain this: drain_inflight sees the real unit and does
        // NOT reach zero within its bound while the unit is still held.
        let still = ctl.drain_inflight(Duration::from_millis(30)).await;
        assert_eq!(
            still, 1,
            "drain must see the real in-flight MINT unit (not zero)"
        );

        // Once the unit finishes and releases (guard drops), drain completes.
        drop(unit);
        assert_eq!(ctl.inflight_count(), 0);
        let drained = ctl.drain_inflight(Duration::from_millis(30)).await;
        assert_eq!(drained, 0, "drain completes once the MINT unit releases");
    }

    #[test]
    fn waiter_admitted_while_active_is_refused_at_acquire_if_idle_started_first() {
        // codex cycle-5 TOCTOU, exact sequence. Models LiveGpuLock::acquire's two-step
        // gate — the non-committing peek during the backoff WAIT, and the authoritative
        // atomic admit at the moment the lock is finally won:
        //
        //   (1) Sweep B is Active and would wait for the GPU (Sweep A holds it). During
        //       the wait it only PEEKS admission (no guard), so it does NOT inflate the
        //       drain count.
        //   (2) The compiler calls enter_idle → phase flips to EnteringIdle.
        //   (3) Sweep A releases; Sweep B finally WINS the raw lock.
        //   (4) At that instant Sweep B re-validates admission — which must REFUSE, so no
        //       MINT GPU work begins during idle.
        let mints = mint_labels();
        let ctl = IdleController::new();

        // (1) Waiting-while-Active peek is open, and holds NO in-flight guard.
        assert!(
            mint_gpu_admission_open_on(&ctl, "intake_coder_sweep", &mints),
            "a MINT holder is admissible while Active (so it would wait for the lock)"
        );
        assert_eq!(
            ctl.inflight_count(),
            0,
            "a queued waiter must not hold an in-flight guard (never stalls the drain)"
        );

        // (2) Compiler enters idle: phase → EnteringIdle, new admissions closed.
        assert_eq!(ctl.begin_enter(), BeginEnter::Begin);
        assert_eq!(ctl.phase(), Phase::EnteringIdle);

        // The waiter still holds nothing, so a concurrent enter_idle drain is NOT blocked
        // by it — it can drain to zero immediately.
        assert_eq!(ctl.inflight_count(), 0);

        // The in-loop peek would now abort the wait promptly (non-retryable refusal).
        assert!(
            !mint_gpu_admission_open_on(&ctl, "intake_coder_sweep", &mints),
            "once idling, the waiter's peek must fail so it aborts the wait"
        );

        // (3)+(4) Even if the waiter still wins the raw lock, the authoritative re-check
        // at acquisition REFUSES it — no guard is taken, no GPU work begins.
        assert!(
            try_admit_gpu_on(&ctl, "intake_coder_sweep", &mints).is_err(),
            "a waiter that wins the lock AFTER idle started must be refused at acquire"
        );
        assert_eq!(
            ctl.inflight_count(),
            0,
            "the refused waiter takes no guard — the drain count stays zero"
        );

        // The compiler (non-MINT holder) is never gated by any of this.
        assert!(
            mint_gpu_admission_open_on(&ctl, "bld-05-compiler", &mints),
            "the compiler is never gated (peek always open)"
        );
        assert!(try_admit_gpu_on(&ctl, "bld-05-compiler", &mints)
            .expect("compiler never gated")
            .is_none());
    }
}
