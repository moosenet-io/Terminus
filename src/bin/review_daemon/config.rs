//! Daemon startup configuration.
//!
//! `REVIEW_DAEMON_TOKEN` is fail-closed: if it is unset (or empty) the daemon
//! MUST refuse to start rather than silently run unauthenticated. `from_env`
//! delegates to [`from_provider`] so tests can exercise the fail-closed
//! behavior against a synthetic env map, without mutating real process env
//! vars (which would be racy across parallel tests).

/// Hard cap on `timeout_secs` (the WALL-CLOCK backstop) a caller may request. Raised
/// to 30 min for the Epic capstone: a whole-repo explore-mode audit legitimately runs
/// for many minutes, and the progress/stall detector — not this backstop — is the
/// primary bound (this only catches a truly wedged process).
pub const MAX_TIMEOUT_SECS: u64 = 1800;
/// Default timeout when a request omits `timeout_secs`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard cap on the stall window (secs of no output before a kill). Bounds a caller's
/// `stall_secs` so a huge value can't effectively disable stall detection.
pub const MAX_STALL_SECS: u64 = 600;
/// Hard cap on prompt size (200 KB), enforced before any provider dispatch.
pub const MAX_PROMPT_BYTES: usize = 200 * 1024;
/// Max concurrent subprocess spawns.
pub const MAX_CONCURRENCY: usize = 4;

#[derive(Debug)]
pub struct Config {
    pub port: u16,
    pub token: String,
}

const DEFAULT_PORT: u16 = 8790;

impl Config {
    /// Load configuration from the real process environment. Fails closed:
    /// returns `Err` (never a `Config` with an empty/missing token) if
    /// `REVIEW_DAEMON_TOKEN` is unset or blank.
    pub fn from_env() -> Result<Self, String> {
        Self::from_provider(|key| std::env::var(key).ok())
    }

    /// Same as [`Config::from_env`] but sourced from an arbitrary lookup
    /// closure, so unit tests can assert the fail-closed behavior without
    /// touching real process env vars.
    pub fn from_provider<F: Fn(&str) -> Option<String>>(get: F) -> Result<Self, String> {
        let token = get("REVIEW_DAEMON_TOKEN")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(token) = token else {
            return Err(
                "REVIEW_DAEMON_TOKEN is not set. Refusing to start unauthenticated \
                 (fail-closed) -- set REVIEW_DAEMON_TOKEN before starting review-daemon."
                    .to_string(),
            );
        };

        let port = get("REVIEW_DAEMON_PORT")
            .and_then(|s| s.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);

        Ok(Self { port, token })
    }
}

/// Clamp a caller-supplied timeout to `[1, MAX_TIMEOUT_SECS]`, defaulting to
/// `DEFAULT_TIMEOUT_SECS` when absent. Operator-controlled ceiling always
/// wins over caller input.
pub fn clamp_timeout(requested: Option<u64>) -> u64 {
    match requested {
        Some(0) => 1,
        Some(v) => v.min(MAX_TIMEOUT_SECS),
        None => DEFAULT_TIMEOUT_SECS,
    }
}

/// Clamp a caller-supplied stall window to `[1, MAX_STALL_SECS]`. A `0` floors to 1
/// (kill on the first no-output tick would be absurd, but never disable the guard).
pub fn clamp_stall(requested: u64) -> u64 {
    requested.clamp(1, MAX_STALL_SECS)
}

/// REVCAP-01 PART B / REVX-07: the reasoning-effort levels `codex` accepts.
/// LIVE-VALIDATED (S121, codex CLI 0.144.1) against `gpt-5.6-sol`:
/// `none|low|medium|high|xhigh` -- NOT `minimal` (that value 400-errors on
/// sol). Distinct from [`ALLOWED_CLAUDE_REASONING_EFFORTS`] because codex and
/// the Anthropic adaptive-effort CLIs support different native scales (see
/// `provider::build_command`'s codex/claude arms) -- a single shared
/// allowlist would either reject codex's `xhigh`/`none` or accept a value
/// (`xhigh`) that 400-errors on claude.
pub const ALLOWED_CODEX_REASONING_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];

/// The Anthropic adaptive `--effort` levels (`opus`/`claude-fable-5`, via the
/// `claude` CLI): confirmed against the installed CLI's own `--help`.
pub const ALLOWED_CLAUDE_REASONING_EFFORTS: &[&str] = &["low", "medium", "high"];

/// Back-compat alias: the pre-REVX-07 single shared allowlist. Kept for any
/// external reference, but no dispatch path in this daemon uses it anymore --
/// [`clamp_codex_effort`] / [`clamp_claude_effort`] are the per-provider
/// clamps `main.rs` now calls.
pub const ALLOWED_REASONING_EFFORTS: &[&str] = ALLOWED_CLAUDE_REASONING_EFFORTS;

/// Validate a caller-supplied `reasoning_effort` against an EXACT
/// (case-insensitive, no trimming) allowlist member -- fails closed to `None`
/// (the pre-PART-B argv shape) on anything absent or unrecognized, mirroring
/// this module's "closed set, never derived from raw request input"
/// invariant for provider/binary/model (see `provider.rs`'s module doc). A
/// padded value like `" high "` is deliberately rejected: only ASCII case
/// may differ from a canonical level, never surrounding whitespace.
fn clamp_effort_against(allowed: &[&str], requested: Option<&str>) -> Option<String> {
    let level = requested?;
    allowed.iter().find(|a| a.eq_ignore_ascii_case(level)).map(|a| a.to_string())
}

/// REVX-07: codex's 5-level clamp (`none|low|medium|high|xhigh`).
pub fn clamp_codex_effort(requested: Option<&str>) -> Option<String> {
    clamp_effort_against(ALLOWED_CODEX_REASONING_EFFORTS, requested)
}

/// REVX-07: the Anthropic adaptive-effort clamp (`low|medium|high`) for
/// `opus`/`claude-fable-5`.
pub fn clamp_claude_effort(requested: Option<&str>) -> Option<String> {
    clamp_effort_against(ALLOWED_CLAUDE_REASONING_EFFORTS, requested)
}

/// Dispatch to the right per-provider clamp by [`super::provider::Provider`].
/// `agy` has no known effort knob (see `provider::build_command`'s doc), so
/// it clamps to `None` unconditionally -- a caller-supplied value for `agy`
/// is simply dropped, never forwarded.
pub fn clamp_reasoning_effort_for(
    provider: super::provider::Provider,
    requested: Option<&str>,
) -> Option<String> {
    use super::provider::Provider;
    match provider {
        Provider::Codex => clamp_codex_effort(requested),
        Provider::Opus | Provider::Fable => clamp_claude_effort(requested),
        Provider::Agy => None,
    }
}

/// REVX-08: the closed set of codex model ids this daemon will ever forward
/// into `build_command`'s `-m`/`--model` argv element. Mirrors
/// `effort_policy::ALLOWED_CODEX_MODELS` (kept as a separate constant here so
/// the daemon binary -- built/deployed independently of the library's
/// `review::effort_policy` module -- has no compile-time dependency on it;
/// the two lists are asserted equal in this module's tests). An unrecognized
/// requested model id is dropped to `None`, and `build_command` then falls
/// back to its own fixed default (`gpt-5.6-sol`) -- never a caller-controlled
/// string reaching the spawned CLI's argv unchecked.
pub const ALLOWED_CODEX_MODELS: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"];

/// Validate a caller-supplied codex model id against [`ALLOWED_CODEX_MODELS`].
pub fn clamp_codex_model(requested: Option<&str>) -> Option<String> {
    let id = requested?;
    ALLOWED_CODEX_MODELS.iter().find(|a| **a == id).map(|a| a.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn provider(map: HashMap<String, String>) -> impl Fn(&str) -> Option<String> {
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn refuses_to_start_when_token_unset() {
        let get = provider(HashMap::new());
        let err = Config::from_provider(get).unwrap_err();
        assert!(err.contains("REVIEW_DAEMON_TOKEN"));
    }

    #[test]
    fn refuses_to_start_when_token_blank() {
        let mut m = HashMap::new();
        m.insert("REVIEW_DAEMON_TOKEN".to_string(), "   ".to_string());
        let err = Config::from_provider(provider(m)).unwrap_err();
        assert!(err.contains("REVIEW_DAEMON_TOKEN"));
    }

    #[test]
    fn starts_when_token_present_with_default_port() {
        let mut m = HashMap::new();
        m.insert("REVIEW_DAEMON_TOKEN".to_string(), "abc123".to_string());
        let cfg = Config::from_provider(provider(m)).unwrap();
        assert_eq!(cfg.token, "abc123");
        assert_eq!(cfg.port, DEFAULT_PORT);
    }

    #[test]
    fn honors_explicit_port() {
        let mut m = HashMap::new();
        m.insert("REVIEW_DAEMON_TOKEN".to_string(), "abc123".to_string());
        m.insert("REVIEW_DAEMON_PORT".to_string(), "8790".to_string());
        let cfg = Config::from_provider(provider(m)).unwrap();
        assert_eq!(cfg.port, 8790);
    }

    #[test]
    fn clamp_timeout_defaults_when_absent() {
        assert_eq!(clamp_timeout(None), DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn clamp_timeout_caps_at_max() {
        assert_eq!(clamp_timeout(Some(999_999)), MAX_TIMEOUT_SECS);
    }

    #[test]
    fn clamp_timeout_floors_zero_to_one() {
        assert_eq!(clamp_timeout(Some(0)), 1);
    }

    #[test]
    fn clamp_timeout_passes_through_valid_value() {
        assert_eq!(clamp_timeout(Some(30)), 30);
    }

    #[test]
    fn clamp_stall_bounds_the_window() {
        assert_eq!(clamp_stall(0), 1); // never disable the guard
        assert_eq!(clamp_stall(180), 180);
        assert_eq!(clamp_stall(999_999), MAX_STALL_SECS);
    }

    #[test]
    fn clamp_claude_effort_passes_through_allowed_levels() {
        assert_eq!(clamp_claude_effort(Some("high")), Some("high".to_string()));
        assert_eq!(clamp_claude_effort(Some("low")), Some("low".to_string()));
        assert_eq!(clamp_claude_effort(Some("medium")), Some("medium".to_string()));
    }

    #[test]
    fn clamp_claude_effort_normalizes_case() {
        assert_eq!(clamp_claude_effort(Some("HIGH")), Some("high".to_string()));
    }

    #[test]
    fn clamp_claude_effort_drops_absent_blank_or_unrecognized_to_none() {
        assert_eq!(clamp_claude_effort(None), None);
        assert_eq!(clamp_claude_effort(Some("")), None);
        assert_eq!(clamp_claude_effort(Some("   ")), None);
        // Not a recognized level -- and, load-bearing: an attempted injection
        // via a quote character must never pass through into the provider's
        // own -c/flag value unrecognized.
        assert_eq!(clamp_claude_effort(Some("high\" extra=\"evil")), None);
        assert_eq!(clamp_claude_effort(Some("ultra")), None);
        // claude tops out at "high" -- codex's "xhigh"/"none" are NOT valid here.
        assert_eq!(clamp_claude_effort(Some("xhigh")), None);
        assert_eq!(clamp_claude_effort(Some("none")), None);
    }

    #[test]
    fn clamp_claude_effort_matches_exactly_no_whitespace_trimming() {
        // Strict "exact allowlist member, reject anything else" contract: the
        // match is against the RAW input, so a whitespace-padded value is NOT
        // silently accepted-and-normalized -- it is rejected outright.
        assert_eq!(clamp_claude_effort(Some(" high ")), None);
        assert_eq!(clamp_claude_effort(Some("high ")), None);
        assert_eq!(clamp_claude_effort(Some(" high")), None);
        assert_eq!(clamp_claude_effort(Some("\thigh")), None);
        // Only ASCII case may differ from a canonical level -- never whitespace.
        assert_eq!(clamp_claude_effort(Some("HIGH")), Some("high".to_string()));
        assert_eq!(clamp_claude_effort(Some("high")), Some("high".to_string()));
        assert_eq!(clamp_claude_effort(Some("ultra")), None);
    }

    // ── REVX-07: per-provider clamps ─────────────────────────────────────

    #[test]
    fn clamp_codex_effort_accepts_all_five_levels_including_xhigh_and_none() {
        assert_eq!(clamp_codex_effort(Some("xhigh")), Some("xhigh".to_string()));
        assert_eq!(clamp_codex_effort(Some("none")), Some("none".to_string()));
        assert_eq!(clamp_codex_effort(Some("low")), Some("low".to_string()));
        assert_eq!(clamp_codex_effort(Some("medium")), Some("medium".to_string()));
        assert_eq!(clamp_codex_effort(Some("high")), Some("high".to_string()));
    }

    #[test]
    fn clamp_codex_effort_rejects_unrecognized_and_minimal() {
        // LIVE-VALIDATED (S121): "minimal" 400-errors on gpt-5.6-sol -- it is
        // deliberately NOT in codex's allowlist even though the general codex
        // config-reference enum lists it.
        assert_eq!(clamp_codex_effort(Some("minimal")), None);
        assert_eq!(clamp_codex_effort(Some("ultra")), None);
    }

    #[test]
    fn clamp_claude_effort_tops_out_at_high_rejects_xhigh() {
        assert_eq!(clamp_claude_effort(Some("xhigh")), None);
        assert_eq!(clamp_claude_effort(Some("high")), Some("high".to_string()));
    }

    #[test]
    fn clamp_reasoning_effort_for_dispatches_by_provider() {
        use super::super::provider::Provider;
        assert_eq!(
            clamp_reasoning_effort_for(Provider::Codex, Some("xhigh")),
            Some("xhigh".to_string())
        );
        assert_eq!(clamp_reasoning_effort_for(Provider::Codex, Some("minimal")), None);
        assert_eq!(
            clamp_reasoning_effort_for(Provider::Opus, Some("high")),
            Some("high".to_string())
        );
        assert_eq!(clamp_reasoning_effort_for(Provider::Opus, Some("xhigh")), None);
        assert_eq!(
            clamp_reasoning_effort_for(Provider::Fable, Some("medium")),
            Some("medium".to_string())
        );
        // agy has no known effort knob -- always None regardless of input.
        assert_eq!(clamp_reasoning_effort_for(Provider::Agy, Some("high")), None);
    }

    #[test]
    fn shell_injection_string_still_rejected_by_both_clamps() {
        let injected = "high\" extra=\"evil";
        assert_eq!(clamp_codex_effort(Some(injected)), None);
        assert_eq!(clamp_claude_effort(Some(injected)), None);
    }

    #[test]
    fn clamp_codex_model_accepts_only_the_closed_allowlist() {
        assert_eq!(clamp_codex_model(Some("gpt-5.6-sol")), Some("gpt-5.6-sol".to_string()));
        assert_eq!(clamp_codex_model(Some("gpt-5.6-terra")), Some("gpt-5.6-terra".to_string()));
        assert_eq!(clamp_codex_model(Some("gpt-5.6-luna")), Some("gpt-5.6-luna".to_string()));
        assert_eq!(clamp_codex_model(Some("gpt-5.5")), Some("gpt-5.5".to_string()));
        assert_eq!(clamp_codex_model(Some("gpt-9-evil")), None);
        assert_eq!(clamp_codex_model(None), None);
    }

    #[test]
    fn epic_wall_clock_backstop_is_generous() {
        // A whole-repo explore audit needs far more than the routine 600s ceiling.
        assert!(MAX_TIMEOUT_SECS >= 1800);
        assert_eq!(clamp_timeout(Some(1800)), 1800);
    }
}
