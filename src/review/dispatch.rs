//! HTTP dispatch to review providers.
//!
//! `opus`/`codex`/`agy` are CLI-backed providers reached over loopback HTTP via
//! the `review-daemon` binary (`src/bin/review_daemon/`) -- per `src/tool.rs`'s
//! no-subprocess-in-tool contract, this module NEVER spawns a process itself.
//! `nemotron`/`qwen_coder` are dispatched directly to OpenRouter's chat-completions
//! endpoint via `reqwest`.
//!
//! Every function here returns `Result<String, String>` where the `Err` is a
//! human-readable degrade reason (`"unavailable: ..."`) -- callers turn that
//! into a `ProviderResult { error: Some(reason), .. }` rather than failing the
//! whole tool call. See `mod.rs::execute` for how a single provider's failure
//! never blocks the others.

use std::time::Instant;

use serde_json::{json, Value};

use crate::error::ToolError;
use crate::review::free_pool;

/// nemotron's fixed, verified-live OpenRouter model tag. Upgraded from the
/// nano-tier `nvidia/nemotron-nano-9b-v2:free` (real but not frontier-class)
/// to NVIDIA's largest free-tier model, re-confirmed live against
/// `GET https://openrouter.ai/api/v1/models` -- present, free-tier, 550B
/// total params, 1M token context.
pub const NEMOTRON_MODEL: &str = "nvidia/nemotron-3-ultra-550b-a55b:free";
/// qwen_coder's fixed OpenRouter model tag. Replaces the former `deepseek`
/// slot: `deepseek/deepseek-r1:free` no longer exists on OpenRouter (no
/// free-tier deepseek model remains at all), and its would-be successor
/// `deepseek/deepseek-r1` is a paid model -- unacceptable for a slot meant to
/// be free. `qwen/qwen3-coder:free` is re-confirmed live, genuinely
/// free-tier, and frontier-class (480B total params, 1M token context),
/// with a code-specialization that fits this tool's review use case well.
pub const QWEN_CODER_MODEL: &str = "qwen/qwen3-coder:free";

pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:8790"; // pii-test-fixture
const DEFAULT_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// OpenRouter chat-completions endpoint: `OPENROUTER_CHAT_URL` if set, else the
/// default. Configurable rather than hardcoded (parallels `free_pool::models_url`)
/// and lets tests point dispatch at a mock server.
fn openrouter_chat_url() -> String {
    std::env::var("OPENROUTER_CHAT_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OPENROUTER_URL.to_string())
}

/// Config for reaching the review-daemon and OpenRouter. Follows this repo's
/// existing plain-env-var secret convention (see `src/litellm/mod.rs`'s
/// `LITELLM_MASTER_KEY`) rather than inventing a new one.
#[derive(Clone, Debug, Default)]
pub struct ReviewConfig {
    pub daemon_url: String,
    pub daemon_token: Option<String>,
    pub openrouter_key: Option<String>,
}

impl ReviewConfig {
    pub fn from_env() -> Self {
        let daemon_url = std::env::var("REVIEW_DAEMON_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_DAEMON_URL.to_string());
        let daemon_token = std::env::var("REVIEW_DAEMON_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let openrouter_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self { daemon_url, daemon_token, openrouter_key }
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(150))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// Dispatch to an `opus`/`codex`/`agy` provider via the review-daemon's
    /// `POST /dispatch`. Returns `Ok(text)` on a genuine reply, `Err(reason)`
    /// (always prefixed `"unavailable: "`) on anything else -- daemon not
    /// configured, unreachable, or itself reporting a structured error.
    pub async fn dispatch_daemon(&self, provider: &str, prompt: &str) -> Result<String, String> {
        let Some(token) = &self.daemon_token else {
            return Err("unavailable: REVIEW_DAEMON_TOKEN not configured".to_string());
        };
        let client = Self::client().map_err(|e| format!("unavailable: {e}"))?;
        let url = format!("{}/dispatch", self.daemon_url);
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .json(&json!({"provider": provider, "prompt": prompt, "timeout_secs": 120}))
            .send()
            .await
            .map_err(|e| format!("unavailable: daemon unreachable: {e}"))?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("unavailable: malformed daemon response: {e}"))?;

        if status.is_success() {
            body.get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "unavailable: daemon response missing 'text'".to_string())
        } else {
            let kind = body.get("error").and_then(Value::as_str).unwrap_or("other");
            let detail = body.get("detail").and_then(Value::as_str).unwrap_or("");
            Err(format!("unavailable: {kind}: {detail}"))
        }
    }

    /// Dispatch directly to OpenRouter's chat-completions endpoint.
    pub async fn dispatch_openrouter(&self, model: &str, prompt: &str) -> Result<String, String> {
        let Some(key) = &self.openrouter_key else {
            return Err("unavailable: OPENROUTER_API_KEY not configured".to_string());
        };
        let client = Self::client().map_err(|e| format!("unavailable: {e}"))?;
        let resp = client
            .post(openrouter_chat_url())
            .bearer_auth(key)
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": prompt}],
                "stream": false,
            }))
            .send()
            .await
            .map_err(|e| format!("unavailable: openrouter unreachable: {e}"))?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("unavailable: malformed openrouter response: {e}"))?;

        if !status.is_success() {
            let msg = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("openrouter error");
            return Err(format!("unavailable: openrouter http {status}: {msg}"));
        }

        let text = body["choices"][0]["message"]["content"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            Err("unavailable: openrouter returned empty content".to_string())
        } else {
            Ok(text)
        }
    }

    /// Dispatch the `free` provider: draw from the daily-curated free-model pool
    /// (`free_pool`), round-robin, and on a 429 put that model in cooldown and
    /// rotate to the next -- so a free review lands on whatever pooled model
    /// still has quota. Degrades cleanly (never panics): no key -> unavailable;
    /// every model rate-limited -> a clear "all cooling down" error; catalog
    /// unreachable -> keep the last-good pool (best-effort refresh).
    pub async fn dispatch_free_pool(&self, prompt: &str) -> Result<String, String> {
        if self.openrouter_key.is_none() {
            return Err("unavailable: OPENROUTER_API_KEY not configured".to_string());
        }
        self.ensure_pool_fresh().await;

        let pool = free_pool::global_pool();
        // Distinguish "no pool at all" (catalog unreachable / curated to zero)
        // from "pool exists but every model is cooling down" -- these are
        // different, actionable failures.
        let attempts = {
            let p = pool.lock().await;
            if p.is_empty() {
                return Err(
                    "unavailable: free-tier pool is unavailable (catalog empty or unreachable)"
                        .to_string(),
                );
            }
            p.len()
        };
        let mut last_err = "unavailable: free-tier pool produced no usable model".to_string();
        for _ in 0..attempts {
            let model = pool.lock().await.next_available(Instant::now());
            let Some(model) = model else {
                return Err(
                    "unavailable: all free-tier models are rate-limited (cooling down)".to_string(),
                );
            };
            match self.dispatch_openrouter(&model, prompt).await {
                Ok(text) => return Ok(text),
                Err(e) => {
                    // Only a rate-limit earns a cooldown; other per-model errors
                    // just rotate (the cursor already advanced) without penalty.
                    if is_openrouter_rate_limited(&e) {
                        pool.lock().await.mark_rate_limited(&model, Instant::now());
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Refresh the global free-model pool from OpenRouter's public catalog if it
    /// is stale (>= 24h) or empty. Best-effort: any failure keeps the last-good
    /// pool rather than clearing it. The catalog endpoint is unauthenticated, so
    /// this needs no key.
    async fn ensure_pool_fresh(&self) {
        let pool = free_pool::global_pool();
        // Fast path: already fresh -> no refresh, no lock contention.
        if !pool.lock().await.is_stale(Instant::now()) {
            return;
        }
        // Serialize refreshers so concurrent stale callers don't stampede the
        // catalog, then RE-CHECK: another refresher may have already applied a
        // fresh pool while we waited on this lock (avoids clobbering it / a
        // needless second fetch).
        let _refresh = free_pool::refresh_lock().lock().await;
        if !pool.lock().await.is_stale(Instant::now()) {
            return;
        }
        match self.fetch_free_catalog().await {
            Ok(models) if !models.is_empty() => {
                pool.lock().await.set_models(models, Instant::now());
            }
            Ok(_) => tracing::warn!("free_pool: catalog scan curated 0 models -- keeping last-good pool"),
            Err(e) => tracing::warn!("free_pool: catalog refresh failed ({e}) -- keeping last-good pool"),
        }
    }

    /// Fetch + curate the OpenRouter model catalog into a pool of model ids.
    async fn fetch_free_catalog(&self) -> Result<Vec<String>, String> {
        let client = Self::client().map_err(|e| e.to_string())?;
        let resp = client
            .get(free_pool::models_url())
            .send()
            .await
            .map_err(|e| format!("models unreachable: {e}"))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("malformed models response: {e}"))?;
        Ok(free_pool::curate(&body))
    }
}

/// Whether an OpenRouter dispatch error string is a rate-limit (HTTP 429),
/// which the free pool treats as "this model is out of quota, rotate + cool it
/// down" rather than a hard failure.
pub fn is_openrouter_rate_limited(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("http 429") || e.contains("too many requests")
}

/// Map a review-provider name (as accepted by `review_run`'s `providers` list)
/// to its OpenRouter model tag, for the two directly-dispatched providers.
pub fn openrouter_model_for(provider: &str) -> Option<&'static str> {
    match provider {
        "nemotron" => Some(NEMOTRON_MODEL),
        "qwen_coder" => Some(QWEN_CODER_MODEL),
        _ => None,
    }
}

/// Whether `provider` is one of the daemon-backed CLI providers.
pub fn is_daemon_provider(provider: &str) -> bool {
    matches!(provider, "opus" | "codex" | "agy")
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn cfg_for(server: &MockServer, token: &str) -> ReviewConfig {
        ReviewConfig {
            daemon_url: server.base_url(),
            daemon_token: Some(token.to_string()),
            openrouter_key: None,
        }
    }

    #[tokio::test]
    async fn dispatch_daemon_returns_text_on_success() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/dispatch")
                .header("authorization", "Bearer testtoken");
            then.status(200).json_body(json!({"text": "Looks good.\nVERDICT: APPROVE"}));
        });
        let cfg = cfg_for(&server, "testtoken");
        let result = cfg.dispatch_daemon("opus", "review this").await.unwrap();
        assert_eq!(result, "Looks good.\nVERDICT: APPROVE");
        mock.assert();
    }

    #[tokio::test]
    async fn dispatch_daemon_degrades_on_structured_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/dispatch");
            then.status(502).json_body(json!({"error": "binary_not_found", "detail": "'claude' not found"}));
        });
        let cfg = cfg_for(&server, "testtoken");
        let err = cfg.dispatch_daemon("opus", "x").await.unwrap_err();
        assert!(err.contains("unavailable"));
        assert!(err.contains("binary_not_found"));
    }

    #[tokio::test]
    async fn dispatch_daemon_missing_token_never_calls_network() {
        let cfg = ReviewConfig {
            daemon_url: "http://127.0.0.1:1".to_string(), // unroutable if actually dialed
            daemon_token: None,
            openrouter_key: None,
        };
        let err = cfg.dispatch_daemon("opus", "x").await.unwrap_err();
        assert!(err.contains("REVIEW_DAEMON_TOKEN"));
    }

    #[tokio::test]
    async fn dispatch_openrouter_missing_key_never_calls_network() {
        let cfg = ReviewConfig {
            daemon_url: DEFAULT_DAEMON_URL.to_string(),
            daemon_token: None,
            openrouter_key: None,
        };
        let err = cfg.dispatch_openrouter(NEMOTRON_MODEL, "x").await.unwrap_err();
        assert!(err.contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn openrouter_model_for_maps_known_providers() {
        assert_eq!(openrouter_model_for("nemotron"), Some(NEMOTRON_MODEL));
        assert_eq!(openrouter_model_for("qwen_coder"), Some(QWEN_CODER_MODEL));
        assert_eq!(openrouter_model_for("opus"), None);
    }

    #[test]
    fn is_daemon_provider_classifies_correctly() {
        assert!(is_daemon_provider("opus"));
        assert!(is_daemon_provider("codex"));
        assert!(is_daemon_provider("agy"));
        assert!(!is_daemon_provider("nemotron"));
        assert!(!is_daemon_provider("qwen_coder"));
        assert!(!is_daemon_provider("free"));
    }

    #[test]
    fn rate_limit_detector_matches_429_and_ignores_others() {
        // The exact shape dispatch_openrouter produces on a throttle.
        assert!(is_openrouter_rate_limited(
            "unavailable: openrouter http 429 Too Many Requests: Provider returned error"
        ));
        assert!(is_openrouter_rate_limited("Too Many Requests"));
        // Non-rate-limit errors must NOT be treated as a cooldown trigger.
        assert!(!is_openrouter_rate_limited("unavailable: openrouter http 500: server error"));
        assert!(!is_openrouter_rate_limited("unavailable: openrouter returned empty content"));
        assert!(!is_openrouter_rate_limited("unavailable: OPENROUTER_API_KEY not configured"));
    }

    #[tokio::test]
    async fn free_pool_dispatch_degrades_when_no_key() {
        let cfg = ReviewConfig {
            daemon_url: DEFAULT_DAEMON_URL.to_string(),
            daemon_token: None,
            openrouter_key: None,
        };
        let err = cfg.dispatch_free_pool("review this").await.unwrap_err();
        assert!(err.contains("OPENROUTER_API_KEY"));
    }

    fn keyed_cfg() -> ReviewConfig {
        ReviewConfig {
            daemon_url: DEFAULT_DAEMON_URL.to_string(),
            daemon_token: None,
            openrouter_key: Some("test-key".to_string()),
        }
    }

    async fn seed_pool(models: Vec<String>) {
        let mut p = free_pool::global_pool().lock().await;
        *p = free_pool::FreePool::default();
        p.set_models(models, Instant::now());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn free_pool_dispatch_rotates_past_a_429_to_the_next_model() {
        let server = MockServer::start();
        let a = "provider-a/qwen3-coder:free";
        let b = "provider-b/qwen3-coder:free";
        // Model A always 429s; model B returns a real verdict.
        server.mock(|when, then| {
            when.method(POST).body_contains(a);
            then.status(429).json_body(json!({"error": {"message": "Too Many Requests"}}));
        });
        server.mock(|when, then| {
            when.method(POST).body_contains(b);
            then.status(200).json_body(json!({
                "choices": [{"message": {"content": "looks fine\nVERDICT: APPROVE"}}]
            }));
        });
        std::env::set_var(
            "OPENROUTER_CHAT_URL",
            format!("{}/api/v1/chat/completions", server.base_url()),
        );
        seed_pool(vec![a.to_string(), b.to_string()]).await;

        let out = keyed_cfg().dispatch_free_pool("review").await.unwrap();
        assert!(out.contains("VERDICT: APPROVE"), "should have failed over to B: {out}");
        // A is now cooling down: the next pick skips it and yields B.
        {
            let mut p = free_pool::global_pool().lock().await;
            assert_eq!(p.next_available(Instant::now()).as_deref(), Some(b));
        }
        std::env::remove_var("OPENROUTER_CHAT_URL");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn free_pool_dispatch_distinguishes_empty_pool_from_all_cooling() {
        // Empty pool (catalog down / curated to zero) -> distinct error.
        seed_pool(vec![]).await;
        let err = keyed_cfg().dispatch_free_pool("x").await.unwrap_err();
        assert!(err.contains("catalog empty or unreachable"), "got: {err}");

        // Non-empty but every model cooling -> the OTHER distinct error.
        let m = "provider-c/qwen3-coder:free".to_string();
        seed_pool(vec![m.clone()]).await;
        {
            let mut p = free_pool::global_pool().lock().await;
            p.mark_rate_limited(&m, Instant::now());
        }
        let err = keyed_cfg().dispatch_free_pool("x").await.unwrap_err();
        assert!(err.contains("rate-limited (cooling down)"), "got: {err}");
    }
}
