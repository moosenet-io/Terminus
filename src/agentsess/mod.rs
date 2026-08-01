//! `agentsess` — observability for coder CLI agent sessions (AGSS-01..03).
//!
//! Harmony can see a tracked repo's Plane status but nothing about the coder
//! CLI agents (Claude Code, codex, aider) actually working that repo. This
//! module is the missing primitive: it enumerates live agent sessions on a
//! host, correlates each to the repository, branch and `PREFIX-NN` work item
//! it is working on, and exposes its recent activity and terminal pane.
//!
//! # This suite is READ-ONLY, deliberately
//!
//! Nothing here writes a file, sends a keystroke, or signals a process. That
//! is a designed boundary, not an accident of scope: being able to *watch* an
//! autonomous agent is useful on its own and carries no risk, whereas being
//! able to *type into* one can alter a build mid-flight. The interactive half
//! (a `send-keys` equivalent) is a SEPARATE, GATED spec — it needs a session
//! allowlist, a control-character whitelist, rate limiting and an audit-log
//! entry before it is safe to ship. **Do not add a send/write capability to
//! this module without that gate.**
//!
//! # Layout
//! - [`model`] — the session types every tool returns
//! - [`exec`] — the host executor (local, or the existing dev SSH door)
//! - [`discover`] — the probes and the correlation logic
//! - [`tools`] — the registered `agentsess_*` tools
//!
//! # Configuration (structural, non-secret — plain env per this crate's convention)
//! - `AGENTSESS_TRANSCRIPT_ROOT` — where agent transcripts live. Defaults to
//!   `$HOME/.claude/projects` for the local host; REQUIRED to observe a remote
//!   host, because assuming the local `HOME` applies remotely would silently
//!   probe the wrong path.
//! - `AGENTSESS_AGENT_PATTERNS` — comma-separated extra program names to treat
//!   as agents, for a CLI this build predates.
//! - `AGENTSESS_MAX_SESSIONS` — result cap (default 50). Truncation is always
//!   reported, never silent.
//!
//! This module reads NO credential and holds NO secret. The one place it
//! touches a process environment narrows the read to a single non-secret
//! variable AT THE SOURCE (a NUL-delimited `grep`), so no other environment
//! entry is ever materialised here or sent across the remote path.

// `exec` and `discover` are crate-private ON PURPOSE. `HostExecutor::run`
// takes an arbitrary argv, and the dev executor carries the fleet's dev-host
// SSH credential — exposing either publicly would hand any consumer of this
// crate a general-purpose remote-command primitive under those credentials.
// The only public entry points are the registered tools, which accept a fixed
// `host` enum and build every command themselves. `model` stays public: it is
// inert data that Harmony deserializes.
pub mod model;
pub(crate) mod discover;
pub(crate) mod exec;
pub(crate) mod tools;

pub use model::{AgentKind, AgentSession, RepoContext, SessionAttachment, SessionsSnapshot};

use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub fn register(registry: &mut ToolRegistry) {
    let tools: Vec<Box<dyn RustTool>> = vec![Box::new(tools::AgentsessList)];
    for tool in tools {
        if let Err(e) = registry.register(tool) {
            tracing::error!("agentsess: failed to register tool: {e}");
        }
    }
}
