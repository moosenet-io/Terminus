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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    ///
    /// `new_grant` mirrors the JSON body's own `new_grant` field (best-effort
    /// parsed; `false` if the body is missing/unparseable — never a false
    /// alarm from a body-parse hiccup). On the INITIAL acquire this is
    /// expected to be `true` (a fresh grant) and is not itself meaningful.
    /// On a HEARTBEAT re-acquire of an ALREADY-held lock, `true` here is an
    /// anomaly: it means Chord's own lock state was reset (restart/crash)
    /// since the last successful heartbeat, so Chord served ungated for
    /// some window between then and now — the exact VRAM-contention gap
    /// this whole mechanism exists to prevent. See `start_chord_heartbeat`
    /// for where this is checked.
    Acknowledged { new_grant: bool },
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
                            // Best-effort parse of `new_grant`; a missing/malformed
                            // body degrades to `false` (never a false alarm from a
                            // parse hiccup — see the variant's doc comment).
                            let new_grant = resp
                                .json::<serde_json::Value>()
                                .await
                                .ok()
                                .and_then(|v| v.get("new_grant").and_then(|g| g.as_bool()))
                                .unwrap_or(false);
                            ChordCall::Acknowledged { new_grant }
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
        // `new_grant` is not meaningful on the initial acquire (a fresh grant
        // is exactly what's expected here) — see `ChordCall::Acknowledged`'s
        // doc comment for why it DOES matter on a heartbeat re-acquire.
        ChordCall::Acknowledged { .. } => Ok(true),
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

/// Fast-retry backoff (seconds) tried on a failed heartbeat before falling
/// back to waiting the full interval — a transient blip (one dropped
/// connection, a GC pause) should not need to wait the full interval and eat
/// into Chord's TTL margin. Three short retries (10s apart, 30s total) is
/// comfortably inside a 120s default interval and a 600s default TTL.
const HEARTBEAT_RETRY_DELAYS_SECS: &[u64] = &[10, 10, 10];

/// Handle one heartbeat call's outcome: log the anomaly if Chord unexpectedly
/// re-granted (see [`ChordCall::Acknowledged`]'s doc comment for why that
/// matters on a heartbeat, unlike on the initial acquire), and fast-retry a
/// failed call a few times (respecting `stop`, so a `release()` mid-retry
/// still tears the heartbeat down promptly) before giving up until the next
/// full interval.
fn handle_heartbeat_outcome(
    outcome: ChordCall,
    holder: &str,
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    match outcome {
        ChordCall::Acknowledged { new_grant: false } => {}
        ChordCall::Acknowledged { new_grant: true } => {
            tracing::error!(
                "gpu_authority: CHORD LOCK GAP DETECTED — heartbeat re-acquire for '{holder}' \
                 came back as a NEW grant, not a refresh of an existing one. This means Chord's \
                 own lock state was reset (restart/crash) since the last successful heartbeat, \
                 and Chord served ungated for some window between then and now — samples \
                 recorded in that window may have contended with Chord for the GPU. Investigate \
                 chord.service's uptime/logs around this time."
            );
        }
        other => {
            tracing::warn!(
                "gpu_authority: chord heartbeat re-acquire returned {other:?} — fast-retrying"
            );
            for delay in HEARTBEAT_RETRY_DELAYS_SECS {
                for _ in 0..*delay {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                match chord_call("acquire", holder) {
                    ChordCall::Acknowledged { new_grant: false } => return,
                    ChordCall::Acknowledged { new_grant: true } => {
                        tracing::error!(
                            "gpu_authority: CHORD LOCK GAP DETECTED — heartbeat re-acquire for \
                             '{holder}' came back as a NEW grant during fast-retry, not a \
                             refresh. Chord's lock state was reset since the last successful \
                             heartbeat; investigate chord.service's uptime/logs around this time."
                        );
                        return;
                    }
                    retry_outcome => tracing::warn!(
                        "gpu_authority: chord heartbeat retry still failing: {retry_outcome:?}"
                    ),
                }
            }
            tracing::warn!(
                "gpu_authority: chord heartbeat re-acquire for '{holder}' still failing after \
                 {} fast retries — will try again next full interval",
                HEARTBEAT_RETRY_DELAYS_SECS.len()
            );
        }
    }
}

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
            handle_heartbeat_outcome(chord_call("acquire", &holder), &holder, &stop_thread);
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
            ChordCall::Acknowledged { .. } | ChordCall::Unreachable => {}
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

// ═══════════════════════════════════════════════════════════════════════════
// Fairness: bounded-backoff acquire retry, shared by every caller of this
// module, and the release-between-units-of-work timing constant.
//
// ## Root cause (S86 GPU-lock starvation)
// `intake_coder_sweep` and `intake_assistant_sweep` both need this module's
// exclusive lock, but for years (well, weeks) each one's `run()` acquired
// ONE [`ExclusiveGuard`] at the very top and held it for its ENTIRE
// multi-hour/multi-day fleet run — by design, because the lock genuinely
// must be held while a case is mid-inference. HFIX-09 gave the assistant
// sweep's *initial* acquire a bounded retry/backoff (see
// [`acquire_with_backoff`]) instead of crash-looping on refusal, which fixed
// the crash-loop but NOT the underlying starvation: whichever sweep is
// already running still holds the lock for its ENTIRE run, so the other
// sweep's backoff loop just waits (quietly, no longer crash-looping) for up
// to its `max_wait` cap and then gives up — for as long as the first sweep
// runs, which for `intake_coder_sweep` is DAYS. Confirmed in production: zero
// `assistant_dimension_score` rows for 2+ days straight for `mem_config =
// 'dynamic_gtt'`, the live run, once `intake_coder_sweep` started.
//
// A one-sided fix (e.g. giving `intake_coder_sweep` the SAME bounded backoff
// on refusal, but still one acquire for its whole run) would not fix
// anything — it would just flip who starves: whichever sweep starts running
// first still holds the lock uninterrupted for its whole run, no matter how
// gracefully the OTHER side backs off while waiting.
//
// ## The fix: release-between-units-of-work
// Both `coder_sweep.rs` and `assistant/runner.rs` now acquire the exclusive
// lock freshly at the START of each unit of work (one (model, backend) pass —
// the natural iteration boundary each fleet loop already has) and RELEASE it
// at the END of that same unit, instead of holding one guard for the whole
// run. Reacquiring after a release goes through the SAME
// [`acquire_with_backoff`] used for the initial acquire, so a unit that
// starts while the other sweep is mid-unit waits (bounded) rather than
// racing/erroring.
//
// ## Avoiding "release-and-immediately-regrab" thrashing
// Releasing and instantly trying to reacquire achieves nothing if the OTHER
// sweep's backoff loop doesn't get a real chance to notice the gap before
// this side grabs it back. The other side, while waiting, only re-checks
// once every [`ACQUIRE_POLL_INTERVAL`] (60s) — so if this side pauses for
// less than that after releasing (the originally-considered "1-2s" pause),
// the worst case is: the other side's last check landed just before this
// side released, so its NEXT check won't happen for up to 60s — comfortably
// longer than a 1-2s gap — and this side will have already reacquired and
// moved on to its next unit before the other side ever sees the lock free.
// That is real, provable starvation-through-thrashing, not a hypothetical:
// see the `alternation_actually_happens_with_the_chosen_pause` /
// `..._starves_the_other_side_with_a_too_short_pause` tests below, which
// simulate the exact timeline and demonstrate both outcomes.
//
// [`INTER_UNIT_RELEASE_PAUSE`] is therefore set to 90s: strictly LONGER than
// [`ACQUIRE_POLL_INTERVAL`] (60s), so ANY sweep that has been waiting for
// even one full poll interval is GUARANTEED to observe the lock free at some
// point during the pause window and win the race — genuine alternation, not
// just a released-then-reacquired lock that never had a real window. The
// extra 30s beyond the bare minimum is safety margin for scheduling jitter
// (tokio/systemd scheduling, the Chord HTTP round trip inside `acquire`).
// ═══════════════════════════════════════════════════════════════════════════

/// How often a caller waiting in [`acquire_with_backoff`] retries a refused
/// acquire. Shared by every caller — the [`INTER_UNIT_RELEASE_PAUSE`] timing
/// argument above only holds if all callers poll at the same cadence.
pub const ACQUIRE_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How often [`acquire_with_backoff`] re-logs progress while still waiting
/// (so a long wait is observable in the log without spamming a line every
/// `ACQUIRE_POLL_INTERVAL`).
pub const ACQUIRE_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// How long to pause AFTER releasing the exclusive lock between units of
/// work, before attempting to reacquire it for the next unit. See the module
/// section doc above ("Avoiding release-and-immediately-regrab thrashing")
/// for the full timing reasoning: this MUST be longer than
/// [`ACQUIRE_POLL_INTERVAL`] for real alternation to happen, not just a
/// released-then-reacquired lock that never gave the other side a real
/// window.
pub const INTER_UNIT_RELEASE_PAUSE: Duration = Duration::from_secs(90);

// ═══════════════════════════════════════════════════════════════════════════
// Max lock-hold safety valve (S86 follow-up)
//
// ## The gap the release-between-units fix left open
// The release-between-units fix above (`INTER_UNIT_RELEASE_PAUSE`,
// `acquire_with_backoff`) releases the exclusive lock at the END of each
// unit of work (one (model, backend) pass for coder-sweep, one model for
// assistant-sweep) — correct in the common case, where a unit finishes in
// bounded time. It silently assumes every unit DOES finish in bounded time.
//
// That assumption breaks for a model with a high transport-error rate.
// `code_v2.rs`'s per-case retry (`TRANSPORT_RETRY_BACKOFF_SECS = [10, 20,
// 40]`) means one failed case costs ~70s of pure retry overhead on top of its
// own inference latency. Observed in production after deploying the
// release-between-units fix: `qwen2.5-coder:32b-instruct` (a long-standing,
// pre-existing ~50%-failure-rate model on this transport-error class) held
// the exclusive lock continuously for 80+ minutes and climbing on a SINGLE
// (model, backend) pass (15 attempts, 8 succeeded — matching the known ~50%
// rate) — with no natural end in sight for potentially hours. Because the
// lock is only released at the END of a unit, `intake_assistant_sweep`'s
// bounded wait (`max_wait`, default 4h) could exhaust ENTIRELY waiting for
// this one pass, without ever getting a turn — defeating the fairness fix's
// intent for any similarly unreliable model.
//
// ## The fix: a hard ceiling on continuous hold time, checked mid-unit
// [`MaxHoldConfig::max_hold`] (via [`max_lock_hold_duration`]) is a SECOND,
// time-based release point, layered ON TOP of the existing end-of-unit
// release — checked after each discrete SUB-unit within a unit (one case for
// coder-sweep, one dimension for assistant-sweep; see `GpuLock::check_max_hold`
// and its call sites in `coder_sweep.rs`'s per-case loop / `assistant/runner.rs`'s
// per-dimension loop). If the CURRENT continuous hold has crossed the
// threshold, [`maybe_release_for_max_hold`] releases the lock, pauses (the
// SAME `INTER_UNIT_RELEASE_PAUSE` the end-of-unit release uses, for the SAME
// alternation-guarantee reasoning above), and reacquires (the SAME bounded
// `acquire_with_backoff`) — then the CALLER resumes the SAME in-progress unit
// exactly where it left off (the next case/dimension in its existing loop;
// nothing is abandoned, restarted, skipped, or duplicated).
//
// ## Why 45 minutes
// - A well-behaved (all-success, zero retries) `(model, backend)` pass over
//   the ~40-case v2 corpus, at real `generate()` latency, is documented (see
//   `code_v2.rs`'s Phase-1 comment) to legitimately take 20-40 minutes END TO
//   END. 45 minutes gives ~12% margin above that documented normal ceiling,
//   so the valve stays SILENT for the overwhelmingly common case — it must
//   not fire on every ordinary model, only on genuinely slow/unreliable ones.
// - The incident this closes held the lock 80+ minutes and climbing with no
//   end in sight. 45 minutes fires well before that point, and well before
//   `intake_assistant_sweep`'s own typical per-model duration, so the other
//   sweep gets a REAL, early window instead of waiting out most of a 4-hour
//   `max_wait` cap.
// - Comfortably above `INTER_UNIT_RELEASE_PAUSE` (90s) and
//   `ACQUIRE_POLL_INTERVAL` (60s) so it can never be confused with, or made
//   to thrash against, the per-unit release cadence — this is a rare safety
//   net for the tail, not a routine cadence for the common case.
// ═══════════════════════════════════════════════════════════════════════════

/// Env var overriding the max lock-hold duration (whole seconds;
/// non-positive/unparsable falls back to the default). ONE shared knob for
/// both sweeps (like `ACQUIRE_POLL_INTERVAL`/`INTER_UNIT_RELEASE_PAUSE` above,
/// not a per-binary env var like the `*_ACQUIRE_MAX_WAIT_SECS` pair) — this is
/// a property of the shared exclusive-lock module, not something the two
/// sweeps have any reason to tune differently.
pub const MAX_LOCK_HOLD_ENV: &str = "INTAKE_GPU_MAX_LOCK_HOLD_SECS";

/// Default max continuous hold before the safety valve forces a mid-unit
/// release+reacquire cycle. See the module section doc above for the full
/// reasoning (20-40min documented normal pass duration + margin, vs. the 80+
/// minute incident this closes).
const MAX_LOCK_HOLD_DEFAULT_SECS: u64 = 45 * 60;

/// Read [`MAX_LOCK_HOLD_ENV`], falling back to [`MAX_LOCK_HOLD_DEFAULT_SECS`]
/// when unset, unparsable, or non-positive.
pub fn max_lock_hold_duration() -> Duration {
    std::env::var(MAX_LOCK_HOLD_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(MAX_LOCK_HOLD_DEFAULT_SECS))
}

/// Bundled config for [`maybe_release_for_max_hold`] — one value per knob it
/// needs, so call sites (and tests) pass ONE struct instead of five
/// positional `Duration`s that are easy to transpose by accident.
#[derive(Debug, Clone, Copy)]
pub struct MaxHoldConfig {
    /// Ceiling on continuous hold time before the valve fires.
    pub max_hold: Duration,
    /// Poll cadence for the bounded reacquire (see [`acquire_with_backoff`]).
    pub poll_interval: Duration,
    /// Progress-log cadence for the bounded reacquire.
    pub progress_log_interval: Duration,
    /// Bound on the reacquire's total wait (same cap the unit's own initial/
    /// per-unit acquire uses — a mid-unit reacquire is no more "owed" success
    /// than any other acquire attempt).
    pub max_wait: Duration,
    /// Pause between release and the reacquire attempt — [`INTER_UNIT_RELEASE_PAUSE`],
    /// for the identical alternation-guarantee reasoning as the end-of-unit release.
    pub release_pause: Duration,
}

/// Pure: given the CURRENT continuous hold duration (`None` ⇒ this caller does
/// not currently hold the lock at all — nothing to do), should the safety
/// valve fire? Separated from the IO (reading the lock file / the real clock)
/// so this decision is unit-testable in isolation.
pub fn should_yield_for_max_hold(held: Option<Duration>, max_hold: Duration) -> bool {
    held.map(|h| h >= max_hold).unwrap_or(false)
}

/// The max lock-hold safety valve. Called after each discrete sub-unit of
/// work (one case, one dimension, ...) completes. If `held_duration()`
/// reports this caller has continuously held the lock for at least
/// `cfg.max_hold`, this releases (via `release`), pauses `cfg.release_pause`,
/// and reacquires via the SAME bounded [`acquire_with_backoff`] every other
/// acquire in this module uses — then returns `Ok(true)`. Otherwise a no-op,
/// `Ok(false)` — BY FAR the common case; a well-behaved unit never crosses
/// `cfg.max_hold` (see the module section doc above for why 45 minutes is
/// comfortably above the documented normal case).
///
/// `Err` only if the reacquire itself exhausts its bounded wait — the same
/// failure shape a normal per-unit (re)acquire produces, so callers handle it
/// identically (typically: abort the current unit as a recorded skip, safe to
/// resume next run — never silently proceed without the lock).
///
/// Generic over the clock, the held-duration probe, the release action, and
/// the acquire action — exactly like [`acquire_with_backoff`] itself — so
/// this is unit-testable with a fake `AcquireClock` and scripted
/// probes/actions, with no real lock file or GPU involved. Production wiring
/// lives in [`GpuLock::check_max_hold`] / [`LiveGpuLock`] below, which supply
/// the real lock-file-backed probe/actions.
pub async fn maybe_release_for_max_hold<C, H, R, F>(
    clock: &C,
    held_duration: H,
    mut release_action: R,
    try_acquire: F,
    cfg: MaxHoldConfig,
    label: &str,
) -> Result<bool, String>
where
    C: AcquireClock,
    H: Fn() -> Option<Duration>,
    R: FnMut(),
    F: FnMut() -> Result<(), String>,
{
    if !should_yield_for_max_hold(held_duration(), cfg.max_hold) {
        return Ok(false);
    }

    tracing::warn!(
        "{label}: SAFETY VALVE — held the exclusive GPU lock beyond the configured max \
         ({:.0?}) with the current unit of work still in progress — releasing MID-UNIT to give \
         any other waiting sweep a real turn, then reacquiring to resume the SAME unit of work \
         where it left off (this is distinct from the normal end-of-unit release — see \
         gpu_authority.rs's \"Max lock-hold safety valve\" module section for why).",
        cfg.max_hold
    );

    release_action();
    clock.sleep(cfg.release_pause).await;

    acquire_with_backoff(
        clock,
        try_acquire,
        is_live_holder_refusal,
        cfg.poll_interval,
        cfg.progress_log_interval,
        cfg.max_wait,
        label,
    )
    .await?;

    tracing::info!(
        "{label}: safety valve reacquired the GPU lock — resuming the same unit of work"
    );
    Ok(true)
}

/// Pure: THIS `holder`'s continuous hold duration recorded in `existing`, as
/// of `now_epoch_secs` (`None` if `existing` belongs to a different holder —
/// the safety valve only ever acts on a lock THIS caller holds). Split from
/// the file IO (`current_hold_duration`) so the holder-matching guard and the
/// duration arithmetic are unit-testable without a real lock file.
fn hold_duration_for(existing: &LockState, holder: &str, now_epoch_secs: u64) -> Option<Duration> {
    if existing.holder != holder {
        return None;
    }
    Some(Duration::from_secs(now_epoch_secs.saturating_sub(existing.acquired_at)))
}

/// THIS `holder`'s current continuous hold duration, if it currently owns the
/// lock (`None` if there is no lock at all, or it is held by a different
/// holder — the safety valve only ever acts on a lock THIS caller holds).
/// Impure (reads the lock file + the real clock) — kept separate from
/// [`maybe_release_for_max_hold`] so that function stays fully unit-testable
/// via injected closures. Used only by [`LiveGpuLock`]'s real wiring.
fn current_hold_duration(holder: &str) -> Option<Duration> {
    let existing = read_lock()?;
    hold_duration_for(&existing, holder, now_epoch())
}

// ═══════════════════════════════════════════════════════════════════════════
// GpuLock: the injectable interface both sweeps drive their fairness policy
// through. Previously duplicated near-identically in `coder_sweep.rs` and
// `assistant/runner.rs`; consolidated here so the new `check_max_hold` method
// (the safety valve above) has ONE home, and both sweeps automatically share
// its exact semantics rather than risking two copies drifting apart.
// ═══════════════════════════════════════════════════════════════════════════

/// Per-unit-of-work GPU lock. Both sweeps drive their release-between-units
/// (and now release-mid-unit-if-it-runs-long) fairness policy through this
/// trait so the policy is unit-testable without a real lock file or GPU (see
/// each caller's `NoopGpuLock`/`ScriptGpuLock` test fakes).
#[async_trait::async_trait]
pub trait GpuLock: Send + Sync {
    /// Acquire for the next unit of work. `Err` only after a bounded wait
    /// gives up (see [`acquire_with_backoff`]).
    async fn acquire(&self) -> Result<(), String>;
    /// Release. Only ever called after a successful `acquire`.
    fn release(&self);
    /// How long to pause after a release before the NEXT acquire is
    /// attempted — see [`INTER_UNIT_RELEASE_PAUSE`].
    fn release_pause(&self) -> Duration;
    /// Max lock-hold safety valve: call after each discrete sub-unit of work
    /// within a unit (one case, one dimension, ...) completes. See
    /// [`maybe_release_for_max_hold`] for the full behavior. `Ok(true)` if it
    /// fired (rare); `Ok(false)` the overwhelming common case.
    async fn check_max_hold(&self) -> Result<bool, String>;
}

/// Live [`GpuLock`]: the real `gpu_authority` lock file, gated by the shared
/// bounded-backoff retry — used by both `intake_coder_sweep` and
/// `intake_assistant_sweep`.
pub struct LiveGpuLock {
    holder: &'static str,
    /// Max total time a bounded (re)acquire wait will spend retrying a
    /// refused acquire before giving up — the caller's own operator-facing
    /// knob (`INTAKE_CODER_ACQUIRE_MAX_WAIT_SECS` /
    /// `INTAKE_ASSISTANT_ACQUIRE_MAX_WAIT_SECS`), injected here rather than
    /// read directly so this module stays agnostic of which binary is
    /// calling it.
    max_wait: Duration,
}

impl LiveGpuLock {
    pub fn new(holder: &'static str, max_wait: Duration) -> Self {
        LiveGpuLock { holder, max_wait }
    }
}

#[async_trait::async_trait]
impl GpuLock for LiveGpuLock {
    async fn acquire(&self) -> Result<(), String> {
        acquire_with_backoff(
            &RealClock,
            || acquire(GpuMode::Exclusive, self.holder),
            is_live_holder_refusal,
            ACQUIRE_POLL_INTERVAL,
            ACQUIRE_PROGRESS_LOG_INTERVAL,
            self.max_wait,
            self.holder,
        )
        .await
    }

    fn release(&self) {
        if let Err(e) = release(self.holder) {
            tracing::warn!(
                "{}: release between units of work failed for '{}': {e}",
                self.holder,
                self.holder
            );
        }
    }

    fn release_pause(&self) -> Duration {
        INTER_UNIT_RELEASE_PAUSE
    }

    async fn check_max_hold(&self) -> Result<bool, String> {
        let holder = self.holder;
        maybe_release_for_max_hold(
            &RealClock,
            || current_hold_duration(holder),
            || {
                if let Err(e) = release(holder) {
                    tracing::warn!(
                        "{holder}: safety-valve release failed (will still attempt to \
                         reacquire): {e}"
                    );
                }
            },
            || acquire(GpuMode::Exclusive, holder),
            MaxHoldConfig {
                max_hold: max_lock_hold_duration(),
                poll_interval: ACQUIRE_POLL_INTERVAL,
                progress_log_interval: ACQUIRE_PROGRESS_LOG_INTERVAL,
                max_wait: self.max_wait,
                release_pause: INTER_UNIT_RELEASE_PAUSE,
            },
            holder,
        )
        .await
    }
}

/// Injectable clock so [`acquire_with_backoff`] is unit-testable without real
/// time passing. Shared by every caller of the bounded-backoff retry
/// (assistant sweep's initial+per-model acquire, coder sweep's per-(model,
/// backend) reacquire) — one implementation, not copy-pasted per caller.
/// Production uses [`RealClock`]; tests use a fake that advances a virtual
/// clock instantly.
#[async_trait::async_trait]
pub trait AcquireClock: Send + Sync {
    fn now(&self) -> std::time::Instant;
    async fn sleep(&self, dur: Duration);
}

pub struct RealClock;

#[async_trait::async_trait]
impl AcquireClock for RealClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// Is `err` (a failure string from [`acquire`] or [`acquire_with_backoff`])
/// the "a live holder currently, actively holds the exclusive lock" refusal —
/// the only refusal worth retrying. Both the local lock-file block (this
/// module's `is_blocked` path) and Chord's own remote lock (`ChordCall::Held`)
/// produce a message containing this phrase.
///
/// Deliberately NOT retried by [`acquire_with_backoff`]: a misconfigured
/// `CHORD_JWT` (`Unauthorized`), a generic Chord/network failure (`Failed`),
/// or a `systemctl`/lock-file-write failure inside [`acquire`] — those are NOT
/// "someone else has it right now," they're a broken acquire, and [`acquire`]
/// stops `policy.stop_services` (e.g. `lemonade-coder.service`) BEFORE it can
/// fail on those paths. Retrying one of those every `poll_interval` for up to
/// `max_wait` would repeatedly stop/restart a production serving unit for
/// hours on a persistent, non-transient error instead of failing fast and
/// visibly. So those fail immediately — systemd's crash-loop remains the
/// (louder, faster) safety net for a genuinely broken acquire, while the
/// bounded wait only kicks in for the expected "the other sweep has it"
/// case.
///
/// Matching is by substring against this module's CURRENT error text —
/// deliberately fail-CLOSED (an unrecognized message is treated as
/// NON-retryable) so a message-format drift here degrades to "fail fast, like
/// before," never to "silently retry forever."
pub fn is_live_holder_refusal(err: &str) -> bool {
    err.contains("held exclusively by")
}

/// Acquire via `try_acquire`, retrying with backoff instead of failing
/// immediately when it returns `Err` (e.g. the GPU is exclusively held by
/// another sweep) — bounded by `max_wait`. Returns the same `Err` shape a
/// one-shot acquire would once the cap is hit, so systemd's
/// `Restart=on-failure` remains the ultimate safety net for a caller that
/// treats exhaustion as fatal, just at a much lower frequency than every
/// retry — a caller that treats a per-unit reacquire failure as a
/// recorded skip (not fatal) still benefits from the same bounded,
/// non-spinning wait.
///
/// `label` is used only in log lines (so a coder-sweep caller's logs say
/// `intake_coder_sweep: ...` and an assistant-sweep caller's say
/// `intake_assistant_sweep: ...`) — it carries no behavioral meaning.
///
/// Generic over the acquired value `T`, the clock, and the retry predicate so
/// this is testable with a fake acquire function, a fake (instant) clock, and
/// an arbitrary `is_retryable` — no real `sleep()` in tests.
pub async fn acquire_with_backoff<T, F, C>(
    clock: &C,
    mut try_acquire: F,
    is_retryable: impl Fn(&str) -> bool,
    poll_interval: Duration,
    progress_log_interval: Duration,
    max_wait: Duration,
    label: &str,
) -> Result<T, String>
where
    F: FnMut() -> Result<T, String>,
    C: AcquireClock,
{
    let start = clock.now();
    let mut waiting = false;
    let mut last_progress_log = start;

    loop {
        match try_acquire() {
            Ok(v) => {
                if waiting {
                    tracing::info!(
                        "{label}: GPU acquired after waiting {:.0?} for another holder to release it",
                        clock.now().duration_since(start)
                    );
                }
                return Ok(v);
            }
            Err(e) => {
                if !is_retryable(&e) {
                    tracing::error!(
                        "{label}: GPU acquire failed with a non-transient error ({e}) — not \
                         retrying (this is not the \"another holder has it right now\" case; \
                         waiting would just repeatedly bounce production services)"
                    );
                    return Err(e);
                }
                let elapsed = clock.now().duration_since(start);
                if elapsed >= max_wait {
                    tracing::error!(
                        "{label}: giving up waiting for the GPU after {:.0?} (cap {:.0?}); last \
                         refusal: {e}",
                        elapsed,
                        max_wait
                    );
                    return Err(format!(
                        "gave up waiting for the GPU after {elapsed:.0?} (cap {max_wait:.0?}): {e}"
                    ));
                }
                if !waiting {
                    waiting = true;
                    last_progress_log = start;
                    tracing::warn!(
                        "{label}: GPU acquire refused ({e}) — waiting for it to free up, \
                         retrying every {poll_interval:.0?} (giving up after {max_wait:.0?})"
                    );
                } else if clock.now().duration_since(last_progress_log) >= progress_log_interval {
                    last_progress_log = clock.now();
                    tracing::warn!(
                        "{label}: still waiting for the GPU after {:.0?} ({e})",
                        elapsed
                    );
                }
                clock.sleep(poll_interval).await;
            }
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
        assert_eq!(
            interpret_chord_acquire(ChordCall::Acknowledged { new_grant: true }),
            Ok(true)
        );
        // `new_grant` isn't meaningful on the initial acquire either way.
        assert_eq!(
            interpret_chord_acquire(ChordCall::Acknowledged { new_grant: false }),
            Ok(true)
        );
    }

    #[test]
    fn handle_heartbeat_outcome_new_grant_true_logs_but_does_not_panic() {
        // The anomaly path must never panic or block — it only logs.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle_heartbeat_outcome(ChordCall::Acknowledged { new_grant: true }, "test", &stop);
    }

    #[test]
    fn handle_heartbeat_outcome_normal_refresh_is_a_silent_noop() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle_heartbeat_outcome(ChordCall::Acknowledged { new_grant: false }, "test", &stop);
    }

    #[test]
    fn handle_heartbeat_outcome_failure_stops_fast_retry_promptly_when_stop_flag_set() {
        // If `stop` is already set, the retry loop's sleep-in-1s-slices must
        // return almost immediately rather than blocking through a full
        // HEARTBEAT_RETRY_DELAYS_SECS cycle (release() tearing the heartbeat
        // down must be prompt, not delayed by an in-flight retry backoff).
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let started = std::time::Instant::now();
        handle_heartbeat_outcome(ChordCall::Unreachable, "test", &stop);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "expected a near-immediate return when stop is already set, took {:?}",
            started.elapsed()
        );
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

    // ── S86 GPU-lock fairness: shared bounded-backoff retry ─────────────────
    // (moved here from `assistant/runner.rs` so `coder_sweep.rs` can reuse the
    // SAME implementation for its between-units reacquire, rather than a
    // copy-paste — see the module section doc above `ACQUIRE_POLL_INTERVAL`.)

    use std::sync::Mutex as StdMutex;

    /// A fake clock: `now()` returns a virtual instant that only advances
    /// when `sleep()` is called (by the amount requested) — no real time
    /// passes. Also records how many times `sleep` was called, so tests can
    /// assert on retry counts without depending on wall-clock timing at all.
    struct FakeClock {
        elapsed: StdMutex<Duration>,
        sleep_calls: StdMutex<u32>,
    }

    impl FakeClock {
        fn new() -> Self {
            FakeClock { elapsed: StdMutex::new(Duration::ZERO), sleep_calls: StdMutex::new(0) }
        }

        fn sleep_call_count(&self) -> u32 {
            *self.sleep_calls.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl AcquireClock for FakeClock {
        fn now(&self) -> std::time::Instant {
            // `Instant` cannot be constructed at an arbitrary offset directly,
            // so anchor a fixed base instant once and add the virtual elapsed
            // duration recorded so far.
            use std::sync::OnceLock;
            static BASE: OnceLock<std::time::Instant> = OnceLock::new();
            let base = *BASE.get_or_init(std::time::Instant::now);
            base + *self.elapsed.lock().unwrap()
        }

        async fn sleep(&self, dur: Duration) {
            *self.sleep_calls.lock().unwrap() += 1;
            *self.elapsed.lock().unwrap() += dur;
            // Deliberately NOT a real sleep — the whole point of the fake
            // clock is that tests run instantly regardless of `dur`.
        }
    }

    #[tokio::test]
    async fn acquire_with_backoff_retries_then_succeeds() {
        let clock = FakeClock::new();
        let attempts = StdMutex::new(0u32);
        let result = acquire_with_backoff(
            &clock,
            || {
                let mut n = attempts.lock().unwrap();
                *n += 1;
                if *n < 4 {
                    Err(format!("refused (attempt {n})"))
                } else {
                    Ok(*n)
                }
            },
            |_| true, // treat every refusal as retryable for this test
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::from_secs(3600),
            "test",
        )
        .await;

        assert_eq!(result, Ok(4), "must return the value from the attempt that finally succeeded");
        assert_eq!(*attempts.lock().unwrap(), 4, "must have tried exactly 4 times (3 refusals + 1 success)");
        assert_eq!(
            clock.sleep_call_count(),
            3,
            "must sleep between refusals only, never after a success"
        );
    }

    #[tokio::test]
    async fn acquire_with_backoff_succeeds_immediately_without_sleeping() {
        let clock = FakeClock::new();
        let result: Result<u32, String> = acquire_with_backoff(
            &clock,
            || Ok(42),
            |_| true,
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::from_secs(3600),
            "test",
        )
        .await;

        assert_eq!(result, Ok(42));
        assert_eq!(clock.sleep_call_count(), 0, "a first-try success must never sleep");
    }

    #[tokio::test]
    async fn acquire_with_backoff_gives_up_after_max_wait_and_stops_retrying() {
        let clock = FakeClock::new();
        let attempts = StdMutex::new(0u32);
        let result: Result<(), String> = acquire_with_backoff(
            &clock,
            || {
                *attempts.lock().unwrap() += 1;
                Err("GPU is held exclusively by 'intake_coder_sweep'".to_string())
            },
            is_live_holder_refusal,
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::from_secs(300), // max_wait: 5 poll intervals
            "test",
        )
        .await;

        assert!(result.is_err(), "must give up rather than retry forever");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("gave up waiting for the GPU"),
            "give-up error must be self-explanatory in a log/journalctl, got: {msg}"
        );
        assert!(
            msg.contains("intake_coder_sweep"),
            "give-up error should carry the last underlying refusal for diagnosis, got: {msg}"
        );

        // With a 60s poll interval and a 300s cap, the loop must terminate
        // (bounded attempts), not spin unboundedly.
        let tries = *attempts.lock().unwrap();
        assert!(tries >= 5 && tries <= 6, "expected roughly max_wait/poll_interval attempts, got {tries}");
    }

    #[tokio::test]
    async fn acquire_with_backoff_never_retries_a_first_try_beyond_max_wait_zero() {
        // A max_wait of 0 means: try once, and if it fails, give up immediately
        // (no sleep at all) — the cap is honored even on the very first refusal.
        let clock = FakeClock::new();
        let attempts = StdMutex::new(0u32);
        let result: Result<(), String> = acquire_with_backoff(
            &clock,
            || {
                *attempts.lock().unwrap() += 1;
                Err("refused".to_string())
            },
            |_| true,
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::ZERO,
            "test",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(*attempts.lock().unwrap(), 1, "max_wait=0 must not retry at all");
        assert_eq!(clock.sleep_call_count(), 0);
    }

    #[tokio::test]
    async fn acquire_with_backoff_fails_fast_on_a_non_retryable_error_without_sleeping() {
        // The masking hazard this predicate exists to prevent: a persistent,
        // non-transient acquire failure (e.g. misconfigured CHORD_JWT) must
        // NOT be retried for up to max_wait — gpu_authority::acquire() stops
        // production services (e.g. lemonade-coder.service) before it can
        // fail on that path, so retrying it every poll_interval would bounce
        // that service repeatedly for hours instead of failing immediately.
        let clock = FakeClock::new();
        let attempts = StdMutex::new(0u32);
        let result: Result<(), String> = acquire_with_backoff(
            &clock,
            || {
                *attempts.lock().unwrap() += 1;
                Err("chord rejected the GPU-exclusive acquire (401/403) — set CHORD_JWT \
                     to a valid lumina token for this harness host"
                    .to_string())
            },
            is_live_holder_refusal,
            Duration::from_secs(60),
            Duration::from_secs(600),
            Duration::from_secs(3600 * 4),
            "test",
        )
        .await;

        assert!(result.is_err(), "a non-retryable refusal must still fail");
        assert_eq!(
            *attempts.lock().unwrap(),
            1,
            "must try exactly once and give up immediately, not retry a non-transient error"
        );
        assert_eq!(
            clock.sleep_call_count(),
            0,
            "must never sleep before failing fast on a non-retryable error"
        );
        assert!(
            result.unwrap_err().contains("CHORD_JWT"),
            "the original error detail must be preserved for diagnosis"
        );
    }

    #[test]
    fn is_live_holder_refusal_recognizes_both_local_and_chord_held_messages() {
        // These are this module's ACTUAL current error strings (local lock
        // block, and Chord's remote `Held` refusal) — if that wording ever
        // drifts, this test should be updated alongside it so the predicate
        // keeps recognizing the retryable case rather than silently falling
        // back to "fail fast" for the expected scenario.
        assert!(is_live_holder_refusal(
            "GPU is held exclusively by 'intake_coder_sweep' (pid 123, mode exclusive, \
             since epoch 100) — refusing to acquire for 'intake_assistant_sweep'"
        ));
        assert!(is_live_holder_refusal(
            "chord reports the GPU is already held exclusively by 'intake_coder_sweep' \
             — refusing to start"
        ));
    }

    #[test]
    fn is_live_holder_refusal_rejects_non_transient_errors() {
        assert!(!is_live_holder_refusal(
            "chord rejected the GPU-exclusive acquire (401/403) — set CHORD_JWT to a valid \
             lumina token for this harness host"
        ));
        assert!(!is_live_holder_refusal("chord GPU-exclusive acquire failed: connection reset"));
        assert!(!is_live_holder_refusal("failed to write GPU lock file: permission denied"));
        assert!(!is_live_holder_refusal("some entirely unrecognized message"));
    }

    // ── S86: does the release-between-units fix actually achieve alternation? ──
    //
    // A synchronous, deterministic discrete-event simulation of two "sweeps"
    // (A = coder-sweep-like, long per-unit work; B = assistant-sweep-like,
    // shorter per-unit work) sharing one lock, each following the EXACT
    // policy this fix implements: hold the lock for `unit_secs`, release,
    // wait `pause_secs`, then try to reacquire; while NOT holding it and
    // refused, retry every `poll_secs`. No real async/tokio scheduling is
    // involved — this is a plain second-by-second state walk, so it is fully
    // deterministic and fast, and it directly encodes the timing argument
    // from the module doc rather than merely asserting on `acquire_with_backoff`
    // in isolation.
    struct SweepSim {
        unit_secs: u64,
        pause_secs: u64,
        poll_secs: u64,
        next_attempt: u64,
        release_at: Option<u64>,
        turns: u32,
    }

    impl SweepSim {
        fn new(unit_secs: u64, pause_secs: u64, poll_secs: u64) -> Self {
            SweepSim { unit_secs, pause_secs, poll_secs, next_attempt: 0, release_at: None, turns: 0 }
        }
    }

    /// Runs the simulation for `total_secs` virtual seconds; returns (a_turns, b_turns).
    fn simulate_alternation(total_secs: u64, mut a: SweepSim, mut b: SweepSim) -> (u32, u32) {
        let mut holder: Option<u8> = None; // 1 = A, 2 = B

        for now in 0..total_secs {
            // Releases due this tick free the lock and schedule this sweep's
            // next attempt AFTER its post-release pause.
            if holder == Some(1) && a.release_at == Some(now) {
                holder = None;
                a.release_at = None;
                a.next_attempt = now + a.pause_secs;
            }
            if holder == Some(2) && b.release_at == Some(now) {
                holder = None;
                b.release_at = None;
                b.next_attempt = now + b.pause_secs;
            }

            // A's attempt this tick (arbitrary tie-break: A acts first).
            if holder.is_none() && now >= a.next_attempt {
                holder = Some(1);
                a.release_at = Some(now + a.unit_secs);
                a.turns += 1;
            } else if holder.is_some() && holder != Some(1) && now >= a.next_attempt {
                a.next_attempt = now + a.poll_secs;
            }

            // B's attempt this tick (checked after A's, so if A just took the
            // lock this very tick, B correctly sees it as unavailable).
            if holder.is_none() && now >= b.next_attempt {
                holder = Some(2);
                b.release_at = Some(now + b.unit_secs);
                b.turns += 1;
            } else if holder.is_some() && holder != Some(2) && now >= b.next_attempt {
                b.next_attempt = now + b.poll_secs;
            }
        }

        (a.turns, b.turns)
    }

    #[test]
    fn alternation_actually_happens_with_the_chosen_pause() {
        // A = coder-sweep-like (long units, e.g. ~10min per model+backend
        // pass), B = assistant-sweep-like (shorter units, e.g. ~2min per
        // model). Both poll every ACQUIRE_POLL_INTERVAL (60s) while waiting,
        // and both pause INTER_UNIT_RELEASE_PAUSE (90s) after releasing
        // before trying to reacquire — the ACTUAL policy this fix
        // implements.
        let poll = ACQUIRE_POLL_INTERVAL.as_secs();
        let pause = INTER_UNIT_RELEASE_PAUSE.as_secs();
        assert!(pause > poll, "the whole proof below depends on pause > poll_interval");

        let a = SweepSim::new(600, pause, poll);
        let b = SweepSim::new(120, pause, poll);

        // 3 hours of virtual time — long enough for many alternations.
        let (a_turns, b_turns) = simulate_alternation(3 * 60 * 60, a, b);

        assert!(a_turns > 3, "coder-sweep-like side must get multiple real turns, got {a_turns}");
        assert!(
            b_turns > 3,
            "assistant-sweep-like side must ALSO get multiple real turns (not starved), got {b_turns}"
        );
    }

    #[test]
    fn a_too_short_pause_reproduces_the_starvation_this_fix_must_avoid() {
        // The originally-considered "1-2s" pause: strictly SHORTER than the
        // 60s poll interval. B's poll checks land at a FIXED phase relative
        // to A's release/reacquire cycle whenever that cycle length (unit +
        // pause) is an exact multiple of the poll interval — 598 + 2 = 600 =
        // 10 * 60 here — so if B's check misses the 2s window once, it
        // misses on EVERY subsequent cycle too, not just "usually": this
        // demonstrates the worst case is not a rare edge case but a stable,
        // reproducible starvation for as long as the two sweeps' cadences
        // stay in that phase relationship (exactly the kind of clean integer
        // cadence real systemd-timer-driven poll loops actually have). This
        // is WHY INTER_UNIT_RELEASE_PAUSE was set to 90s instead of 1-2s — a
        // short pause releases and reacquires the lock on paper, but never
        // gives the other side a REAL window, so it can achieve nothing in
        // practice for as long as the run lasts.
        let poll = ACQUIRE_POLL_INTERVAL.as_secs();
        let too_short_pause = 2u64;
        assert!(too_short_pause < poll);

        let a = SweepSim::new(598, too_short_pause, poll); // 598 + 2 = 600 = 10 * poll
        let b = SweepSim::new(120, too_short_pause, poll);

        // 24h of virtual time — long enough that "B eventually gets lucky"
        // would have shown up if it were going to.
        let (a_turns, b_turns) = simulate_alternation(24 * 60 * 60, a, b);

        assert!(a_turns > 0, "sanity: A still runs");
        assert_eq!(
            b_turns, 0,
            "with a too-short pause whose cycle phase-locks against the poll interval, B never \
             wins the race even once in 24 hours — proving a brief-but-inadequate pause is worse \
             than no fix at all (it LOOKS like fairness in the code but delivers none in practice)"
        );
    }

    #[test]
    fn alternation_holds_regardless_of_the_exact_unit_lengths() {
        // Unlike the too-short-pause case above, a pause LONGER than the
        // poll interval guarantees alternation for ANY unit lengths — not
        // just ones that happen to avoid an unlucky phase lock. Re-run the
        // "chosen pause" proof with the SAME unit length (598s) that phase-
        // locked the too-short-pause test into total starvation, to show the
        // 90s pause is not itself relying on a lucky phase relationship.
        let poll = ACQUIRE_POLL_INTERVAL.as_secs();
        let pause = INTER_UNIT_RELEASE_PAUSE.as_secs();

        let a = SweepSim::new(598, pause, poll);
        let b = SweepSim::new(120, pause, poll);

        let (a_turns, b_turns) = simulate_alternation(3 * 60 * 60, a, b);

        assert!(a_turns > 3);
        assert!(b_turns > 3, "must NOT reproduce the phase-lock starvation seen with a too-short pause");
    }

    // ── Max lock-hold safety valve ──────────────────────────────────────────

    #[test]
    fn max_lock_hold_env_default_and_override() {
        std::env::remove_var(MAX_LOCK_HOLD_ENV);
        assert_eq!(max_lock_hold_duration(), Duration::from_secs(MAX_LOCK_HOLD_DEFAULT_SECS));
        assert_eq!(max_lock_hold_duration(), Duration::from_secs(45 * 60));

        std::env::set_var(MAX_LOCK_HOLD_ENV, "0");
        assert_eq!(max_lock_hold_duration(), Duration::from_secs(MAX_LOCK_HOLD_DEFAULT_SECS), "zero rejected");

        std::env::set_var(MAX_LOCK_HOLD_ENV, "not-a-number");
        assert_eq!(max_lock_hold_duration(), Duration::from_secs(MAX_LOCK_HOLD_DEFAULT_SECS));

        std::env::set_var(MAX_LOCK_HOLD_ENV, "600");
        assert_eq!(max_lock_hold_duration(), Duration::from_secs(600));

        std::env::remove_var(MAX_LOCK_HOLD_ENV);
    }

    #[test]
    fn max_lock_hold_default_is_comfortably_above_the_documented_normal_pass_duration() {
        // `code_v2.rs`'s Phase-1 comment documents a well-behaved ~40-case
        // pass legitimately taking 20-40 minutes end to end with ZERO
        // retries. The default must sit ABOVE that documented ceiling (with
        // margin) so the valve stays silent for the common case, while still
        // being comfortably BELOW the hours a genuinely unreliable model can
        // run unbounded (the production incident: 80+ minutes and climbing).
        let default = max_lock_hold_duration();
        assert!(
            default > Duration::from_secs(40 * 60),
            "must sit above the documented normal-case ceiling (40min), got {default:?}"
        );
        assert!(
            default < Duration::from_secs(70 * 60),
            "must fire well before the observed incident's 80+ minute mark, got {default:?}"
        );
        // Must never be confused with, or thrash against, the per-unit release
        // cadence — it is a rare safety net, not a routine cadence.
        assert!(default > INTER_UNIT_RELEASE_PAUSE * 10);
        assert!(default > ACQUIRE_POLL_INTERVAL * 10);
    }

    #[test]
    fn should_yield_for_max_hold_pure_decision() {
        assert!(!should_yield_for_max_hold(None, Duration::from_secs(60)), "no hold at all ⇒ never yield");
        assert!(!should_yield_for_max_hold(Some(Duration::from_secs(59)), Duration::from_secs(60)));
        assert!(should_yield_for_max_hold(Some(Duration::from_secs(60)), Duration::from_secs(60)), "boundary is inclusive");
        assert!(should_yield_for_max_hold(Some(Duration::from_secs(600)), Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn well_behaved_unit_never_triggers_the_valve() {
        // Held duration always well under the max — the overwhelmingly common
        // case must see ZERO release/reacquire activity.
        let clock = FakeClock::new();
        let release_calls = StdMutex::new(0u32);
        let acquire_calls = StdMutex::new(0u32);

        let fired = maybe_release_for_max_hold(
            &clock,
            || Some(Duration::from_secs(120)), // well under the 1800s max below
            || *release_calls.lock().unwrap() += 1,
            || {
                *acquire_calls.lock().unwrap() += 1;
                Ok(())
            },
            MaxHoldConfig {
                max_hold: Duration::from_secs(1800),
                poll_interval: Duration::from_secs(60),
                progress_log_interval: Duration::from_secs(600),
                max_wait: Duration::from_secs(4 * 3600),
                release_pause: Duration::from_secs(90),
            },
            "test",
        )
        .await
        .unwrap();

        assert!(!fired);
        assert_eq!(*release_calls.lock().unwrap(), 0);
        assert_eq!(*acquire_calls.lock().unwrap(), 0);
        assert_eq!(clock.sleep_call_count(), 0, "no pause without a release");
    }

    #[tokio::test]
    async fn slow_unreliable_unit_triggers_the_valve_and_reacquires() {
        // Held duration has crossed the max — the valve must release, pause,
        // then reacquire (in that exact order), and report that it fired.
        let clock = FakeClock::new();
        let calls = StdMutex::new(Vec::<&'static str>::new());

        let fired = maybe_release_for_max_hold(
            &clock,
            || Some(Duration::from_secs(2000)), // over the 1800s max
            || calls.lock().unwrap().push("release"),
            || {
                calls.lock().unwrap().push("acquire");
                Ok(())
            },
            MaxHoldConfig {
                max_hold: Duration::from_secs(1800),
                poll_interval: Duration::from_secs(60),
                progress_log_interval: Duration::from_secs(600),
                max_wait: Duration::from_secs(4 * 3600),
                release_pause: Duration::from_secs(90),
            },
            "test",
        )
        .await
        .unwrap();

        assert!(fired);
        assert_eq!(*calls.lock().unwrap(), vec!["release", "acquire"], "release must happen BEFORE reacquire");
        // The pause between release and reacquire went through the clock
        // (fake/instant — no real time elapses in the test), never a raw
        // real sleep.
        assert_eq!(clock.sleep_call_count(), 1);
    }

    #[tokio::test]
    async fn valve_reacquire_retries_through_transient_refusals_then_succeeds() {
        // The reacquire after a mid-unit release goes through the SAME
        // bounded `acquire_with_backoff` as any other acquire — a transient
        // refusal (the other sweep briefly holding it) must be retried, not
        // treated as a hard failure.
        let clock = FakeClock::new();
        let attempts = StdMutex::new(0u32);

        let fired = maybe_release_for_max_hold(
            &clock,
            || Some(Duration::from_secs(2000)),
            || {},
            || {
                let mut n = attempts.lock().unwrap();
                *n += 1;
                if *n < 3 {
                    Err("GPU is held exclusively by 'other-sweep'".to_string())
                } else {
                    Ok(())
                }
            },
            MaxHoldConfig {
                max_hold: Duration::from_secs(1800),
                poll_interval: Duration::from_secs(60),
                progress_log_interval: Duration::from_secs(600),
                max_wait: Duration::from_secs(4 * 3600),
                release_pause: Duration::from_secs(90),
            },
            "test",
        )
        .await
        .unwrap();

        assert!(fired);
        assert_eq!(*attempts.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn valve_reacquire_failure_after_max_wait_surfaces_as_err() {
        // If the reacquire itself exhausts its bounded wait, the caller must
        // see `Err` (never silently proceed as if it still holds the lock).
        let clock = FakeClock::new();

        let result = maybe_release_for_max_hold(
            &clock,
            || Some(Duration::from_secs(2000)),
            || {},
            || Err("GPU is held exclusively by 'other-sweep'".to_string()),
            MaxHoldConfig {
                max_hold: Duration::from_secs(1800),
                poll_interval: Duration::from_secs(60),
                progress_log_interval: Duration::from_secs(600),
                max_wait: Duration::from_secs(300), // small cap for a fast test
                release_pause: Duration::from_secs(90),
            },
            "test",
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("gave up waiting for the GPU"));
    }

    #[tokio::test]
    async fn valve_never_acts_on_a_lock_this_caller_does_not_hold() {
        // `held_duration` returning `None` (not the current holder / no lock
        // at all) must be a pure no-op — the valve only ever acts on a lock
        // THIS caller holds.
        let clock = FakeClock::new();
        let release_calls = StdMutex::new(0u32);

        let fired = maybe_release_for_max_hold(
            &clock,
            || None,
            || *release_calls.lock().unwrap() += 1,
            || Ok(()),
            MaxHoldConfig {
                max_hold: Duration::from_secs(1), // trivially small — would fire if held_duration were Some
                poll_interval: Duration::from_secs(60),
                progress_log_interval: Duration::from_secs(600),
                max_wait: Duration::from_secs(3600),
                release_pause: Duration::from_secs(90),
            },
            "test",
        )
        .await
        .unwrap();

        assert!(!fired);
        assert_eq!(*release_calls.lock().unwrap(), 0);
    }

    #[test]
    fn hold_duration_for_ignores_a_lock_held_by_someone_else() {
        // `current_hold_duration`'s holder-matching guard, tested via its
        // pure half (`hold_duration_for`) — no real lock file needed. Must
        // never report a hold duration for a lock some OTHER holder owns,
        // even if the recorded `acquired_at` would otherwise compute a huge
        // duration.
        let existing = LockState {
            holder: "some-other-process".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 0,
            stopped_services: vec![],
            chord_notified: false,
        };
        assert_eq!(hold_duration_for(&existing, "intake_coder_sweep", 100_000), None);
    }

    #[test]
    fn hold_duration_for_computes_elapsed_when_the_holder_matches() {
        let existing = LockState {
            holder: "intake_coder_sweep".into(),
            mode: "exclusive".into(),
            pid: 1,
            acquired_at: 1_000,
            stopped_services: vec![],
            chord_notified: false,
        };
        assert_eq!(
            hold_duration_for(&existing, "intake_coder_sweep", 1_500),
            Some(Duration::from_secs(500))
        );
        // Never underflows/panics even if `now` somehow lands before
        // `acquired_at` (clock skew) — saturates to zero instead.
        assert_eq!(
            hold_duration_for(&existing, "intake_coder_sweep", 500),
            Some(Duration::ZERO)
        );
    }
}
