//! Seamless OpenRouter free-tier pool for the `free` review provider.
//!
//! The OpenRouter free tier throttles per model (a popular free model like
//! `qwen/qwen3-coder:free` returns a persistent HTTP 429), so pinning ONE free
//! model is fragile. Instead the `free` provider draws from a POOL of
//! high-quality free models, discovered by a daily (24h-TTL) scan of
//! OpenRouter's public `/models` catalog and curated down to a strong,
//! text-capable subset. Dispatch round-robins across the pool and, on a 429,
//! puts that model in a short cooldown and rotates to the next -- so a free
//! review lands on whatever pooled model still has quota.
//!
//! Everything here is pure/curation logic plus a process-global pool
//! (`OnceLock<tokio::sync::Mutex<FreePool>>`) so the round-robin cursor, the
//! per-model cooldowns, and the 24h refresh timestamp persist across
//! `review_run` calls. The actual OpenRouter chat dispatch stays in
//! `dispatch.rs`; this module only decides WHICH model to use next.
//!
//! ## Config (env, all optional -- sensible defaults)
//!   - `FREE_POOL_MIN_CONTEXT` (default 32768) -- minimum context_length to pool
//!   - `FREE_POOL_MAX_SIZE`    (default 12)     -- cap on pool size
//!   - `FREE_POOL_COOLDOWN_SECS` (default 600)  -- per-model cooldown after a 429
//!   - `FREE_POOL_TTL_SECS`    (default 86400)  -- daily catalog refresh interval
//!   - `FREE_POOL_FAMILIES`    -- comma-separated id-substring allowlist override
//! No secret is needed here: the `/models` catalog endpoint is unauthenticated.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Default public, unauthenticated OpenRouter model catalog. Overridable via
/// `FREE_POOL_MODELS_URL` (see [`models_url`]) rather than being hardcoded at
/// the call site.
pub const DEFAULT_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// The catalog URL to scan: `FREE_POOL_MODELS_URL` if set, else the default.
pub fn models_url() -> String {
    std::env::var("FREE_POOL_MODELS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODELS_URL.to_string())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

fn min_context() -> u64 {
    env_u64("FREE_POOL_MIN_CONTEXT", 32_768)
}
fn max_size() -> usize {
    env_u64("FREE_POOL_MAX_SIZE", 12) as usize
}
fn cooldown() -> Duration {
    Duration::from_secs(env_u64("FREE_POOL_COOLDOWN_SECS", 600))
}
fn ttl() -> Duration {
    Duration::from_secs(env_u64("FREE_POOL_TTL_SECS", 86_400))
}

/// Default family allowlist (id substrings, matched case-insensitively). These
/// are the strong, general/coder text models on OpenRouter's free tier; the
/// allowlist is what keeps tiny/audio/vision/safety-classifier free models out
/// of the review pool. Override wholesale via `FREE_POOL_FAMILIES`.
const DEFAULT_FAMILIES: &[&str] = &[
    "qwen3-coder",
    "qwen3-next",
    "qwen3-235",
    "qwen3-max",
    "nemotron-3-ultra",
    "nemotron-3-super",
    "nemotron-3-nano-30",
    "gemma-4-31",
    "gemma-4-26",
    "gpt-oss-120",
    "gpt-oss-20",
    "llama-3.3-70",
    "hermes-3-llama-3.1-405",
    "north-mini-code",
    "deepseek",
    "glm-4",
];

fn families() -> Vec<String> {
    match std::env::var("FREE_POOL_FAMILIES") {
        Ok(s) if !s.trim().is_empty() => s
            .split(',')
            .map(|f| f.trim().to_ascii_lowercase())
            .filter(|f| !f.is_empty())
            .collect(),
        _ => DEFAULT_FAMILIES.iter().map(|f| f.to_string()).collect(),
    }
}

/// Curate a raw OpenRouter `/models` catalog body into an ordered pool of model
/// ids. Pure: no I/O, no env beyond the tuning knobs read here, so it is
/// exercised directly in tests against a catalog fixture.
///
/// Keeps a model iff ALL hold: free (`pricing.prompt` and `pricing.completion`
/// both `"0"`), emits text (`architecture.output_modalities` contains
/// `"text"`), `context_length >= min_context`, and its id contains at least one
/// allowlisted family substring. Result is sorted by context_length desc
/// (strongest-context first), de-duplicated, and capped to `max_size`.
pub fn curate(catalog: &Value) -> Vec<String> {
    let min_ctx = min_context();
    let fams = families();
    let mut kept: Vec<(u64, String)> = Vec::new();

    let Some(models) = catalog.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    for m in models {
        let Some(id) = m.get("id").and_then(Value::as_str) else { continue };
        let pricing = m.get("pricing");
        let is_free = pricing
            .and_then(|p| Some((p.get("prompt")?.as_str()?, p.get("completion")?.as_str()?)))
            .map(|(p, c)| p == "0" && c == "0")
            .unwrap_or(false);
        if !is_free {
            continue;
        }
        let emits_text = m
            .get("architecture")
            .and_then(|a| a.get("output_modalities"))
            .and_then(Value::as_array)
            .map(|mods| mods.iter().any(|x| x.as_str() == Some("text")))
            .unwrap_or(false);
        if !emits_text {
            continue;
        }
        let ctx = m.get("context_length").and_then(Value::as_u64).unwrap_or(0);
        if ctx < min_ctx {
            continue;
        }
        let id_lc = id.to_ascii_lowercase();
        if !fams.iter().any(|f| id_lc.contains(f.as_str())) {
            continue;
        }
        if kept.iter().any(|(_, kid)| kid == id) {
            continue;
        }
        kept.push((ctx, id.to_string()));
    }
    // Strongest context first, then lexicographic id for a stable order when
    // two models share a context length.
    kept.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    kept.into_iter().take(max_size()).map(|(_, id)| id).collect()
}

/// Round-robin pool of curated free-model ids with per-model cooldowns and a
/// refresh timestamp. Mutated only under the global `tokio::sync::Mutex`.
#[derive(Default)]
pub struct FreePool {
    models: Vec<String>,
    cursor: usize,
    /// model id -> the `Instant` its 429 cooldown expires.
    cooldown: HashMap<String, Instant>,
    refreshed_at: Option<Instant>,
}

impl FreePool {
    /// Whether the catalog is empty or older than the TTL (needs a refresh).
    pub fn is_stale(&self, now: Instant) -> bool {
        match self.refreshed_at {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= ttl(),
        }
    }

    /// Replace the curated model list (after a successful catalog scan) and
    /// stamp the refresh time. Cooldowns for models no longer present are
    /// dropped; cooldowns for surviving models are kept. Cursor is clamped.
    pub fn set_models(&mut self, models: Vec<String>, now: Instant) {
        self.cooldown.retain(|id, _| models.contains(id));
        if self.models != models {
            self.cursor = 0;
        }
        self.models = models;
        if !self.models.is_empty() {
            self.cursor %= self.models.len();
        }
        self.refreshed_at = Some(now);
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// Next model that is not in cooldown, advancing the round-robin cursor.
    /// Returns `None` if the pool is empty or every model is currently cooling
    /// down. Scans at most `models.len()` entries.
    pub fn next_available(&mut self, now: Instant) -> Option<String> {
        let n = self.models.len();
        if n == 0 {
            return None;
        }
        for _ in 0..n {
            let idx = self.cursor % n;
            self.cursor = (self.cursor + 1) % n;
            let id = &self.models[idx];
            let cooling = self.cooldown.get(id).is_some_and(|until| *until > now);
            if !cooling {
                return Some(id.clone());
            }
        }
        None
    }

    /// Put `id` in cooldown after a rate-limit, until `now + cooldown()`.
    pub fn mark_rate_limited(&mut self, id: &str, now: Instant) {
        self.cooldown.insert(id.to_string(), now + cooldown());
    }
}

/// Process-global pool. `tokio::sync::Mutex` (not `std`) because callers hold it
/// only briefly (pick a model / mark a cooldown); the network fetch happens
/// outside this lock.
pub fn global_pool() -> &'static tokio::sync::Mutex<FreePool> {
    static POOL: OnceLock<tokio::sync::Mutex<FreePool>> = OnceLock::new();
    POOL.get_or_init(|| tokio::sync::Mutex::new(FreePool::default()))
}

/// Serializes catalog refreshes so N concurrent stale callers don't each fetch
/// the catalog (a stampede) and race to overwrite the pool. A refresher takes
/// this, re-checks staleness, and only then fetches + applies; the others wait,
/// then see a fresh pool and skip. Held only across a refresh, never a dispatch.
pub fn refresh_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(id: &str, ctx: u64, free: bool, text: bool) -> Value {
        json!({
            "id": id,
            "context_length": ctx,
            "pricing": {"prompt": if free {"0"} else {"0.0000009"},
                        "completion": if free {"0"} else {"0.0000009"}},
            "architecture": {"output_modalities": if text { vec!["text"] } else { vec!["audio"] }},
        })
    }

    fn catalog() -> Value {
        json!({"data": [
            model("qwen/qwen3-coder:free", 1_048_576, true, true),          // keep
            model("nvidia/nemotron-3-ultra-550b-a55b:free", 1_000_000, true, true), // keep
            model("meta-llama/llama-3.3-70b-instruct:free", 131_072, true, true),   // keep
            model("google/lyria-3-pro-preview", 1_048_576, true, false),   // drop: audio-only
            model("meta-llama/llama-3.2-3b-instruct:free", 131_072, true, true),    // drop: not allowlisted (tiny)
            model("some/paid-model", 200_000, false, true),                // drop: not free
            model("liquid/lfm-2.5-1.2b-instruct:free", 32_768, true, true), // drop: not allowlisted
            model("qwen/qwen3-coder:free", 1_048_576, true, true),         // dup: dedup
            model("nvidia/nemotron-3-nano-30b-a3b:free", 8_000, true, true), // drop: ctx below floor
        ]})
    }

    #[test]
    fn curate_keeps_only_free_text_allowlisted_above_ctx_floor_dedup_sorted() {
        let ids = curate(&catalog());
        assert_eq!(
            ids,
            vec![
                "qwen/qwen3-coder:free".to_string(),               // 1_048_576
                "nvidia/nemotron-3-ultra-550b-a55b:free".to_string(), // 1_000_000
                "meta-llama/llama-3.3-70b-instruct:free".to_string(), // 131_072
            ]
        );
    }

    #[test]
    fn curate_empty_on_missing_data_or_no_matches() {
        assert!(curate(&json!({})).is_empty());
        assert!(curate(&json!({"data": []})).is_empty());
        assert!(curate(&json!({"data": [model("some/paid", 200_000, false, true)]})).is_empty());
    }

    #[test]
    fn next_available_round_robins_and_wraps() {
        let now = Instant::now();
        let mut p = FreePool::default();
        p.set_models(vec!["a".into(), "b".into(), "c".into()], now);
        assert_eq!(p.next_available(now).as_deref(), Some("a"));
        assert_eq!(p.next_available(now).as_deref(), Some("b"));
        assert_eq!(p.next_available(now).as_deref(), Some("c"));
        assert_eq!(p.next_available(now).as_deref(), Some("a")); // wrapped
    }

    #[test]
    fn next_available_skips_cooling_models_and_none_when_all_cooling() {
        let now = Instant::now();
        let mut p = FreePool::default();
        p.set_models(vec!["a".into(), "b".into()], now);
        p.mark_rate_limited("a", now);
        // "a" is cooling -> we get "b" (twice, since "a" stays skipped).
        assert_eq!(p.next_available(now).as_deref(), Some("b"));
        assert_eq!(p.next_available(now).as_deref(), Some("b"));
        p.mark_rate_limited("b", now);
        assert_eq!(p.next_available(now), None); // all cooling
    }

    #[test]
    fn cooldown_expires_after_the_window() {
        let now = Instant::now();
        let mut p = FreePool::default();
        p.set_models(vec!["a".into()], now);
        p.mark_rate_limited("a", now);
        // Just after the cooldown window, "a" is available again.
        let later = now + cooldown() + Duration::from_secs(1);
        assert_eq!(p.next_available(later).as_deref(), Some("a"));
    }

    #[test]
    fn is_stale_true_when_never_refreshed() {
        let p = FreePool::default();
        assert!(p.is_stale(Instant::now()));
    }

    #[test]
    #[serial_test::serial]
    fn models_url_honors_override_else_default() {
        std::env::remove_var("FREE_POOL_MODELS_URL");
        assert_eq!(models_url(), DEFAULT_MODELS_URL);
        std::env::set_var("FREE_POOL_MODELS_URL", "http://catalog.internal/models"); // pii-test-fixture
        assert_eq!(models_url(), "http://catalog.internal/models");
        std::env::remove_var("FREE_POOL_MODELS_URL");
    }

    #[test]
    fn empty_and_all_cooling_are_distinguishable_at_the_pool() {
        let now = Instant::now();
        let empty = FreePool::default();
        assert!(empty.is_empty());
        let mut cooling = FreePool::default();
        cooling.set_models(vec!["a".into()], now);
        cooling.mark_rate_limited("a", now);
        assert!(!cooling.is_empty()); // NOT empty ...
        assert_eq!(cooling.next_available(now), None); // ... but nothing available
    }

    #[test]
    fn set_models_stamps_fresh_and_drops_stale_cooldowns() {
        let now = Instant::now();
        let mut p = FreePool::default();
        p.set_models(vec!["a".into(), "b".into()], now);
        p.mark_rate_limited("b", now);
        assert!(!p.is_stale(now));
        // Refresh without "b": its cooldown entry is dropped.
        p.set_models(vec!["a".into(), "c".into()], now);
        assert!(!p.cooldown.contains_key("b"));
    }
}
