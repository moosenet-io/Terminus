//! Startup secret bootstrap for downstream Gitea / Plane / GitHub credentials.
//!
//! Any Terminus binary that constructs `gitea` / `plane` / `github` tool
//! clients can call [`bootstrap_gitea_plane_github_secrets`] once at startup —
//! before the first `*::from_env()` client is built — to freshen the relevant
//! `GITEA_*` / `PLANE_*` / `GITHUB_*` env vars from the runtime secret store.
//! A credential rotated in the store is then picked up on the next restart,
//! instead of requiring someone to notice a rotation, re-run a fetch-and-splice
//! script against a static `.env`, and restart the service before writes work
//! again.
//!
//! This generalizes the per-binary startup fetch first built for
//! `terminus_personal` (PSEC-02) into one reusable entry point. It reuses the
//! batch-fetch client shared with the guarded `infisical_get_secrets_batch`
//! MCP tool (PSEC-01) as the transport rather than reinventing it — the
//! bootstrap credential and project coordinates are the only inputs.
//!
//! ## Behavior contract
//! - **Not configured** — when the bootstrap credential
//!   (`INFISICAL_URL` / `INFISICAL_CLIENT_ID` / `INFISICAL_CLIENT_SECRET`) is
//!   not fully set, or no project id is supplied, nothing is attempted and the
//!   caller keeps whatever is already in the process environment (e.g. a
//!   static `.env` loaded by a systemd `EnvironmentFile=`).
//! - **Fetched** — the named downstream keys present in the store are applied
//!   to the process environment via `std::env::set_var`, so every
//!   `X::from_env()`-style client built afterward transparently sees the
//!   current value.
//! - **Failed** — any auth failure, network error, or malformed response falls
//!   back cleanly to the static environment. It is never a hard startup
//!   failure and never panics or hangs.
//! - No secret VALUE is ever logged — only counts and, for absent keys, key
//!   NAMES.

use crate::<secret-manager>::{fetch_secrets_batch, InfisicalConfig}; // pii-test-fixture

/// The downstream credential keys a Gitea/Plane/GitHub-writing binary needs.
///
/// Deliberately a fixed, named allowlist (not "apply every key found at this
/// path") so a shared store path holding secrets for other services can never
/// leak into this process's environment.
///
/// **BREAKING (S105/GPAT):** the unsuffixed `GITEA_TOKEN` was removed here — the
/// Gitea tool now authenticates solely via per-identity `GITEA_PAT_<NAME>`
/// tokens, materialized dynamically through [`PAT_KEY_PREFIXES`] below (default
/// identity `moose` → `GITEA_PAT_MOOSE`). There is no single unsuffixed Gitea
/// token to materialize any more, so nothing in this fixed allowlist reads one.
pub const GITEA_PLANE_GITHUB_SECRET_KEYS: &[&str] = &[
    "PLANE_API_URL",
    "PLANE_API_KEY",
    "PLANE_WORKSPACE",
    "GITEA_URL",
    "GITHUB_TOKEN",
];

/// Prefixes for named-identity Personal Access Tokens. In addition to the fixed
/// [`GITEA_PLANE_GITHUB_SECRET_KEYS`] above, any secret key at the fetch path
/// beginning with one of these prefixes is materialized into the process
/// environment too, so the per-identity token resolution in each tool
/// (`PlaneClient::for_identity` / `GiteaClient::for_identity`) can see it:
///
/// - `PLANE_PAT_*` — Plane identities (`PLANE_PAT_CLAUDE`, `PLANE_PAT_MOOSE`, …).
/// - `GITEA_PAT_*` — Gitea identities (`GITEA_PAT_MOOSE`, `GITEA_PAT_HARMONY`,
///   `GITEA_PAT_LUMINA`, …) — S105/GPAT, replacing the retired `GITEA_TOKEN`.
/// - `GITHUB_PAT_*` — GitHub identities (`GITHUB_PAT_MOOSE`, …) — S106/GITX-03,
///   the git-public `github` adapter's per-identity model, alongside the
///   unsuffixed `GITHUB_TOKEN` fallback in the fixed allowlist above.
///
/// A *dynamic prefix match*, not another fixed list: a newly-provisioned
/// identity becomes usable on the next restart with no code change. Matching is
/// scoped to exactly these prefixes (never "set every key found at the path"),
/// preserving the same anti-leak property as the fixed allowlist above.
pub const PAT_KEY_PREFIXES: &[&str] = &["PLANE_PAT_", "GITEA_PAT_", "GITHUB_PAT_"];

/// Outcome of the startup bootstrap attempt. Returned to the caller so it can
/// log the result (via [`log_secret_bootstrap_outcome`]) and so tests can
/// assert on the result directly rather than scraping log text.
#[derive(Debug, PartialEq, Eq)]
pub enum SecretBootstrapOutcome {
    /// The bootstrap credential or the project id was not configured — nothing
    /// was attempted, and the static environment is used unchanged.
    NotConfigured,
    /// The fetch succeeded; `count` downstream keys were found and applied to
    /// the process environment. `missing` names (never values) any of
    /// [`GITEA_PLANE_GITHUB_SECRET_KEYS`] the store did not have at this path.
    /// `identities` is how many of `count` are named-identity PATs
    /// (`PLANE_PAT_*` / `GITEA_PAT_*`, picked up dynamically via
    /// [`PAT_KEY_PREFIXES`]) — a set-but-blank value is treated as missing for
    /// both counters, never set.
    Fetched {
        count: usize,
        missing: Vec<String>,
        identities: usize,
    },
    /// The fetch was attempted but failed (auth failure, network error,
    /// malformed response, ...) — the caller falls back to whatever is already
    /// in the process environment. `reason` is a display-formatted error,
    /// never a secret value.
    Failed { reason: String },
}

/// Fetch this process's downstream Gitea/Plane/GitHub credentials from the
/// runtime secret store and apply them to the process environment, so every
/// `X::from_env()`-style client constructed after this call sees the current
/// value.
///
/// The bootstrap credential is read from the environment
/// (`InfisicalConfig::from_env`); `project_id` / `environment` / `secret_path`
/// are supplied by the caller so each binary controls its own coordinates
/// without any value being hardcoded here. A `None` or empty `project_id`
/// short-circuits to [`SecretBootstrapOutcome::NotConfigured`] before any
/// network call.
///
/// Falls back cleanly (never panics, never hangs, never hard-fails startup)
/// when the store is not configured or the fetch fails. Never logs or echoes
/// any fetched secret value — only counts and, for absent keys, key NAMES.
pub async fn bootstrap_gitea_plane_github_secrets(
    project_id: Option<&str>,
    environment: &str,
    secret_path: &str,
) -> SecretBootstrapOutcome {
    let config = InfisicalConfig::from_env();
    if !config.is_configured() {
        return SecretBootstrapOutcome::NotConfigured;
    }

    let project_id = match project_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => p,
        None => return SecretBootstrapOutcome::NotConfigured,
    };

    match fetch_secrets_batch(&config, project_id, environment, secret_path).await {
        Ok(fetched) => {
            let mut count = 0usize;
            let mut missing = Vec::new();
            // Fixed downstream allowlist: the base connection secrets. A
            // present-but-blank value is treated as MISSING (never set into the
            // environment) so a blank provider value can never silently clobber
            // a valid static-env fallback with an empty string.
            for key in GITEA_PLANE_GITHUB_SECRET_KEYS {
                match fetched.get(*key).filter(|v| !v.is_empty()) {
                    Some(value) => {
                        std::env::set_var(key, value);
                        count += 1;
                    }
                    None => missing.push((*key).to_string()),
                }
            }
            // Named-identity PATs: materialize every non-blank `PLANE_PAT_*` /
            // `GITEA_PAT_*` key found at this path (dynamic prefix match — a
            // newly-provisioned identity works with no code change). Same
            // blank-as-missing rule. Not tracked in `missing`: identities are
            // provisioned ad hoc, so there is no fixed expected set for one to
            // be "missing" from.
            let mut identities = 0usize;
            for (key, value) in &fetched {
                let is_pat = PAT_KEY_PREFIXES.iter().any(|p| key.starts_with(p));
                if is_pat && !value.is_empty() {
                    std::env::set_var(key, value);
                    count += 1;
                    identities += 1;
                }
            }
            SecretBootstrapOutcome::Fetched {
                count,
                missing,
                identities,
            }
        }
        Err(e) => SecretBootstrapOutcome::Failed {
            reason: e.to_string(),
        },
    }
}

/// Log the outcome of the startup bootstrap. Split out from
/// [`bootstrap_gitea_plane_github_secrets`] so tests can assert on the returned
/// enum directly without needing to capture tracing output. Logs key names and
/// counts only — never a secret value.
pub fn log_secret_bootstrap_outcome(outcome: &SecretBootstrapOutcome) {
    match outcome {
        SecretBootstrapOutcome::NotConfigured => {
            tracing::info!(
                "secret bootstrap skipped: bootstrap credential or project id not configured, using static environment"
            );
        }
        SecretBootstrapOutcome::Fetched { count, missing, identities } => {
            tracing::info!("secret bootstrap: applied {count} downstream secrets ({identities} named PAT identities) to the process environment");
            if !missing.is_empty() {
                tracing::warn!(
                    "secret bootstrap: store did not include: {} (using static environment for these, if present)",
                    missing.join(", ")
                );
            }
        }
        SecretBootstrapOutcome::Failed { reason } => {
            tracing::warn!(
                "secret bootstrap failed ({reason}), falling back to static environment"
            );
        }
    }
}

// ── Tests: startup-time downstream secret bootstrap ──────────────────────────
//
// All env-var mutation is process-global, so every test clears the full set of
// relevant keys before AND after itself and runs #[serial] (matching the
// convention used by this crate's other secret-fetch tests).

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;
    use std::sync::{Arc, Mutex};

    const ALL_TEST_ENV_KEYS: &[&str] = &[
        "INFISICAL_URL",
        "INFISICAL_CLIENT_ID",
        "INFISICAL_CLIENT_SECRET",
        "PLANE_API_URL",
        "PLANE_API_KEY",
        "PLANE_WORKSPACE",
        "GITEA_URL",
        "GITHUB_TOKEN",
        "PLANE_PAT_CLAUDE",
        "GITEA_PAT_MOOSE",
        "GITEA_PAT_HARMONY",
        "GITEA_PAT_FUTURE",
        "GITEA_PAT_BLANK",
        "SOME_OTHER_SERVICE_SECRET",
    ];

    fn clear_all_env() {
        for key in ALL_TEST_ENV_KEYS {
            std::env::remove_var(key);
        }
    }

    fn configure_bootstrap_credential(base_url: &str) {
        std::env::set_var("INFISICAL_URL", base_url);
        std::env::set_var("INFISICAL_CLIENT_ID", "cid"); // pii-test-fixture
        std::env::set_var("INFISICAL_CLIENT_SECRET", "csecret"); // pii-test-fixture
    }

    fn mock_login(server: &MockServer, token: &str) {
        server.mock(|when, then| {
            when.method(POST).path("/api/v1/auth/universal-auth/login");
            then.status(200).json_body(json!({ "accessToken": token }));
        });
    }

    // ── NotConfigured: proceeds cleanly, no crash, no hang, no env mutation ──

    #[tokio::test]
    #[serial]
    async fn not_configured_falls_back_without_crash_or_env_mutation() {
        clear_all_env();

        let outcome =
            bootstrap_gitea_plane_github_secrets(Some("proj1"), "prod", "/").await;
        assert_eq!(outcome, SecretBootstrapOutcome::NotConfigured);
        for key in GITEA_PLANE_GITHUB_SECRET_KEYS {
            assert!(std::env::var(key).is_err(), "{key} should not have been set");
        }

        clear_all_env();
    }

    #[tokio::test]
    #[serial]
    async fn credential_configured_but_no_project_id_is_not_configured() {
        clear_all_env();
        // A configured bootstrap credential but no project id must short-circuit
        // BEFORE any network call. base_url points nowhere dial-able to prove no
        // request is attempted.
        std::env::set_var("INFISICAL_URL", "http://127.0.0.1:1");
        std::env::set_var("INFISICAL_CLIENT_ID", "cid"); // pii-test-fixture
        std::env::set_var("INFISICAL_CLIENT_SECRET", "csecret"); // pii-test-fixture

        let outcome = bootstrap_gitea_plane_github_secrets(None, "prod", "/").await;
        assert_eq!(outcome, SecretBootstrapOutcome::NotConfigured);

        // An empty / whitespace-only project id is treated the same way.
        let outcome_empty =
            bootstrap_gitea_plane_github_secrets(Some("   "), "prod", "/").await;
        assert_eq!(outcome_empty, SecretBootstrapOutcome::NotConfigured);

        clear_all_env();
    }

    // ── Fetched: values actually applied to the process environment ──────────

    #[tokio::test]
    #[serial]
    async fn fetched_secrets_are_applied_to_process_environment() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-1"); // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).json_body(json!({
                "secrets": [
                    { "secretKey": "PLANE_API_KEY", "secretValue": "fixture-plane-key" },
                    { "secretKey": "GITHUB_TOKEN", "secretValue": "fixture-github-token" }
                ]
            }));
        });
        configure_bootstrap_credential(&server.base_url());

        let outcome =
            bootstrap_gitea_plane_github_secrets(Some("proj1"), "prod", "/").await;
        match outcome {
            SecretBootstrapOutcome::Fetched { count, missing, identities } => {
                assert_eq!(count, 2);
                assert_eq!(missing.len(), GITEA_PLANE_GITHUB_SECRET_KEYS.len() - 2);
                assert_eq!(identities, 0);
            }
            other => panic!("expected Fetched, got {other:?}"),
        }
        assert_eq!(std::env::var("PLANE_API_KEY").unwrap(), "fixture-plane-key");
        assert_eq!(std::env::var("GITHUB_TOKEN").unwrap(), "fixture-github-token");

        clear_all_env();
    }

    // ── Named-identity PATs (S105/GPAT): dynamic PLANE_PAT_* / GITEA_PAT_*
    //    pickup, blank-as-missing, and no leakage of unrelated keys. ───────────
    #[tokio::test]
    #[serial]
    async fn named_identity_pats_are_materialized_dynamically() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-pat"); // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).json_body(json!({
                "secrets": [
                    // A base allowlist key (not a PAT) to prove coexistence.
                    { "secretKey": "GITEA_URL", "secretValue": "http://gitea.example.com" },
                    // Plane + Gitea PATs must BOTH be picked up dynamically.
                    { "secretKey": "PLANE_PAT_CLAUDE", "secretValue": "fixture-plane-claude" },
                    { "secretKey": "GITEA_PAT_MOOSE", "secretValue": "fixture-gitea-moose" },
                    { "secretKey": "GITEA_PAT_HARMONY", "secretValue": "fixture-gitea-harmony" },
                    // An identity the code has never heard of — picked up by prefix.
                    { "secretKey": "GITEA_PAT_FUTURE", "secretValue": "fixture-gitea-future" },
                    // A set-but-blank PAT must be treated as missing (not set).
                    { "secretKey": "GITEA_PAT_BLANK", "secretValue": "" },
                    // Unrelated secret sharing the path must NEVER be imported.
                    { "secretKey": "SOME_OTHER_SERVICE_SECRET", "secretValue": "should-not-leak" }
                ]
            }));
        });
        configure_bootstrap_credential(&server.base_url());

        let outcome =
            bootstrap_gitea_plane_github_secrets(Some("proj1"), "prod", "/").await;
        match outcome {
            SecretBootstrapOutcome::Fetched { count, identities, .. } => {
                // 1 base key (GITEA_URL) + 4 non-blank PATs (1 plane + 3 gitea) = 5.
                assert_eq!(count, 5, "expected 1 base + 4 non-blank PATs");
                assert_eq!(identities, 4, "blank PAT must not count; plane+gitea both count");
            }
            other => panic!("expected Fetched, got {other:?}"),
        }

        assert_eq!(std::env::var("GITEA_PAT_MOOSE").unwrap(), "fixture-gitea-moose");
        assert_eq!(std::env::var("GITEA_PAT_HARMONY").unwrap(), "fixture-gitea-harmony");
        assert_eq!(std::env::var("GITEA_PAT_FUTURE").unwrap(), "fixture-gitea-future");
        assert_eq!(std::env::var("PLANE_PAT_CLAUDE").unwrap(), "fixture-plane-claude");
        // Blank PAT is treated as missing, never set...
        assert!(std::env::var("GITEA_PAT_BLANK").is_err());
        // ...and an unrelated key never leaks in.
        assert!(std::env::var("SOME_OTHER_SERVICE_SECRET").is_err());

        clear_all_env();
    }

    #[tokio::test]
    #[serial]
    async fn empty_store_response_is_fetched_zero_not_an_error() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-2"); // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).json_body(json!({ "secrets": [] }));
        });
        configure_bootstrap_credential(&server.base_url());

        let outcome =
            bootstrap_gitea_plane_github_secrets(Some("proj1"), "prod", "/").await;
        match outcome {
            SecretBootstrapOutcome::Fetched { count, missing, identities } => {
                assert_eq!(count, 0);
                assert_eq!(missing.len(), GITEA_PLANE_GITHUB_SECRET_KEYS.len());
                assert_eq!(identities, 0);
            }
            other => panic!("expected Fetched{{count:0,..}}, got {other:?}"),
        }

        clear_all_env();
    }

    // ── Failed: falls back cleanly, never panics, never touches existing env ─

    #[tokio::test]
    #[serial]
    async fn fetch_failure_falls_back_cleanly_without_panic() {
        clear_all_env();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/v1/auth/universal-auth/login");
            then.status(401)
                .json_body(json!({ "message": "invalid credentials" }));
        });
        configure_bootstrap_credential(&server.base_url());
        // Pre-seed a static fallback value to prove a failed fetch leaves it
        // untouched (this is what a static `.env`-sourced value looks like in
        // production).
        std::env::set_var("GITEA_PAT_MOOSE", "static-fallback-token"); // pii-test-fixture

        let outcome =
            bootstrap_gitea_plane_github_secrets(Some("proj1"), "prod", "/").await;
        assert!(matches!(outcome, SecretBootstrapOutcome::Failed { .. }));
        assert_eq!(std::env::var("GITEA_PAT_MOOSE").unwrap(), "static-fallback-token");

        clear_all_env();
    }

    #[tokio::test]
    #[serial]
    async fn malformed_response_falls_back_cleanly() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-3"); // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).body("not valid json {{{");
        });
        configure_bootstrap_credential(&server.base_url());

        let outcome =
            bootstrap_gitea_plane_github_secrets(Some("proj1"), "prod", "/").await;
        assert!(matches!(outcome, SecretBootstrapOutcome::Failed { .. }));
        for key in GITEA_PLANE_GITHUB_SECRET_KEYS {
            assert!(std::env::var(key).is_err());
        }

        clear_all_env();
    }

    // ── Never logs a secret value ────────────────────────────────────────────

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    #[serial]
    async fn no_secret_value_ever_appears_in_log_output() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-4"); // pii-test-fixture
        const SECRET_MARKER: &str = "TOTALLY-SECRET-FIXTURE-VALUE-DO-NOT-LOG"; // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).json_body(json!({
                "secrets": [
                    { "secretKey": "PLANE_API_KEY", "secretValue": SECRET_MARKER }
                ]
            }));
        });
        configure_bootstrap_credential(&server.base_url());

        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let buf_for_writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || SharedBuf(buf_for_writer.clone()))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let guard = tracing::dispatcher::set_default(&dispatch);

        let outcome =
            bootstrap_gitea_plane_github_secrets(Some("proj1"), "prod", "/").await;
        log_secret_bootstrap_outcome(&outcome);

        drop(guard);

        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap_or_default();
        assert!(
            !captured.contains(SECRET_MARKER),
            "log output must never contain a fetched secret value, got: {captured}"
        );
        // Sanity: the fetch actually happened (otherwise this test would pass
        // trivially by never logging anything at all).
        assert!(matches!(outcome, SecretBootstrapOutcome::Fetched { count: 1, .. }));

        clear_all_env();
    }
}
