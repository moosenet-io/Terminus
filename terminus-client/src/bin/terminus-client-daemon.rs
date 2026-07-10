//! `terminus-client-daemon` (TCLI-05) -- presents a plain MCP endpoint on
//! **loopback only** to a local MCP client (Claude Code, initially, per
//! TCLI-06), and forwards every request it receives to a terminus primary
//! over the TCLI-04 mTLS transport.
//!
//! ## Why loopback-only is safe here
//! The local endpoint speaks plain (no-mTLS) MCP -- that's only an
//! acceptable posture because it never leaves the loopback interface: the
//! OUTBOUND hop to the primary (everything this process actually forwards
//! to) is mTLS the whole way. This binary always binds `127.0.0.1`
//! (hardcoded, not sourced from an env var) precisely so a config typo can
//! never widen that to a LAN- or internet-reachable bind -- see
//! [`LOCAL_BIND_ADDR`].
//!
//! ## Runtime configuration (env-sourced; NO literals -- S1/S6)
//! - `TERMINUS_CLIENT_IDENTITY` -- required. This daemon's enrollment
//!   identity (embedded in its issued cert's CN/SAN and JWT `sub` claim by
//!   the primary). No default -- an identity name is inherently
//!   deployment-specific, not something to guess a literal for.
//! - `TERMINUS_ENROLLMENT_SHARED_SECRET` -- required. The bootstrap
//!   credential for `TERMINUS_CLIENT_IDENTITY`'s enrollment, sourced from
//!   this process's own runtime-materialized environment (the same
//!   env-materialized-secret convention `terminus_rs::pki::enroll` already
//!   uses server-side for this exact variable name -- see that crate's
//!   `config.rs` module doc). Never hardcoded, never read from anywhere but
//!   the process environment.
//! - `TERMINUS_PRIMARY_URL` -- the primary's plain HTTP+JWT base URL, used
//!   ONLY for the one-shot `/enroll` call (TCLI-02). Defaults to
//!   `http://127.0.0.1:8300` (loopback default -- an operator pointing this
//!   daemon at a remote primary must set this explicitly, never assume a
//!   LAN literal).
//! - `TERMINUS_MTLS_HOST` -- host of the primary's mTLS listener (TCLI-03).
//!   Defaults to `127.0.0.1`.
//! - `TERMINUS_MTLS_PORT` -- port of the primary's mTLS listener. Defaults
//!   to `8301`, matching `terminus_rs::config::mtls_port`'s own default.
//! - `TERMINUS_MTLS_SERVER_IDENTITY` -- the primary's mTLS server-cert
//!   identity (CN/SAN), used as the TLS `ServerName` for hostname
//!   verification. Defaults to `terminus-primary`, matching
//!   `terminus_rs::config::mtls_server_identity`'s own default.
//! - `TERMINUS_CLIENT_LOCAL_PORT` -- loopback port this daemon serves its
//!   own local MCP endpoint on. Defaults to `8310`.
//! - `TERMINUS_CLIENT_FORWARD_TIMEOUT_SECS` -- per-forwarded-request
//!   timeout. Defaults to 15s (see
//!   `terminus_client::forward::DEFAULT_FORWARD_TIMEOUT`).
//! - `TERMINUS_CLIENT_CATALOG_TTL_SECS` -- tool-catalog cache TTL. Defaults
//!   to 60s (see `terminus_client::mcp_server::DEFAULT_CATALOG_TTL`).
//!
//! ## Startup behavior (TCLI-05 APPROACH step 2 / EDGE CASE: fail fast)
//! Before accepting any local MCP connection, `main()` enrolls (or reuses a
//! valid cached credential) and completes one mTLS handshake against the
//! primary. If EITHER step fails, this prints a clear, sanitized error to
//! stderr and exits with a non-zero status immediately -- it never starts
//! the local listener in a partially-working state, and it never hangs or
//! retry-loops indefinitely on its own (matching TCLI-04's "the calling
//! program decides fallback behavior" contract -- for this program, the
//! decision is "fail fast, let systemd/the operator restart it").

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use terminus_client::enroll::EnrollConfig;
use terminus_client::forward::{establish_initial_connection, PrimaryConfig, DEFAULT_FORWARD_TIMEOUT};
use terminus_client::mcp_server::{build_router, DaemonState, DEFAULT_CATALOG_TTL};
use terminus_client::transport::ConnectConfig;

/// Always loopback -- see the module doc's "why loopback-only is safe here"
/// section. Deliberately NOT sourced from an environment variable.
const LOCAL_BIND_ADDR: &str = "127.0.0.1";

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn build_primary_config() -> Result<PrimaryConfig, String> {
    let identity = env_nonempty("TERMINUS_CLIENT_IDENTITY")
        .ok_or_else(|| "TERMINUS_CLIENT_IDENTITY is required (this daemon's enrollment identity)".to_string())?;

    // Per the TCLI-04 crate's documented contract: the bootstrap secret is
    // supplied by the CALLING program from its own secret store. This
    // binary's "own secret store" is the runtime-materialized process
    // environment (never a literal) -- the exact convention
    // `terminus_rs::pki::enroll` already uses server-side for this same
    // variable name.
    let shared_secret = env_nonempty("TERMINUS_ENROLLMENT_SHARED_SECRET").ok_or_else(|| {
        "TERMINUS_ENROLLMENT_SHARED_SECRET is required (materialized into the process environment at deploy time, never hardcoded)".to_string()
    })?;

    let primary_url = env_nonempty("TERMINUS_PRIMARY_URL").unwrap_or_else(|| "http://127.0.0.1:8300".to_string());
    let mtls_host = env_nonempty("TERMINUS_MTLS_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
    let mtls_port: u16 = env_nonempty("TERMINUS_MTLS_PORT").and_then(|v| v.parse().ok()).unwrap_or(8301);
    let mtls_server_identity =
        env_nonempty("TERMINUS_MTLS_SERVER_IDENTITY").unwrap_or_else(|| "terminus-primary".to_string());
    let forward_timeout_secs: u64 = env_nonempty("TERMINUS_CLIENT_FORWARD_TIMEOUT_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FORWARD_TIMEOUT.as_secs());

    let enroll_cfg = EnrollConfig::new(primary_url, identity, shared_secret);
    let connect_cfg = ConnectConfig { host: mtls_host, port: mtls_port, server_name: mtls_server_identity };

    let mut cfg = PrimaryConfig::new(enroll_cfg, connect_cfg);
    cfg.timeout = Duration::from_secs(forward_timeout_secs);
    Ok(cfg)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let primary = match build_primary_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("terminus-client-daemon: configuration error: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        "terminus-client-daemon: connecting to primary mTLS listener at {}:{}",
        primary.connect.host,
        primary.connect.port
    );

    // TCLI-05 APPROACH step 2 / EDGE CASE: fail fast, before accepting any
    // local connection, if the initial mTLS session can't be established.
    if let Err(e) = establish_initial_connection(&primary).await {
        eprintln!("terminus-client-daemon: failed to establish initial mTLS connection to the primary: {e}");
        std::process::exit(1);
    }
    tracing::info!("terminus-client-daemon: initial mTLS connection to the primary established");

    let catalog_ttl_secs: u64 = env_nonempty("TERMINUS_CLIENT_CATALOG_TTL_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_CATALOG_TTL.as_secs());

    let state = Arc::new(DaemonState::with_catalog_ttl(
        primary,
        "terminus-client-daemon",
        env!("CARGO_PKG_VERSION"),
        Duration::from_secs(catalog_ttl_secs),
    ));

    let local_port: u16 = env_nonempty("TERMINUS_CLIENT_LOCAL_PORT").and_then(|v| v.parse().ok()).unwrap_or(8310);

    let listener = match tokio::net::TcpListener::bind((LOCAL_BIND_ADDR, local_port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("terminus-client-daemon: failed to bind local MCP endpoint on {LOCAL_BIND_ADDR}:{local_port}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!("terminus-client-daemon: serving local MCP endpoint on {LOCAL_BIND_ADDR}:{local_port}");

    // PWQ-03: connect-info-aware serving so `handle_mcp` can mint a stable
    // per-connection fairness scope (see `DaemonState::connection_scope`) --
    // a plain `axum::serve(listener, router)` never exposes the peer address
    // to extractors.
    let router = build_router(state);
    let make_service = router.into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, make_service).await {
        eprintln!("terminus-client-daemon: local MCP listener exited with an error: {e}");
        std::process::exit(1);
    }
}
