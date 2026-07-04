//! Canonical timeout resolution shared by the intake suites (MINT Phase 2,
//! item 3).
//!
//! Four call sites each computed their own per-case/per-tier timeout with
//! near-identical boilerplate before this refactor:
//!   - `runner.rs`'s `tier_timeout()`           — `INTAKE_TIER_TIMEOUT_SEC`, default 600s (context-stress suite tiers).
//!   - `code.rs`'s `code_timeout()`              — `INTAKE_CODE_TIMEOUT_SEC`, default 300s (v1 code suite).
//!   - `agent.rs`'s `agent_timeout()`             — `INTAKE_AGENT_TIMEOUT_SEC`, default 180s (agent suite).
//!   - `code_v2.rs`'s `tier_default_timeout()`    — blitz/standard/deep tier table (v2 code suite), no env var.
//!
//! The first three share IDENTICAL "env var override, else a flat default"
//! mechanics; the fourth is a "tier name → default seconds" lookup (no env
//! involvement — per-case `timeout_s` in the v2 manifest is that suite's own
//! override, applied by the caller before ever consulting this table).
//! Consolidated here into two small canonical functions that all four call
//! sites now delegate to — this is pure deduplication of the resolution
//! mechanics; every call site keeps its EXACT original default/env-var/tier
//! values, so no timeout actually changes for any existing caller.

use std::time::Duration;

/// Read `env_var` (if set and parseable as `u64`) as an override; else fall
/// back to `default_secs`. The shared shape `tier_timeout`/`code_timeout`/
/// `agent_timeout` each reimplemented separately before this refactor.
pub fn env_timeout_secs(env_var: &str, default_secs: u64) -> u64 {
    std::env::var(env_var)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default_secs)
}

/// Same as [`env_timeout_secs`], wrapped in a [`Duration`] — every call site
/// immediately did this conversion itself.
pub fn env_timeout(env_var: &str, default_secs: u64) -> Duration {
    Duration::from_secs(env_timeout_secs(env_var, default_secs))
}

/// Tier-name → default seconds table for the v2 code suite's
/// `tier_default_timeout` call site: `blitz` 60s, `standard` 120s, `deep`
/// 300s, any other/unrecognized tier string 120s (the pre-existing
/// fallback). Case-insensitive, matching the original.
pub fn tier_default_secs(tier: &str) -> u64 {
    match tier.to_lowercase().as_str() {
        "blitz" => 60,
        "standard" => 120,
        "deep" => 300,
        _ => 120,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serializes tests that mutate process-global env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn env_timeout_secs_defaults_when_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TERMINUS_TIMEOUTS_TEST_UNSET");
        assert_eq!(env_timeout_secs("TERMINUS_TIMEOUTS_TEST_UNSET", 42), 42);
    }

    #[test]
    fn env_timeout_secs_override_wins() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TERMINUS_TIMEOUTS_TEST_OVERRIDE", "99");
        assert_eq!(env_timeout_secs("TERMINUS_TIMEOUTS_TEST_OVERRIDE", 42), 99);
        std::env::remove_var("TERMINUS_TIMEOUTS_TEST_OVERRIDE");
    }

    #[test]
    fn env_timeout_secs_garbage_falls_back_to_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TERMINUS_TIMEOUTS_TEST_GARBAGE", "not-a-number");
        assert_eq!(env_timeout_secs("TERMINUS_TIMEOUTS_TEST_GARBAGE", 42), 42);
        std::env::remove_var("TERMINUS_TIMEOUTS_TEST_GARBAGE");
    }

    #[test]
    fn env_timeout_wraps_duration() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TERMINUS_TIMEOUTS_TEST_DURATION");
        assert_eq!(
            env_timeout("TERMINUS_TIMEOUTS_TEST_DURATION", 7),
            Duration::from_secs(7)
        );
    }

    // ---- the four original call sites' exact values, via the canonical fns ----

    #[test]
    fn tier_timeout_default_matches_context_suite_original_600s() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INTAKE_TIER_TIMEOUT_SEC");
        assert_eq!(env_timeout_secs("INTAKE_TIER_TIMEOUT_SEC", 600), 600);
    }

    #[test]
    fn code_timeout_default_matches_v1_code_suite_original_300s() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INTAKE_CODE_TIMEOUT_SEC");
        assert_eq!(env_timeout_secs("INTAKE_CODE_TIMEOUT_SEC", 300), 300);
    }

    #[test]
    fn agent_timeout_default_matches_agent_suite_original_180s() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INTAKE_AGENT_TIMEOUT_SEC");
        assert_eq!(env_timeout_secs("INTAKE_AGENT_TIMEOUT_SEC", 180), 180);
    }

    #[test]
    fn tier_default_secs_matches_code_v2_original_table() {
        assert_eq!(tier_default_secs("blitz"), 60);
        assert_eq!(tier_default_secs("standard"), 120);
        assert_eq!(tier_default_secs("deep"), 300);
        assert_eq!(tier_default_secs("BLITZ"), 60, "case-insensitive, as original");
        assert_eq!(tier_default_secs("unknown-tier"), 120, "unrecognized tier falls back to 120s");
    }
}
