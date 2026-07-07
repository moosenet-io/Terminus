//! Sanitized child-process environment.
//!
//! Ported byte-for-byte (logic, not code layout) from Harmony's
//! `harmony-core/src/providers/subprocess.rs::sanitized_env`, per the review-daemon
//! spec's requirement to mirror that env-sanitization exactly. Computed ONCE at
//! daemon startup and never re-derived per request — no caller-supplied env vars
//! are ever merged into a child process's environment.

use std::collections::HashMap;

/// Patterns that indicate a secret in environment variable names.
const SECRET_PATTERNS: &[&str] = &["TOKEN", "KEY", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"];

/// Env vars that are safe to pass to subprocesses.
const ALLOWED_VARS: &[&str] = &[
    "HOME", "PATH", "LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE",
    "TERM", "SHELL", "USER", "LOGNAME", "HOSTNAME",
    "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR",
    "TMPDIR", "TMP", "TEMP",
];

/// Build a sanitized environment for subprocess execution from an arbitrary
/// iterator of (key, value) pairs. Split out from [`sanitized_env`] so tests can
/// feed a synthetic env without mutating the real process environment.
pub fn sanitize_from<I: IntoIterator<Item = (String, String)>>(vars: I) -> HashMap<String, String> {
    let mut env = HashMap::new();

    for (key, val) in vars {
        let upper = key.to_uppercase();

        // Always allow explicitly safe vars.
        if ALLOWED_VARS.iter().any(|a| upper == *a) {
            env.insert(key, val);
            continue;
        }

        // Block anything matching secret patterns.
        if SECRET_PATTERNS.iter().any(|pat| upper.contains(pat)) {
            continue;
        }

        // Block Harmony/<secret-manager>/Plane/Gitea internal vars. // pii-test-fixture
        if upper.starts_with("HARMONY_")
            || upper.starts_with("INFISICAL_")
            || upper.starts_with("PLANE_")
            || upper.starts_with("GITEA_")
        {
            continue;
        }

        env.insert(key, val);
    }

    env
}

/// Build a sanitized environment for subprocess execution, sourced from the
/// real process environment. Call exactly once at daemon startup; the result
/// must be cached (e.g. in `AppState`) and reused for every dispatch — never
/// re-read `std::env::vars()` per request.
pub fn sanitized_env() -> HashMap<String, String> {
    sanitize_from(std::env::vars())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn strips_secret_like_vars() {
        let env = sanitize_from(v(&[
            ("HOME", "/home/test"),
            ("SOME_API_TOKEN", "shh"),
            ("DB_PASSWORD", "shh"),
            ("MY_CREDENTIAL_FILE", "shh"),
        ]));
        assert!(env.contains_key("HOME"));
        assert!(!env.contains_key("SOME_API_TOKEN"));
        assert!(!env.contains_key("DB_PASSWORD"));
        assert!(!env.contains_key("MY_CREDENTIAL_FILE"));
    }

    #[test]
    fn strips_internal_prefixes() {
        let env = sanitize_from(v(&[
            ("HARMONY_CLAUDE_MODEL", "x"),
            ("INFISICAL_CLIENT_ID", "x"),
            ("PLANE_API_URL", "x"),
            ("GITEA_TOKEN_UNUSED", "x"),
        ]));
        assert!(env.is_empty());
    }

    #[test]
    fn keeps_allowlisted_vars() {
        let env = sanitize_from(v(&[("LANG", "en_US.UTF-8"), ("TERM", "xterm-256color"), ("PATH", "/usr/bin")]));
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn passes_general_safe_vars_through() {
        let env = sanitize_from(v(&[("EDITOR", "vim")]));
        assert!(env.contains_key("EDITOR"));
    }

    #[test]
    fn review_daemon_token_itself_is_stripped() {
        // REVIEW_DAEMON_TOKEN contains "TOKEN" -- must never leak into a child process env.
        let env = sanitize_from(v(&[("REVIEW_DAEMON_TOKEN", "supersecret")]));
        assert!(env.is_empty());
    }
}
