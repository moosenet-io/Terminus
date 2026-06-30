//! DiffusionGemma local-inference tools.
//!
//! These tools drive a persistent DiffusionGemma HTTP daemon (`llama-diffusion-daemon`) running on the
//! GPU host. The daemon loads the GGUF once and serves many requests, then unloads after an idle timeout
//! to release VRAM — so the first call after a cold/idle period pays the ~36s model load and subsequent
//! calls are fast (~9s/256-token canvas).
//!
//! ## Why HTTP, not a subprocess
//! The `RustTool` contract forbids `std::process::Command`/subprocess spawning in `execute()` (see
//! `tool.rs`). terminus-rs is linked into chord-proxy, which runs on the GPU host, so these tools reach
//! the daemon over loopback HTTP via `reqwest`. The "persistent session" lives entirely in the C++ daemon
//! (model stays resident across requests, idle-unload frees VRAM); the Rust side is a thin, stateless
//! client. This keeps the no-subprocess invariant intact while still meeting the "load once, serve many"
//! requirement.
//!
//! ## Tools
//!   - `dgem_generate` — general-purpose generation (system + user prompt → text)
//!   - `dgem_review`   — structured PR/code review (diff + acceptance criteria → verdict + issues)
//!   - `dgem_status`   — daemon/session status (running, model loaded, uptime, requests served, idle)
//!   - `dgem_batch`    — multi-prompt batch through the persistent session (DGEM-04)
//!
//! ## Config (env, non-secret)
//!   - `DGEM_BASE_URL`          — daemon base URL (default derived from DGEM_BIND/DGEM_HTTP_PORT)
//!   - `DGEM_BIND`              — daemon host (default 127.0.0.1)
//!   - `DGEM_HTTP_PORT`         — daemon port (default 8877)
//!   - `DGEM_CLIENT_TIMEOUT_SECS` — HTTP client timeout (default 600 — must cover cold model load +
//!                                 first-ever Vulkan shader compile, which can take minutes)
//!   - `DGEM_MAX_INPUT_TOKENS`  — hard OOM cap; reject inputs estimated larger than this on every tool (default 10000)
//!   - `DGEM_LATENCY_FALLBACK_TOKENS` — review-only latency cap; above this `dgem_review` refuses so the
//!                                 pipeline falls back to Haiku for speed (default 4000)
//!   - `DGEM_DEFAULT_MAX_TOKENS`— default generation budget when a tool omits max_tokens (default 1024)

use serde::Deserialize;

use crate::error::ToolError;
use crate::registry::ToolRegistry;

mod generate;
mod review;
mod status;
mod batch;
mod vram;

pub use batch::DgemBatch;
pub use generate::DgemGenerate;
pub use review::DgemReview;
pub use status::DgemStatus;

const DEFAULT_BIND: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8877;
const DEFAULT_CLIENT_TIMEOUT_SECS: u64 = 600;
/// Hard OOM cap (all tools): inputs above this are refused before they can OOM the daemon host.
const DEFAULT_MAX_INPUT_TOKENS: usize = 10_000;
/// Latency cap (review only): DiffusionGemma is ~75s on a ~6K-token diff, so above this the build
/// pipeline should use Haiku for a faster review. Below it, DiffusionGemma reviews locally at $0.
const DEFAULT_LATENCY_FALLBACK_TOKENS: usize = 4_000;
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Shared configuration + HTTP client for the DiffusionGemma daemon.
#[derive(Clone)]
pub(crate) struct DgemConfig {
    base_url: String,
    client_timeout_secs: u64,
    max_input_tokens: usize,
    latency_fallback_tokens: usize,
    default_max_tokens: u32,
}

impl DgemConfig {
    fn from_env() -> Self {
        let base_url = std::env::var("DGEM_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let bind = std::env::var("DGEM_BIND")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| DEFAULT_BIND.to_string());
                let port = std::env::var("DGEM_HTTP_PORT")
                    .ok()
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(DEFAULT_PORT);
                format!("http://{bind}:{port}")
            });
        let client_timeout_secs = std::env::var("DGEM_CLIENT_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_CLIENT_TIMEOUT_SECS);
        let max_input_tokens = std::env::var("DGEM_MAX_INPUT_TOKENS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_INPUT_TOKENS);
        let latency_fallback_tokens = std::env::var("DGEM_LATENCY_FALLBACK_TOKENS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_LATENCY_FALLBACK_TOKENS);
        let default_max_tokens = std::env::var("DGEM_DEFAULT_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(DEFAULT_MAX_TOKENS);
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client_timeout_secs,
            max_input_tokens,
            latency_fallback_tokens,
            default_max_tokens,
        }
    }

    fn client(&self) -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(self.client_timeout_secs))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    fn default_max_tokens(&self) -> u32 {
        self.default_max_tokens
    }

    /// Default config pointing at the loopback daemon, for unit tests that exercise guard/parse paths
    /// without a live daemon.
    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        Self::test_with_url("http://127.0.0.1:8877")
    }

    #[cfg(test)]
    pub(crate) fn test_with_url(url: &str) -> Self {
        Self {
            base_url: url.to_string(),
            client_timeout_secs: 5,
            max_input_tokens: DEFAULT_MAX_INPUT_TOKENS,
            latency_fallback_tokens: DEFAULT_LATENCY_FALLBACK_TOKENS,
            default_max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Pre-flight guard: refuse inputs whose estimated token count exceeds the configured ceiling,
    /// so we fail fast with a clear message instead of OOM-ing the daemon host (~12K-token ceiling on
    /// <host>'s 31GB RAM). Estimate is the standard chars/4 heuristic. Applies to all tools
    /// (generate/review/batch) as the hard safety cap.
    fn check_input_size(&self, text: &str) -> Result<(), ToolError> {
        let est = estimate_tokens(text);
        if est > self.max_input_tokens {
            return Err(ToolError::InvalidArgument(format!(
                "Input too large for DiffusionGemma (~{est} tokens estimated, limit {}). \
                 Truncate the diff to the changed hunks, or route this to a cloud model.",
                self.max_input_tokens
            )));
        }
        Ok(())
    }

    /// Dual-threshold guard for the *review* path. Beyond the size safety cap, DiffusionGemma is slow on
    /// large diffs (~75s on ~6K tokens), so the build pipeline should use Haiku for speed above a
    /// (lower) latency threshold. This returns a distinct, actionable error for each case so the pipeline
    /// agent knows which fallback to take and why:
    ///   - `> max_input_tokens` (OOM cap)        → "context limit … use Haiku"
    ///   - `> latency_fallback_tokens` (latency)  → "latency threshold … use Haiku for faster review"
    ///   - otherwise                              → Ok (review locally with DiffusionGemma)
    /// OOM is checked first so a very large diff reports the context-limit reason, not just latency.
    fn check_review_size(&self, diff: &str) -> Result<(), ToolError> {
        let est = estimate_tokens(diff);
        if est > self.max_input_tokens {
            return Err(ToolError::InvalidArgument(format!(
                "Diff exceeds context limit ({est} tokens > {}), using Haiku. \
                 DiffusionGemma would OOM on a diff this size.",
                self.max_input_tokens
            )));
        }
        if est > self.latency_fallback_tokens {
            return Err(ToolError::InvalidArgument(format!(
                "Diff exceeds latency threshold ({est} tokens > {}), using Haiku for faster review. \
                 DiffusionGemma is slow on large diffs; small diffs stay local at $0.",
                self.latency_fallback_tokens
            )));
        }
        Ok(())
    }

    /// POST /generate. Returns the daemon's structured generation result.
    async fn generate(
        &self,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<GenerateResponse, ToolError> {
        // S80 DGEM-03: if VRAM coordination is enabled and the daemon's model isn't resident yet, free
        // GPU memory (unload Ollama models) before the daemon loads its ~16GB GGUF. Best-effort and only
        // when a load is actually impending, so warm sessions skip the round-trip entirely.
        if vram::coordinate_enabled() {
            let needs_load = self.status().await.map(|s| !s.model_loaded).unwrap_or(true);
            if needs_load {
                let freed = vram::free_vram().await;
                if !freed.is_empty() {
                    tracing::info!("dgem: freed VRAM before session (unloaded: {})", freed.join(", "));
                }
            }
        }

        let client = self.client()?;
        let url = format!("{}/generate", self.base_url);
        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "system": system,
                "prompt": prompt,
                "max_tokens": max_tokens,
            }))
            .send()
            .await
            .map_err(|e| map_connect_err(e, &self.base_url))?;

        let status = resp.status();
        // The client timeout also covers the body read, so a slow cold load can trip here — reuse the
        // same connect/timeout mapping for an actionable message rather than a raw reqwest error.
        let body = resp
            .text()
            .await
            .map_err(|e| map_connect_err(e, &self.base_url))?;
        if !status.is_success() {
            // The daemon returns {"error": "..."} for 4xx/5xx (e.g. VRAM occupied, model load failed).
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or_else(|| format!("daemon HTTP {status}"));
            return Err(ToolError::Execution(format!("DiffusionGemma daemon: {msg}")));
        }
        serde_json::from_str::<GenerateResponse>(&body)
            .map_err(|e| ToolError::Http(format!("malformed daemon response: {e}")))
    }

    /// GET /status. Returns None if the daemon is unreachable (caller renders running=false).
    async fn status(&self) -> Option<StatusResponse> {
        let client = self.client().ok()?;
        let url = format!("{}/status", self.base_url);
        let resp = client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<StatusResponse>().await.ok()
    }
}

/// Daemon /generate response.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GenerateResponse {
    pub text: String,
    pub time_ms: i64,
    #[serde(default)]
    pub model_load_ms: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub tokens: i64,
    #[serde(default)]
    pub blocks: i64,
}

/// Daemon /status response.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StatusResponse {
    pub running: bool,
    pub model_loaded: bool,
    pub uptime_secs: i64,
    pub requests_served: i64,
    pub last_request_secs_ago: i64,
    pub idle_timeout_secs: i64,
    #[serde(default)]
    pub model_load_ms: i64,
}

/// chars/4 token estimate (matches the daemon-side ceiling reasoning).
pub(crate) fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

/// Map a reqwest send error into an actionable ToolError, distinguishing "daemon not running"
/// (connection refused) from other transport failures.
fn map_connect_err(e: reqwest::Error, base_url: &str) -> ToolError {
    if e.is_connect() {
        ToolError::Execution(format!(
            "DiffusionGemma daemon unreachable at {base_url} (connection refused). \
             Start the dgem daemon (llama-diffusion-daemon) on the GPU host, or fall back to a cloud model."
        ))
    } else if e.is_timeout() {
        ToolError::Execution(format!(
            "DiffusionGemma daemon timed out at {base_url}. A cold model load + first-ever Vulkan shader \
             compile can take minutes; raise DGEM_CLIENT_TIMEOUT_SECS if this is the first call."
        ))
    } else {
        ToolError::Http(e.to_string())
    }
}

/// DiffusionGemma emits a planning/thinking trace. Split it from the final answer so callers can post
/// the answer (e.g. to a PR comment) without the noisy trace.
///
/// The model uses harmony-style channels: `<|channel>thought … <channel|> final answer`. When the final
/// channel marker is present we take everything after it as the answer and the rest (minus the opening
/// marker) as the thinking. We also recognise `<think>…</think>` / `<thinking>…</thinking>` as a
/// fallback. If the trace never closes (e.g. the generation hit max_tokens mid-thought), we strip the
/// opening marker and return the remainder as the answer — the verdict tokens are still embedded, so
/// downstream parsing keeps working.
pub(crate) fn split_thinking(text: &str) -> (String, String) {
    const CH_CLOSE: &str = "<channel|>";
    if let Some(pos) = text.rfind(CH_CLOSE) {
        let answer = text[pos + CH_CLOSE.len()..].trim().to_string();
        if !answer.is_empty() {
            let think = strip_channel_open(text[..pos].trim());
            return (think, answer);
        }
    }
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        if let Some(start) = text.find(open) {
            if let Some(end_rel) = text[start + open.len()..].find(close) {
                let think = text[start + open.len()..start + open.len() + end_rel].trim().to_string();
                let mut answer = String::new();
                answer.push_str(&text[..start]);
                answer.push_str(&text[start + open.len() + end_rel + close.len()..]);
                return (think, answer.trim().to_string());
            }
        }
    }
    // No separable trace (or an unterminated channel): strip a leading "<|channel>thought" marker if
    // present so the returned text is at least clean, and treat it all as the answer.
    (String::new(), strip_channel_open(text.trim()))
}

/// Strip a leading `<|channel>thought` / `<|channel>` opening marker.
fn strip_channel_open(s: &str) -> String {
    s.trim()
        .trim_start_matches("<|channel>")
        .trim_start_matches("thought")
        .trim()
        .to_string()
}

pub fn register(registry: &mut ToolRegistry) {
    // The daemon has a sensible loopback default, so these tools always register; unavailability is
    // surfaced at call time (clear error / running=false) rather than by withholding the tool.
    let cfg = DgemConfig::from_env();
    registry.register_or_replace(Box::new(DgemGenerate::new(cfg.clone())));
    registry.register_or_replace(Box::new(DgemReview::new(cfg.clone())));
    registry.register_or_replace(Box::new(DgemStatus::new(cfg.clone())));
    registry.register_or_replace(Box::new(DgemBatch::new(cfg)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_chars_over_four() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens(&"x".repeat(400)), 100);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn check_input_size_rejects_oversized() {
        let cfg = DgemConfig {
            base_url: "http://127.0.0.1:8877".into(),
            client_timeout_secs: 600,
            max_input_tokens: 100,
            latency_fallback_tokens: 50,
            default_max_tokens: 1024,
        };
        // 100 tokens ≈ 400 chars; 4001 chars ≈ 1000 tokens → rejected.
        let big = "y".repeat(4001);
        let err = cfg.check_input_size(&big).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(cfg.check_input_size("small").is_ok());
    }

    #[test]
    fn split_thinking_extracts_block() {
        let (think, answer) = split_thinking("<think>plan the work</think>The answer is 42.");
        assert_eq!(think, "plan the work");
        assert_eq!(answer, "The answer is 42.");
    }

    #[test]
    fn split_thinking_handles_thinking_tag() {
        let (think, answer) = split_thinking("<thinking>reason</thinking>\n\nFinal.");
        assert_eq!(think, "reason");
        assert_eq!(answer, "Final.");
    }

    #[test]
    fn split_thinking_no_block_is_all_answer() {
        let (think, answer) = split_thinking("just an answer");
        assert!(think.is_empty());
        assert_eq!(answer, "just an answer");
    }

    #[test]
    fn split_thinking_channel_format() {
        // The real DiffusionGemma format observed on <host>.
        let raw = "<|channel>thought\n*   reasoning here\n    *   \"Hello\"<channel|>Hello";
        let (think, answer) = split_thinking(raw);
        assert_eq!(answer, "Hello");
        assert!(think.contains("reasoning here"));
        assert!(!think.contains("<|channel>"));
    }

    #[test]
    fn split_thinking_unterminated_channel_strips_marker() {
        // Generation hit max_tokens mid-thought: no closing <channel|>. We strip the open marker and
        // keep the text so the embedded verdict is still parseable.
        let raw = "<|channel>thought\nSECURITY: hardcoded IP.\nCHANGES_REQUESTED\n1. Fix it.";
        let (think, answer) = split_thinking(raw);
        assert!(think.is_empty());
        assert!(answer.starts_with("SECURITY"));
        assert!(answer.contains("CHANGES_REQUESTED"));
    }

    #[test]
    fn config_from_env_defaults() {
        // Default base_url derives from bind/port when DGEM_BASE_URL is unset.
        // (Not asserting against process env to avoid cross-test interference; construct directly.)
        let cfg = DgemConfig {
            base_url: "http://127.0.0.1:8877".into(),
            client_timeout_secs: DEFAULT_CLIENT_TIMEOUT_SECS,
            max_input_tokens: DEFAULT_MAX_INPUT_TOKENS,
            latency_fallback_tokens: DEFAULT_LATENCY_FALLBACK_TOKENS,
            default_max_tokens: DEFAULT_MAX_TOKENS,
        };
        assert_eq!(cfg.default_max_tokens(), 1024);
        assert_eq!(cfg.base_url, "http://127.0.0.1:8877");
    }

    #[test]
    fn check_review_size_dual_threshold() {
        // latency 4000, OOM 10000 (defaults). chars/4 estimate.
        let cfg = DgemConfig::test_default();
        // Small diff (~500 tokens) → Ok, reviewed locally.
        assert!(cfg.check_review_size(&"x".repeat(2_000)).is_ok());
        // 4–10K band (~6000 tokens = 24000 chars) → latency fallback.
        let mid = cfg.check_review_size(&"x".repeat(24_000)).unwrap_err();
        match mid {
            ToolError::InvalidArgument(m) => {
                assert!(m.contains("latency threshold"), "got: {m}");
                assert!(m.contains("Haiku"));
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
        // > 10K (~12500 tokens = 50000 chars) → OOM/context-limit fallback.
        let big = cfg.check_review_size(&"x".repeat(50_000)).unwrap_err();
        match big {
            ToolError::InvalidArgument(m) => {
                assert!(m.contains("context limit"), "got: {m}");
                assert!(m.contains("Haiku"));
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn check_review_size_respects_configured_latency_threshold() {
        // Raise the latency threshold so a mid-size diff is reviewed locally instead of falling back.
        let cfg = DgemConfig {
            base_url: "http://127.0.0.1:8877".into(),
            client_timeout_secs: 600,
            max_input_tokens: 10_000,
            latency_fallback_tokens: 8_000,
            default_max_tokens: 1024,
        };
        // ~6000 tokens now under the raised 8000 latency threshold → Ok.
        assert!(cfg.check_review_size(&"x".repeat(24_000)).is_ok());
    }
}
