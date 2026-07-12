//! `terminus-worker-sdk`: the thin authoring surface for a Terminus "tool
//! worker" process.
//!
//! A worker is, in the common case, "`impl RustTool` for one or a handful of
//! tools + a few lines of `main.rs`":
//!
//! ```no_run
//! use terminus_worker_sdk::{RustTool, ToolError, Worker};
//! use serde_json::Value;
//!
//! struct Echo;
//!
//! #[async_trait::async_trait]
//! impl RustTool for Echo {
//!     fn name(&self) -> &str { "echo" }
//!     fn description(&self) -> &str { "Echoes its input back" }
//!     fn parameters(&self) -> Value {
//!         serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
//!     }
//!     async fn execute(&self, args: Value) -> Result<String, ToolError> {
//!         Ok(args.get("text").and_then(Value::as_str).unwrap_or("").to_string())
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     Worker::builder("echo-worker", "0.1.0")
//!         .capability_class("core")
//!         .tool(Box::new(Echo))
//!         .serve("/tmp/echo-worker.sock")
//!         .await?;
//!     Ok(())
//! }
//! ```
//!
//! ## What this crate is (and isn't)
//! - IS: a re-export of the main `terminus-rs` crate's tool-authoring types
//!   ([`RustTool`], [`ToolOutput`], [`ToolError`], [`ToolInfo`]) plus a
//!   worker-side server for the `initialize`/`tools/list`/`tools/call` MCP
//!   subset, wired to dispatch by tool name.
//! - IS NOT: a relocation of those types out of `terminus-rs` (they still
//!   live there; this crate depends on it by path and re-exports) or a
//!   client for the daemon-side broker transport (`terminus-rs`'s
//!   `broker::transport::WorkerTransport`, a sibling, unmerged item as of
//!   this writing) -- this crate's [`server`] module is a self-contained
//!   worker-side listener, not that transport's counterpart. Reconciling
//!   the two wire formats is a follow-up item once the broker transport
//!   lands.

pub mod manifest;
pub mod server;

use std::collections::HashMap;
use std::sync::Arc;

pub use manifest::{ManifestError, WorkerManifest};
pub use server::ServeError;

// Re-export the tool-authoring surface from the main crate unchanged --
// TMOD-03 deliberately does NOT relocate these types (a sibling item is
// concurrently editing `terminus-rs::registry`).
pub use terminus_rs::error::ToolError;
pub use terminus_rs::registry::ToolInfo;
pub use terminus_rs::tool::{RustTool, ToolOutput};

/// Builder for a worker process: register one or more [`RustTool`] impls,
/// set a name/semver/capability class, then [`Worker::serve`] a Unix domain
/// socket.
pub struct Worker {
    name: String,
    semver: String,
    capability_class: String,
    tools: HashMap<String, Box<dyn RustTool>>,
    /// Names that collided at `tool()` time -- kept separately from `tools`
    /// (which always holds the LAST registration for a colliding name) so
    /// `serve()` can refuse to start with a precise, ordered error instead
    /// of silently keeping only the last one.
    duplicate_names: Vec<String>,
}

impl Worker {
    /// Start building a worker. `name` is the worker's stable identity;
    /// `semver` must be a plain `MAJOR.MINOR.PATCH` string (validated at
    /// [`Worker::serve`] time, not here, so a builder chain can be
    /// constructed independent of validation order).
    pub fn builder(name: impl Into<String>, semver: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            semver: semver.into(),
            capability_class: "core".to_string(),
            tools: HashMap::new(),
            duplicate_names: Vec::new(),
        }
    }

    /// Set the coarse capability/tier hint advertised in the worker's
    /// manifest (e.g. `"core"`, `"personal"`, `"mesh-upstream"`). Defaults
    /// to `"core"` if never called.
    pub fn capability_class(mut self, class: impl Into<String>) -> Self {
        self.capability_class = class.into();
        self
    }

    /// Register a tool. Panics-free: a duplicate name is reported by
    /// [`Worker::serve`] returning `Err(ManifestError::DuplicateTool)`
    /// rather than rejected here, so a builder chain never needs to unwrap
    /// mid-chain -- `serve()` is the single place that fails closed.
    pub fn tool(mut self, tool: Box<dyn RustTool>) -> Self {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            self.duplicate_names.push(name.clone());
        }
        self.tools.insert(name, tool);
        self
    }

    /// Validate the manifest (name non-empty, semver well-formed, no
    /// duplicate tool names) and, if valid, bind `socket_path` and serve the
    /// `initialize`/`tools/list`/`tools/call` subset forever.
    ///
    /// A worker declaring zero tools is valid (empty catalog, no panic) --
    /// only a malformed semver, empty name, or a genuine name collision
    /// refuse to start.
    pub async fn serve(self, socket_path: &str) -> Result<(), WorkerStartError> {
        if self.name.trim().is_empty() {
            return Err(ManifestError::EmptyName.into());
        }
        manifest::validate_semver(&self.semver)?;
        if let Some(dup) = self.duplicate_names.first() {
            return Err(ManifestError::DuplicateTool(dup.clone()).into());
        }

        let manifest = WorkerManifest {
            name: self.name,
            semver: self.semver,
            capability_class: self.capability_class,
            tools: self
                .tools
                .values()
                .map(|t| ToolInfo {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: t.parameters(),
                })
                .collect(),
        };
        let state = Arc::new(server::WorkerState { manifest, tools: self.tools });
        server::serve(socket_path, state).await?;
        Ok(())
    }
}

/// Errors starting a worker: either the manifest failed validation, or the
/// socket server itself failed to bind/serve.
#[derive(Debug, thiserror::Error)]
pub enum WorkerStartError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Serve(#[from] ServeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct NoOp(&'static str);

    #[async_trait::async_trait]
    impl RustTool for NoOp {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "no-op"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("ok".to_string())
        }
    }

    use serde_json::Value;

    fn temp_socket_path(tag: &str) -> String {
        format!(
            "{}/terminus-worker-sdk-libtest-{}-{}.sock",
            std::env::temp_dir().display(),
            tag,
            std::process::id()
        )
    }

    #[tokio::test]
    async fn empty_name_refuses_to_start() {
        let err = Worker::builder("", "1.0.0")
            .tool(Box::new(NoOp("a")))
            .serve(&temp_socket_path("empty-name"))
            .await
            .unwrap_err();
        assert!(matches!(err, WorkerStartError::Manifest(ManifestError::EmptyName)));
    }

    #[tokio::test]
    async fn malformed_semver_refuses_to_start() {
        let err = Worker::builder("w", "not-a-version")
            .serve(&temp_socket_path("bad-semver"))
            .await
            .unwrap_err();
        assert!(matches!(err, WorkerStartError::Manifest(ManifestError::InvalidSemver(_))));
    }

    #[tokio::test]
    async fn duplicate_tool_names_refused_at_build_time() {
        let err = Worker::builder("w", "1.0.0")
            .tool(Box::new(NoOp("dup")))
            .tool(Box::new(NoOp("dup")))
            .serve(&temp_socket_path("dup-tool"))
            .await
            .unwrap_err();
        assert!(matches!(err, WorkerStartError::Manifest(ManifestError::DuplicateTool(name)) if name == "dup"));
    }

    #[tokio::test]
    async fn zero_tools_starts_without_panicking() {
        let socket_path = temp_socket_path("zero-tools");
        let path_for_serve = socket_path.clone();
        let handle = tokio::spawn(async move {
            Worker::builder("empty-worker", "1.0.0").serve(&path_for_serve).await
        });

        // The server loops forever on success -- give it a moment to bind,
        // then confirm it's actually listening (didn't error out) rather
        // than waiting on the handle, which would hang forever on success.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "zero-tool worker should be serving, not exited");

        let conn = tokio::net::UnixStream::connect(&socket_path).await;
        assert!(conn.is_ok(), "zero-tool worker socket should accept connections");

        handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }
}
