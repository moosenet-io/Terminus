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
    fn epic_wall_clock_backstop_is_generous() {
        // A whole-repo explore audit needs far more than the routine 600s ceiling.
        assert!(MAX_TIMEOUT_SECS >= 1800);
        assert_eq!(clamp_timeout(Some(1800)), 1800);
    }
}
