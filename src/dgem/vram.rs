//! Best-effort VRAM coordination for the DiffusionGemma session (S80 DGEM-03).
//!
//! DiffusionGemma and the Ollama hot model share the GPU host's (unified) memory. Before the dgem
//! daemon loads its ~16GB GGUF, that VRAM must be free. This module frees it by asking Ollama to unload
//! its currently-loaded models (a request with `keep_alive: 0` evicts a model immediately).
//!
//! Scope and limits (deliberately honest):
//!   - **Gated.** Coordination only runs when `DGEM_COORDINATE_VRAM` is truthy. Default OFF so it never
//!     disrupts a host until an operator opts in — the integration test on pvf1 freed VRAM manually.
//!   - **Graceful.** Every failure (Ollama unreachable, unexpected response) is logged and swallowed;
//!     coordination never fails the dgem call. The daemon's own "VRAM occupied" error remains the
//!     backstop if freeing didn't help.
//!   - **Restore is on-demand, not explicit.** The dgem tool can't observe when the daemon idle-unloads,
//!     so it does not try to reload the prior model itself. Ollama reloads a model on its next request
//!     (lazy load), so the prior hot model is restored naturally the next time a client/Chord uses it.
//!     This is why we return the unloaded names for logging only.
//!
//! Config (env, non-secret):
//!   - `DGEM_COORDINATE_VRAM` — `1`/`true`/`yes` to enable (default off).
//!   - `OLLAMA_BASE_URL` — Ollama base (default `http://127.0.0.1:11434`).

use serde::Deserialize;

const DEFAULT_OLLAMA_BASE: &str = "http://127.0.0.1:11434";

/// Whether VRAM coordination is enabled (`DGEM_COORDINATE_VRAM` truthy).
pub(crate) fn coordinate_enabled() -> bool {
    matches!(
        std::env::var("DGEM_COORDINATE_VRAM").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn ollama_base() -> String {
    std::env::var("OLLAMA_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

#[derive(Deserialize)]
struct PsResponse {
    #[serde(default)]
    models: Vec<PsModel>,
}

#[derive(Deserialize)]
struct PsModel {
    #[serde(default)]
    name: String,
}

/// Free GPU memory for a DiffusionGemma session by unloading every currently-loaded Ollama model.
/// Returns the names that were asked to unload (for logging). Best-effort: returns an empty vec on any
/// error, never panics, never blocks the caller for long (short timeouts).
pub(crate) async fn free_vram() -> Vec<String> {
    let base = ollama_base();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("dgem vram: cannot build client: {e}");
            return Vec::new();
        }
    };

    // Which models are loaded right now?
    let loaded: Vec<String> = match client.get(format!("{base}/api/ps")).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<PsResponse>().await {
            Ok(ps) => ps.models.into_iter().map(|m| m.name).filter(|n| !n.is_empty()).collect(),
            Err(e) => {
                tracing::warn!("dgem vram: /api/ps parse failed: {e}");
                return Vec::new();
            }
        },
        Ok(resp) => {
            tracing::warn!("dgem vram: /api/ps HTTP {}", resp.status());
            return Vec::new();
        }
        Err(e) => {
            tracing::warn!("dgem vram: Ollama unreachable at {base} ({e}); skipping VRAM free");
            return Vec::new();
        }
    };

    // Evict each by requesting generation with keep_alive: 0 (Ollama unloads immediately).
    let mut unloaded = Vec::new();
    for name in loaded {
        let body = serde_json::json!({ "model": name, "keep_alive": 0 });
        match client.post(format!("{base}/api/generate")).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("dgem vram: unloaded Ollama model '{name}' to free VRAM");
                unloaded.push(name);
            }
            Ok(resp) => tracing::warn!("dgem vram: unload '{name}' HTTP {}", resp.status()),
            Err(e) => tracing::warn!("dgem vram: unload '{name}' failed: {e}"),
        }
    }
    unloaded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn coordinate_disabled_by_default() {
        std::env::remove_var("DGEM_COORDINATE_VRAM");
        assert!(!coordinate_enabled());
        std::env::set_var("DGEM_COORDINATE_VRAM", "1");
        assert!(coordinate_enabled());
        std::env::set_var("DGEM_COORDINATE_VRAM", "no");
        assert!(!coordinate_enabled());
        std::env::remove_var("DGEM_COORDINATE_VRAM");
    }

    #[tokio::test]
    #[serial]
    async fn free_vram_graceful_when_ollama_unreachable() {
        // Point at a dead port — must return empty, never panic or error.
        std::env::set_var("OLLAMA_BASE_URL", "http://127.0.0.1:1");
        let out = free_vram().await;
        assert!(out.is_empty());
        std::env::remove_var("OLLAMA_BASE_URL");
    }
}
