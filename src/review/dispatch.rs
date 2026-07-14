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
/// `gpt56`'s OpenRouter model tag: the GPT-5.6 **Luna** tier ($1/$6 per 1M in/out) —
/// the cost-conscious "middle of the road" GPT-5.6 (the deep Sol tier is $5/$30). A
/// PAID model (no `:free` suffix), so its dispatch is credit-guarded (see
/// [`ReviewConfig::openrouter_credits`] and the `gpt56` path in `dispatch_provider_raw`).
pub const GPT56_MODEL: &str = "openai/gpt-5.6-luna";

/// The default minimum OpenRouter credit balance (USD) below which a PAID model
/// dispatch is refused (degrades that provider rather than spending the last of the
/// balance). Overridable via `OPENROUTER_MIN_CREDITS`. Keeps a paid capstone lens
/// from bottoming out the account / becoming a money sink.
const DEFAULT_MIN_OPENROUTER_CREDITS: f64 = 1.0;

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

/// REVCAP-01 PART B: the effort level sent for an INTENSIVE-SUBSTITUTE review
/// (a substitute standing in for a down frontier provider must review HARDER,
/// not at parity -- see `DaemonOpts::intensive`). Value convention mirrors
/// `codex`'s own `model_reasoning_effort` levels (`"low"|"medium"|"high"`).
const INTENSIVE_REASONING_EFFORT: &str = "high";

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
        Self::client_with_timeout(150)
    }

    /// A client with a caller-chosen request timeout. The Epic capstone's explore
    /// mode needs a MUCH larger HTTP timeout than a routine review because its
    /// auditors legitimately run for many minutes (the review-daemon's progress /
    /// stall-detector — not this wall-clock — decides when a genuinely-stalled
    /// provider is killed; this ceiling is only a backstop against a wedged socket).
    fn client_with_timeout(secs: u64) -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(secs))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// Dispatch to an `opus`/`codex`/`agy` provider via the review-daemon's
    /// `POST /dispatch`. Returns `Ok(text)` on a genuine reply, `Err(reason)`
    /// (always prefixed `"unavailable: "`) on anything else -- daemon not
    /// configured, unreachable, or itself reporting a structured error.
    pub async fn dispatch_daemon(
        &self,
        provider: &str,
        prompt: &str,
        opts: &DaemonOpts,
    ) -> Result<String, String> {
        let Some(token) = &self.daemon_token else {
            return Err("unavailable: REVIEW_DAEMON_TOKEN not configured".to_string());
        };
        let client =
            Self::client_with_timeout(opts.client_timeout_secs).map_err(|e| format!("unavailable: {e}"))?;
        let url = format!("{}/dispatch", self.daemon_url);
        let mut req_body = json!({
            "provider": provider,
            "prompt": prompt,
            "timeout_secs": opts.timeout_secs,
        });
        // Epic capstone extras (the daemon defaults them off for a routine review):
        // explore mode (read-only tools + repo cwd) + progress/stall detection.
        if opts.explore {
            req_body["explore"] = json!(true);
        }
        if let Some(stall) = opts.stall_secs {
            req_body["stall_secs"] = json!(stall);
        }
        if let Some(repo) = &opts.repo_path {
            req_body["repo_path"] = json!(repo);
        }
        // REVCAP-01 PART B: intensive-substitute extra -- omitted entirely (routine
        // reviews, and the Epic capstone, are byte-for-byte unchanged) unless this
        // dispatch is an intensive-substitute review (see `DaemonOpts::intensive`).
        if let Some(effort) = &opts.reasoning_effort {
            req_body["reasoning_effort"] = json!(effort);
        }
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .json(&req_body)
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

    /// Query OpenRouter's credits endpoint (`GET /api/v1/credits`) → `(remaining,
    /// total_granted, total_usage)` in USD. Used both by the `openrouter_credits`
    /// tracker tool and by the pre-flight guard that refuses a PAID model dispatch
    /// when the balance is below the floor (so a paid capstone lens can never bottom
    /// out the account). `Err` (unconfigured / unreachable / malformed) is a
    /// human-readable degrade reason, never a panic.
    pub async fn openrouter_credits(&self) -> Result<(f64, f64, f64), String> {
        let Some(key) = &self.openrouter_key else {
            return Err("unavailable: OPENROUTER_API_KEY not configured".to_string());
        };
        let client = Self::client().map_err(|e| format!("unavailable: {e}"))?;
        let url = openrouter_credits_url();
        let resp = client
            .get(&url)
            .bearer_auth(key)
            .send()
            .await
            .map_err(|e| format!("unavailable: openrouter credits unreachable: {e}"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("unavailable: malformed credits response: {e}"))?;
        if !status.is_success() {
            return Err(format!("unavailable: openrouter credits http {status}"));
        }
        // OpenRouter shape: {"data": {"total_credits": <granted>, "total_usage": <spent>}}.
        let data = body.get("data").unwrap_or(&body);
        let granted = data.get("total_credits").and_then(Value::as_f64).unwrap_or(0.0);
        let usage = data.get("total_usage").and_then(Value::as_f64).unwrap_or(0.0);
        Ok((granted - usage, granted, usage))
    }

    /// Pre-flight credit guard for a PAID OpenRouter model. Free models (`:free`)
    /// skip the check (they cost nothing). For a paid model, refuse the dispatch
    /// when the remaining balance is below [`min_openrouter_credits`] — degrading
    /// that one provider (`"unavailable: openrouter credits low ..."`) rather than
    /// spending the last of the balance. A credits-lookup failure FAILS OPEN (we do
    /// not block a review on an inability to read the balance) but is logged.
    pub(crate) async fn guard_paid_model(&self, model: &str) -> Result<(), String> {
        if !is_paid_openrouter_model(model) {
            return Ok(());
        }
        match self.openrouter_credits().await {
            Ok((remaining, _, _)) => {
                let floor = min_openrouter_credits();
                if remaining < floor {
                    Err(format!(
                        "unavailable: openrouter credits low (${remaining:.2} < ${floor:.2} floor) — \
                         refusing paid model '{model}' to avoid bottoming out the account"
                    ))
                } else {
                    Ok(())
                }
            }
            Err(e) => {
                tracing::warn!("openrouter credit guard: could not read balance ({e}); failing open");
                Ok(())
            }
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
        "gpt56" => Some(GPT56_MODEL),
        _ => None,
    }
}

/// Whether `provider` is one of the daemon-backed CLI providers. `claude-fable-5`
/// (the capstone Fable lens) routes to the daemon's `claude` CLI like `opus`.
pub fn is_daemon_provider(provider: &str) -> bool {
    matches!(provider, "opus" | "codex" | "agy" | "claude-fable-5")
}

/// OpenRouter credits endpoint: `OPENROUTER_CREDITS_URL` if set, else the default
/// (parallels [`openrouter_chat_url`]; lets tests point at a mock).
fn openrouter_credits_url() -> String {
    std::env::var("OPENROUTER_CREDITS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://openrouter.ai/api/v1/credits".to_string())
}

/// Whether an OpenRouter model tag is PAID (costs credits). OpenRouter marks free
/// models with a `:free` suffix; everything else is paid and credit-guarded.
pub fn is_paid_openrouter_model(model: &str) -> bool {
    !model.ends_with(":free")
}

/// The minimum remaining OpenRouter balance (USD) below which a paid dispatch is
/// refused. From `OPENROUTER_MIN_CREDITS`, else [`DEFAULT_MIN_OPENROUTER_CREDITS`].
pub fn min_openrouter_credits() -> f64 {
    std::env::var("OPENROUTER_MIN_CREDITS")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v >= 0.0)
        .unwrap_or(DEFAULT_MIN_OPENROUTER_CREDITS)
}

/// Per-dispatch options for the review-daemon. A routine review uses [`Self::routine`]
/// (tools off, 120s budget); the Epic capstone uses [`Self::epic`] (explore mode +
/// progress/stall detection + a long wall-clock BACKSTOP, since a whole-repo audit
/// legitimately runs for many minutes and the daemon's stall-detector — not a
/// wall-clock timeout — decides when to kill a genuinely-stalled provider);
/// REVCAP-01 PART B's [`Self::intensive`] is for a provider standing in for a
/// currently-DOWN frontier reviewer -- it must review HARDER than parity, which
/// this codebase proved (live, this session) needs BOTH a raised reasoning effort
/// AND a longer wall-clock budget: a genuinely deep 2-pass-equivalent review of a
/// large diff at the routine 120s backstop reliably TIMES OUT before it can emit a
/// verdict.
#[derive(Clone, Debug)]
pub struct DaemonOpts {
    /// Wall-clock backstop (secs) sent to the daemon; the stall-detector is primary.
    pub timeout_secs: u64,
    /// HTTP client ceiling (secs); must exceed `timeout_secs`.
    pub client_timeout_secs: u64,
    /// Explore mode: the claude slots get read-only tools + a repo cwd.
    pub explore: bool,
    /// Kill a provider only after this many secs of NO output (a genuine stall).
    pub stall_secs: Option<u64>,
    /// Repo checkout the auditors may read from in explore mode.
    pub repo_path: Option<String>,
    /// REVCAP-01 PART B: requested reasoning/thinking effort (e.g. `"high"`),
    /// forwarded to the review-daemon's `/dispatch` body and from there into the
    /// spawned CLI's own effort flag (see `provider::build_command`). `None` on
    /// every pre-PART-B preset ([`Self::routine`], [`Self::epic`]) -- omitted from
    /// the wire body entirely, so routine/epic dispatches are byte-for-byte
    /// unchanged. Distinct from `explore`: intensity here is effort + time budget
    /// + a harder-refutation prompt role, NOT repo-read access -- explore mode
    /// makes opus enter an agentic tool-loop and never emit a `VERDICT:` line
    /// (proven live), so [`Self::intensive`] deliberately keeps `explore: false`.
    pub reasoning_effort: Option<String>,
}

impl DaemonOpts {
    /// Routine per-item/per-sprint review: unchanged pre-capstone behavior.
    pub fn routine() -> Self {
        Self {
            timeout_secs: 120,
            client_timeout_secs: 150,
            explore: false,
            stall_secs: None,
            repo_path: None,
            reasoning_effort: None,
        }
    }

    /// Epic capstone: explore + stall detection + long backstop.
    pub fn epic(repo_path: Option<String>) -> Self {
        Self {
            timeout_secs: 1800,
            client_timeout_secs: 1900,
            explore: true,
            stall_secs: Some(180),
            repo_path,
            reasoning_effort: None,
        }
    }

    /// REVCAP-01 PART B: an INTENSIVE-SUBSTITUTE review -- dispatched when this
    /// provider is standing in for a currently-DOWN frontier reviewer (see
    /// `review::mod`'s per-provider selection). Raises reasoning effort to
    /// [`INTENSIVE_REASONING_EFFORT`] and gives the run a 900s wall-clock backstop
    /// (vs. routine's 120s, the proven failure point for a genuinely deep review of
    /// a large diff) plus a 240s no-output stall window (mirrors the Epic
    /// capstone's stall-detection shape, scaled down from its whole-repo 180s/1800s
    /// budget). Deliberately `explore: false` and no `repo_path` -- this is a
    /// deeper single-pass read of the SAME diff/context every other panel member
    /// gets, not a repo-exploring audit; explore mode is proven to make opus enter
    /// an agentic tool-loop and never emit a `VERDICT:` line.
    pub fn intensive() -> Self {
        Self {
            timeout_secs: 900,
            client_timeout_secs: 950,
            explore: false,
            stall_secs: Some(240),
            repo_path: None,
            reasoning_effort: Some(INTENSIVE_REASONING_EFFORT.to_string()),
        }
    }
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
        let result = cfg.dispatch_daemon("opus", "review this", &DaemonOpts::routine()).await.unwrap();
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
        let err = cfg.dispatch_daemon("opus", "x", &DaemonOpts::routine()).await.unwrap_err();
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
        let err = cfg.dispatch_daemon("opus", "x", &DaemonOpts::routine()).await.unwrap_err();
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
        // The Fable capstone lens routes to the daemon's claude CLI.
        assert!(is_daemon_provider("claude-fable-5"));
        assert!(!is_daemon_provider("nemotron"));
        assert!(!is_daemon_provider("qwen_coder"));
        assert!(!is_daemon_provider("free"));
        // gpt56 is an OpenRouter model, NOT a daemon provider.
        assert!(!is_daemon_provider("gpt56"));
    }

    #[test]
    fn gpt56_maps_to_the_luna_tier_and_is_paid() {
        assert_eq!(openrouter_model_for("gpt56"), Some(GPT56_MODEL));
        assert_eq!(GPT56_MODEL, "openai/gpt-5.6-luna");
        // gpt56 is PAID (credit-guarded); the free lenses are not.
        assert!(is_paid_openrouter_model(GPT56_MODEL));
        assert!(!is_paid_openrouter_model(NEMOTRON_MODEL));
        assert!(!is_paid_openrouter_model(QWEN_CODER_MODEL));
        assert!(!is_paid_openrouter_model("anything:free"));
    }

    #[test]
    fn epic_daemon_opts_enable_explore_and_stall() {
        let routine = DaemonOpts::routine();
        assert!(!routine.explore && routine.stall_secs.is_none() && routine.timeout_secs == 120);
        let epic = DaemonOpts::epic(Some("/repo".into()));
        assert!(epic.explore);
        assert!(epic.stall_secs.is_some());
        assert!(epic.timeout_secs > routine.timeout_secs);
        assert!(epic.client_timeout_secs > epic.timeout_secs);
        assert_eq!(epic.repo_path.as_deref(), Some("/repo"));
    }

    #[test]
    fn intensive_daemon_opts_raise_effort_and_timeout_but_stay_out_of_explore_mode() {
        let routine = DaemonOpts::routine();
        let intensive = DaemonOpts::intensive();
        // The proven failure point: routine's 120s backstop is too short for a
        // genuinely deep review of a large diff -- intensive must exceed it.
        assert!(intensive.timeout_secs > routine.timeout_secs);
        assert!(intensive.client_timeout_secs > intensive.timeout_secs);
        assert_eq!(intensive.reasoning_effort.as_deref(), Some(INTENSIVE_REASONING_EFFORT));
        assert!(routine.reasoning_effort.is_none());
        // explore stays false: explore mode is proven to make opus enter an
        // agentic tool-loop and never emit a VERDICT: line -- intensity here is
        // effort + time budget + prompt role, never repo-read access.
        assert!(!intensive.explore);
        assert!(intensive.repo_path.is_none());
        assert!(intensive.stall_secs.is_some());
    }

    #[tokio::test]
    async fn dispatch_daemon_sends_reasoning_effort_only_when_set() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/dispatch")
                .json_body_partial(r#"{"reasoning_effort": "high"}"#);
            then.status(200).json_body(json!({"text": "VERDICT: APPROVE"}));
        });
        let cfg = cfg_for(&server, "testtoken");
        cfg.dispatch_daemon("opus", "review this", &DaemonOpts::intensive()).await.unwrap();
        mock.assert();

        // A routine dispatch (no reasoning_effort set) must NOT include the key at
        // all -- verified by asserting the mock only matches a body lacking it (a
        // regression that always includes the key would make this mock miss).
        fn body_excludes_reasoning_effort(req: &httpmock::prelude::HttpMockRequest) -> bool {
            let body = req.body.as_deref().unwrap_or(&[]);
            let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            v.get("reasoning_effort").is_none()
        }
        let server2 = MockServer::start();
        let mock2 = server2.mock(|when, then| {
            when.method(POST).path("/dispatch").matches(body_excludes_reasoning_effort);
            then.status(200).json_body(json!({"text": "VERDICT: APPROVE"}));
        });
        let cfg2 = cfg_for(&server2, "testtoken");
        cfg2.dispatch_daemon("opus", "review this", &DaemonOpts::routine()).await.unwrap();
        mock2.assert();
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
