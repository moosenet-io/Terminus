//! HFIX-07: proactive GPU-runner authority — operating modes, exclusive-use
//! locking, and idempotent enforcement of the runner config each mode needs.
//!
//! ## Why this exists
//! Two real incidents on <host> drove this:
//! 1. A manual smoke-test run alongside the coder sweep stacked two inference
//!    jobs in VRAM and produced false "wedge" timeouts (see the
//!    `gfx1151-vram-contention` memory) — reactive, discovered after the
//!    fact, blamed on the wrong cause initially.
//! 2. `ollama.service`'s shared `OLLAMA_MAX_LOADED_MODELS=4` /
//!    `OLLAMA_KEEP_ALIVE=2h` let the coder sweep's OWN sequential
//!    model-switching stack up to 4 models in VRAM at once (observed: 3
//!    models, ~33GB, still climbing) — nobody had ever proactively checked
//!    or asserted what ollama's runner config should be FOR a test run before
//!    starting one; a human had to notice `ollama ps` looked wrong mid-run
//!    and fix it by hand with a one-off systemd drop-in.
//!
//! [`crate::intake::lifecycle`] already arbitrates between LAUNCH-based GPU
//! backends (llama-server-style, unit-managed) — but explicitly treats Ollama
//! as "assumed up and managed elsewhere" (`ensure_up`'s first check). This
//! module is the missing authority over Ollama's OWN internal model-loading
//! behavior, plus a genuine exclusive-use lock so two GPU-heavy jobs can
//! never silently overlap again.
//!
//! ## Model
//! - [`GpuMode`] — `Exclusive` (a test/sweep run: one Ollama-resident model
//!   at a time, competing services stopped) or `Shared` (normal production
//!   serving: Ollama's base multi-model config, competing services left
//!   alone).
//! - [`acquire`] — proactively APPLIES the mode's policy (not just checks
//!   it): stops the mode's declared competing services (recording which ones
//!   were actually active, so `release` only restarts those, never services
//!   an operator had already intentionally stopped for an unrelated reason),
//!   and brings Ollama's runner config to the policy's declared state —
//!   idempotently: if it already matches, nothing is touched (no needless
//!   restart/eviction on every resume of an already-exclusive run).
//! - The lock file records the acquiring PID. A holder whose process has
//!   since died is treated as abandoned (self-healing) rather than wedging
//!   every future acquire forever — a crashed sweep must not permanently
//!   lock the GPU.
//! - `release` restores exactly the services THIS acquire stopped; it does
//!   NOT revert Ollama's runner config (successive exclusive-mode tools —
//!   e.g. a sweep run followed by an ad hoc case rerun — should not bounce
//!   Ollama, and evicting every resident model, between each other). Use
//!   [`acquire`] with [`GpuMode::Shared`] to explicitly hand the GPU back to
//!   production serving.

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// An operating mode for the GPU/runner stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMode {
    /// A test/sweep run: exactly one Ollama-resident model at a time,
    /// competing production services stopped.
    Exclusive,
    /// Normal production serving: Ollama's base multi-model config, no
    /// services touched.
    Shared,
}

impl GpuMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GpuMode::Exclusive => "exclusive",
            GpuMode::Shared => "shared",
        }
    }
}

/// The runner config a [`GpuMode`] requires. Pure data — the load-bearing
/// values are declared here, once, rather than as one-off shell commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModePolicy {
    /// Desired `OLLAMA_MAX_LOADED_MODELS`. `None` ⇒ don't manage Ollama's
    /// drop-in at all for this mode (leave whatever is currently in place).
    pub ollama_max_loaded_models: Option<u32>,
    /// Services to stop while this mode is held (only ones found ACTIVE at
    /// acquire time are recorded for restart on release).
    pub stop_services: Vec<String>,
}

/// The policy for a given mode. Pure — the ONE place these values are
/// declared; everything else reads through here.
pub fn policy_for(mode: GpuMode) -> ModePolicy {
    match mode {
        GpuMode::Exclusive => ModePolicy {
            ollama_max_loaded_models: Some(1),
            stop_services: vec!["chord.service".to_string(), "lemonade-coder.service".to_string()],
        },
        GpuMode::Shared => ModePolicy {
            // `None`: reverting to "shared" means REMOVING the exclusive-mode
            // drop-in (falling back to the base unit's own value), not
            // asserting a specific number here — the base config is the
            // single source of truth for production tuning.
            ollama_max_loaded_models: None,
            stop_services: vec![],
        },
    }
}

/// Where the exclusive-use lock lives. `/run` is tmpfs — cleared on reboot,
/// which is correct: a reboot releases every lock, there is nothing to
/// resume.
fn lock_path() -> &'static Path {
    Path::new("/run/gpu-authority.lock")
}

/// The Ollama drop-in this module manages. Filename matters: it must sort
/// AFTER `override.conf` (the base production tuning drop-in) so THIS file's
/// `Environment=` wins when both set the same key — systemd drop-ins merge
/// in lexical filename order, later wins per-key. (Found the hard way: an
/// earlier `99-...` name sorted BEFORE `override.conf` and was silently
/// overridden by it.)
fn ollama_dropin_path() -> &'static Path {
    Path::new("/etc/systemd/system/ollama.service.d/zz-gpu-authority-exclusive.conf")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockState {
    holder: String,
    mode: String,
    pid: u32,
    acquired_at: u64,
    /// Services this acquire actually stopped (were active beforehand) —
    /// `release` restarts exactly these, never a service that was already
    /// stopped for an unrelated operator reason.
    stopped_services: Vec<String>,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_lock() -> Option<LockState> {
    let raw = std::fs::read_to_string(lock_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_lock(state: &LockState) -> Result<(), String> {
    let raw = serde_json::to_string(state).map_err(|e| format!("serialize lock: {e}"))?;
    std::fs::write(lock_path(), raw).map_err(|e| format!("write lock: {e}"))
}

fn clear_lock() {
    let _ = std::fs::remove_file(lock_path());
}

/// Is `pid` a live process? Pure wrapper over `/proc/<pid>` — no `kill -0`
/// needed (would require CAP_SYS_PTRACE-style checks for other users; `/proc`
/// existing is sufficient and matches how this binary already runs as root).
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Decide whether an existing lock blocks a NEW acquire by `holder`. Pure —
/// the actual PID-liveness/self-lock exceptions, separated from the file IO
/// so the decision logic is unit-testable without `/proc` or `/run`.
///
/// - No lock ⇒ not blocked.
/// - Same holder ⇒ not blocked (idempotent re-acquire, e.g. a resumed run).
/// - Different holder, but its recorded PID is no longer alive ⇒ not blocked
///   (abandoned lock from a crashed process — self-healing, a dead process
///   must never wedge the GPU forever).
/// - Different holder, PID alive ⇒ blocked.
fn is_blocked(existing: &LockState, holder: &str, pid_is_alive: bool) -> bool {
    existing.holder != holder && pid_is_alive
}

/// Current process id. Thin wrapper (not pure) so [`is_blocked`] itself stays
/// pure/testable.
fn my_pid() -> u32 {
    std::process::id()
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("spawn {cmd}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{cmd} {} exited {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn systemctl_is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", unit])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The exact drop-in content for a given `max_loaded_models` value. Pure —
/// separated so the idempotency check (does the on-disk file already say
/// this?) doesn't need any IO to compute what it's comparing against.
fn dropin_content(max_loaded_models: u32) -> String {
    format!(
        "# Managed by gpu_authority (HFIX-07) — do not hand-edit.\n\
         # Exclusive-mode override: one Ollama-resident model at a time, so a\n\
         # test/sweep run's own sequential model-switching cannot stack models\n\
         # in VRAM past the point a human notices `ollama ps` looks wrong.\n\
         [Service]\n\
         Environment=OLLAMA_MAX_LOADED_MODELS={max_loaded_models}\n"
    )
}

/// Does the on-disk drop-in already match `max_loaded_models`? `false` if
/// the file doesn't exist or content differs — either way, acquire needs to
/// (re)write it and restart Ollama.
fn dropin_matches(max_loaded_models: u32) -> bool {
    std::fs::read_to_string(ollama_dropin_path())
        .map(|existing| existing == dropin_content(max_loaded_models))
        .unwrap_or(false)
}

/// Apply (or remove) the Ollama drop-in for `desired` and reconcile the
/// service — IDEMPOTENT: if the on-disk state already matches, nothing is
/// touched (no needless restart/eviction of an already-correct exclusive
/// run). Only actually restarts Ollama when the config genuinely changes.
fn reconcile_ollama(desired: Option<u32>) -> Result<(), String> {
    match desired {
        Some(n) => {
            if dropin_matches(n) {
                return Ok(()); // already correct — do not touch a live Ollama
            }
            let dir = ollama_dropin_path()
                .parent()
                .ok_or("drop-in path has no parent dir")?;
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
            std::fs::write(ollama_dropin_path(), dropin_content(n))
                .map_err(|e| format!("write drop-in: {e}"))?;
        }
        None => {
            // Shared mode: remove the drop-in entirely if present (fall back
            // to the base unit's own tuning). Idempotent — fine if absent.
            if !ollama_dropin_path().exists() {
                return Ok(());
            }
            std::fs::remove_file(ollama_dropin_path())
                .map_err(|e| format!("remove drop-in: {e}"))?;
        }
    }
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["restart", "ollama"])
}

/// Acquire `mode` on behalf of `holder` (a short label, e.g. the binary
/// name). Proactively APPLIES the mode's policy — stops the mode's declared
/// competing services (recording which were actually active) and reconciles
/// Ollama's runner config — rather than merely checking it, so a test never
/// has to discover contention after the fact.
///
/// `Err` when a DIFFERENT, still-alive holder already has the lock — never
/// silently races another exclusive user for the GPU.
pub fn acquire(mode: GpuMode, holder: &str) -> Result<(), String> {
    if let Some(existing) = read_lock() {
        if is_blocked(&existing, holder, pid_alive(existing.pid)) {
            return Err(format!(
                "GPU is held exclusively by '{}' (pid {}, mode {}, since epoch {}) — refusing to acquire for '{holder}'",
                existing.holder, existing.pid, existing.mode, existing.acquired_at
            ));
        }
    }

    let policy = policy_for(mode);

    let mut stopped = Vec::new();
    for svc in &policy.stop_services {
        if systemctl_is_active(svc) {
            run("systemctl", &["stop", svc])?;
            stopped.push(svc.clone());
        }
    }

    if let Some(n) = policy.ollama_max_loaded_models {
        reconcile_ollama(Some(n))?;
    }

    write_lock(&LockState {
        holder: holder.to_string(),
        mode: mode.as_str().to_string(),
        pid: my_pid(),
        acquired_at: now_epoch(),
        stopped_services: stopped,
    })
}

/// Release `holder`'s lock, restarting exactly the services THIS acquire
/// stopped. Does NOT touch Ollama's runner config (see module doc) — call
/// [`acquire`] with [`GpuMode::Shared`] to explicitly hand the GPU back to
/// production serving.
///
/// `Ok` (no-op) if no lock exists. `Err` if a DIFFERENT holder currently owns
/// it — a release can never clear someone else's lock.
pub fn release(holder: &str) -> Result<(), String> {
    let Some(existing) = read_lock() else {
        return Ok(());
    };
    if existing.holder != holder {
        return Err(format!(
            "lock is held by '{}', not '{holder}' — refusing to release someone else's lock",
            existing.holder
        ));
    }
    for svc in &existing.stopped_services {
        let _ = run("systemctl", &["start", svc]);
    }
    clear_lock();
    Ok(())
}

/// A point-in-time snapshot for `gpu_mode status` / a pre-flight check.
#[derive(Debug, Clone)]
pub struct GpuStatus {
    pub lock: Option<(String, String, u32, bool)>, // (holder, mode, pid, pid_alive)
    pub ollama_dropin_present: bool,
}

/// Proactive status query — no side effects. A test harness (or an
/// operator) can call this BEFORE deciding to acquire, to see what it would
/// be contending with.
pub fn status() -> GpuStatus {
    let lock = read_lock().map(|l| {
        let alive = pid_alive(l.pid);
        (l.holder, l.mode, l.pid, alive)
    });
    GpuStatus {
        lock,
        ollama_dropin_present: ollama_dropin_path().exists(),
    }
}

/// RAII guard: acquires on construction (via [`acquire`]), releases on drop.
/// A crashed/panicking holder still releases (unless the process is killed
/// with SIGKILL, in which case the PID-liveness check in a future `acquire`
/// self-heals the abandoned lock).
pub struct ExclusiveGuard {
    holder: String,
}

impl ExclusiveGuard {
    pub fn acquire(mode: GpuMode, holder: &str) -> Result<Self, String> {
        acquire(mode, holder)?;
        Ok(ExclusiveGuard { holder: holder.to_string() })
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        if let Err(e) = release(&self.holder) {
            tracing::warn!("gpu_authority: release on drop failed for '{}': {e}", self.holder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_exclusive_forces_single_model_and_stops_competitors() {
        let p = policy_for(GpuMode::Exclusive);
        assert_eq!(p.ollama_max_loaded_models, Some(1));
        assert!(p.stop_services.contains(&"chord.service".to_string()));
        assert!(p.stop_services.contains(&"lemonade-coder.service".to_string()));
    }

    #[test]
    fn policy_shared_touches_nothing() {
        let p = policy_for(GpuMode::Shared);
        assert_eq!(p.ollama_max_loaded_models, None);
        assert!(p.stop_services.is_empty());
    }

    #[test]
    fn is_blocked_same_holder_never_blocks() {
        let existing = LockState {
            holder: "sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec![],
        };
        // Even if (hypothetically) alive, the SAME holder re-acquiring is fine.
        assert!(!is_blocked(&existing, "sweep", true));
    }

    #[test]
    fn is_blocked_different_holder_alive_pid_blocks() {
        let existing = LockState {
            holder: "sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec![],
        };
        assert!(is_blocked(&existing, "case-rerun", true));
    }

    #[test]
    fn is_blocked_different_holder_dead_pid_self_heals() {
        // A crashed holder must never wedge the GPU forever.
        let existing = LockState {
            holder: "sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec![],
        };
        assert!(!is_blocked(&existing, "case-rerun", false));
    }

    #[test]
    fn dropin_content_is_deterministic_and_shows_the_value() {
        let a = dropin_content(1);
        let b = dropin_content(1);
        assert_eq!(a, b);
        assert!(a.contains("OLLAMA_MAX_LOADED_MODELS=1"));
        assert!(dropin_content(4).contains("OLLAMA_MAX_LOADED_MODELS=4"));
        assert_ne!(dropin_content(1), dropin_content(4));
    }

    #[test]
    fn gpu_mode_as_str_matches_lock_state_convention() {
        assert_eq!(GpuMode::Exclusive.as_str(), "exclusive");
        assert_eq!(GpuMode::Shared.as_str(), "shared");
    }
}
