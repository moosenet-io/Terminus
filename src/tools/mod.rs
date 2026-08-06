//! Terminus tool modules grouped under `tools/`.
//!
//! Most tool modules live at the crate root (one dir per integration). The
//! `tools/` namespace hosts the S85 serving control/status tools (SRV-07), which
//! sit ON TOP of the serving intake foundation (`crate::intake::serving`) and the
//! Chord control plane rather than a single external integration, and (as of
//! DOCGEN-01, S95) the `docgen` sovereign documentation-engine scaffold.

pub mod docgen;
/// RMCP-11 (S132): `rmcp_session_list` / `rmcp_session_revoke` — the operator's
/// view of, and cut-off control over, remote-MCP connector sessions. Lives here
/// rather than under `oauth/` for the same reason the serving tools do: it sits
/// on top of a subsystem (`crate::oauth`) rather than wrapping one external
/// integration, and the subsystem must stay usable without the tool layer.
pub mod rmcp_session;
/// RMCP-12 (S132): `rmcp_server_owner_set` / `rmcp_server_owner_list` — the
/// operator's control over which account administers which federated server.
/// RMCP-08 (S132): `rmcp_client_*` and the initial-access-token tools — the
/// two ways a connector's `client_id` comes into existence, and its lifecycle.
/// TERM #654: `rmcp_account_*` — how an account comes into existence at all,
/// including the one-shot first-operator bootstrap. Every other tool here
/// presupposes an account; nothing created one until this module.
pub mod rmcp_account;
pub mod rmcp_client;
pub mod rmcp_owner;
pub mod serving_tools;

use crate::registry::ToolRegistry;

/// Register every tool under `tools/`.
pub fn register(registry: &mut ToolRegistry) {
    docgen::register(registry);
    rmcp_account::register(registry);
    rmcp_client::register(registry);
    rmcp_owner::register(registry);
    rmcp_session::register(registry);
    serving_tools::register(registry);
}
