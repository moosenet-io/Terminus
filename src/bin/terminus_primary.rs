//! `terminus-primary`: the aggregated-core-registry gateway binary (TGW-01 —
//! Terminus Primary Gateway sprint, S108).
//!
//! Per the operator-authorized Gateway architecture and the S108 spec's
//! orchestrator-resolved design decisions:
//! - **(1) ALONGSIDE.** This binary runs beside `terminus_personal` (the
//!   existing personal-registry deployment, serving `register_personal`'s
//!   fleet-app tool subset) and beside Chord's own `:8099`-style proxy port
//!   — it does NOT narrow or replace either. Narrowing Chord's
//!   client-facing surface is an explicitly deferred, separately-approved
//!   follow-up (TGW-05), not part of this item.
//! - **(2) Core registry only, here.** This binary registers ONLY
//!   `registry::register_all` (the core tool set — git-public, plane,
//!   gitea, github, etc.) into its `ToolRegistry`. It deliberately does
//!   **NOT** call `registry::register_personal` locally: personal-registry
//!   tools (the `terminus_personal` subset) are reached via federation,
//!   built in TGW-02, not by aggregating both registration functions into
//!   one `ToolRegistry` here. This also sidesteps a REAL, pre-existing
//!   collision — `register_all` and `register_personal` both register the
//!   `plane`/`gitea`/`github`/`sundry` tool modules under the same names
//!   (see `crate::registry::core_personal_name_collisions` and its test),
//!   so a single combined registry would immediately drop entries via each
//!   module's own silent `tracing::warn!`-and-drop duplicate handling. Not
//!   building that combined registry in the first place is the correct fix
//!   for TGW-01's scope; TGW-02 handles personal-tool reachability without
//!   ever registering `register_personal` into this process's registry.
//! - **(3) Independent auto-generated CA.** No code branch is needed for
//!   this: `crate::pki::ca()`'s existing load-or-generate precedence reads
//!   `TERMINUS_CA_CERT`/`TERMINUS_CA_KEY` from THIS process's own
//!   environment (or its own local store file, `TERMINUS_CA_STORE_PATH`),
//!   so deploying `terminus-primary` with its own independently provisioned
//!   CA material (never `terminus_personal`'s own) already yields an
//!   independent CA purely from separate provisioning at deploy time
//!   (TGW-05) — see `crate::pki` module docs for the precedence.
//!
//! ## What this item does NOT add
//! Per the TGW-01 spec item's explicit scope boundary: no inference
//! proxying to Chord (TGW-03), no personal-tool federation (TGW-02), and no
//! per-user auth/audit/rate-limit pipeline (TGW-04). This binary, at the end
//! of THIS item, serves the core tool set over mTLS with `/enroll` wired —
//! nothing more. Reviewers should not expect TGW-02/03/04 behavior yet.
//!
//! ## Runtime configuration (env-sourced; NO literals)
//! - `TERMINUS_PRIMARY_PORT` — plain HTTP+JWT listener bind port. Defaults
//!   to `8310` — distinct from `terminus_personal`'s `TERMINUS_PERSONAL_PORT`
//!   default (`8300`) so both binaries can run side by side on the same
//!   host (design decision #1) with no collision.
//! - `TERMINUS_PRIMARY_BIND` — plain listener bind address. Defaults to
//!   `127.0.0.1`, same defense-in-depth posture as `terminus_personal`'s own
//!   default (`/mcp` is unauthenticated unless `TERMINUS_PRIMARY_TOKEN` is
//!   set, so this process binds loopback-only by default and relies on a
//!   reverse proxy / the mTLS listener for wider reachability).
//! - `TERMINUS_PRIMARY_TOKEN` — optional. If set, the plain `/mcp` listener
//!   requires `Authorization: Bearer <value>`.
//! - `TERMINUS_PRIMARY_MTLS_BIND` / `TERMINUS_PRIMARY_MTLS_PORT` /
//!   `TERMINUS_PRIMARY_MTLS_SERVER_IDENTITY` — the mTLS listener's own
//!   config (`crate::config::mtls_primary_bind_addr`/`mtls_primary_port`/
//!   `mtls_primary_server_identity`), a SEPARATE var family from
//!   `terminus_personal`'s `TERMINUS_MTLS_*`. See `crate::config`'s "TGW-01"
//!   section for the defaults and why they're distinct.
//! - CA/PKI material (`TERMINUS_CA_CERT`/`TERMINUS_CA_KEY`, or the local
//!   store at `TERMINUS_CA_STORE_PATH`) and the enrollment secrets
//!   (`TERMINUS_ENROLLMENT_SHARED_SECRET`, `TERMINUS_JWT_SIGNING_KEY`) are
//!   read the same way every other Terminus binary reads them — see
//!   `crate::pki` and `crate::pki::enroll` module docs. This binary does no
//!   startup secret-store-fetch bootstrap of its own (unlike // pii-test-fixture
//!   `terminus_personal`'s `fetch_downstream_secrets_from_infisical`) — // pii-test-fixture
//!   deployment (TGW-05) provisions its host environment directly; a
//!   startup secret-store fetch for this binary is out of this item's scope
//!   and can be added later without touching the shared `pki::server` setup
//!   this item builds.

use terminus_rs::pki::server::{build_gateway_router, spawn_mtls_listener, GatewayServerConfig};
use terminus_rs::registry::{register_all, ToolRegistry};

#[tokio::main]
async fn main() {
    terminus_rs::intake::init_tracing();

    let port: u16 = std::env::var("TERMINUS_PRIMARY_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8310);

    let bind_addr = std::env::var("TERMINUS_PRIMARY_BIND")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let auth_token = std::env::var("TERMINUS_PRIMARY_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());

    // TGW-01 design decision #2: core tools ONLY. Deliberately no
    // `register_personal` call here — see the module doc above.
    let mut registry = ToolRegistry::new();
    register_all(&mut registry);

    tracing::info!(
        "terminus_primary: {} tools registered, binding {bind_addr}:{port} (auth: {})",
        registry.len(),
        if auth_token.is_some() { "token" } else { "none" }
    );

    let gateway_config = GatewayServerConfig {
        server_name: "terminus-primary".to_string(),
        server_version: terminus_rs::VERSION.to_string(),
        auth_token,
        mtls_bind: terminus_rs::config::mtls_primary_bind_addr(),
        mtls_port: terminus_rs::config::mtls_primary_port(),
        mtls_server_identity: terminus_rs::config::mtls_primary_server_identity(),
    };

    // Same shared setup `terminus_personal` uses (TGW-01 extraction, see
    // `terminus_rs::pki::server` module docs): the `/mcp`+`/enroll` router,
    // then the background mTLS listener on this binary's own
    // `TERMINUS_PRIMARY_MTLS_*`-derived config.
    let router = build_gateway_router(registry, &gateway_config);
    spawn_mtls_listener(router.clone(), &gateway_config);

    let listener = tokio::net::TcpListener::bind(format!("{bind_addr}:{port}"))
        .await
        .unwrap_or_else(|e| panic!("terminus_primary: failed to bind {bind_addr}:{port}: {e}"));

    axum::serve(listener, router)
        .await
        .expect("terminus_primary: server error");
}

#[cfg(test)]
mod tests {
    use terminus_rs::registry::{register_all, ToolRegistry};

    /// `terminus_primary`'s registry-building step, exercised directly
    /// (mirrors the exact call `main()` makes) -- confirms core tools land
    /// and, per design decision #2, that this binary's registry is built
    /// from `register_all` alone (no `register_personal` mixed in, so no
    /// plane/gitea/github/sundry collision -- see
    /// `terminus_rs::registry::core_personal_name_collisions`).
    #[test]
    fn primary_registry_build_matches_main_and_has_core_tools() {
        let mut registry = ToolRegistry::new();
        register_all(&mut registry);

        assert!(registry.len() > 0, "register_all should populate the registry");
        // Spot-check a few representative core tools from different
        // modules, proving this is genuinely the core/`register_all` set.
        assert!(registry.contains("plane_list_projects"));
        assert!(registry.contains("gitea_list_identities"));
        assert!(registry.contains("github_list_repos"));
    }
}
