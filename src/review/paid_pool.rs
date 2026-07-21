//! REVX-09/11 — the paid, pooled OpenRouter provider (`paid`).
//!
//! Modeled directly on [`super::free_pool`]: a round-robin pool with
//! per-model 429-cooldown failover. The key difference from the free pool is
//! that this is NOT a discovered catalog scan -- it is a small, CURATED,
//! config-overridable list of paid, cost-effective, capstone-quality
//! frontier models (Kimi K2/K2-thinking, DeepSeek V3.2, GLM-5/4.6,
//! Gemini 2.5 Pro per the S121 research findings). It runs ALONGSIDE the free
//! pool and the sub-backed daemon providers (opus/codex/agy) to add diverse
//! paid insight to a panel, gated behind:
//!   - a funded `OPENROUTER_API_KEY` (read at the dispatch boundary in
//!     `dispatch.rs`, same as every other OpenRouter path -- never hardcoded
//!     here);
//!   - the existing `guard_paid_model` credit floor (refuses a paid dispatch
//!     below `OPENROUTER_MIN_CREDITS`);
//!   - REVX-11's runtime ON/OFF toggle ([`is_enabled`]/[`set_enabled`]),
//!     which defaults OFF (`REVIEW_PAID_POOL_ENABLED`, default `false`) since
//!     paid spend is operator-gated, not implicitly on.
//!
//! ## Config (env, all optional -- sensible defaults)
//!   - `REVIEW_PAID_POOL_MODELS`       -- comma-separated model-id override
//!     (wholesale replacement of the default 6-model set)
//!   - `REVIEW_PAID_POOL_COOLDOWN_SECS` (default 600) -- per-model cooldown
//!     after a 429, mirrors `FREE_POOL_COOLDOWN_SECS`
//!   - `REVIEW_PAID_POOL_ENABLED`      (default `false`) -- the REVX-11
//!     runtime toggle's process-lifetime DEFAULT (an agent can flip it at
//!     runtime via the `review_paid_pool_toggle` tool without touching this
//!     env var; the env var only seeds the initial state)

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The default curated paid model set (S121 research: cost-effective,
/// capstone-quality reasoning models on OpenRouter). Overridable wholesale
/// via `REVIEW_PAID_POOL_MODELS`.
pub const DEFAULT_PAID_MODELS: &[&str] = &[
    "moonshotai/kimi-k2",
    "moonshotai/kimi-k2-thinking",
    "deepseek/deepseek-v3.2",
    "z-ai/glm-5",
    "z-ai/glm-4.6",
    "google/gemini-2.5-pro",
];

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

fn cooldown() -> Duration {
    Duration::from_secs(env_u64("REVIEW_PAID_POOL_COOLDOWN_SECS", 600))
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            if v.is_empty() {
                default
            } else {
                !matches!(v.as_str(), "0" | "false" | "off" | "no")
            }
        }
    }
}

/// The configured paid model list: `REVIEW_PAID_POOL_MODELS` (comma-separated)
/// if set and non-empty after trimming/filtering, else [`DEFAULT_PAID_MODELS`].
/// Mirrors `free_pool`'s `env_list` "empty override -> fall back to built-in"
/// convention (REVX-06 edge case) so a caller can never accidentally configure
/// an empty pool by setting the var to `""` or `","`.
pub fn configured_models() -> Vec<String> {
    match std::env::var("REVIEW_PAID_POOL_MODELS") {
        Ok(v) if !v.trim().is_empty() => {
            let items: Vec<String> =
                v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if items.is_empty() {
                DEFAULT_PAID_MODELS.iter().map(|s| s.to_string()).collect()
            } else {
                items
            }
        }
        _ => DEFAULT_PAID_MODELS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Round-robin pool of curated paid-model ids with per-model cooldowns. Unlike
/// [`super::free_pool::FreePool`] there is no catalog-fetch/staleness concept
/// here -- the model list is config, read fresh from [`configured_models`] on
/// each pick -- but the same round-robin-cursor + cooldown-map shape is kept so
/// dispatch failover behaves identically to the free pool.
#[derive(Default)]
pub struct PaidPool {
    cursor: usize,
    cooldown: HashMap<String, Instant>,
}

impl PaidPool {
    /// Next model (from `models`) that is not in cooldown, advancing the
    /// round-robin cursor. `None` if `models` is empty or every model is
    /// currently cooling down. Scans at most `models.len()` entries.
    pub fn next_available(&mut self, models: &[String], now: Instant) -> Option<String> {
        let n = models.len();
        if n == 0 {
            return None;
        }
        self.cursor %= n;
        for _ in 0..n {
            let idx = self.cursor % n;
            self.cursor = (self.cursor + 1) % n;
            let id = &models[idx];
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

/// Process-global pool (mirrors `free_pool::global_pool`'s
/// `OnceLock<tokio::sync::Mutex<..>>` pattern).
pub fn global_pool() -> &'static tokio::sync::Mutex<PaidPool> {
    static POOL: OnceLock<tokio::sync::Mutex<PaidPool>> = OnceLock::new();
    POOL.get_or_init(|| tokio::sync::Mutex::new(PaidPool::default()))
}

// ── REVX-11: runtime ON/OFF toggle ──────────────────────────────────────────

/// Process-global enabled flag, seeded from `REVIEW_PAID_POOL_ENABLED`
/// (default `false` -- paid spend is operator-gated) on first use, then
/// mutable for the process lifetime via [`set_enabled`]. An operator wanting
/// a PERMANENT default sets the env var (read again only if the process
/// restarts); an agent flips it at runtime via the `review_paid_pool_toggle`
/// tool without a redeploy.
fn enabled_cell() -> &'static AtomicBool {
    static ENABLED: OnceLock<AtomicBool> = OnceLock::new();
    ENABLED.get_or_init(|| AtomicBool::new(env_bool("REVIEW_PAID_POOL_ENABLED", false)))
}

/// Whether the paid pool is currently enabled.
pub fn is_enabled() -> bool {
    enabled_cell().load(Ordering::SeqCst)
}

/// Flip the paid pool on/off at runtime. Survives across `review_run` calls
/// (process lifetime); resets to the `REVIEW_PAID_POOL_ENABLED` env default on
/// a process restart.
pub fn set_enabled(enabled: bool) {
    enabled_cell().store(enabled, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn configured_models_defaults_to_the_curated_six() {
        std::env::remove_var("REVIEW_PAID_POOL_MODELS");
        let models = configured_models();
        assert_eq!(models.len(), 6);
        assert!(models.contains(&"moonshotai/kimi-k2".to_string()));
        assert!(models.contains(&"moonshotai/kimi-k2-thinking".to_string()));
        assert!(models.contains(&"deepseek/deepseek-v3.2".to_string()));
        assert!(models.contains(&"z-ai/glm-5".to_string()));
        assert!(models.contains(&"z-ai/glm-4.6".to_string()));
        assert!(models.contains(&"google/gemini-2.5-pro".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn configured_models_override_replaces_wholesale() {
        std::env::set_var("REVIEW_PAID_POOL_MODELS", "foo/bar, baz/qux");
        let models = configured_models();
        assert_eq!(models, vec!["foo/bar".to_string(), "baz/qux".to_string()]);
        std::env::remove_var("REVIEW_PAID_POOL_MODELS");
    }

    #[test]
    #[serial_test::serial]
    fn configured_models_empty_override_falls_back_to_default() {
        std::env::set_var("REVIEW_PAID_POOL_MODELS", "  ,  ,");
        let models = configured_models();
        assert_eq!(models.len(), 6, "empty override must fall back to the built-in 6");
        std::env::remove_var("REVIEW_PAID_POOL_MODELS");
    }

    #[test]
    fn next_available_round_robins_and_wraps() {
        let now = Instant::now();
        let models = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut p = PaidPool::default();
        assert_eq!(p.next_available(&models, now).as_deref(), Some("a"));
        assert_eq!(p.next_available(&models, now).as_deref(), Some("b"));
        assert_eq!(p.next_available(&models, now).as_deref(), Some("c"));
        assert_eq!(p.next_available(&models, now).as_deref(), Some("a"));
    }

    #[test]
    fn next_available_skips_cooling_and_none_when_all_cooling() {
        let now = Instant::now();
        let models = vec!["a".to_string(), "b".to_string()];
        let mut p = PaidPool::default();
        p.mark_rate_limited("a", now);
        assert_eq!(p.next_available(&models, now).as_deref(), Some("b"));
        p.mark_rate_limited("b", now);
        assert_eq!(p.next_available(&models, now), None);
    }

    #[test]
    fn next_available_empty_models_is_none() {
        let now = Instant::now();
        let mut p = PaidPool::default();
        assert_eq!(p.next_available(&[], now), None);
    }

    #[test]
    fn cooldown_expires_after_the_window() {
        let now = Instant::now();
        let models = vec!["a".to_string()];
        let mut p = PaidPool::default();
        p.mark_rate_limited("a", now);
        let later = now + cooldown() + Duration::from_secs(1);
        assert_eq!(p.next_available(&models, later).as_deref(), Some("a"));
    }

    // ── REVX-11: runtime toggle ─────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn toggle_flips_state_and_is_readable() {
        set_enabled(false);
        assert!(!is_enabled());
        set_enabled(true);
        assert!(is_enabled());
        set_enabled(false);
        assert!(!is_enabled());
    }
}
