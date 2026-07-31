//! Core RustTool trait that every Rust tool implementation must satisfy.
//!
//! Implementing this trait is all a tool module needs to do. The ToolRegistry
//! discovers and dispatches to all registered implementations at runtime.

use serde_json::Value;
use crate::error::ToolError;

/// A tool's result: always a human-readable text summary (`content` in MCP's
/// `CallToolResult`), optionally paired with a structured JSON payload (MCP's
/// `structuredContent`) for callers that need to destructure typed data
/// rather than parse prose.
///
/// EGJS-01: this is the additive structured-output mechanism -- native MCP
/// `structuredContent` alongside `content`, chosen over a `format:"json"`
/// tool argument because it needs no schema/argument change at all (existing
/// callers that only read `content[0].text` are completely unaffected, and a
/// structured-aware caller like Harmony's egress client can look for
/// `result.structuredContent` first and fall back to parsing text only for
/// tools that haven't been upgraded yet).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    pub structured: Option<Value>,
}

impl ToolOutput {
    pub fn text_only(text: impl Into<String>) -> Self {
        Self { text: text.into(), structured: None }
    }

    pub fn with_structured(text: impl Into<String>, structured: Value) -> Self {
        Self { text: text.into(), structured: Some(structured) }
    }
}

/// What the DISPATCH LAYER knows about the caller of ONE tool invocation.
///
/// TRTR-05 (privacy). Some tools can answer a question by reaching for context
/// the OPERATOR owns — the operator's calendar, the operator's configured home
/// and work addresses — even though the tool itself looks stateless. That is
/// safe for the operator and a disclosure for anyone else, so a tool that does
/// it must know who is asking. This struct is the ONLY channel by which it may
/// find out: it is constructed by the gateway from the same server-verified
/// `Principal` that `GatewayFramework::guard` authorizes, and it is never
/// derived from tool arguments, a header, or an env var.
///
/// **Fail closed by construction.** `Default` (and `untrusted()`) is "we know
/// nothing about this caller", with every capability `false`. Every existing
/// dispatch path that does not thread a caller therefore gets the safe value,
/// and a NEW dispatch path that forgets to thread one is safe by default rather
/// than accidentally operator-privileged. A spurious "which location did you
/// mean?" costs one conversational turn; a leaked home or appointment address
/// cannot be taken back.
///
/// Each flag is a permission to USE ONE SOURCE of operator context, and the
/// gateway grants it only when the caller is already authorized for the tool
/// that exposes that source directly (`google_calendar_today`,
/// `commute_estimate`) — so an inference can never disclose anything the caller
/// could not have fetched for itself. See
/// `crate::gateway_framework::GatewayFramework::caller_context`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallerContext {
    may_infer_from_calendar: bool,
    may_infer_from_routine: bool,
}

impl CallerContext {
    /// The fail-closed value: an unknown, unauthenticated or unrecognised
    /// caller. No operator context may be used on its behalf.
    pub const fn untrusted() -> Self {
        Self { may_infer_from_calendar: false, may_infer_from_routine: false }
    }

    /// Build a context from an explicit per-source decision. Only the gateway
    /// (and tests) should call this — see the struct doc.
    pub const fn new(may_infer_from_calendar: bool, may_infer_from_routine: bool) -> Self {
        Self { may_infer_from_calendar, may_infer_from_routine }
    }

    /// May a tool consult the OPERATOR's calendar on this caller's behalf?
    pub const fn may_infer_from_calendar(&self) -> bool {
        self.may_infer_from_calendar
    }

    /// May a tool consult the OPERATOR's configured home/work routine on this
    /// caller's behalf?
    pub const fn may_infer_from_routine(&self) -> bool {
        self.may_infer_from_routine
    }
}

/// A Rust tool implementation that can be registered in the ToolRegistry
/// and used as a fallback when the fleet-host MCP backend is unavailable.
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

    /// Execute the tool, optionally returning a structured JSON payload
    /// alongside the text summary (EGJS-01). Default implementation calls
    /// `execute()` and returns no structured payload, so every existing tool
    /// is unaffected unless it deliberately overrides this method (typically
    /// tools whose result is typed data -- Plane/Gitea read tools -- override
    /// it to also emit `structured`, usually by sharing a private `run()`
    /// helper with `execute()` rather than duplicating the fetch/parse logic).
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let text = self.execute(args).await?;
        Ok(ToolOutput { text, structured: None })
    }

    /// Execute the tool knowing WHO is calling (TRTR-05).
    ///
    /// The default implementation ignores the caller and delegates to
    /// `execute_structured`, so the ~400 tools that answer the same way for
    /// everybody are untouched. A tool overrides this ONLY if its answer can
    /// otherwise contain operator/household context the caller did not supply
    /// and may not be entitled to (today: `weather`, whose location resolution
    /// can reach the operator's calendar and home/work addresses).
    ///
    /// An overriding tool must treat [`CallerContext::untrusted`] — the value
    /// every un-plumbed path and `execute()` itself produce — as "not the
    /// operator", never as "unknown, proceed".
    async fn execute_with_caller(
        &self,
        args: Value,
        _caller: CallerContext,
    ) -> Result<ToolOutput, ToolError> {
        self.execute_structured(args).await
    }
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

    // ── EGJS-01: default execute_structured ────────────────────────────────

    #[tokio::test]
    async fn test_default_execute_structured_wraps_text_with_no_structured_payload() {
        let tool = NoOpTool;
        let output = tool.execute_structured(serde_json::json!({})).await.unwrap();
        assert_eq!(output.text, "ok");
        assert_eq!(output.structured, None);
    }

    struct StructuredTool;

    #[async_trait::async_trait]
    impl RustTool for StructuredTool {
        fn name(&self) -> &str { "structured" }
        fn description(&self) -> &str { "Returns structured data" }
        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("id: 42".into())
        }
        async fn execute_structured(&self, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::with_structured("id: 42", serde_json::json!({"id": 42})))
        }
    }

    #[tokio::test]
    async fn test_overridden_execute_structured_carries_structured_payload() {
        let tool = StructuredTool;
        let output = tool.execute_structured(serde_json::json!({})).await.unwrap();
        assert_eq!(output.text, "id: 42");
        assert_eq!(output.structured, Some(serde_json::json!({"id": 42})));
        // execute() itself is untouched -- same text, still just a String.
        let text = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(text, "id: 42");
    }

    #[test]
    fn test_tool_output_text_only_has_no_structured_payload() {
        let out = ToolOutput::text_only("hello");
        assert_eq!(out.text, "hello");
        assert_eq!(out.structured, None);
    }
}
