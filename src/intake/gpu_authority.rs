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
    /// Services to stop via `systemctl` while this mode is held (only ones found
    /// ACTIVE at acquire time are recorded for restart on release).
    ///
    /// NOTE: `chord.service` is DELIBERATELY NOT here anymore. Chord is the
    /// always-on fleet backbone; stopping it left it `inactive (dead)` for a
    /// full multi-day sweep. Chord now yields the GPU via its own
    /// `/v1/gpu-exclusive/{acquire,release}` HTTP API (see
    /// [`notify_chord_exclusive`]) — it STAYS UP and only gates its inference
    /// paths. `lemonade-coder.service` (a simple llama-server serving process
    /// with no stay-alive/coordinate requirement) keeps the systemctl mechanism.
    pub stop_services: Vec<String>,
    /// Whether to take Chord's GPU via its HTTP acquire/release API (instead of
    /// killing `chord.service`). `true` for [`GpuMode::Exclusive`]. On acquire
    /// this POSTs `/v1/gpu-exclusive/acquire` and starts a background heartbeat;
    /// on release it stops the heartbeat and POSTs `/v1/gpu-exclusive/release`.
    pub notify_chord_exclusive: bool,
}

/// The policy for a given mode. Pure — the ONE place these values are
/// declared; everything else reads through here.
pub fn policy_for(mode: GpuMode) -> ModePolicy {
    match mode {
        GpuMode::Exclusive => ModePolicy {
            ollama_max_loaded_models: Some(1),
            // chord is handled over HTTP (notify_chord_exclusive), NOT stopped.
            stop_services: vec!["lemonade-coder.service".to_string()],
            notify_chord_exclusive: true,
        },
        GpuMode::Shared => ModePolicy {
            // `None`: reverting to "shared" means REMOVING the exclusive-mode
            // drop-in (falling back to the base unit's own value), not
            // asserting a specific number here — the base config is the
            // single source of truth for production tuning.
            ollama_max_loaded_models: None,
            stop_services: vec![],
            notify_chord_exclusive: false,
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
    /// Whether THIS acquire successfully took Chord's GPU over its
    /// `/v1/gpu-exclusive/acquire` HTTP API (and started a heartbeat). `release`
    /// posts `/v1/gpu-exclusive/release` (and stops the heartbeat) iff this is
    /// true — so a run where Chord was unreachable (nothing acquired) never
    /// tries to release a lock it never took. `#[serde(default)]` so a lock file
    /// written by a pre-this-change binary still deserializes (as `false`).
    #[serde(default)]
    chord_notified: bool,
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

/// Decide whether an existing lock blocks a NEW acquire by `holder` running
/// as `current_pid`. Pure — the actual PID-liveness/self-lock exceptions,
/// separated from the file IO so the decision logic is unit-testable
/// without `/proc` or `/run`.
///
/// - No lock (caller doesn't even get here) / recorded PID no longer alive
///   ⇒ not blocked (abandoned lock from a crashed process — self-healing, a
///   dead process must never wedge the GPU forever). Holder string is
///   irrelevant here: a dead PID's lock is up for grabs by anyone.
/// - PID alive AND it's a genuine same-process reentrant acquire (see
///   [`is_idempotent_reacquire`]) ⇒ not blocked.
/// - PID alive, otherwise (different process — REGARDLESS of whether the
///   holder STRING happens to match) ⇒ blocked. This is the PID-aware fix:
///   two distinct OS processes that happen to share a holder label (e.g. two
///   overlapping `intake_coder_sweep` invocations during a systemd restart
///   window) must never be treated as the same reentrant caller just because
///   the label matches.
fn is_blocked(existing: &LockState, holder: &str, pid_is_alive: bool, current_pid: u32) -> bool {
    pid_is_alive && !is_idempotent_reacquire(existing, holder, current_pid)
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

/// Whether a NEW acquire by `holder`, running as `current_pid`, given an
/// EXISTING lock, should be a pure no-op — the lock is already held BY THIS
/// EXACT PROCESS under this exact holder label, so there is nothing to
/// (re-)apply.
///
/// This is what makes nested acquisition SAFE (MINT Phase 2 item 7): `mint`'s
/// dispatcher pre-acquires under the SAME holder label a subcommand's own
/// library function acquires under internally (e.g. `"intake_coder_sweep"`),
/// so the subcommand's own `ExclusiveGuard::acquire` call — running in the
/// SAME OS process — sees "already held by me" and takes this no-op path
/// instead of re-deriving a FRESH `stopped_services` list (which would come
/// back empty — the services are already stopped from the outer acquire —
/// and silently overwrite/lose the record of what the outer acquire actually
/// stopped, so `release` would restart nothing).
///
/// Requires BOTH the holder string AND the recorded PID to match. Matching
/// only the holder string is NOT sufficient: two genuinely different OS
/// processes can share a holder label (e.g. two overlapping
/// `intake_coder_sweep` invocations in a narrow systemd-restart window), and
/// that case must be BLOCKED (if the first is still alive) rather than
/// silently treated as a no-op that skips stopping services / reconciling
/// Ollama for the second process and corrupts whose `stopped_services` list
/// `release` will use. A DIFFERENT holder (or the same holder but a
/// different, dead PID) is NOT this case — that's a takeover, which must
/// still go through the full apply path below (stop services, reconcile
/// Ollama, write a fresh `LockState`).
fn is_idempotent_reacquire(existing: &LockState, holder: &str, current_pid: u32) -> bool {
    existing.holder == holder && existing.pid == current_pid
}

// ── Chord GPU-exclusive HTTP coordination ────────────────────────────────────
//
// Instead of `systemctl stop chord.service` (which left the fleet backbone dead
// for days), Chord yields the GPU via its authenticated
// `/v1/gpu-exclusive/{acquire,release}` endpoints and STAYS UP. Chord auto-clears
// an abandoned lock via a wall-clock TTL, so this side must HEARTBEAT (re-acquire)
// periodically to hold the GPU across a long sweep; a crashed sweep simply stops
// heartbeating and Chord resumes serving on its own.

/// Base URL of Chord's proxy port (where the gpu-exclusive endpoints live).
/// Prefers `CHORD_GPU_EXCLUSIVE_URL`, then `CHORD_PROXY_URL`, else the local
/// loopback default (co-located with Chord on the same host). Loopback default,
/// not an infra literal — same convention as `sysversion::chord_control_base`.
fn chord_base_url() -> String {
    for key in ["CHORD_GPU_EXCLUSIVE_URL", "CHORD_PROXY_URL"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return v.trim_end_matches('/').to_string();
            }
        }
    }
    "http://127.0.0.1:8099".to_string()
}

/// Optional bearer token for Chord's JWT auth (`CHORD_JWT`). When Chord runs
/// with `CHORD_JWT_SECRET` set, the harness host must supply a valid
/// lumina-subject token here or acquire returns 401. When Chord's secret is
/// empty (single-tenant dev/bench posture) no token is needed.
fn chord_auth_token() -> Option<String> {
    std::env::var("CHORD_JWT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Heartbeat interval (seconds) — how often to re-`acquire` (refresh) Chord's
/// lock while held. From `CHORD_GPU_EXCLUSIVE_HEARTBEAT_SECS`, default 120.
/// Must be comfortably under Chord's TTL (default 600s) so a brief stall never
/// lets the lock expire mid-sweep.
fn chord_heartbeat_secs() -> u64 {
    std::env::var("CHORD_GPU_EXCLUSIVE_HEARTBEAT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(120)
}

/// Outcome of a single Chord gpu-exclusive HTTP call. Split from interpretation
/// so the decision logic ([`interpret_chord_acquire`]) is unit-testable without
/// a network.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChordCall {
    /// 2xx — Chord acknowledged (granted / refreshed / released).
    Acknowledged,
    /// Transport failure (connection refused/timeout) — Chord is not serving, so
    /// there is nothing contending for the GPU; safe to proceed without it.
    Unreachable,
    /// 409 — Chord reports the GPU is already held by a DIFFERENT holder.
    Held { holder: String },
    /// 401/403 — auth rejected (missing/invalid `CHORD_JWT`).
    Unauthorized,
    /// Any other non-2xx / unexpected condition.
    Failed(String),
}

/// The path segment under `/v1/gpu-exclusive/`.
fn chord_url(base: &str, action: &str) -> String {
    format!("{}/v1/gpu-exclusive/{action}", base.trim_end_matches('/'))
}

/// Perform one gpu-exclusive HTTP call synchronously, safe to call from BOTH a
/// sync `Drop` and inside a running tokio runtime: the async reqwest call runs
/// on a fresh current-thread runtime in a dedicated OS thread, so there is never
/// a "runtime within a runtime" panic. Never itself panics — a thread/runtime
/// failure degrades to [`ChordCall::Failed`].
fn chord_call(action: &str, holder: &str) -> ChordCall {
    let url = chord_url(&chord_base_url(), action);
    let token = chord_auth_token();
    let holder = holder.to_string();
    std::thread::scope(|s| {
        s.spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => return ChordCall::Failed(format!("runtime build: {e}")),
            };
            rt.block_on(async move {
                let client = match reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => return ChordCall::Failed(format!("client build: {e}")),
                };
                let mut req = client
                    .post(&url)
                    .json(&serde_json::json!({ "holder": holder }));
                if let Some(t) = &token {
                    req = req.header("authorization", format!("Bearer {t}"));
                }
                match req.send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            ChordCall::Acknowledged
                        } else if status.as_u16() == 409 {
                            // Body carries the current holder; best-effort parse.
                            let holder = resp
                                .json::<serde_json::Value>()
                                .await
                                .ok()
                                .and_then(|v| {
                                    v.get("holder").and_then(|h| h.as_str()).map(String::from)
                                })
                                .unwrap_or_else(|| "unknown".to_string());
                            ChordCall::Held { holder }
                        } else if matches!(status.as_u16(), 401 | 403) {
                            ChordCall::Unauthorized
                        } else {
                            ChordCall::Failed(format!("HTTP {}", status.as_u16()))
                        }
                    }
                    // Any transport-level error ⇒ treat Chord as not serving.
                    Err(_) => ChordCall::Unreachable,
                }
            })
        })
        .join()
        .unwrap_or_else(|_| ChordCall::Failed("chord call thread panicked".into()))
    })
}

/// Pure: map an acquire-time [`ChordCall`] to "did we take Chord's GPU?"
/// (`Ok(chord_notified)`) or a hard refusal (`Err`). Separated from the HTTP so
/// the policy is unit-testable.
///
/// - `Acknowledged` ⇒ `Ok(true)`  — Chord yielded; we must heartbeat + release.
/// - `Unreachable`  ⇒ `Ok(false)` — Chord isn't up; nothing to coordinate, but
///   the sweep can still run (matches the old `systemctl_is_active`-gated
///   "only stop it if it's actually running" behaviour).
/// - `Held`         ⇒ `Err`       — someone else already holds Chord's GPU;
///   never silently race (same discipline as the local-lock block).
/// - `Unauthorized` ⇒ `Err`       — misconfig; fail loudly with the fix.
/// - `Failed`       ⇒ `Err`.
fn interpret_chord_acquire(outcome: ChordCall) -> Result<bool, String> {
    match outcome {
        ChordCall::Acknowledged => Ok(true),
        ChordCall::Unreachable => Ok(false),
        ChordCall::Held { holder } => Err(format!(
            "chord reports the GPU is already held exclusively by '{holder}' — refusing to start"
        )),
        ChordCall::Unauthorized => Err(
            "chord rejected the GPU-exclusive acquire (401/403) — set CHORD_JWT to a valid \
             lumina token for this harness host"
                .to_string(),
        ),
        ChordCall::Failed(e) => Err(format!("chord GPU-exclusive acquire failed: {e}")),
    }
}

/// A running heartbeat: a stop flag + the thread refreshing Chord's lock.
struct ChordHeartbeat {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

/// Process-global heartbeat handle. There is one GPU / one exclusive lock per
/// process, so one heartbeat. Managed by [`start_chord_heartbeat`] /
/// [`stop_chord_heartbeat`] (called from `acquire`/`release`), NOT by the guard
/// struct — so a nested-guard release (see `is_idempotent_reacquire`) that runs
/// on whichever guard drops first still tears the heartbeat down correctly.
static CHORD_HEARTBEAT: std::sync::Mutex<Option<ChordHeartbeat>> =
    std::sync::Mutex::new(None);

/// Start (or restart) the background heartbeat that re-`acquire`s Chord's lock
/// every [`chord_heartbeat_secs`] so it never hits Chord's abandoned-lock TTL
/// while the sweep is running.
fn start_chord_heartbeat(holder: &str) {
    stop_chord_heartbeat(); // never stack two heartbeats
    let holder = holder.to_string();
    let interval = chord_heartbeat_secs();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        loop {
            // Sleep the interval in 1s slices so a release() tears the heartbeat
            // down promptly rather than after a full interval.
            for _ in 0..interval {
                if stop_thread.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if stop_thread.load(Ordering::Relaxed) {
                return;
            }
            match chord_call("acquire", &holder) {
                ChordCall::Acknowledged => {}
                other => tracing::warn!(
                    "gpu_authority: chord heartbeat re-acquire returned {other:?} (will retry next interval)"
                ),
            }
        }
    });
    if let Ok(mut guard) = CHORD_HEARTBEAT.lock() {
        *guard = Some(ChordHeartbeat { stop, handle });
    }
}

/// Stop the background heartbeat (if any), joining its thread.
fn stop_chord_heartbeat() {
    let hb = CHORD_HEARTBEAT.lock().ok().and_then(|mut g| g.take());
    if let Some(hb) = hb {
        hb.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = hb.handle.join();
    }
}

/// Acquire `mode` on behalf of `holder` (a short label, e.g. the binary
/// name). Proactively APPLIES the mode's policy — stops the mode's declared
/// competing services (recording which were actually active) and reconciles
/// Ollama's runner config — rather than merely checking it, so a test never
/// has to discover contention after the fact.
///
/// `Err` when a DIFFERENT process still alive already has the lock — never
/// silently races another exclusive user for the GPU. This is now
/// PID-aware, not just holder-string-aware: a re-acquire is a noop ONLY when
/// it's the SAME OS process re-acquiring under the SAME holder label (see
/// [`is_idempotent_reacquire`]) — this is what makes a nested acquire (e.g.
/// `mint`'s dispatcher, then the subcommand's own internal acquire, under the
/// identical holder label, in the SAME process) safe rather than a
/// lost-state footgun. A DIFFERENT process that happens to use the same
/// holder LABEL (e.g. two overlapping invocations of the same binary in a
/// systemd-restart window) is blocked like any other contender, never
/// treated as idempotent just because the string matches.
pub fn acquire(mode: GpuMode, holder: &str) -> Result<(), String> {
    if let Some(existing) = read_lock() {
        let alive = pid_alive(existing.pid);
        let this_pid = my_pid();
        if is_blocked(&existing, holder, alive, this_pid) {
            return Err(format!(
                "GPU is held exclusively by '{}' (pid {}, mode {}, since epoch {}) — refusing to acquire for '{holder}'",
                existing.holder, existing.pid, existing.mode, existing.acquired_at
            ));
        }
        if is_idempotent_reacquire(&existing, holder, this_pid) {
            return Ok(());
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

    // Take Chord's GPU over HTTP (instead of killing chord.service). A refusal
    // (409/auth/failure) aborts the acquire — the caller sees the error and
    // does not start the sweep. Chord being unreachable is NOT an error
    // (nothing is contending for the GPU).
    //
    // On a refusal we must restore any services already stopped above
    // (`stopped`) before returning — otherwise a rejected acquire leaves
    // e.g. lemonade-coder.service down with no caller in a position to know
    // it needs restarting (the caller sees `Err`, i.e. "you never acquired,"
    // not "acquire partially ran and left the host mutated"). Mirrors the
    // exact restart step `release()` performs for a successful acquire's
    // `stopped_services`.
    let mut chord_notified = false;
    if policy.notify_chord_exclusive {
        chord_notified = match interpret_chord_acquire(chord_call("acquire", holder)) {
            Ok(v) => v,
            Err(e) => {
                for svc in &stopped {
                    let _ = run("systemctl", &["start", svc]);
                }
                return Err(e);
            }
        };
        if !chord_notified {
            tracing::warn!(
                "gpu_authority: chord unreachable at acquire — proceeding without it (it is not \
                 serving, so nothing is contending for the GPU)"
            );
        }
    }

    // Write the lock BEFORE starting the heartbeat: if `write_lock` itself
    // fails, the caller's `Err` must mean "no lock, no heartbeat, no
    // ongoing side effects" — starting the heartbeat first would otherwise
    // leave an orphaned background thread heartbeating Chord's lock for a
    // holder that, per this function's return value, never acquired it.
    if let Err(e) = write_lock(&LockState {
        holder: holder.to_string(),
        mode: mode.as_str().to_string(),
        pid: my_pid(),
        acquired_at: now_epoch(),
        stopped_services: stopped.clone(),
        chord_notified,
    }) {
        // Full rollback: Chord's remote lock (if we just took it) and any
        // services stopped above must not outlive this failed acquire —
        // otherwise Chord sits exclusively held (until its own TTL expiry)
        // and/or lemonade-coder stays down for a holder that never actually
        // acquired anything locally.
        if chord_notified {
            let _ = chord_call("release", holder);
        }
        for svc in &stopped {
            let _ = run("systemctl", &["start", svc]);
        }
        return Err(e);
    }

    if chord_notified {
        start_chord_heartbeat(holder);
    }

    Ok(())
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
    // Hand Chord's GPU back FIRST (stop the heartbeat, then release) — only if
    // THIS acquire actually took it. Best-effort: a release must never fail just
    // because Chord is momentarily unreachable (it will TTL-expire the lock on
    // its own). Order matters: stop the heartbeat before releasing so a
    // heartbeat re-acquire can't race in after the release.
    if existing.chord_notified {
        stop_chord_heartbeat();
        match chord_call("release", holder) {
            ChordCall::Acknowledged | ChordCall::Unreachable => {}
            other => tracing::warn!(
                "gpu_authority: chord GPU-exclusive release returned {other:?} (lock will TTL-expire on Chord's side)"
            ),
        }
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
    fn policy_exclusive_forces_single_model_and_handles_chord_over_http() {
        let p = policy_for(GpuMode::Exclusive);
        assert_eq!(p.ollama_max_loaded_models, Some(1));
        // chord is NO LONGER stopped via systemctl — it yields the GPU over HTTP.
        assert!(!p.stop_services.contains(&"chord.service".to_string()));
        assert!(p.notify_chord_exclusive);
        // lemonade-coder keeps the simple systemctl stop/start mechanism.
        assert!(p.stop_services.contains(&"lemonade-coder.service".to_string()));
    }

    #[test]
    fn policy_shared_touches_nothing() {
        let p = policy_for(GpuMode::Shared);
        assert_eq!(p.ollama_max_loaded_models, None);
        assert!(p.stop_services.is_empty());
        assert!(!p.notify_chord_exclusive);
    }

    #[test]
    fn is_blocked_same_holder_same_pid_never_blocks() {
        // Case 2: genuine same-process reentrance (this test's own pid is
        // the recorded pid) — never blocked, regardless of "alive".
        let current = my_pid();
        let existing = LockState {
            holder: "sweep".into(),
            mode: "exclusive".into(),
            pid: current,
            acquired_at: 0,
            stopped_services: vec![],
            chord_notified: false,
        };
        assert!(!is_blocked(&existing, "sweep", true, current));
    }

    #[test]
    fn is_blocked_different_holder_alive_pid_blocks() {
        let current = my_pid();
        let existing = LockState {
            holder: "sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec![],
            chord_notified: false,
        };
        assert!(is_blocked(&existing, "case-rerun", true, current));
    }

    #[test]
    fn is_blocked_different_holder_dead_pid_self_heals() {
        // A crashed holder must never wedge the GPU forever.
        let current = my_pid();
        let existing = LockState {
            holder: "sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec![],
            chord_notified: false,
        };
        assert!(!is_blocked(&existing, "case-rerun", false, current));
    }

    #[test]
    fn is_blocked_same_holder_string_different_alive_pid_still_blocks() {
        // Case 3 (the gap this fix closes): a genuinely DIFFERENT, alive OS
        // process using the SAME holder label (e.g. two overlapping
        // `intake_coder_sweep` invocations that both happen to land in a
        // narrow systemd-restart window) must be BLOCKED, not waved through
        // as an idempotent reacquire just because the holder STRING matches.
        //
        // PID 1 (init) is guaranteed to exist/be alive on any running
        // system or container, and is never this test process's own pid —
        // same technique as the dead/alive-PID tests above, but backed by a
        // real `/proc` check instead of a hardcoded bool.
        let current = my_pid();
        assert_ne!(current, 1, "test process must not itself be pid 1");
        assert!(pid_alive(1), "pid 1 (init) must be alive for this test to be meaningful");

        let existing = LockState {
            holder: "intake_coder_sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec!["lemonade-coder.service".to_string()],
            chord_notified: false,
        };

        assert!(!is_idempotent_reacquire(&existing, "intake_coder_sweep", current));
        assert!(is_blocked(&existing, "intake_coder_sweep", pid_alive(existing.pid), current));
    }

    // ---- is_idempotent_reacquire (Phase 2 item 7 + PID-aware fix) ----

    #[test]
    fn is_idempotent_reacquire_same_holder_same_pid_is_a_noop() {
        // Case 2: genuine same-process reentrance.
        let current = my_pid();
        let existing = LockState {
            holder: "intake_coder_sweep".into(),
            mode: "exclusive".into(),
            pid: current,
            acquired_at: 0,
            stopped_services: vec!["lemonade-coder.service".to_string()],
            chord_notified: false,
        };
        assert!(is_idempotent_reacquire(&existing, "intake_coder_sweep", current));
    }

    #[test]
    fn is_idempotent_reacquire_different_holder_is_not_a_noop() {
        // A DIFFERENT holder (even one whose PID has since died, the
        // self-heal takeover case) must still go through the full apply
        // path — it is taking over the lock, not resuming its own.
        let current = my_pid();
        let existing = LockState {
            holder: "intake_coder_sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec!["lemonade-coder.service".to_string()],
            chord_notified: false,
        };
        assert!(!is_idempotent_reacquire(&existing, "intake_coder_case", current));
    }

    #[test]
    fn is_idempotent_reacquire_same_holder_different_pid_is_not_a_noop() {
        // Case 3 (the gap this fix closes): same holder STRING, but the
        // recorded pid belongs to a different process than the one calling
        // now — must NOT be treated as idempotent.
        let current = my_pid();
        let existing = LockState {
            holder: "intake_coder_sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec!["lemonade-coder.service".to_string()],
            chord_notified: false,
        };
        assert_ne!(existing.pid, current);
        assert!(!is_idempotent_reacquire(&existing, "intake_coder_sweep", current));
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

    // ── Chord GPU-exclusive HTTP coordination ────────────────────────────────

    #[test]
    fn chord_url_builds_the_v1_gpu_exclusive_path() {
        assert_eq!(
            chord_url("http://127.0.0.1:8099", "acquire"),
            "http://127.0.0.1:8099/v1/gpu-exclusive/acquire"
        );
        // Trailing slash on the base is normalized away (no double slash).
        assert_eq!(
            chord_url("http://host:8099/", "release"),
            "http://host:8099/v1/gpu-exclusive/release"
        );
    }

    #[test]
    fn interpret_chord_acquire_acknowledged_notifies() {
        assert_eq!(interpret_chord_acquire(ChordCall::Acknowledged), Ok(true));
    }

    #[test]
    fn interpret_chord_acquire_unreachable_proceeds_without_chord() {
        // Chord not serving ⇒ nothing contends for the GPU ⇒ proceed, but we did
        // NOT take (and must not later release) a Chord lock.
        assert_eq!(interpret_chord_acquire(ChordCall::Unreachable), Ok(false));
    }

    #[test]
    fn interpret_chord_acquire_held_refuses() {
        let r = interpret_chord_acquire(ChordCall::Held {
            holder: "someone_else".into(),
        });
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("someone_else"));
    }

    #[test]
    fn interpret_chord_acquire_unauthorized_refuses_with_fix() {
        let r = interpret_chord_acquire(ChordCall::Unauthorized);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("CHORD_JWT"));
    }

    #[test]
    fn interpret_chord_acquire_failed_refuses() {
        assert!(interpret_chord_acquire(ChordCall::Failed("HTTP 500".into())).is_err());
    }

    #[test]
    fn chord_base_url_prefers_explicit_then_proxy_then_loopback() {
        // Clean slate.
        std::env::remove_var("CHORD_GPU_EXCLUSIVE_URL");
        std::env::remove_var("CHORD_PROXY_URL");
        assert_eq!(chord_base_url(), "http://127.0.0.1:8099");

        std::env::set_var("CHORD_PROXY_URL", "http://proxy:9099/");
        assert_eq!(chord_base_url(), "http://proxy:9099");

        std::env::set_var("CHORD_GPU_EXCLUSIVE_URL", "http://explicit:1234");
        assert_eq!(chord_base_url(), "http://explicit:1234");

        std::env::remove_var("CHORD_GPU_EXCLUSIVE_URL");
        std::env::remove_var("CHORD_PROXY_URL");
    }

    #[test]
    fn chord_heartbeat_secs_defaults_and_overrides() {
        std::env::remove_var("CHORD_GPU_EXCLUSIVE_HEARTBEAT_SECS");
        assert_eq!(chord_heartbeat_secs(), 120);
        std::env::set_var("CHORD_GPU_EXCLUSIVE_HEARTBEAT_SECS", "0");
        assert_eq!(chord_heartbeat_secs(), 120); // zero rejected
        std::env::set_var("CHORD_GPU_EXCLUSIVE_HEARTBEAT_SECS", "45");
        assert_eq!(chord_heartbeat_secs(), 45);
        std::env::remove_var("CHORD_GPU_EXCLUSIVE_HEARTBEAT_SECS");
    }

    #[test]
    fn lock_state_deserializes_without_chord_notified_field() {
        // A lock file written by a pre-this-change binary has no chord_notified
        // key; serde(default) must still parse it (as false) so an in-flight
        // upgrade never wedges on an unparseable lock.
        let legacy = r#"{"holder":"intake_coder_sweep","mode":"exclusive","pid":123,"acquired_at":42,"stopped_services":["lemonade-coder.service"]}"#;
        let parsed: LockState = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.holder, "intake_coder_sweep");
        assert!(!parsed.chord_notified);
    }
}
