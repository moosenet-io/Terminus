//! Backend-aware inference with normalized metrics (P5).
//!
//! `infer_with_metrics` resolves a model's tagged **backend** (from the chord
//! model-registry file), runs the request against that backend's URL using the
//! backend's wire protocol, and returns a single normalized [`InferMetrics`]
//! (throughput, TTFT, tokens, VRAM, oom/error) regardless of backend kind.
//!
//! This is the shared function that both (a) the test harness calls in-process
//! to profile each model on its **correct hardware**, and (b) chord exposes at
//! `POST /v1/infer` so external clients get the same metrics. Keeping it in
//! `terminus-rs` (the lower crate) lets chord-proxy call it without a dependency
//! cycle.
//!
//! Step-2 scope: the **Ollama** wire path (parity with `context::generate`). The
//! `llama-server` (GPU) path is added in step 5; until then a model tagged to a
//! llama-server backend returns a clear, non-silent error.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::intake::context;

/// Normalized per-inference metrics, backend-agnostic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InferMetrics {
    pub response: String,
    pub throughput_tok_per_sec: Option<f64>,
    /// Time to first token (≈ prompt-eval/prefill duration), ms.
    pub ttft_ms: Option<i32>,
    pub total_time_ms: Option<i32>,
    pub response_tokens: Option<i32>,
    /// GPU VRAM in use on the device, MB (sysfs; None if unreadable / CPU host).
    pub vram_mb: Option<u64>,
    pub oom: bool,
    pub error: Option<String>,
    /// Backend that served the request (for attribution).
    pub backend: Option<String>,
    /// Hardware the backend runs on (`"gpu"` | `"cpu"`).
    pub hardware: Option<String>,
    /// MINT-DIFF-01: fixed canvas blocks generated, `kind == "daemon"`
    /// (diffusion) backends only. Deliberately a SEPARATE field from
    /// `response_tokens` — a diffusion model's "block" is not a token, and
    /// conflating them would let a block-count silently masquerade as a
    /// token-throughput number downstream. `None` for every other backend kind.
    pub blocks: Option<i64>,
}

/// How to spawn a unit-less on-demand backend (the generic `llama-gpu`).
#[derive(Debug, Clone, Deserialize)]
pub struct LaunchSpec {
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_model_arg")]
    pub model_arg: String,
}

fn default_model_arg() -> String {
    "-m".to_string()
}

/// A model's resolved backend (with the fields lifecycle needs).
#[derive(Debug, Clone)]
pub struct ResolvedBackend {
    pub name: String,
    pub url: String,
    pub kind: String,     // "ollama" | "llama-server" | "daemon" | "openai" (BT-01)
    pub hardware: String, // "gpu" | "cpu"
    pub always_on: bool,
    pub unit: Option<String>,
    pub launch: Option<LaunchSpec>,
    /// BT-01: env var holding the bearer token for an OpenAI-compatible backend
    /// (OpenRouter, or Chord's JWT). `None` for unauthenticated local serves
    /// (lemonade/vLLM). Read at call time, never stored/logged.
    pub api_key_env: Option<String>,
    /// The requesting model's local Ollama root (for blob resolution), if known.
    pub model_local_path: Option<String>,
    /// Direct GGUF path for a non-Ollama model (first shard if sharded); when set
    /// it is used for `-m` instead of Ollama-blob resolution.
    pub model_gguf_path: Option<String>,
}

// ── Minimal read-only view of the chord registry file (no chord-proxy dep) ──

#[derive(Deserialize, Default)]
struct RegFile {
    #[serde(default)]
    models: HashMap<String, RegModel>,
    #[serde(default)]
    backends: HashMap<String, RegBackend>,
}

#[derive(Deserialize, Default)]
struct RegModel {
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    local_path: Option<String>,
    #[serde(default)]
    gguf_path: Option<String>,
}

#[derive(Deserialize)]
struct RegBackend {
    url: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    hardware: Option<String>,
    #[serde(default)]
    always_on: bool,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    launch: Option<LaunchSpec>,
    #[serde(default)]
    api_key_env: Option<String>,
}

/// Chord model-registry path, from `MODEL_REGISTRY_PATH`. No compiled-in
/// default (PII remediation 2026-07: the old fallback was a real
/// sweep-harness host path). `None` when unset is treated exactly like the
/// pre-existing "registry file absent" case below (same graceful-degrade
/// fallback to the default Ollama backend) — this is not a security
/// boundary, just a discovery path, so there is no new failure mode here,
/// only the removal of a compiled-in real path.
fn registry_path() -> Option<String> {
    std::env::var("MODEL_REGISTRY_PATH").ok().filter(|s| !s.trim().is_empty())
}

/// Process-global backend override for profiling: when set, EVERY model resolves
/// to this backend regardless of its tag. Lets the harness evaluate a model on a
/// SPECIFIC hardware (e.g. the same model on `llama-gpu` AND `ollama`) for the
/// both-CPU-and-GPU sizing comparison. Safe because intake runs are sequential;
/// set it before a suite and clear it after.
static BACKEND_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Set (or clear with `None`) the global backend override.
pub fn set_backend_override(backend: Option<String>) {
    if let Ok(mut g) = BACKEND_OVERRIDE.lock() {
        *g = backend;
    }
}

fn backend_override() -> Option<String> {
    BACKEND_OVERRIDE.lock().ok().and_then(|g| g.clone())
}

/// MINT Phase 6 (`--remote`): process-global remote Ollama base-URL override.
/// When set, redirects ONLY the default primary Ollama backend's base URL to a
/// different host (for cross-host inference comparison — e.g. profiling the
/// same model served on another GPU host) — see [`apply_remote_override`] for
/// the exact composition rule. Same lifecycle contract as [`set_backend_override`]:
/// intake runs are sequential, so set it before a suite and clear it after.
/// It does NOT touch `gpu_authority`'s host-local lock — the harness still runs
/// on (and locks) its own GPU; only the inference target URL moves.
static REMOTE_OLLAMA_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Set (or clear with `None`) the global remote-Ollama-URL override.
pub fn set_remote_ollama_url(url: Option<String>) {
    if let Ok(mut g) = REMOTE_OLLAMA_URL.lock() {
        *g = url;
    }
}

fn remote_ollama_url() -> Option<String> {
    REMOTE_OLLAMA_URL.lock().ok().and_then(|g| g.clone())
}

/// Compose the MINT Phase 6 `--remote` override onto an already-resolved
/// backend. The override redirects ONLY the default primary Ollama backend —
/// the one untagged models and the registry-absent fallback resolve to,
/// identified by `name == "ollama"` AND `kind == "ollama"`. A model pinned to
/// ANY other backend — a differently-named ollama backend (e.g. `ollama-cpu`)
/// or a non-ollama kind (e.g. `llama-server`) — keeps its own registry routing
/// untouched, so `--remote` never silently reroutes a llama-server model or a
/// deliberately-pinned CPU pass onto a remote host. A blank/whitespace remote
/// URL is a no-op. Pure (no globals/env) so the rule is unit-testable.
pub fn apply_remote_override(mut backend: ResolvedBackend, remote: Option<&str>) -> ResolvedBackend {
    if let Some(url) = remote {
        let url = url.trim().trim_end_matches('/');
        if !url.is_empty() && backend.name == "ollama" && backend.kind == "ollama" {
            backend.url = url.to_string();
        }
    }
    backend
}

/// Resolve a model's backend from the registry file. Falls back to the default
/// Ollama base (`context::ollama_base`) when the file is absent, legacy-format,
/// or the model/backend is untagged — so behavior is unchanged until models are
/// tagged.
pub fn resolve_backend(model: &str) -> ResolvedBackend {
    let resolved = resolve_backend_at(
        model,
        registry_path().as_deref().unwrap_or(""),
        &context::ollama_base(),
        backend_override().as_deref(),
    );
    // MINT Phase 6: redirect the default primary ollama backend to the remote
    // inference target when `--remote`/`MINT_REMOTE_OLLAMA_URL` is active. This
    // is the single choke point every dispatch path (infer_with_metrics,
    // embed_with_metrics, model_available) already funnels through, so the
    // override reaches them all without threading a new param through each.
    apply_remote_override(resolved, remote_ollama_url().as_deref())
}

/// Resolve against an explicit registry path + fallback URL (no env reads) —
/// the testable core of [`resolve_backend`]. `override_backend`, when set, forces
/// that backend for any model (the both-hardware profiling path).
pub fn resolve_backend_at(
    model: &str,
    registry_path: &str,
    fallback_url: &str,
    override_backend: Option<&str>,
) -> ResolvedBackend {
    let fallback = || ResolvedBackend {
        name: "ollama".to_string(),
        url: fallback_url.trim_end_matches('/').to_string(),
        kind: "ollama".to_string(),
        hardware: "cpu".to_string(),
        always_on: true,
        unit: None,
        launch: None,
        api_key_env: None,
        model_local_path: None,
        model_gguf_path: None,
    };

    let text = match std::fs::read_to_string(registry_path) {
        Ok(t) => t,
        Err(_) => return fallback(),
    };
    let reg: RegFile = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(_) => return fallback(),
    };
    let model_local_path = reg.models.get(model).and_then(|m| m.local_path.clone());
    let model_gguf_path = reg.models.get(model).and_then(|m| m.gguf_path.clone());
    // Override (forced backend) → model's tag → the primary "ollama" if defined.
    let name = override_backend
        .map(|s| s.to_string())
        .or_else(|| reg.models.get(model).and_then(|m| m.backend.clone()))
        .or_else(|| reg.backends.contains_key("ollama").then(|| "ollama".to_string()));

    match name.and_then(|n| reg.backends.get(&n).map(|b| (n, b))) {
        Some((n, b)) => ResolvedBackend {
            name: n,
            url: b.url.trim_end_matches('/').to_string(),
            kind: b.kind.clone().unwrap_or_else(|| "ollama".to_string()),
            hardware: b.hardware.clone().unwrap_or_else(|| "cpu".to_string()),
            always_on: b.always_on,
            unit: b.unit.clone(),
            launch: b.launch.clone(),
            api_key_env: b.api_key_env.clone(),
            model_local_path,
            model_gguf_path,
        },
        None => fallback(),
    }
}

/// All GPU-hardware backends defined in the registry, as `(name, unit)` pairs
/// (unit `None` ⇒ spawned as a transient `chord-<name>` unit). Used by lifecycle
/// GPU arbitration to free the single GPU before starting another GPU backend.
pub fn gpu_backends() -> Vec<(String, Option<String>)> {
    let Some(path) = registry_path() else {
        return Vec::new();
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let reg: RegFile = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    reg.backends
        .into_iter()
        .filter(|(_, b)| b.hardware.as_deref() == Some("gpu"))
        .map(|(name, b)| (name, b.unit))
        .collect()
}

/// Current GPU VRAM-in-use (MB) from sysfs (`mem_info_vram_used`). Best-effort;
/// `None` on a host without an amdgpu card or when unreadable.
pub fn vram_used_mb() -> Option<u64> {
    for n in 0..4 {
        let p = format!("/sys/class/drm/card{n}/device/mem_info_vram_used");
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(bytes) = s.trim().parse::<u64>() {
                return Some(bytes / 1024 / 1024);
            }
        }
    }
    None
}

/// Fast pre-flight: is `model` present in its resolved backend's Ollama
/// registry (`/api/tags`)? HFIX-05: without this, a model missing from
/// ollama's local registry (e.g. temporarily removed during disk cleanup)
/// produced one "model not found" 404 PER CASE — up to 200 wasted rows for a
/// single model — instead of one clean, diagnosable skip. Only meaningful
/// for `kind == "ollama"` backends (a `llama-server` backend resolves a GGUF
/// path directly, not a pull registry, so it always reports available here).
/// Fail-open (`true`) on any transport/parse error — a flaky `/api/tags`
/// must never wrongly skip a model that IS actually available; the existing
/// per-case retry already covers real transient failures once inference is
/// attempted.
pub async fn model_available(model: &str) -> bool {
    let backend = resolve_backend(model);
    if backend.kind != "ollama" {
        return true;
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return true,
    };
    let resp = match client
        .get(format!("{}/api/tags", backend.url))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return true,
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return true,
    };
    tags_contains_model(&body, model)
}

/// Pure core of [`model_available`]: does a parsed `/api/tags` body list
/// `model`? Tolerant of either `name` or `model` as the tag field (both
/// appear across Ollama versions) and of a missing/malformed `models` array
/// (treated as "can't tell" — `true`, matching the fail-open policy).
fn tags_contains_model(body: &serde_json::Value, model: &str) -> bool {
    let Some(models) = body.get("models").and_then(|m| m.as_array()) else {
        return true;
    };
    models.iter().any(|m| {
        m.get("name").and_then(|n| n.as_str()) == Some(model)
            || m.get("model").and_then(|n| n.as_str()) == Some(model)
    })
}

/// Run `model`/`prompt` on its tagged backend and return normalized metrics.
/// Never panics — transport/HTTP/backend errors land in `InferMetrics::error`.
pub async fn infer_with_metrics(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
    timeout: Duration,
) -> InferMetrics {
    let backend = resolve_backend(model);
    let mut m = InferMetrics {
        backend: Some(backend.name.clone()),
        hardware: Some(backend.hardware.clone()),
        ..Default::default()
    };

    // Start the backend on demand (GPU arbitration + model load) if needed.
    if let Err(e) = crate::intake::lifecycle::ensure_up(&backend, model).await {
        m.error = Some(format!("backend '{}' unavailable: {e}", backend.name));
        return m;
    }

    match backend.kind.as_str() {
        "ollama" => {
            let g = context::generate_at(client, &backend.url, model, prompt, timeout).await;
            m.response = g.response;
            m.throughput_tok_per_sec = g.throughput_tok_per_sec;
            m.total_time_ms = g.total_time_ms;
            m.oom = g.oom;
            m.error = g.error;
            if !m.response.is_empty() {
                m.response_tokens = Some(context::estimate_tokens(&m.response) as i32);
            }
        }
        "llama-server" => llama_server_infer(client, &backend.url, prompt, timeout, &mut m).await,
        "daemon" => diffusion_infer(prompt, timeout, &mut m).await,
        // BT-01: any OpenAI-compatible backend — lemonade-coder, vLLM, OpenRouter, or
        // Chord itself. This is what unblocks profiling the variety of backends Chord
        // serves (previously MINT could only reach ollama/llama.cpp/dgem).
        "openai" => {
            let auth = backend
                .api_key_env
                .as_deref()
                .and_then(|k| std::env::var(k).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            openai_infer(
                client,
                &backend.url,
                model,
                prompt,
                timeout,
                auth.as_deref(),
                &mut m,
            )
            .await;
        }
        other => {
            m.error = Some(format!("backend '{}' has unsupported kind '{other}'", backend.name));
        }
    }
    m.vram_mb = vram_used_mb();
    m
}

/// MINT-DIFF-01: the `kind == "daemon"` arm of [`infer_with_metrics`] —
/// diffusion models (DiffusionGemma / dgem) run as a persistent C++ daemon,
/// not an Ollama/llama-server wire protocol. Mirrors [`llama_server_infer`]'s
/// shape (fill `m` in place, never panic, every failure lands in `m.error`)
/// but dispatches through [`crate::dgem::diffusion_generate`] — the SAME
/// client/config/VRAM-coordination/error-mapping every other dgem tool uses,
/// so this doesn't open a second, divergent HTTP path to the daemon.
///
/// `backend.url` is intentionally NOT used here: the dgem client resolves its
/// own base URL from `DGEM_BASE_URL`/`DGEM_BIND`/`DGEM_HTTP_PORT` (env, sane
/// defaults, never a literal — see `dgem::mod`'s config doc). A future
/// registry entry's `url` field is not required to reach the daemon.
///
/// Diffusion generates in fixed canvas blocks, not a token stream — there is
/// no meaningful `throughput_tok_per_sec` here, so it is deliberately left
/// `None` (`ttft_ms` likewise: the daemon has no separate prefill phase to
/// report). `total_time_ms` is the daemon's own wall-clock `time_ms`
/// (generation only; `model_load_ms` is reported separately and NOT folded
/// in, so a cold-load run doesn't look like a slow generation).
async fn diffusion_infer(prompt: &str, timeout: Duration, m: &mut InferMetrics) {
    // Matches dgem's own DEFAULT_MAX_TOKENS (see `dgem::mod` config doc); kept
    // as a local literal rather than importing dgem's private default const so
    // this arm doesn't reach into dgem's module-private config internals.
    const DIFFUSION_INFER_MAX_TOKENS: u32 = 1024;
    let fut = crate::dgem::diffusion_generate("", prompt, DIFFUSION_INFER_MAX_TOKENS);
    let result = match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => {
            m.error = Some(format!("diffusion daemon timed out after {timeout:?}"));
            return;
        }
    };
    match result {
        Ok(resp) => {
            m.response = resp.text;
            m.total_time_ms = Some(resp.time_ms as i32);
            m.response_tokens = if resp.tokens > 0 { Some(resp.tokens as i32) } else { None };
            m.blocks = if resp.blocks > 0 { Some(resp.blocks) } else { None };
            // No token-stream throughput for a block-diffusion model — see doc above.
            m.throughput_tok_per_sec = None;
            m.ttft_ms = None;
        }
        Err(e) => {
            let msg = e.to_string();
            m.oom = msg.to_lowercase().contains("memory") || msg.to_lowercase().contains("oom");
            m.error = Some(msg);
        }
    }
}

/// Normalized result of a single embedding request via the unified path.
///
/// `embedding` is the dense vector; `dimensionality` is its length; `latency_ms`
/// is the wall-clock round-trip. On any failure (transport, HTTP, parse, or a
/// backend whose kind does not support embeddings) `error` is set and `embedding`
/// is empty — callers never panic and never see a fabricated vector.
#[derive(Debug, Clone, Default)]
pub struct EmbedMetrics {
    pub embedding: Vec<f32>,
    pub dimensionality: usize,
    pub latency_ms: i64,
    pub error: Option<String>,
    /// Backend that served the request (for attribution).
    pub backend: Option<String>,
    /// Hardware the backend runs on (`"gpu"` | `"cpu"`).
    pub hardware: Option<String>,
}

/// Embed `text` for `model` through Chord's unified backend-routing path.
///
/// This is the embedding analogue of [`infer_with_metrics`]: it resolves the
/// model's tagged backend via [`resolve_backend`] (P5 routing) and dispatches to
/// that backend's embeddings endpoint. The dim-6 embeddings sub-harness is a
/// *client* of this function — it NEVER opens an Ollama socket directly.
///
/// Backend support: the Ollama wire path (`/api/embeddings`) is implemented. A
/// `llama-server` or otherwise non-embedding backend kind returns a clear,
/// non-silent error (so a non-embedding candidate is skipped cleanly upstream,
/// not crashed). Never panics — every failure lands in `EmbedMetrics::error`.
pub async fn embed_with_metrics(
    client: &reqwest::Client,
    model: &str,
    text: &str,
    timeout: Duration,
) -> EmbedMetrics {
    let backend = resolve_backend(model);
    let mut m = EmbedMetrics {
        backend: Some(backend.name.clone()),
        hardware: Some(backend.hardware.clone()),
        ..Default::default()
    };

    if let Err(e) = crate::intake::lifecycle::ensure_up(&backend, model).await {
        m.error = Some(format!("backend '{}' unavailable: {e}", backend.name));
        return m;
    }

    match backend.kind.as_str() {
        "ollama" => ollama_embed(client, &backend.url, model, text, timeout, &mut m).await,
        other => {
            // No embeddings wire path for this backend kind: a clear, non-silent
            // error that the runner turns into a clean "skip" (not a crash).
            m.error = Some(format!(
                "backend '{}' kind '{other}' does not support embeddings",
                backend.name
            ));
        }
    }
    m
}

/// Ollama `/api/embeddings` (non-streaming). Fills `m.embedding`/`dimensionality`
/// and measures wall-clock latency. Errors (transport/HTTP/parse/empty vector)
/// land in `m.error`; the vector is never fabricated.
async fn ollama_embed(
    client: &reqwest::Client,
    base: &str,
    model: &str,
    text: &str,
    timeout: Duration,
    m: &mut EmbedMetrics,
) {
    let body = serde_json::json!({ "model": model, "prompt": text });
    let started = std::time::Instant::now();
    let resp = client
        .post(format!("{base}/api/embeddings"))
        .json(&body)
        .timeout(timeout)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            m.error = Some(e.to_string());
            return;
        }
    };
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let txt = resp.text().await.unwrap_or_default();
        m.error = Some(format!("Ollama embeddings HTTP {code}: {txt}"));
        return;
    }
    let latency_ms = started.elapsed().as_millis() as i64;
    let parsed: OllamaEmbedResponse = match resp.json().await {
        Ok(p) => p,
        Err(e) => {
            m.error = Some(format!("embeddings response parse error: {e}"));
            return;
        }
    };
    if let Some(err) = parsed.error {
        m.error = Some(err);
        return;
    }
    if parsed.embedding.is_empty() {
        // A non-embedding model often returns 200 with an empty vector — treat
        // that as "not an embedding model", not a usable result.
        m.error = Some("embeddings endpoint returned an empty vector".to_string());
        return;
    }
    m.dimensionality = parsed.embedding.len();
    m.embedding = parsed.embedding;
    m.latency_ms = latency_ms;
}

/// Subset of Ollama `/api/embeddings` response we consume.
#[derive(Deserialize)]
struct OllamaEmbedResponse {
    #[serde(default)]
    embedding: Vec<f32>,
    #[serde(default)]
    error: Option<String>,
}

/// llama.cpp `llama-server` `/completion` (the server is pinned to one model via
/// `-m`, so no model name is sent). Fills `m` from the `timings` block.
/// BT-01: OpenAI-compatible inference (`POST {base}/v1/chat/completions`). Profiles any
/// backend speaking the OpenAI wire protocol — lemonade-coder (:8081), vLLM, OpenRouter,
/// or Chord's own proxy. Timing is measured LOCALLY (wall clock) because the OpenAI schema
/// carries no llama.cpp-style server timings; token counts come from `usage.completion_tokens`
/// when present, else are estimated. `auth` is an optional bearer token (OpenRouter key /
/// Chord JWT), resolved from the backend's `api_key_env` — never logged.
async fn openai_infer(
    client: &reqwest::Client,
    base: &str,
    model: &str,
    prompt: &str,
    timeout: Duration,
    auth: Option<&str>,
    m: &mut InferMetrics,
) {
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "stream": false,
    });
    let started = std::time::Instant::now();
    let mut req = client
        .post(format!("{}/v1/chat/completions", base.trim_end_matches('/')))
        .json(&body)
        .timeout(timeout);
    if let Some(t) = auth {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            m.oom = msg.to_lowercase().contains("memory") || msg.to_lowercase().contains("oom");
            m.error = Some(msg);
            return;
        }
    };
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let txt = resp.text().await.unwrap_or_default();
        m.oom = code == 500 && txt.to_lowercase().contains("memory");
        m.error = Some(format!("openai HTTP {code}: {txt}"));
        return;
    }
    let v: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            m.error = Some(format!("openai response parse error: {e}"));
            return;
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as i32;
    m.response = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    m.response_tokens = v
        .pointer("/usage/completion_tokens")
        .and_then(|t| t.as_i64())
        .map(|t| t as i32)
        .or_else(|| (!m.response.is_empty()).then(|| context::estimate_tokens(&m.response) as i32));
    m.total_time_ms = Some(elapsed_ms);
    if let (Some(tok), true) = (m.response_tokens, elapsed_ms > 0) {
        m.throughput_tok_per_sec = Some(tok as f64 / (elapsed_ms as f64 / 1000.0));
    }
}

async fn llama_server_infer(
    client: &reqwest::Client,
    base: &str,
    prompt: &str,
    timeout: Duration,
    m: &mut InferMetrics,
) {
    let body = serde_json::json!({
        "prompt": prompt,
        "n_predict": -1,      // until EOS/context; the request timeout bounds it
        "stream": false,
        "cache_prompt": true,
    });
    let resp = client
        .post(format!("{base}/completion"))
        .json(&body)
        .timeout(timeout)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            m.oom = msg.to_lowercase().contains("memory") || msg.to_lowercase().contains("oom");
            m.error = Some(msg);
            return;
        }
    };
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let txt = resp.text().await.unwrap_or_default();
        m.oom = code == 500 && txt.to_lowercase().contains("memory");
        m.error = Some(format!("llama-server HTTP {code}: {txt}"));
        return;
    }
    let parsed: LlamaCompletion = match resp.json().await {
        Ok(p) => p,
        Err(e) => {
            m.error = Some(format!("llama-server response parse error: {e}"));
            return;
        }
    };
    m.response = parsed.content;
    if !m.response.is_empty() {
        m.response_tokens = Some(context::estimate_tokens(&m.response) as i32);
    }
    if let Some(t) = parsed.timings {
        m.throughput_tok_per_sec = t.predicted_per_second;
        m.ttft_ms = t.prompt_ms.map(|v| v as i32);
        m.response_tokens = t.predicted_n.or(m.response_tokens);
        let total = t.prompt_ms.unwrap_or(0.0) + t.predicted_ms.unwrap_or(0.0);
        if total > 0.0 {
            m.total_time_ms = Some(total as i32);
        }
    }
}

/// Subset of llama.cpp `/completion` response we consume.
#[derive(Deserialize)]
struct LlamaCompletion {
    #[serde(default)]
    content: String,
    #[serde(default)]
    timings: Option<LlamaTimings>,
}

#[derive(Deserialize)]
struct LlamaTimings {
    #[serde(default)]
    prompt_ms: Option<f64>,
    #[serde(default)]
    predicted_n: Option<i32>,
    #[serde(default)]
    predicted_ms: Option<f64>,
    #[serde(default)]
    predicted_per_second: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    /// Write `body` to a unique temp file and return its path (avoids env-var
    /// races between parallel tests).
    fn tmp_registry(tag: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("infer-test-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn resolve_falls_back_when_no_registry() {
        let b = resolve_backend_at(
            "anything:latest",
            "/nonexistent/registry.json",
            "http://localhost:11434/",  // pii-test-fixture
            None,
        );
        assert_eq!(b.kind, "ollama");
        assert_eq!(b.url, "http://localhost:11434");  // pii-test-fixture
    }

    #[test]
    fn override_forces_backend_regardless_of_tag() {
        let dir = std::env::temp_dir().join("infer-test-override");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        // Loopback URLs are interpolated (rather than embedded directly in the
        // raw string) so each can carry the repo's pii-test-fixture marker on
        // its own line without corrupting the JSON this gets parsed as.
        let ollama_url = "http://localhost:11434"; // pii-test-fixture
        let llama_gpu_url = "http://localhost:8082"; // pii-test-fixture
        std::fs::write(
            &path,
            format!(
                r#"{{
                "models": {{ "m:1": {{ "backend": "ollama" }} }},
                "backends": {{
                    "ollama": {{ "url": "{ollama_url}", "kind": "ollama", "hardware": "cpu" }},
                    "llama-gpu": {{ "url": "{llama_gpu_url}", "kind": "llama-server", "hardware": "gpu" }}
                }}
            }}"#
            ),
        )
        .unwrap();
        // Tagged ollama, but the override forces llama-gpu.
        let b = resolve_backend_at("m:1", path.to_str().unwrap(), "http://fb", Some("llama-gpu"));
        assert_eq!(b.name, "llama-gpu");
        assert_eq!(b.hardware, "gpu");
    }

    #[test]
    fn resolve_reads_tagged_backend() {
        // Loopback URL interpolated onto its own taggable line — see comment in
        // `override_forces_backend_regardless_of_tag` above for why.
        let llama_gpu_url = "http://localhost:8082/"; // pii-test-fixture
        let path = tmp_registry(
            "reg",
            &format!(
                r#"{{
                "models": {{ "qwen3-coder:30b": {{ "backend": "llama-gpu" }} }},
                "backends": {{ "llama-gpu": {{ "url": "{llama_gpu_url}", "kind": "llama-server", "hardware": "gpu" }} }}
            }}"#
            ),
        );
        let b = resolve_backend_at("qwen3-coder:30b", path.to_str().unwrap(), "http://fallback", None);
        assert_eq!(b.name, "llama-gpu");
        assert_eq!(b.kind, "llama-server");
        assert_eq!(b.hardware, "gpu");
        assert_eq!(b.url, "http://localhost:8082"); // trailing slash trimmed // pii-test-fixture
    }

    #[test]
    fn legacy_flat_registry_resolves_to_fallback() {
        let path = tmp_registry(
            "legacy",
            r#"{"qwen3:8b":{"name":"qwen3:8b","tier":"warm"}}"#,
        );
        let b = resolve_backend_at("qwen3:8b", path.to_str().unwrap(), "http://localhost:11434", None);  // pii-test-fixture
        assert_eq!(b.kind, "ollama"); // legacy format, no tag → fallback
    }

    // ---- MINT Phase 6: --remote composition rule (pure core) ----

    fn ollama_backend(url: &str) -> ResolvedBackend {
        ResolvedBackend {
            name: "ollama".to_string(),
            url: url.to_string(),
            kind: "ollama".to_string(),
            hardware: "gpu".to_string(),
            always_on: true,
            unit: None,
            launch: None,
            api_key_env: None,
            model_local_path: None,
            model_gguf_path: None,
        }
    }

    #[test]
    fn remote_override_redirects_default_ollama_backend() {
        let b = apply_remote_override(ollama_backend("http://127.0.0.1:11434"), Some("http://pvf2:11434"));  // pii-test-fixture
        assert_eq!(b.url, "http://pvf2:11434");
        assert_eq!(b.name, "ollama");
        assert_eq!(b.kind, "ollama");
    }

    #[test]
    fn remote_override_trims_trailing_slash() {
        let b = apply_remote_override(ollama_backend("http://127.0.0.1:11434"), Some("http://pvf2:11434/"));  // pii-test-fixture
        assert_eq!(b.url, "http://pvf2:11434");
    }

    #[test]
    fn remote_override_none_leaves_backend_untouched() {
        let b = apply_remote_override(ollama_backend("http://127.0.0.1:11434"), None);  // pii-test-fixture
        assert_eq!(b.url, "http://127.0.0.1:11434");  // pii-test-fixture
    }

    #[test]
    fn remote_override_blank_is_noop() {
        let b = apply_remote_override(ollama_backend("http://127.0.0.1:11434"), Some("   "));  // pii-test-fixture
        assert_eq!(b.url, "http://127.0.0.1:11434");  // pii-test-fixture
    }

    #[test]
    fn remote_override_skips_pinned_non_default_ollama_backend() {
        // A model pinned to a differently-named ollama backend (e.g. the CPU
        // pass) keeps its own routing — --remote only moves the default GPU
        // "ollama" backend.
        let mut cpu = ollama_backend("http://127.0.0.1:11434");  // pii-test-fixture
        cpu.name = "ollama-cpu".to_string();
        let b = apply_remote_override(cpu, Some("http://pvf2:11434"));
        assert_eq!(b.url, "http://127.0.0.1:11434", "pinned ollama-cpu must not be rerouted");  // pii-test-fixture
    }

    #[test]
    fn remote_override_skips_llama_server_backend() {
        let mut ls = ollama_backend("http://127.0.0.1:8082");  // pii-test-fixture
        ls.name = "llama-gpu".to_string();
        ls.kind = "llama-server".to_string();
        let b = apply_remote_override(ls, Some("http://pvf2:11434"));
        assert_eq!(b.url, "http://127.0.0.1:8082", "llama-server backend must not be rerouted");  // pii-test-fixture
    }

    #[test]
    fn remote_override_reaches_resolve_backend_choke_point() {
        // End-to-end through the single resolution choke point every dispatch
        // path funnels through: with the global set, an untagged model (which
        // resolves to the default "ollama" backend) comes out pointed at the
        // remote URL — proving the override reaches where a request dispatches
        // without needing a live remote Ollama.
        // Loopback URL interpolated onto its own taggable line — see comment in
        // `override_forces_backend_regardless_of_tag` above for why.
        let ollama_url = "http://127.0.0.1:11434"; // pii-test-fixture
        let path = tmp_registry(
            "remote-choke",
            &format!(
                r#"{{
                "models": {{}},
                "backends": {{ "ollama": {{ "url": "{ollama_url}", "kind": "ollama", "hardware": "gpu" }} }}
            }}"#
            ),
        );
        std::env::set_var("MODEL_REGISTRY_PATH", &path);
        set_remote_ollama_url(Some("http://pvf2:11434".to_string()));
        let b = resolve_backend("untagged:latest");
        set_remote_ollama_url(None);
        std::env::remove_var("MODEL_REGISTRY_PATH");
        assert_eq!(b.name, "ollama");
        assert_eq!(b.url, "http://pvf2:11434");
    }

    // ---- HFIX-05: /api/tags membership check (pure core) ----

    #[test]
    fn tags_contains_model_true_when_name_matches() {
        let body = serde_json::json!({"models": [{"name": "gemma3:12b"}, {"name": "qwen3:32b"}]});
        assert!(tags_contains_model(&body, "qwen3:32b"));
    }

    #[test]
    fn tags_contains_model_true_when_model_field_matches() {
        // Some Ollama versions key the tag as "model" instead of "name".
        let body = serde_json::json!({"models": [{"model": "starcoder2:15b"}]});
        assert!(tags_contains_model(&body, "starcoder2:15b"));
    }

    #[test]
    fn tags_contains_model_false_when_absent() {
        let body = serde_json::json!({"models": [{"name": "gemma3:12b"}]});
        assert!(!tags_contains_model(&body, "qwen3-coder:30b"));
    }

    #[test]
    fn tags_contains_model_fails_open_on_malformed_body() {
        // No "models" array at all — can't tell, so don't wrongly skip.
        let body = serde_json::json!({"unexpected": "shape"});
        assert!(tags_contains_model(&body, "qwen3:32b"));
    }

    #[test]
    fn tags_contains_model_empty_list_means_genuinely_absent() {
        let body = serde_json::json!({"models": []});
        assert!(!tags_contains_model(&body, "qwen3:32b"));
    }
}
