//! Core RustTool trait that every Rust tool implementation must satisfy.
//!
//! Implementing this trait is all a tool module needs to do. The ToolRegistry
//! discovers and dispatches to all registered implementations at runtime.

use serde_json::Value;
use crate::error::ToolError;

/// A Rust tool implementation that can be registered in the ToolRegistry
/// and used as a fallback when the <host> MCP backend is unavailable.
///
/// ## Contract
/// - `name()` must be stable across restarts — it is the dispatch key
/// - `parameters()` must return a valid JSON Schema object describing inputs
/// - `execute()` must be safe to call concurrently (Send + Sync)
/// - `execute()` must NEVER use shell commands or subprocess calls
/// - `execute()` must use typed HTTP clients (reqwest) or parameterized SQL (sqlx)
///   for all external I/O
#[async_trait::async_trait]
pub trait RustTool: Send + Sync + 'static {
    /// The tool's stable identifier. Matches the MCP tool name it replaces.
    fn name(&self) -> &str;

    /// Human-readable description shown in the tool catalog.
    fn description(&self) -> &str;

    /// JSON Schema describing accepted arguments.
    fn parameters(&self) -> Value;

    /// Execute the tool. Returns a text result or a ToolError.
    async fn execute(&self, args: Value) -> Result<String, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoOpTool;

    #[async_trait::async_trait]
    impl RustTool for NoOpTool {
        fn name(&self) -> &str { "noop" }
        fn description(&self) -> &str { "Does nothing" }
        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("ok".into())
        }
    }

    #[tokio::test]
    async fn test_rust_tool_trait_implementable() {
        let tool = NoOpTool;
        assert_eq!(tool.name(), "noop");
        assert_eq!(tool.description(), "Does nothing");

        let params = tool.parameters();
        assert_eq!(params["type"], "object");

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(result, "ok");
    }

    #[tokio::test]
    async fn test_rust_tool_send_sync_boxable() {
        let tool: Box<dyn RustTool> = Box::new(NoOpTool);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(result, "ok");
    }

    #[tokio::test]
    async fn test_rust_tool_arc_shareable() {
        let tool = std::sync::Arc::new(NoOpTool);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(result, "ok");
    }
}
