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

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
    // Model load on the GPU can take a while; poll until healthy.
    wait_healthy(&backend.url, Duration::from_secs(180)).await
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

async fn wait_healthy(base: &str, max: Duration) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < max {
        if health_ok(base).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(format!("backend at {base} did not become healthy within {max:?}"))
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
}
