//! On-demand backend lifecycle (P5): start a tagged backend before inference
//! and free the single GPU first (arbitration), so no backend perpetually holds
//! the GPU. Always-on / Ollama / daemon backends are assumed up.
//!
//! Generic GPU backends (no systemd unit) are launched as **transient systemd
//! units** via `systemd-run --unit=chord-<name> --collect <bin> <args> -m <blob>`
//! so they survive the spawning request and stop cleanly with `systemctl stop`.
//! The model's GGUF is resolved from its local Ollama blob (largest layer).
//!
//! Chord runs as root, so `systemctl` / `systemd-run` are available. This module
//! only manages backends declared in the registry (no arbitrary unit control).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::time::Instant;

use crate::intake::infer::{self, ResolvedBackend};

/// Ensure `backend` is running and ready to serve `model`. Returns `Ok(())` when
/// the backend answers `/health`, or an error string describing the failure.
pub async fn ensure_up(backend: &ResolvedBackend, model: &str) -> Result<(), String> {
    // Always-on / Ollama / daemon backends are assumed up and managed elsewhere.
    if backend.always_on || backend.kind == "ollama" || backend.kind == "daemon" {
        return Ok(());
    }
    // Mark in-use NOW so the idle-stop sweep never stops a backend the harness
    // (in-process) or chat path is actively driving. Both paths call ensure_up,
    // so this single touch covers both — the sweep reads the same file.
    touch_used(&backend.name);
    // Already serving? A unit-based backend serves a fixed model, so being up is
    // enough. A generic launch-based backend is pinned to ONE model: if it is up
    // but loaded with a DIFFERENT model, it must be restarted with this one
    // (otherwise a second GPU-tagged model would be served the first's weights).
    if health_ok(&backend.url).await {
        if backend.unit.is_some() || current_model(&backend.name).as_deref() == Some(model) {
            return Ok(());
        }
        stop(backend); // up but wrong model → relaunch below
    }

    // Single GPU: stop every OTHER GPU backend before starting this one.
    if backend.hardware == "gpu" {
        free_gpu(&backend.name);
    }

    if let Some(unit) = &backend.unit {
        run(["systemctl", "start", unit])
            .map_err(|e| format!("start {unit}: {e}"))?;
    } else if let Some(launch) = &backend.launch {
        // A direct GGUF path (non-Ollama model, e.g. an imported sharded HF GGUF)
        // is used verbatim for `-m`; otherwise resolve the Ollama blob.
        let blob = if let Some(p) = &backend.model_gguf_path {
            PathBuf::from(p)
        } else {
            let local = backend.model_local_path.as_deref().ok_or_else(|| {
                format!("model '{model}' is not local (no local_path); pull it first")
            })?;
            resolve_blob(local, model)
                .ok_or_else(|| format!("could not resolve GGUF blob for '{model}' under {local}"))?
        };
        let unit_name = transient_unit(&backend.name);
        let _ = run(["systemctl", "stop", &unit_name]); // clear any stale unit
        let mut argv: Vec<String> = vec![
            format!("--unit={unit_name}"),
            "--collect".to_string(),
            launch.bin.clone(),
        ];
        argv.extend(launch.args.clone());
        argv.push(launch.model_arg.clone());
        argv.push(blob.to_string_lossy().to_string());
        run_argv("systemd-run", &argv)
            .map_err(|e| format!("systemd-run {unit_name}: {e}"))?;
    } else {
        return Err(format!(
            "backend '{}' is on-demand but has neither a unit nor a launch spec",
            backend.name
        ));
    }

    set_current_model(&backend.name, model);
    // Poll until healthy, but FAIL FAST if the just-launched unit dies, and cap
    // the ceiling BELOW lumina's 120s client timeout (configurable, default 90s)
    // so a slow-but-not-crashed backend can never outlast the caller. The unit
    // we watch is exactly the one started above: a declared `unit` for
    // unit-based backends, else the transient `chord-<name>.service` created by
    // `systemd-run` for launch-based backends.
    let unit = backend
        .unit
        .clone()
        .unwrap_or_else(|| transient_unit(&backend.name));
    let max = Duration::from_secs(crate::config::ensure_up_timeout_secs());
    wait_healthy_or_unit_dead(&backend.url, &unit, max).await
}

/// Path of the state file recording which model a backend is currently serving.
fn current_model_file(backend: &str) -> PathBuf {
    PathBuf::from(format!("/run/chord-backend-{backend}.model"))
}

/// The model a generic backend is currently loaded with, if known.
fn current_model(backend: &str) -> Option<String> {
    std::fs::read_to_string(current_model_file(backend))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Record the model a backend was just launched with (best-effort).
fn set_current_model(backend: &str, model: &str) {
    let _ = std::fs::write(current_model_file(backend), model);
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record that `backend` was just used (epoch secs), for the idle-stop sweep.
fn touch_used(backend: &str) {
    let _ = std::fs::write(
        PathBuf::from(format!("/run/chord-backend-{backend}.lastused")),
        now_epoch().to_string(),
    );
}

/// Seconds since a backend was last used, or `None` if never recorded.
pub fn idle_secs(backend: &str) -> Option<u64> {
    let s = std::fs::read_to_string(PathBuf::from(format!(
        "/run/chord-backend-{backend}.lastused"
    )))
    .ok()?;
    let last: u64 = s.trim().parse().ok()?;
    Some(now_epoch().saturating_sub(last))
}

/// Stop an on-demand backend (its unit, or its transient `chord-<name>` unit).
/// Best-effort; always-on/ollama/daemon backends are left running.
pub fn stop(backend: &ResolvedBackend) {
    if backend.always_on || backend.kind == "ollama" || backend.kind == "daemon" {
        return;
    }
    match &backend.unit {
        Some(unit) => {
            let _ = run(["systemctl", "stop", unit]);
        }
        None => {
            let _ = run(["systemctl", "stop", &transient_unit(&backend.name)]);
        }
    }
}

/// Stop every GPU backend except `keep` (frees the single GPU). Stops both
/// declared units and transient `chord-<name>` units.
fn free_gpu(keep: &str) {
    for (name, unit) in infer::gpu_backends() {
        if name == keep {
            continue;
        }
        if let Some(unit) = unit {
            let _ = run(["systemctl", "stop", &unit]);
        }
        let _ = run(["systemctl", "stop", &transient_unit(&name)]);
    }
}

fn transient_unit(backend: &str) -> String {
    format!("chord-{backend}.service")
}

// ── GGUF blob resolution ────────────────────────────────────────────────────

/// Resolve a model's weights GGUF (the largest layer blob) under its local
/// Ollama root. `local_path` is the Ollama root (holds `manifests/` + `blobs/`).
pub fn resolve_blob(local_path: &str, model: &str) -> Option<PathBuf> {
    let (body, tag) = model.rsplit_once(':')?;
    let model_dir = body.rsplit('/').next()?; // last path component of the name
    let manifests = Path::new(local_path).join("manifests");
    let leaf = find_manifest_leaf(&manifests, model_dir, tag)?;
    let text = std::fs::read_to_string(&leaf).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let layers = v.get("layers")?.as_array()?;
    let best = layers
        .iter()
        .max_by_key(|l| l.get("size").and_then(|s| s.as_u64()).unwrap_or(0))?;
    let digest = best.get("digest")?.as_str()?; // "sha256:abc…"
    let blob = Path::new(local_path)
        .join("blobs")
        .join(digest.replace(':', "-"));
    blob.exists().then_some(blob)
}

/// Find the manifest leaf file `<…>/<model_dir>/<tag>` under `root` (Ollama
/// stores manifests at `manifests/<host>/<ns>/<model>/<tag>`). Bounded recursive
/// search; returns the first match.
fn find_manifest_leaf(root: &Path, model_dir: &str, tag: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(root).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_manifest_leaf(&path, model_dir, tag) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(tag)
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some(model_dir)
        {
            return Some(path);
        }
    }
    None
}

// ── Process + health helpers ────────────────────────────────────────────────

fn run<const N: usize>(args: [&str; N]) -> Result<(), String> {
    let (cmd, rest) = args.split_first().ok_or("empty command")?;
    run_argv(cmd, &rest.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn run_argv(cmd: &str, args: &[String]) -> Result<(), String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("spawn {cmd}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{cmd} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

async fn health_ok(base: &str) -> bool {
    let client = reqwest::Client::new();
    matches!(
        client
            .get(format!("{base}/health"))
            .timeout(Duration::from_secs(3))
            .send()
            .await,
        Ok(r) if r.status().is_success()
    )
}

/// Grace window after launch during which a `unknown`/absent unit state is NOT
/// yet treated as death — a transient `systemd-run --collect` unit can briefly
/// read `unknown`/`activating` for a beat before it registers, and a
/// healthy-but-slow start must never be false-failed. `failed`/`inactive`/
/// `deactivating` are conclusive and fail fast regardless of this window.
const UNIT_DEATH_GRACE: Duration = Duration::from_secs(6);

/// Hard bound on a single `systemctl is-active` invocation so a stalled
/// systemd/dbus can never block the poll loop past its wall-clock ceiling. A
/// call that exceeds this is treated as INDETERMINATE (never as death — a call
/// that didn't complete tells us nothing).
const SYSTEMCTL_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a systemd unit has DIED — read from `systemctl is-active <unit>`,
/// which also covers the `systemctl is-failed` case. Conclusive dead states are
/// `failed` (the transient unit crashed, e.g. llama-server on an unloadable
/// arch) and `inactive`/`deactivating` (exited/collected). A still-coming-up
/// unit reads `activating` and a healthy one `active` — neither is dead, so
/// polling continues.
///
/// `--collect` GC edge case (`past_grace`): a transient unit launched with
/// `systemd-run --collect` that crashes almost immediately is garbage-collected
/// before the first poll, so `is-active` returns `unknown` (or empty) with no
/// `failed`/`inactive` left to observe — the crash would otherwise be MISSED and
/// the full ceiling burned. So `unknown`/empty is treated as dead too, but ONLY
/// once `past_grace` is set (the [`UNIT_DEATH_GRACE`] window since launch has
/// elapsed) — and the caller only reaches this check when the health port is
/// still down (the poll checks health first). That pair guards against a
/// launch-race false-fail while still catching the crash-then-GC case.
///
/// Graceful when the state can't be determined: if `systemctl` cannot be
/// spawned (systemd absent) OR the call does not return within
/// [`SYSTEMCTL_CALL_TIMEOUT`] (systemd/dbus stall), this returns `false` — the
/// caller then falls back to the pure health-timeout behavior rather than
/// false-failing a healthy-but-slow launch, and a stalled call can never block
/// the poll loop past its ceiling. Never panics.
///
/// Async + bounded: uses `tokio::process::Command` under a hard
/// [`SYSTEMCTL_CALL_TIMEOUT`] so no single poll iteration can exceed roughly the
/// health probe (3s) plus this call (2s), keeping the outer wall-clock ceiling
/// strictly honored even if dbus hangs.
async fn unit_dead(unit: &str, past_grace: bool) -> bool {
    let call = tokio::process::Command::new("systemctl")
        .args(["is-active", unit])
        .output();
    let out = match tokio::time::timeout(SYSTEMCTL_CALL_TIMEOUT, call).await {
        Ok(Ok(out)) => out,
        // Spawn error (systemctl absent / not spawnable) OR the call timed out
        // (systemd/dbus stall) → INDETERMINATE. Never conclude death from a call
        // that didn't complete; keep polling / fall back to health + timeout.
        Ok(Err(_)) | Err(_) => return false,
    };
    let state = String::from_utf8_lossy(&out.stdout);
    unit_dead_decision(&state, systemd_is_init(), past_grace)
}

/// Whether systemd is the actual init system (PID 1), the standard `sd_booted()`
/// check (`/run/systemd/system` exists). Computed ONCE and cached — this is a
/// host fact that never changes during a process's life, and it is the real
/// discriminator between the two indistinguishable `is-active`=`unknown`+nonzero
/// cases: a genuinely crashed+GC'd `--collect` unit on a real systemd host, vs a
/// non-PID-1 container/CI where `systemctl` can't answer at all.
fn systemd_is_init() -> bool {
    static IS_INIT: OnceLock<bool> = OnceLock::new();
    *IS_INIT.get_or_init(|| Path::new("/run/systemd/system").is_dir())
}

/// Pure decision for [`unit_dead`], factored out so it is unit-tested without a
/// real `systemctl`. `is_init` is [`systemd_is_init`].
///
/// - `failed` / `inactive` / `deactivating` are RECOGNIZED real dead states →
///   dead, conclusive and independent of `is_init`, exit status, or grace (a
///   crashed unit prints these on a nonzero `is-active` exit).
/// - `unknown` / empty stdout is AMBIGUOUS and is resolved by the ONE real
///   discriminator, `is_init`:
///     * systemd IS init (production) → trust the unit-state path: after the
///       grace this means the `--collect` unit genuinely crashed and was GC'd →
///       DEAD, regardless of exit status (restores fast crash-detection).
///     * systemd is NOT init (container/CI/non-PID-1) → `systemctl` can't
///       meaningfully answer, so the unit-state shortcut is worthless →
///       INDETERMINATE → never dead-by-unit-state; the caller falls back to the
///       health-poll + timeout path (never false-fails a slow launch).
/// - anything else (`active` / `activating` / `reloading` / …) → not dead.
fn unit_dead_decision(state: &str, is_init: bool, past_grace: bool) -> bool {
    match state.trim() {
        "failed" | "inactive" | "deactivating" => true,
        "unknown" | "" => is_init && past_grace,
        _ => false,
    }
}

/// A backend that never became ready, distinguishing a CRASHED unit (fail fast)
/// from simply running out the health budget.
#[derive(Debug, PartialEq, Eq)]
enum NotReady {
    /// The just-launched unit entered a dead state before answering `/health`.
    UnitDead,
    /// The health budget elapsed without the unit dying or answering.
    Timeout,
}

/// Poll `is_healthy` until it succeeds (⇒ `Ok`), the unit dies (⇒
/// `Err(UnitDead)`, immediately — no more waiting), or `max` elapses (⇒
/// `Err(Timeout)`). A HEALTHY result wins even if the unit momentarily reads
/// dead. The unit-death check is passed a `past_grace` flag (whether `grace`
/// has elapsed since the start) so an ambiguous `unknown` GC state is only
/// treated as death after the launch-race window. Both checks are injectable so
/// the fail-fast behavior is unit-tested without a real systemd/HTTP backend.
async fn poll_until_ready<HF, HR, UF, UR>(
    max: Duration,
    interval: Duration,
    grace: Duration,
    mut is_healthy: HF,
    mut is_unit_dead: UF,
) -> Result<(), NotReady>
where
    HF: FnMut() -> HR,
    HR: Future<Output = bool>,
    UF: FnMut(bool) -> UR,
    UR: Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        // Enforce the ceiling at loop entry so we never START a fresh probe cycle
        // past the deadline (codex T5): the health (≤3s) + unit-state (≤2s) probes
        // below must not be launched once the budget is spent. A single in-flight
        // iteration may still overshoot by that bounded ~5s, which the 110s hard
        // cap keeps strictly under lumina's 120s client timeout.
        if start.elapsed() >= max {
            return Err(NotReady::Timeout);
        }
        if is_healthy().await {
            return Ok(());
        }
        // Fail fast: a dead unit will never become healthy, so don't burn the
        // rest of the budget polling a corpse (the 180s dead-port stall this
        // fixes). `past_grace` gates only the ambiguous `unknown` GC state.
        let past_grace = start.elapsed() >= grace;
        if is_unit_dead(past_grace).await {
            return Err(NotReady::UnitDead);
        }
        if start.elapsed() >= max {
            return Err(NotReady::Timeout);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Poll `base`/`/health` until healthy, failing fast if `unit` dies, capped at
/// `max`. The real wiring of [`poll_until_ready`] for on-demand backends.
async fn wait_healthy_or_unit_dead(base: &str, unit: &str, max: Duration) -> Result<(), String> {
    poll_until_ready(
        max,
        Duration::from_secs(2),
        UNIT_DEATH_GRACE,
        || health_ok(base),
        |past_grace| async move { unit_dead(unit, past_grace).await },
    )
    .await
    .map_err(|reason| match reason {
        NotReady::UnitDead => {
            format!("backend unit '{unit}' died before {base} became healthy")
        }
        NotReady::Timeout => {
            format!("backend at {base} did not become healthy within {max:?}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_unit_name() {
        assert_eq!(transient_unit("llama-gpu"), "chord-llama-gpu.service");
    }

    #[test]
    fn resolve_blob_picks_largest_layer() {
        // Build a tiny fake Ollama root: manifests/<host>/<ns>/<model>/<tag>.
        let root = std::env::temp_dir().join("lifecycle-blob-test");
        let man_dir = root.join("manifests/registry.ollama.ai/library/fakemodel");
        std::fs::create_dir_all(&man_dir).unwrap();
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        // Two blobs; the larger is the weights.
        std::fs::write(root.join("blobs/sha256-small"), b"x").unwrap();
        std::fs::write(root.join("blobs/sha256-big"), vec![0u8; 16]).unwrap();
        let manifest = r#"{"layers":[
            {"digest":"sha256:small","size":1},
            {"digest":"sha256:big","size":999}
        ]}"#;
        std::fs::write(man_dir.join("v1"), manifest).unwrap();

        let blob = resolve_blob(root.to_str().unwrap(), "fakemodel:v1").unwrap();
        assert!(blob.ends_with("blobs/sha256-big"));
    }

    #[test]
    fn resolve_blob_none_when_missing() {
        assert!(resolve_blob("/nonexistent", "x:1").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn poll_fails_fast_when_unit_dead_not_waiting_full_budget() {
        // Backend never answers /health, but its unit reads a conclusive dead
        // state (e.g. `failed`) → Err(UnitDead) on the FIRST iteration, long
        // before the 120s ceiling. Conclusive death ignores past_grace.
        let start = Instant::now();
        let r = poll_until_ready(
            Duration::from_secs(120),
            Duration::from_secs(2),
            UNIT_DEATH_GRACE,
            || async { false },      // never healthy
            |_past_grace| async { true }, // unit conclusively dead
        )
        .await;
        assert_eq!(r, Err(NotReady::UnitDead));
        // With the paused clock, a fail-fast return advances virtual time by ~0;
        // a regression that polled to the ceiling would auto-advance ~120s.
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must fail fast on a dead unit, not poll the full budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn poll_ok_when_healthy_even_if_unit_reads_dead() {
        // Healthy wins immediately (the success path is preserved).
        let r = poll_until_ready(
            Duration::from_secs(120),
            Duration::from_secs(2),
            UNIT_DEATH_GRACE,
            || async { true },
            |_past_grace| async { true },
        )
        .await;
        assert_eq!(r, Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_times_out_when_neither_healthy_nor_dead() {
        // A slow-but-alive backend eventually hits the (capped) ceiling and
        // returns Timeout — the caller then falls back to the default backend.
        let r = poll_until_ready(
            Duration::from_secs(90),
            Duration::from_secs(2),
            UNIT_DEATH_GRACE,
            || async { false },
            |_past_grace| async { false },
        )
        .await;
        assert_eq!(r, Err(NotReady::Timeout));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_treats_unknown_gc_state_as_dead_only_after_grace() {
        // The `systemd-run --collect` crash-then-GC case: the unit reads
        // `unknown` from the very FIRST poll (mocked here as "dead iff
        // past_grace", exactly unit_dead's rule for the unknown/empty state).
        // It must NOT false-fail during the launch-race grace window, but must
        // then fail well before the full ceiling — not burn it like the bug did.
        let start = Instant::now();
        let r = poll_until_ready(
            Duration::from_secs(90),
            Duration::from_secs(2),
            Duration::from_secs(6),
            || async { false },                     // never healthy
            |past_grace| async move { past_grace }, // unknown ⇒ dead iff past grace
        )
        .await;
        assert_eq!(r, Err(NotReady::UnitDead));
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_secs(6),
            "must not false-fail a still-registering unit before the grace window"
        );
        assert!(
            elapsed < Duration::from_secs(90),
            "must catch the crash-then-GC unknown state well before the ceiling"
        );
    }

    #[test]
    fn unit_dead_decision_discriminates_by_systemd_is_init() {
        // Recognized real dead states are conclusive regardless of is_init or
        // grace (a crashed unit prints these on a NONZERO is-active exit).
        assert!(unit_dead_decision("failed", false, false));
        assert!(unit_dead_decision("failed", true, false));
        assert!(unit_dead_decision("inactive", false, true));
        assert!(unit_dead_decision("deactivating", true, false));

        // T2 restored — real systemd host: a --collect unit gone `unknown`/empty
        // after the grace genuinely crashed → DEAD, exit-status-independent.
        assert!(unit_dead_decision("unknown", true, true));
        assert!(unit_dead_decision("", true, true));
        // …but never before the grace (launch-race window).
        assert!(!unit_dead_decision("unknown", true, false));
        assert!(!unit_dead_decision("", true, false));

        // T3 preserved — NOT systemd-init (container/CI/non-PID-1): unknown/empty
        // is INDETERMINATE, never dead-by-unit-state even past the grace, so a
        // healthy-but-slow launch is never false-failed (falls to health/timeout).
        assert!(!unit_dead_decision("unknown", false, true));
        assert!(!unit_dead_decision("", false, true));

        // Live states are never dead, either way.
        assert!(!unit_dead_decision("active", true, true));
        assert!(!unit_dead_decision("activating", true, true));
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_systemctl_is_indeterminate_and_loop_still_hits_ceiling() {
        // T4: a stalled `systemctl is-active` must NOT block the loop or be read
        // as death. Model the stall exactly as unit_dead bounds it: the call
        // hangs, but the SYSTEMCTL_CALL_TIMEOUT wrap resolves it INDETERMINATE
        // (false) — so the loop keeps polling and terminates cleanly at the
        // ceiling instead of hanging past lumina's client timeout.
        let start = Instant::now();
        let r = poll_until_ready(
            Duration::from_secs(90),
            Duration::from_secs(2),
            UNIT_DEATH_GRACE,
            || async { false }, // never healthy
            |_past_grace| async {
                let hang = std::future::pending::<()>();
                // Ok(()) never happens ⇒ times out ⇒ indeterminate ⇒ not dead.
                tokio::time::timeout(SYSTEMCTL_CALL_TIMEOUT, hang)
                    .await
                    .is_ok()
            },
        )
        .await;
        assert_eq!(r, Err(NotReady::Timeout));
        // Ceiling honored despite the stalling unit check on every iteration.
        assert!(start.elapsed() >= Duration::from_secs(90));
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "must not blow past lumina's client timeout even if dbus hangs"
        );
    }

    #[test]
    // PCON-08: mutates the process-global `CHORD_ENSURE_UP_TIMEOUT_SECS` that
    // `crate::config::ensure_up_timeout_secs()` reads; joins the `intake_env`
    // serial group so no parallel test observes a half-applied value.
    #[serial_test::serial(intake_env)]
    fn ensure_up_timeout_is_clamped_below_client_timeout() {
        // The effective ceiling can never reach lumina's 120s client timeout,
        // no matter how large the env is set (T1).
        std::env::set_var("CHORD_ENSURE_UP_TIMEOUT_SECS", "180");
        assert_eq!(
            crate::config::ensure_up_timeout_secs(),
            crate::config::ENSURE_UP_TIMEOUT_HARD_CAP
        );
        assert!(crate::config::ENSURE_UP_TIMEOUT_HARD_CAP < 120);
        // A smaller-than-cap value passes through unchanged.
        std::env::set_var("CHORD_ENSURE_UP_TIMEOUT_SECS", "30");
        assert_eq!(crate::config::ensure_up_timeout_secs(), 30);
        std::env::remove_var("CHORD_ENSURE_UP_TIMEOUT_SECS");
    }
}
