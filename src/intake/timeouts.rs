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

// ---------------------------------------------------------------------------
// Reload-cost timeout adjustment (fix for the qwen2.5-coder:32b-instruct
// production stall)
// ---------------------------------------------------------------------------
//
// Root cause (confirmed by direct source reading, not re-diagnosed here):
// `tier_default_secs` above reflects task DIFFICULTY (blitz/standard/deep),
// not model size or whether a given request will force Ollama to reload its
// runner in-process. `context.rs` recomputes `num_ctx` per request from the
// prompt's token estimate (`next_pow2_ctx`), and Ollama reloads whenever the
// requested context size differs from what's currently loaded — a reload
// that happens INSIDE the timed HTTP call. For the fleet's largest model
// (32B), a cold reload alone measured 20-40s in manual testing, which can
// consume most or all of a 60s "blitz" budget before any generation happens.
// The functions below add a flat, additive allowance on top of the
// difficulty tier for large models specifically — the tier's difficulty
// semantics are unchanged; this is a separate, orthogonal adjustment layered
// on top (see `reload_adjusted_timeout_secs`).
//
// Design note (why "large model by name" and not "did this specific request
// actually change num_ctx"): the latter would need cross-request state (the
// context size the target Ollama instance currently has loaded), which lives
// nowhere accessible to this module and would add a shared-state dependency
// disproportionate to this fix's scope. Model size is a directly-measured
// proxy for reload cost (the observed 20-40s figure IS the 32B model's cold
// reload; nothing in the fleet's smaller models showed a comparable cost),
// so it's the pragmatic minimal signal: it fires for the model class that
// actually exhibited the problem, and never fires for the fleet's small/fast
// models, which is the common case this fix must leave unchanged.

/// Params-in-billions threshold at/above which a model is treated as "large"
/// for the reload-cost timeout allowance. Override via
/// `INTAKE_LARGE_MODEL_PARAMS_B`. Default 30 — the fleet's chronically-stuck
/// `qwen2.5-coder:32b-instruct` is 32B; the fleet's mid-size coders (14B and
/// under) are not, matching the confirmed root cause (only the fleet's
/// largest model showed a reload cost large enough to threaten a 60-300s
/// tier budget).
pub const DEFAULT_LARGE_MODEL_PARAMS_B: u64 = 30;

/// Flat allowance (seconds) added to a large model's effective timeout to
/// account for an in-request Ollama runner reload triggered by a
/// context-size change. Manually measured cold-reload cost for the fleet's
/// 32B model was 20-40s; 45s keeps a safety margin above the observed
/// maximum without being large enough to meaningfully change the retry
/// loop's worst-case wall-clock budget (see `code_v2.rs`'s
/// `TRANSPORT_RETRY_BACKOFF_SECS`, unaffected by this change: the allowance
/// is flat per attempt, not cumulative or multiplicative, so the retry
/// loop's upper bound on total time grows by a fixed, bounded amount rather
/// than compounding). Override via `INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC`.
pub const DEFAULT_RELOAD_ALLOWANCE_SEC: u64 = 45;

/// Extract a parameter-count-in-billions figure from a model name, if the
/// name encodes one as a distinct token (e.g. `qwen2.5-coder:32b-instruct`
/// → `Some(32)`, `gpt-oss-120b` → `Some(120)`). Tokens are split on any
/// non-alphanumeric separator (`:`, `-`, `.`, `/`, …) — this mirrors how
/// Ollama model tags compose size into the tag itself. A token counts only
/// when it is ALL DIGITS immediately followed by a single trailing `b`/`B`,
/// so ordinary name segments (`qwen2`, `coder`, `instruct`, `gemma3`) never
/// false-match. Returns the FIRST such token found (Ollama tags place the
/// size token once, so this is unambiguous in practice). `None` if no token
/// matches.
pub fn model_param_billions(model_name: &str) -> Option<u64> {
    for tok in model_name.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.len() < 2 {
            continue;
        }
        let (digits, suffix) = tok.split_at(tok.len() - 1);
        if !suffix.eq_ignore_ascii_case("b") {
            continue;
        }
        if let Ok(n) = digits.parse::<u64>() {
            return Some(n);
        }
    }
    None
}

/// The large-model threshold (billions of params), from
/// `INTAKE_LARGE_MODEL_PARAMS_B` or [`DEFAULT_LARGE_MODEL_PARAMS_B`].
pub fn large_model_threshold_b() -> u64 {
    env_timeout_secs("INTAKE_LARGE_MODEL_PARAMS_B", DEFAULT_LARGE_MODEL_PARAMS_B)
}

/// The reload-cost allowance (seconds), from
/// `INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC` or [`DEFAULT_RELOAD_ALLOWANCE_SEC`].
pub fn reload_allowance_secs() -> u64 {
    env_timeout_secs("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC", DEFAULT_RELOAD_ALLOWANCE_SEC)
}

/// True when `model_name`'s parsed param count meets or exceeds
/// [`large_model_threshold_b`]. A name with no parseable size token (`None`
/// from [`model_param_billions`]) is never treated as large — an unknown
/// size must not silently inflate every case's timeout.
pub fn is_large_model(model_name: &str) -> bool {
    model_param_billions(model_name)
        .map(|b| b >= large_model_threshold_b())
        .unwrap_or(false)
}

/// Additive reload-cost adjustment on top of a resolved difficulty-tier
/// timeout (`tier_default_secs`, or a case's own explicit override — either
/// way, `base_secs` is the difficulty budget this call layers on top of).
/// Does NOT change what blitz/standard/deep (or a per-case override) MEAN
/// for difficulty: for any model that isn't [`is_large_model`], this returns
/// `base_secs` unchanged, so the common case (small/mid model, no reload
/// risk) is byte-for-byte identical to pre-fix behavior. Only large models
/// get `base_secs + reload_allowance_secs()`.
pub fn reload_adjusted_timeout_secs(base_secs: u64, model_name: &str) -> u64 {
    if is_large_model(model_name) {
        base_secs + reload_allowance_secs()
    } else {
        base_secs
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

    // ---- model_param_billions ----

    #[test]
    fn model_param_billions_parses_the_production_stall_model() {
        assert_eq!(model_param_billions("qwen2.5-coder:32b-instruct"), Some(32));
    }

    #[test]
    fn model_param_billions_parses_various_ollama_tag_shapes() {
        assert_eq!(model_param_billions("gpt-oss-120b"), Some(120));
        assert_eq!(model_param_billions("llama3.3:70b"), Some(70));
        assert_eq!(model_param_billions("gemma3:12b"), Some(12));
        assert_eq!(model_param_billions("qwen2.5-coder:14b-instruct"), Some(14));
        assert_eq!(model_param_billions("QWEN3:8B"), Some(8), "case-insensitive suffix");
    }

    #[test]
    fn model_param_billions_none_when_no_size_token() {
        assert_eq!(model_param_billions("qwen3-coder:next"), None);
        assert_eq!(model_param_billions("mistral-nemo"), None);
        assert_eq!(model_param_billions(""), None);
    }

    #[test]
    fn model_param_billions_does_not_false_match_plain_name_segments() {
        // "coder", "instruct", "gemma3" etc. must never parse as a size —
        // only an ALL-DIGITS token with a trailing b/B counts.
        assert_eq!(model_param_billions("coder"), None);
        assert_eq!(model_param_billions("gemma3"), None);
        assert_eq!(model_param_billions("b"), None, "bare 'b' with no digits");
    }

    // ---- is_large_model / reload_adjusted_timeout_secs ----

    #[test]
    #[serial_test::serial(intake_env)]
    fn is_large_model_true_at_and_above_default_threshold() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INTAKE_LARGE_MODEL_PARAMS_B");
        assert!(is_large_model("qwen2.5-coder:32b-instruct"), "32B >= default 30B threshold");
        assert!(is_large_model("llama3.3:70b"));
        assert!(!is_large_model("qwen2.5-coder:14b-instruct"), "14B < default 30B threshold");
        assert!(!is_large_model("qwen3:8b"));
    }

    #[test]
    fn is_large_model_false_for_unparseable_name() {
        // An unknown/unparseable size must NEVER silently inflate every
        // case's timeout — absence of a size token is not "assume large".
        assert!(!is_large_model("qwen3-coder:next"));
        assert!(!is_large_model("mystery-model"));
    }

    #[test]
    #[serial_test::serial(intake_env)]
    fn large_model_threshold_and_allowance_are_env_overridable() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INTAKE_LARGE_MODEL_PARAMS_B", "16");
        assert!(is_large_model("qwen2.5-coder:24b"), "24B now above the lowered 16B threshold");
        std::env::remove_var("INTAKE_LARGE_MODEL_PARAMS_B");

        std::env::set_var("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC", "99");
        assert_eq!(reload_allowance_secs(), 99);
        std::env::remove_var("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC");
    }

    #[test]
    #[serial_test::serial(intake_env)]
    fn reload_adjusted_timeout_secs_unchanged_for_small_model_common_case() {
        // The common case: small model, no reload risk. This is the
        // regression this test guards — the fix must NOT change behavior
        // for the 99% case, only the specific large-model scenario.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INTAKE_LARGE_MODEL_PARAMS_B");
        std::env::remove_var("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC");
        assert_eq!(reload_adjusted_timeout_secs(60, "qwen2.5-coder:14b-instruct"), 60);
        assert_eq!(reload_adjusted_timeout_secs(120, "qwen3:8b"), 120);
        assert_eq!(reload_adjusted_timeout_secs(300, "qwen3-coder:next"), 300, "unparseable size stays unadjusted");
    }

    #[test]
    #[serial_test::serial(intake_env)]
    fn reload_adjusted_timeout_secs_adds_allowance_for_large_model() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INTAKE_LARGE_MODEL_PARAMS_B");
        std::env::remove_var("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC");
        // blitz (60s) is exactly the tier that starved on the production
        // stall (60s tier vs. a 20-40s measured cold reload).
        assert_eq!(reload_adjusted_timeout_secs(60, "qwen2.5-coder:32b-instruct"), 105);
        assert_eq!(reload_adjusted_timeout_secs(300, "llama3.3:70b"), 345);
    }

    #[test]
    #[serial_test::serial(intake_env)]
    fn reload_adjusted_timeout_secs_allowance_is_flat_not_multiplicative() {
        // Adversarial concern: the allowance must not scale with base_secs
        // (which would let a "deep" tier balloon disproportionately) — it's
        // always exactly `reload_allowance_secs()` more, a bounded constant.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INTAKE_LARGE_MODEL_PARAMS_B");
        std::env::remove_var("INTAKE_RELOAD_TIMEOUT_ALLOWANCE_SEC");
        let allowance = reload_allowance_secs();
        for base in [60, 120, 300, 900] {
            assert_eq!(reload_adjusted_timeout_secs(base, "qwen2.5-coder:32b-instruct"), base + allowance);
        }
    }
}
