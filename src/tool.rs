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

/// What the DISPATCH LAYER knows about the caller of ONE tool invocation
/// (TRTR-05, privacy).
///
/// Re-exported from [`crate::gateway_framework::caller_context`], where it is
/// DEFINED so that its entitled constructor can be `pub(super)` — i.e. so the
/// gateway module that owns the `AllowlistPolicy` decision is the only place in
/// the crate that can mint an entitled context, checked by the compiler rather
/// than asserted in a doc comment. From here (and everywhere else outside
/// `gateway_framework`) the only reachable constructors are
/// [`CallerContext::untrusted`] and `Default`, both fully unentitled.
///
/// See that module for the full contract.
pub use crate::gateway_framework::caller_context::{CallerContext, PersonScope};

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

    /// Execute the tool knowing who is calling AND which caller-scoped record
    /// the answer belongs to (LOCREG-01).
    ///
    /// [`CallerContext`] answers "what may this caller SEE" and deliberately
    /// carries no identity — it is a set of capability bits and nothing else,
    /// and TRTR-05 keeps it that way on purpose. A per-caller STORE needs the
    /// other half: which record is theirs. That is
    /// [`CallerKey`](crate::locations::CallerKey), derived by the dispatch layer
    /// from the same server-verified principal the gateway authorized, and it is
    /// passed alongside the context rather than folded into it so that neither
    /// type acquires the other's job.
    ///
    /// Additive by construction: the default delegates to
    /// `execute_with_caller`, so every existing tool — including the ones that
    /// already override `execute_with_caller` — is completely unaffected. A tool
    /// overrides this ONLY if it reads or writes data filed under the caller.
    ///
    /// `None` is the fail-closed value and means "the dispatch path did not know
    /// who this is". A tool that keys data on the caller has no correct record to
    /// answer from in that case and MUST decline, never fall back to a shared or
    /// default record.
    async fn execute_with_caller_key(
        &self,
        args: Value,
        caller: CallerContext,
        _key: Option<crate::locations::CallerKey>,
    ) -> Result<ToolOutput, ToolError> {
        self.execute_with_caller(args, caller).await
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

    // ── TRTR-05: the caller-entitlement boundary, seen from OUTSIDE the
    //    gateway module (this module is `crate::tool`, not
    //    `crate::gateway_framework`) ──────────────────────────────────────

    /// The entitled constructor is not reachable from here — enforced by the
    /// COMPILER, not by this assertion.
    ///
    /// `CallerContext::from_allowlist_decision` is `pub(super)` to
    /// `crate::gateway_framework`, so a call to it from this module (or any
    /// other module outside that tree, or any downstream crate) is a hard
    /// `E0624`. That half of the invariant CANNOT be asserted at runtime — code
    /// violating it does not build, and a test that tried would take the whole
    /// test binary with it — so it is checked by a `compile_fail` DOCTEST on
    /// `CallerContext::untrusted` instead (`cargo test --doc`; no `trybuild`
    /// dependency, this repo has no compile-fail harness and TRTR-05 does not
    /// add one). Note `cargo test --lib` alone does NOT run it.
    ///
    /// What THIS test asserts is the observable consequence available to
    /// `--lib`: everything an out-of-gateway caller CAN construct is
    /// unentitled, so no tool can obtain operator context without the gateway.
    #[test]
    fn trtr05_caller_context_reachable_from_outside_the_gateway_is_always_untrusted() {
        for ctx in [CallerContext::untrusted(), CallerContext::default(), CallerContext::default()] {
            assert!(
                !ctx.may_infer_from_calendar(),
                "a context built outside the gateway must never grant calendar inference"
            );
            assert!(
                !ctx.may_infer_from_routine(),
                "a context built outside the gateway must never grant routine inference"
            );
        }
    }

    /// TRTR-05 req: the ~400 tools that do not override `execute_with_caller`
    /// keep answering exactly as before, for ANY caller — the default
    /// delegation to `execute_structured` is untouched by the lockdown.
    #[tokio::test]
    async fn trtr05_default_execute_with_caller_still_delegates_unchanged() {
        let plain = NoOpTool;
        let out = plain
            .execute_with_caller(serde_json::json!({}), CallerContext::untrusted())
            .await
            .unwrap();
        assert_eq!(out.text, "ok");
        assert_eq!(out.structured, None);

        // ...including for a tool that overrides only `execute_structured`.
        let structured = StructuredTool;
        let out = structured
            .execute_with_caller(serde_json::json!({}), CallerContext::default())
            .await
            .unwrap();
        assert_eq!(out.text, "id: 42");
        assert_eq!(out.structured, Some(serde_json::json!({"id": 42})));
    }
}
