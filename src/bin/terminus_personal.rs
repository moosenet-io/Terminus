//! `terminus_personal`: the second Rust Terminus deployment — the
//! "private/personal" instance, run on a separate fleet host away from the
//! primary build-pipeline binaries, exposing the operator's own
//! network/admin tools.
//!
//! This is the second of the two intended Rust Terminus deployments. It
//! serves ONLY the personal-utility/admin tool subset
//! (`registry::register_personal`) over the same streamable-HTTP MCP wire
//! protocol the legacy Python fleet host already speaks — see `mcp_server`
//! module docs for the protocol shape and the confirmed-live `initialize`
//! probe this was matched against.
//!
//! ## Runtime configuration (env-sourced; NO literals)
//! - `TERMINUS_PERSONAL_PORT` — bind port. Defaults to `8300` (does not
//!   collide with the legacy Python host's own port, or the reverse-proxy
//!   port in front of it — this binary gets deployed behind its own,
//!   separate reverse-proxy location/port during the deploy phase, run
//!   side-by-side, never replacing the Python service).
//! - `TERMINUS_PERSONAL_BIND` — bind address. Defaults to `127.0.0.1`: since
//!   `/mcp` is unauthenticated by default (see below) and exposes admin-grade
//!   tools (ansible, dev, gitea/github/plane writes, ...), the process binds
//!   loopback-only by default and relies on a reverse proxy for LAN
//!   reachability — the same defense-in-depth shape the legacy Python host
//!   gets from its own reverse-proxy front door. Set to `0.0.0.0` explicitly
//!   if a deployment genuinely wants the raw port LAN-reachable.
//! - `TERMINUS_PERSONAL_TOKEN` — optional. If set, `/mcp` requires
//!   `Authorization: Bearer <value>`. If unset, `/mcp` is unauthenticated,
//!   matching the confirmed posture of the existing legacy Python host.
//! - Individual tool modules (plane, gitea, github, ansible, network, ...)
//!   each read their own env vars directly (e.g. `PLANE_API_URL`,
//!   `GITEA_PAT_<NAME>`) exactly as they already do for every other Terminus bin —
//!   this binary does no special config wiring beyond the two vars above,
//!   PLUS the startup-time <secret-manager> fetch described below. // pii-test-fixture
//!
//! ## Downstream secrets: fetched fresh from <secret-manager> at every startup (PSEC-02) // pii-test-fixture
//! Before building the tool registry, `main()` calls
//! `fetch_downstream_secrets_from_infisical()`, which — when
//! `INFISICAL_URL`/`INFISICAL_CLIENT_ID`/`INFISICAL_CLIENT_SECRET` (the one
//! bootstrap credential) are configured — fetches this process's downstream
//! secrets (`PLANE_API_URL`, `PLANE_API_KEY`, `PLANE_WORKSPACE`, `GITEA_URL`,
//! `GITHUB_TOKEN`) fresh from <secret-manager> and sets them into the // pii-test-fixture
//! process environment via `std::env::set_var`, so every `X::from_env()`-style
//! tool client constructed afterward transparently picks up the CURRENT
//! value — never a stale one left behind in a static `.env` after a rotation.
//! The named-identity PATs — both Plane (`PLANE_PAT_*` — CLAUDE/HARMONY/MOOSE/
//! GEMINI/CODEX/LUMINA) and Gitea (`GITEA_PAT_*` — MOOSE/HARMONY/LUMINA), plus // pii-test-fixture
//! any provisioned later — are materialized the same way, but via a dynamic
//! `*_PAT_` prefix match rather than a fixed list, so a newly-added identity
//! becomes usable on the next restart with no code change; this is what lets
//! `plane_list_identities` / `gitea_list_identities` actually report the
//! operator's configured identities instead of an empty set. The unsuffixed
//! `GITEA_TOKEN` is intentionally GONE (S105/GPAT) — Gitea auth is now solely
//! per-identity `GITEA_PAT_<NAME>`. See `PAT_KEY_PREFIXES`.
//! This closes a real recurring operational problem: a rotated Plane
//! credential previously required someone to notice, manually re-run a
//! fetch-and-splice script, and restart the service before writes worked
//! again; now the next restart alone picks up the rotation.
//!
//! If <secret-manager> isn't configured, or the fetch fails for any reason (auth // pii-test-fixture
//! failure, network error, unreachable host), this falls back cleanly to
//! whatever is already in the process environment (e.g. a static `.env`
//! loaded by the systemd unit's `EnvironmentFile=`) — it is NEVER a hard
//! startup failure. No secret value is ever logged; only counts and (for
//! missing keys) key names are logged.
//!
//! Reuses the <secret-manager> Universal Auth client already shared with the guarded // pii-test-fixture
//! `infisical_get_secrets_batch` MCP tool (`src/<secret-manager>/mod.rs`, // pii-test-fixture
//! `fetch_secrets_batch()`, extracted in PSEC-01) — this startup call has no
//! approval gate of its own, since it's a process-internal bootstrap action,
//! not an operator-invoked one; the gate stays exactly where it was, on the
//! MCP tool surface only.
//!
//! Additional env vars for this fetch (all optional; the fetch is skipped —
//! falling back to the static environment — unless the bootstrap credential
//! AND the project id are both present):
//! - `TERMINUS_PERSONAL_INFISICAL_PROJECT_ID` — the <secret-manager> workspace/project // pii-test-fixture
//!   ID to fetch from. No default (deliberately not hardcoded — see S1).
//! - `TERMINUS_PERSONAL_INFISICAL_ENVIRONMENT` — <secret-manager> environment slug. // pii-test-fixture
//!   Defaults to `prod`.
//! - `TERMINUS_PERSONAL_INFISICAL_SECRET_PATH` — folder path within the
//!   environment. Defaults to `/`.

use std::sync::Arc;

use terminus_rs::<secret-manager>::{fetch_secrets_batch, InfisicalConfig}; // pii-test-fixture
use terminus_rs::mcp_server::{build_router, McpServerState};
use terminus_rs::pki::enroll::build_enroll_router;
use terminus_rs::registry::{register_personal, ToolRegistry};

/// The downstream secret keys this process needs, fetched from <secret-manager> at // pii-test-fixture
/// startup rather than relying on a static `.env`. Deliberately a fixed,
/// named allowlist (not "set every key found at this path") so a shared
/// <secret-manager> path containing secrets for other services never leaks into // pii-test-fixture
/// this process's environment.
///
/// **BREAKING (S105/GPAT):** the unsuffixed `GITEA_TOKEN` was removed here — the
/// Gitea tool now authenticates solely via per-identity `GITEA_PAT_<NAME>`
/// tokens (picked up dynamically via `PAT_KEY_PREFIXES` below), so there is no
/// single unsuffixed Gitea token to materialize any more.
const DOWNSTREAM_SECRET_KEYS: &[&str] = &[
    "PLANE_API_URL",
    "PLANE_API_KEY",
    "PLANE_WORKSPACE",
    "GITEA_URL",
    "GITHUB_TOKEN",
];

/// Prefixes for named-identity Personal Access Tokens. In addition to the fixed
/// `DOWNSTREAM_SECRET_KEYS` above, any secret key at the fetch path that begins
/// with one of these prefixes is materialized into the process environment too,
/// so the per-identity resolution in each tool can see it:
///
/// - `PLANE_PAT_*` — Plane identities (`PLANE_PAT_CLAUDE`, `PLANE_PAT_HARMONY`,
///   `PLANE_PAT_MOOSE`, `PLANE_PAT_GEMINI`, `PLANE_PAT_CODEX`, `PLANE_PAT_LUMINA`,
///   …) for `plane_list_identities` / `PlaneClient::for_identity`.
/// - `GITEA_PAT_*` — Gitea identities (`GITEA_PAT_MOOSE`, `GITEA_PAT_HARMONY`,
///   `GITEA_PAT_LUMINA`, …) for `gitea_list_identities` / `GiteaClient::for_identity`
///   (S105/GPAT — replaces the retired unsuffixed `GITEA_TOKEN`).
///
/// This is deliberately a *dynamic prefix match*, not another fixed list: a
/// newly-provisioned identity becomes usable on the next restart with no code
/// change. Matching is scoped to exactly these prefixes (never "set every key
/// found at the path"), preserving the same anti-leak property as the fixed
/// allowlist above — an unrelated secret for another service sharing the path is
/// never imported.
const PAT_KEY_PREFIXES: &[&str] = &["PLANE_PAT_", "GITEA_PAT_"];

/// Outcome of the startup <secret-manager> fetch attempt, for the caller (`main()`) // pii-test-fixture
/// to log and for tests to assert on directly rather than scraping log text.
#[derive(Debug, PartialEq, Eq)]
enum SecretFetchOutcome {
    /// `INFISICAL_URL`/`INFISICAL_CLIENT_ID`/`INFISICAL_CLIENT_SECRET` or the
    /// project-id env var aren't configured — nothing was attempted.
    NotConfigured,
    /// The fetch succeeded; `count` non-blank keys were found and set into
    /// the process environment. `missing` names (never values) any of
    /// `DOWNSTREAM_SECRET_KEYS` that <secret-manager> didn't have at this path. // pii-test-fixture
    /// `identities` is how many of `count` are named-identity PATs
    /// (`PLANE_PAT_*` / `GITEA_PAT_*`, picked up dynamically) — a set-but-blank
    /// value is treated as missing for both, never set.
    Fetched {
        count: usize,
        missing: Vec<String>,
        identities: usize,
    },
    /// The fetch was attempted but failed (auth failure, network error,
    /// malformed response, ...) — callers fall back to whatever is already
    /// in the process environment. `reason` is a display-formatted
    /// `ToolError` — never a secret value.
    Failed { reason: String },
}

/// Attempt to fetch this process's downstream secrets (Plane/Gitea/GitHub
/// credentials) fresh from <secret-manager> and set them into the process // pii-test-fixture
/// environment, so every `X::from_env()`-style client constructed after this
/// point sees the current value. Falls back cleanly (never panics, never
/// hangs, never hard-fails startup) when <secret-manager> isn't configured or the // pii-test-fixture
/// fetch fails — see the module doc comment above for the full rationale.
///
/// Never logs or echoes any fetched secret value — only counts and, for
/// missing keys, key NAMES (never values).
async fn fetch_downstream_secrets_from_infisical() -> SecretFetchOutcome {
    let config = InfisicalConfig::from_env();
    if !config.is_configured() {
        return SecretFetchOutcome::NotConfigured;
    }

    let project_id = match std::env::var("TERMINUS_PERSONAL_INFISICAL_PROJECT_ID")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(p) => p,
        None => return SecretFetchOutcome::NotConfigured,
    };
    let environment = std::env::var("TERMINUS_PERSONAL_INFISICAL_ENVIRONMENT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "prod".to_string());
    let secret_path = std::env::var("TERMINUS_PERSONAL_INFISICAL_SECRET_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/".to_string());

    match fetch_secrets_batch(&config, &project_id, &environment, &secret_path).await {
        Ok(fetched) => {
            let mut count = 0usize;
            let mut missing = Vec::new();
            // Fixed downstream allowlist: the six base connection secrets. A
            // present-but-blank value is treated as MISSING (never set into the
            // environment), so a blank provider value can never silently clobber
            // a valid static-env fallback with an empty string (the CSEC-02
            // lesson learned in the sibling Chord client).
            for key in DOWNSTREAM_SECRET_KEYS {
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
            // blank-as-missing rule as above. Not tracked in `missing`:
            // identities are provisioned ad hoc, so there is no fixed expected
            // set for one to be "missing" from.
            let mut identities = 0usize;
            for (key, value) in &fetched {
                let is_pat = PAT_KEY_PREFIXES.iter().any(|p| key.starts_with(p));
                if is_pat && !value.is_empty() {
                    std::env::set_var(key, value);
                    count += 1;
                    identities += 1;
                }
            }
            SecretFetchOutcome::Fetched {
                count,
                missing,
                identities,
            }
        }
        Err(e) => SecretFetchOutcome::Failed {
            reason: e.to_string(),
        },
    }
}

/// Log the outcome of the startup <secret-manager> fetch. Split out from // pii-test-fixture
/// `fetch_downstream_secrets_from_infisical()` so tests can assert on the
/// returned enum directly without needing to capture tracing output.
fn log_secret_fetch_outcome(outcome: &SecretFetchOutcome) {
    match outcome {
        SecretFetchOutcome::NotConfigured => {
            tracing::info!(
                "terminus_personal: <secret-manager> not configured (INFISICAL_URL/INFISICAL_CLIENT_ID/INFISICAL_CLIENT_SECRET/TERMINUS_PERSONAL_INFISICAL_PROJECT_ID unset), using static environment" // pii-test-fixture
            );
        }
        SecretFetchOutcome::Fetched {
            count,
            missing,
            identities,
        } => {
            tracing::info!("terminus_personal: fetched {count} secrets ({identities} named PAT identities) from <secret-manager>"); // pii-test-fixture
            if !missing.is_empty() {
                tracing::warn!(
                    "terminus_personal: <secret-manager> fetch did not include: {} (using static environment for these, if present)", // pii-test-fixture
                    missing.join(", ")
                );
            }
        }
        SecretFetchOutcome::Failed { reason } => {
            tracing::warn!(
                "terminus_personal: <secret-manager> fetch failed ({reason}), falling back to static environment" // pii-test-fixture
            );
        }
    }
}

#[tokio::main]
async fn main() {
    terminus_rs::intake::init_tracing();

    let secret_outcome = fetch_downstream_secrets_from_infisical().await;
    log_secret_fetch_outcome(&secret_outcome);

    let port: u16 = std::env::var("TERMINUS_PERSONAL_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8300);

    let bind_addr = std::env::var("TERMINUS_PERSONAL_BIND")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let auth_token = std::env::var("TERMINUS_PERSONAL_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());

    let mut registry = ToolRegistry::new();
    register_personal(&mut registry);

    tracing::info!(
        "terminus_personal: {} tools registered, binding {bind_addr}:{port} (auth: {})",
        registry.len(),
        if auth_token.is_some() { "token" } else { "none" }
    );

    let state = Arc::new(McpServerState {
        registry,
        server_name: "terminus-personal".to_string(),
        server_version: terminus_rs::VERSION.to_string(),
        auth_token,
    });

    // TCLI-02: the enrollment endpoint is a fully separate, additive router
    // (its own request/response shape + auth model — see
    // `terminus_rs::pki::enroll` module docs) merged alongside the existing
    // `/mcp`/`/healthz` router. `build_router`/`McpServerState` above are
    // untouched by this merge -- existing clients see no behavior change.
    let router = build_router(state).merge(build_enroll_router());

    // TCLI-03: the mTLS listener is a SECOND, additive listener on a
    // separate port (`crate::config::mtls_port`, default 8301 — never
    // `TERMINUS_PERSONAL_PORT`'s 8300) serving the SAME `router` built
    // above. It is spawned as its own background task; the plain
    // HTTP+JWT listener below (`axum::serve(listener, router)`) is
    // completely unchanged by its presence — this task failing to start
    // (e.g. CA/server-cert bootstrap failure) is logged as an error and
    // does not prevent the existing plain listener from serving normally.
    {
        let mtls_router = router.clone();
        tokio::spawn(async move {
            let ca = match terminus_rs::pki::ca() {
                Ok(ca) => ca,
                Err(e) => {
                    tracing::error!(
                        "terminus_personal: mTLS listener disabled -- CA bootstrap failed: {e}"
                    );
                    return;
                }
            };
            let server_identity = terminus_rs::config::mtls_server_identity();
            let (server_cert_pem, server_key_pem) =
                match terminus_rs::pki::mtls::issue_server_cert(ca, &server_identity) {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::error!(
                            "terminus_personal: mTLS listener disabled -- server cert issuance failed: {e}"
                        );
                        return;
                    }
                };
            let tls_config = match terminus_rs::pki::mtls::build_server_config(
                ca.cert_pem(),
                &server_cert_pem,
                &server_key_pem,
            ) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::error!(
                        "terminus_personal: mTLS listener disabled -- TLS config build failed: {e}"
                    );
                    return;
                }
            };

            let mtls_bind = terminus_rs::config::mtls_bind_addr();
            let mtls_port = terminus_rs::config::mtls_port();
            tracing::info!(
                "terminus_personal: starting mTLS listener on {mtls_bind}:{mtls_port} (identity={server_identity})"
            );
            if let Err(e) =
                terminus_rs::pki::mtls::run_listener(&mtls_bind, mtls_port, tls_config, mtls_router)
                    .await
            {
                tracing::error!("terminus_personal: mTLS listener stopped: {e}");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(format!("{bind_addr}:{port}"))
        .await
        .unwrap_or_else(|e| panic!("terminus_personal: failed to bind {bind_addr}:{port}: {e}"));

    axum::serve(listener, router)
        .await
        .expect("terminus_personal: server error");
}

// ── Tests (PSEC-02): startup-time <secret-manager> secret fetch ─────────────────────── // pii-test-fixture
//
// All env-var mutation is process-global, so every test clears the full set
// of relevant keys before AND after itself and runs #[serial] (matching the
// convention already used by src/<secret-manager>/mod.rs's own tests). // pii-test-fixture

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
        "TERMINUS_PERSONAL_INFISICAL_PROJECT_ID",
        "TERMINUS_PERSONAL_INFISICAL_ENVIRONMENT",
        "TERMINUS_PERSONAL_INFISICAL_SECRET_PATH",
        "PLANE_API_URL",
        "PLANE_API_KEY",
        "PLANE_WORKSPACE",
        "GITEA_URL",
        "GITEA_TOKEN",
        "GITHUB_TOKEN",
        "PLANE_PAT_CLAUDE",
        "PLANE_PAT_HARMONY",
        "PLANE_PAT_FUTURE",
        "PLANE_PAT_BLANK",
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

    fn configure_bootstrap(base_url: &str) {
        std::env::set_var("INFISICAL_URL", base_url);
        std::env::set_var("INFISICAL_CLIENT_ID", "cid"); // pii-test-fixture
        std::env::set_var("INFISICAL_CLIENT_SECRET", "csecret"); // pii-test-fixture
        std::env::set_var("TERMINUS_PERSONAL_INFISICAL_PROJECT_ID", "proj1");
    }

    fn mock_login(server: &MockServer, token: &str) {
        server.mock(|when, then| {
            when.method(POST).path("/api/v1/auth/universal-auth/login");
            then.status(200).json_body(json!({ "accessToken": token }));
        });
    }

    // ── NotConfigured: proceeds cleanly, no crash, no hang, no env mutation ──────

    #[tokio::test]
    #[serial]
    async fn not_configured_falls_back_without_crash_or_env_mutation() {
        clear_all_env();

        let outcome = fetch_downstream_secrets_from_infisical().await;
        assert_eq!(outcome, SecretFetchOutcome::NotConfigured);
        for key in DOWNSTREAM_SECRET_KEYS {
            assert!(std::env::var(key).is_err(), "{key} should not have been set");
        }

        clear_all_env();
    }

    #[tokio::test]
    #[serial]
    async fn bootstrap_configured_but_project_id_missing_is_not_configured() {
        clear_all_env();
        // Deliberately never dialed: TERMINUS_PERSONAL_INFISICAL_PROJECT_ID is
        // left unset, so fetch_downstream_secrets_from_infisical() must return
        // before attempting any network call.
        std::env::set_var("INFISICAL_URL", "http://127.0.0.1:1");
        std::env::set_var("INFISICAL_CLIENT_ID", "cid"); // pii-test-fixture
        std::env::set_var("INFISICAL_CLIENT_SECRET", "csecret"); // pii-test-fixture

        let outcome = fetch_downstream_secrets_from_infisical().await;
        assert_eq!(outcome, SecretFetchOutcome::NotConfigured);

        clear_all_env();
    }

    // ── Fetched: values actually set into the process environment ───────────────

    #[tokio::test]
    #[serial]
    async fn fetched_secrets_are_set_into_process_environment() {
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
        configure_bootstrap(&server.base_url());

        let outcome = fetch_downstream_secrets_from_infisical().await;
        match outcome {
            SecretFetchOutcome::Fetched {
                count,
                missing,
                identities,
            } => {
                assert_eq!(count, 2);
                assert_eq!(missing.len(), DOWNSTREAM_SECRET_KEYS.len() - 2);
                assert_eq!(identities, 0);
            }
            other => panic!("expected Fetched, got {other:?}"),
        }
        assert_eq!(
            std::env::var("PLANE_API_KEY").unwrap(),
            "fixture-plane-key"
        );
        assert_eq!(
            std::env::var("GITHUB_TOKEN").unwrap(),
            "fixture-github-token"
        );

        clear_all_env();
    }

    // ── Named-identity PATs: dynamic PLANE_PAT_* pickup, blank-as-missing,
    //    and no leakage of unrelated keys sharing the path ───────────────────────

    #[tokio::test]
    #[serial]
    async fn plane_pat_named_identities_are_materialized_dynamically() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-pat"); // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).json_body(json!({
                "secrets": [
                    // A base allowlist key (not a PAT) to prove the two paths coexist.
                    { "secretKey": "PLANE_API_KEY", "secretValue": "fixture-plane-key" },
                    // Known named identities.
                    { "secretKey": "PLANE_PAT_CLAUDE", "secretValue": "fixture-pat-claude" },
                    { "secretKey": "PLANE_PAT_HARMONY", "secretValue": "fixture-pat-harmony" },
                    // An identity the code has never heard of — must still be picked
                    // up purely by the PLANE_PAT_ prefix (future-proofing).
                    { "secretKey": "PLANE_PAT_FUTURE", "secretValue": "fixture-pat-future" },
                    // A set-but-blank PAT must be treated as missing (not set).
                    { "secretKey": "PLANE_PAT_BLANK", "secretValue": "" },
                    // An unrelated secret sharing the path must NEVER be imported.
                    { "secretKey": "SOME_OTHER_SERVICE_SECRET", "secretValue": "should-not-leak" }
                ]
            }));
        });
        configure_bootstrap(&server.base_url());

        let outcome = fetch_downstream_secrets_from_infisical().await;
        match outcome {
            SecretFetchOutcome::Fetched {
                count,
                identities,
                ..
            } => {
                // 1 base key (PLANE_API_KEY) + 3 non-blank PATs = 4 set.
                assert_eq!(count, 4, "expected 1 base + 3 non-blank PATs");
                assert_eq!(identities, 3, "blank PAT must not count as an identity");
            }
            other => panic!("expected Fetched, got {other:?}"),
        }

        // Known + future identities are materialized into the environment...
        assert_eq!(std::env::var("PLANE_PAT_CLAUDE").unwrap(), "fixture-pat-claude");
        assert_eq!(std::env::var("PLANE_PAT_HARMONY").unwrap(), "fixture-pat-harmony");
        assert_eq!(std::env::var("PLANE_PAT_FUTURE").unwrap(), "fixture-pat-future");
        // ...a blank PAT value is treated as missing, never set...
        assert!(std::env::var("PLANE_PAT_BLANK").is_err());
        // ...and a non-PLANE_PAT_ key for another service never leaks in.
        assert!(std::env::var("SOME_OTHER_SERVICE_SECRET").is_err());

        clear_all_env();
    }

    // ── GITEA_PAT_* (S105/GPAT): the dynamic PAT pickup covers Gitea identities
    //    too, alongside Plane's, with the same blank-as-missing + anti-leak rules.
    #[tokio::test]
    #[serial]
    async fn gitea_pat_named_identities_are_materialized_dynamically() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-gitea-pat"); // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).json_body(json!({
                "secrets": [
                    // A base allowlist key (not a PAT) to prove coexistence.
                    { "secretKey": "GITEA_URL", "secretValue": "http://gitea.example.com" },
                    // A Plane PAT and Gitea PATs must BOTH be picked up dynamically.
                    { "secretKey": "PLANE_PAT_HARMONY", "secretValue": "fixture-plane-harmony" },
                    { "secretKey": "GITEA_PAT_MOOSE", "secretValue": "fixture-gitea-moose" },
                    { "secretKey": "GITEA_PAT_HARMONY", "secretValue": "fixture-gitea-harmony" },
                    // An identity the code has never heard of — picked up purely by prefix.
                    { "secretKey": "GITEA_PAT_FUTURE", "secretValue": "fixture-gitea-future" },
                    // A set-but-blank Gitea PAT must be treated as missing (not set).
                    { "secretKey": "GITEA_PAT_BLANK", "secretValue": "" },
                    // Unrelated secret sharing the path must NEVER be imported.
                    { "secretKey": "SOME_OTHER_SERVICE_SECRET", "secretValue": "should-not-leak" }
                ]
            }));
        });
        configure_bootstrap(&server.base_url());

        let outcome = fetch_downstream_secrets_from_infisical().await;
        match outcome {
            SecretFetchOutcome::Fetched {
                count,
                identities,
                ..
            } => {
                // 1 base key (GITEA_URL) + 4 non-blank PATs (1 plane + 3 gitea) = 5.
                assert_eq!(count, 5, "expected 1 base + 4 non-blank PATs");
                assert_eq!(identities, 4, "blank PAT must not count; plane+gitea both count");
            }
            other => panic!("expected Fetched, got {other:?}"),
        }

        assert_eq!(std::env::var("GITEA_PAT_MOOSE").unwrap(), "fixture-gitea-moose");
        assert_eq!(std::env::var("GITEA_PAT_HARMONY").unwrap(), "fixture-gitea-harmony");
        assert_eq!(std::env::var("GITEA_PAT_FUTURE").unwrap(), "fixture-gitea-future");
        assert_eq!(std::env::var("PLANE_PAT_HARMONY").unwrap(), "fixture-plane-harmony");
        // Blank Gitea PAT is treated as missing, never set...
        assert!(std::env::var("GITEA_PAT_BLANK").is_err());
        // ...and an unrelated key never leaks in.
        assert!(std::env::var("SOME_OTHER_SERVICE_SECRET").is_err());

        clear_all_env();
    }

    #[tokio::test]
    #[serial]
    async fn empty_infisical_response_is_fetched_zero_not_an_error() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-2"); // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).json_body(json!({ "secrets": [] }));
        });
        configure_bootstrap(&server.base_url());

        let outcome = fetch_downstream_secrets_from_infisical().await;
        match outcome {
            SecretFetchOutcome::Fetched {
                count,
                missing,
                identities,
            } => {
                assert_eq!(count, 0);
                assert_eq!(missing.len(), DOWNSTREAM_SECRET_KEYS.len());
                assert_eq!(identities, 0);
            }
            other => panic!("expected Fetched{{count:0,..}}, got {other:?}"),
        }

        clear_all_env();
    }

    // ── Failed: falls back cleanly, never panics, never touches existing env ────

    #[tokio::test]
    #[serial]
    async fn fetch_failure_falls_back_cleanly_without_panic() {
        clear_all_env();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/v1/auth/universal-auth/login");
            then.status(401).json_body(json!({ "message": "invalid credentials" }));
        });
        configure_bootstrap(&server.base_url());
        // Pre-seed a static fallback value to prove a failed fetch leaves it
        // untouched (this is what a static `.env`-sourced value would look
        // like in production).
        std::env::set_var("GITHUB_TOKEN", "static-fallback-token"); // pii-test-fixture

        let outcome = fetch_downstream_secrets_from_infisical().await;
        assert!(matches!(outcome, SecretFetchOutcome::Failed { .. }));
        assert_eq!(
            std::env::var("GITHUB_TOKEN").unwrap(),
            "static-fallback-token"
        );

        clear_all_env();
    }

    #[tokio::test]
    #[serial]
    async fn malformed_infisical_response_falls_back_cleanly() {
        clear_all_env();
        let server = MockServer::start();
        mock_login(&server, "tok-3"); // pii-test-fixture
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/secrets/raw");
            then.status(200).body("not valid json {{{");
        });
        configure_bootstrap(&server.base_url());

        let outcome = fetch_downstream_secrets_from_infisical().await;
        assert!(matches!(outcome, SecretFetchOutcome::Failed { .. }));
        for key in DOWNSTREAM_SECRET_KEYS {
            assert!(std::env::var(key).is_err());
        }

        clear_all_env();
    }

    // ── Never logs a secret value ────────────────────────────────────────────────

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
        configure_bootstrap(&server.base_url());

        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let buf_for_writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || SharedBuf(buf_for_writer.clone()))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let guard = tracing::dispatcher::set_default(&dispatch);

        let outcome = fetch_downstream_secrets_from_infisical().await;
        log_secret_fetch_outcome(&outcome);

        drop(guard);

        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap_or_default();
        assert!(
            !captured.contains(SECRET_MARKER),
            "log output must never contain a fetched secret value, got: {captured}"
        );
        // Sanity: the fetch actually happened (otherwise this test would pass
        // trivially by never logging anything at all).
        assert!(matches!(outcome, SecretFetchOutcome::Fetched { count: 1, .. }));

        clear_all_env();
    }
}
