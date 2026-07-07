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
//! - `TERMINUS_PERSONAL_TOKEN` — optional. If set, `/mcp` requires
//!   `Authorization: Bearer <value>`. If unset, `/mcp` is unauthenticated,
//!   matching the confirmed posture of the existing legacy Python host.
//! - Individual tool modules (plane, gitea, github, ansible, network, ...)
//!   each read their own env vars directly (e.g. `PLANE_API_URL`,
//!   `GITEA_TOKEN`) exactly as they already do for every other Terminus bin —
//!   this binary does no special config wiring beyond the two vars above.
//!   Secrets are expected to be sourced from the fleet's secret store into
//!   the process environment by the systemd unit's `ExecStartPre` (see
//!   deploy docs), not committed to any file.

use std::sync::Arc;

use terminus_rs::mcp_server::{build_router, McpServerState};
use terminus_rs::registry::{register_personal, ToolRegistry};

#[tokio::main]
async fn main() {
    terminus_rs::intake::init_tracing();

    let port: u16 = std::env::var("TERMINUS_PERSONAL_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8300);

    let auth_token = std::env::var("TERMINUS_PERSONAL_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());

    let mut registry = ToolRegistry::new();
    register_personal(&mut registry);

    tracing::info!(
        "terminus_personal: {} tools registered, binding 0.0.0.0:{port} (auth: {})",
        registry.len(),
        if auth_token.is_some() { "token" } else { "none" }
    );

    let state = Arc::new(McpServerState {
        registry,
        server_name: "terminus-personal".to_string(),
        server_version: terminus_rs::VERSION.to_string(),
        auth_token,
    });

    let router = build_router(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .unwrap_or_else(|e| panic!("terminus_personal: failed to bind 0.0.0.0:{port}: {e}"));

    axum::serve(listener, router)
        .await
        .expect("terminus_personal: server error");
}
