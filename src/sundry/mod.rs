//! Sundry trivial tools — ported 1:1 from the Python MCP server on the legacy
//! Terminus host (`ai-terminus`, streamable-HTTP MCP endpoint). These are small
//! utility/one-liner tools that don't warrant a dedicated module of their own.
//!
//! Verified against the live legacy Terminus server via `tools/list` (for schema) and
//! `tools/call` (for real output shape) on 2026-07-06: — pii-test-fixture
//!
//!   - `health`                — static `{"ok": true}` liveness ping.
//!   - `echo`                  — returns the `text` argument verbatim.
//!   - `utc_now`               — current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
//!   - `constellation_version` — static build/deployment metadata plus a live
//!                               timestamp. All non-timestamp fields (constellation
//!                               name, version, session, mcp_hub, agent_fleet,
//!                               orchestrator, plugin_architecture, skills_standard)
//!                               were observed constant across repeated calls on
//!                               the live server, so they are ported as fixed
//!                               values (byte-for-byte match at port time) rather
//!                               than derived — matching the operator's "1:1
//!                               stub" instruction. A human audit is expected
//!                               later to decide whether these should become
//!                               dynamic (e.g. `CARGO_PKG_VERSION`).
//!   - `vector_onboard`        — static Vector operating-manual JSON blob
//!                               (guardrails, submission instructions, cost
//!                               limits). Config-driven fields (`active_projects`,
//!                               `conventions`) were empty on the live server;
//!                               ported as empty arrays to match.
//!   - `searxng_search`        — single HTTP GET against a MooseNet SearXNG
//!                               instance (reached via NPM/nginx-proxy-manager)
//!                               with `format=json`, response body passed
//!                               through verbatim (matches the live server's
//!                               pass-through JSON shape: `query`,
//!                               `number_of_results`, `results`, `answers`,
//!                               `corrections`, `infoboxes`, `suggestions`,
//!                               `unresponsive_engines`).
//!
//! ## Configuration (env vars — no hardcoded hosts/secrets)
//!   SEARXNG_URL — base URL of the SearXNG instance (e.g.
//!                 "https://search.moosenet.internal"). Required for
//!                 `searxng_search`; if unset the tool returns NotConfigured.

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Tool: health
// ---------------------------------------------------------------------------

pub struct Health;

#[async_trait]
impl RustTool for Health {
    fn name(&self) -> &str {
        "health"
    }

    fn description(&self) -> &str {
        // The legacy Python tool's live docstring is empty; the terminus-rs registry
        // requires a non-empty description for every tool, so a short one is supplied here.
        "Liveness ping. Returns {\"ok\": true} if the server is responding."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Ok(serde_json::to_string_pretty(&json!({"ok": true}))
            .unwrap_or_else(|_| "{\"ok\": true}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tool: echo
// ---------------------------------------------------------------------------

pub struct Echo;

#[async_trait]
impl RustTool for Echo {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        // The legacy Python tool's live docstring is empty; supplying a short one to
        // satisfy the terminus-rs non-empty-description invariant.
        "Echo the given text back verbatim. Useful for connectivity checks."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"}
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("text is required".into()))?;
        Ok(text.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: utc_now
// ---------------------------------------------------------------------------

pub struct UtcNow;

#[async_trait]
impl RustTool for UtcNow {
    fn name(&self) -> &str {
        "utc_now"
    }

    fn description(&self) -> &str {
        // The legacy Python tool's live docstring is empty; supplying a short one to
        // satisfy the terminus-rs non-empty-description invariant.
        "Return the current UTC time as an ISO-8601 timestamp."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Ok(Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: constellation_version
// ---------------------------------------------------------------------------

pub struct ConstellationVersion;

#[async_trait]
impl RustTool for ConstellationVersion {
    fn name(&self) -> &str {
        "constellation_version"
    }

    fn description(&self) -> &str {
        "Return Lumina Constellation version info and build metadata.\nUse this to verify the MCP server is running and check deployment info."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
        let body = json!({
            "constellation": "Lumina Constellation",
            "version": "0.12.0",
            "session": 12,
            "mcp_hub": "the Terminus MCP hub container",
            "agent_fleet": "the agent fleet host",
            "orchestrator": "the orchestrator container (agent runtime v0.24.0)",
            "plugin_architecture": true,
            "skills_standard": "agentskills.io",
            "timestamp": timestamp,
        });
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_onboard
// ---------------------------------------------------------------------------

pub struct VectorOnboard;

#[async_trait]
impl RustTool for VectorOnboard {
    fn name(&self) -> &str {
        "vector_onboard"
    }

    fn description(&self) -> &str {
        "Get Vector operating manual. Call this before delegating work to Vector.\nReturns: guardrails, active projects, conventions, available models, how to submit tasks.\n\nAny agent (Lumina, Seer, etc.) should call this before their first Vector interaction\nin a session to understand current state and operating rules."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let body = json!({
            "agent": "vector",
            "version": "1.0",
            "status": "active",
            "system_guardrails": [
                "Never merge own PRs",
                "Write tests before committing",
                "Cost gate max $2/task"
            ],
            "active_projects": [],
            "conventions": [],
            "how_to_submit": {
                "via_nexus": "nexus_send(from_agent='lumina', to_agent='vector', message_type='work_order', payload=json.dumps({'op':'maintenance','task':'<description>','repo':'<path>'}))",
                "via_mcp": "vector_submit(task='<description>', repo='<path>', cost_budget=2.0)"
            },
            "cost_limits": {
                "max_per_task": 2.0,
                "max_per_day": 10.0
            },
            "calx_active": true,
            "skill_aware": true
        });
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tool: searxng_search
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SearxngConfig {
    base_url: String,
}

impl SearxngConfig {
    fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("SEARXNG_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("SEARXNG_URL is not set".into()))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("MooseNet-MCP/1.0")
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }
}

pub struct SearxngSearch;

#[async_trait]
impl RustTool for SearxngSearch {
    fn name(&self) -> &str {
        "searxng_search"
    }

    fn description(&self) -> &str {
        "Query MooseNet SearXNG via NPM and return JSON results."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "q": {"type": "string"},
                "categories": {"type": "string", "default": "general"},
                "language": {"type": "string", "default": "en-US"}
            },
            "required": ["q"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let q = args
            .get("q")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("q is required".into()))?;
        let categories = args
            .get("categories")
            .and_then(Value::as_str)
            .unwrap_or("general");
        let language = args
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("en-US");

        let config = SearxngConfig::from_env()?;
        let client = SearxngConfig::client()?;
        let url = format!("{}/search", config.base_url);

        let resp = client
            .get(&url)
            .query(&[
                ("q", q),
                ("categories", categories),
                ("language", language),
                ("format", "json"),
            ])
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "SearXNG returned HTTP {}",
                resp.status()
            )));
        }

        let body: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(Health),
        Box::new(Echo),
        Box::new(UtcNow),
        Box::new(ConstellationVersion),
        Box::new(VectorOnboard),
        Box::new(SearxngSearch),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            tracing::error!("sundry: failed to register tool: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    fn full_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        reg
    }

    #[test]
    fn test_sundry_tools_registered() {
        let reg = full_registry();
        for name in [
            "health",
            "echo",
            "utc_now",
            "constellation_version",
            "vector_onboard",
            "searxng_search",
        ] {
            assert!(reg.contains(name), "{name} must be registered");
        }
        assert_eq!(reg.len(), 6);
    }

    #[tokio::test]
    async fn test_health_returns_ok_true() {
        let tool = Health;
        let out = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
    }

    #[tokio::test]
    async fn test_echo_returns_text_verbatim() {
        let tool = Echo;
        let out = tool.execute(json!({"text": "hello world"})).await.unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn test_echo_missing_text_errors() {
        let tool = Echo;
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_utc_now_format() {
        let tool = UtcNow;
        let out = tool.execute(json!({})).await.unwrap();
        // Format: YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(out.len(), 20);
        assert!(out.ends_with('Z'));
        assert!(chrono::DateTime::parse_from_rfc3339(&out.replace('Z', "+00:00")).is_ok());
    }

    #[tokio::test]
    async fn test_constellation_version_shape() {
        let tool = ConstellationVersion;
        let out = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["constellation"], "Lumina Constellation");
        assert_eq!(parsed["version"], "0.12.0");
        assert_eq!(parsed["plugin_architecture"], true);
        assert!(parsed["timestamp"].as_str().is_some());
    }

    #[tokio::test]
    async fn test_vector_onboard_shape() {
        let tool = VectorOnboard;
        let out = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["agent"], "vector");
        assert!(parsed["system_guardrails"].as_array().unwrap().len() == 3);
        assert_eq!(parsed["cost_limits"]["max_per_task"], 2.0);
    }

    #[tokio::test]
    #[serial]
    async fn test_searxng_search_not_configured_without_env() {
        std::env::remove_var("SEARXNG_URL");
        let tool = SearxngSearch;
        let result = tool.execute(json!({"q": "rust"})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_searxng_search_missing_q_errors() {
        std::env::set_var("SEARXNG_URL", "http://example.invalid");
        let tool = SearxngSearch;
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
        std::env::remove_var("SEARXNG_URL");
    }

    #[tokio::test]
    #[serial]
    async fn test_searxng_search_passthrough_json() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/search")
                .query_param("q", "rust")
                .query_param("format", "json");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "query": "rust",
                    "number_of_results": 0,
                    "results": [],
                    "answers": [],
                    "corrections": [],
                    "infoboxes": [],
                    "suggestions": [],
                    "unresponsive_engines": []
                }));
        });

        std::env::set_var("SEARXNG_URL", server.base_url());
        let tool = SearxngSearch;
        let out = tool.execute(json!({"q": "rust"})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["query"], "rust");
        mock.assert();
        std::env::remove_var("SEARXNG_URL");
    }

    #[tokio::test]
    #[serial]
    async fn test_searxng_search_http_error_propagates() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(500);
        });

        std::env::set_var("SEARXNG_URL", server.base_url());
        let tool = SearxngSearch;
        let result = tool.execute(json!({"q": "rust"})).await;
        assert!(matches!(result, Err(ToolError::Http(_))));
        std::env::remove_var("SEARXNG_URL");
    }
}
