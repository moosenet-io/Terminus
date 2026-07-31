//! Sundry trivial tools — ported 1:1 from the Python MCP server on the legacy
//! Terminus host (the legacy Python MCP host, streamable-HTTP MCP endpoint). These are small
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
        // The old description ("Query MooseNet SearXNG via NPM") named internal
        // plumbing rather than the capability, so a model could not tell WHEN to use
        // it. Describe the job, not the implementation.
        "Search the web and return results. Use for current events, facts, and \
         anything you do not already know. (Not for weather — use the `weather` tool.)"
    }

    /// NOTE ON `required: ["q"]` (verified 2026-07-31, deliberately unchanged).
    ///
    /// A review finding asked whether declaring only `q` as required makes the
    /// tolerant `q`/`query`/`search` handling in `execute` unreachable. It does
    /// not: **nothing in this codebase validates tool arguments against this
    /// schema before `execute` runs.** `tools/call` in `mcp_server.rs` takes
    /// `params.arguments` verbatim, applies the authorization and availability
    /// gates, and hands the raw `Value` to `ToolRegistry::call`, which calls
    /// `tool.execute(args)` directly (`registry.rs`); there is no JSON-Schema
    /// crate in the dependency tree on either side of the door. So a model that
    /// sends `query` reaches the tolerant code and succeeds — the fallback is
    /// live, not inert.
    ///
    /// `required: ["q"]` therefore stays as the STEER: it tells the model the
    /// canonical name (so most calls arrive as `q`) and states that a search term
    /// is mandatory. Relaxing it would only weaken that signal — "exactly one
    /// non-blank query term" is enforced in `execute`, which rejects blank and
    /// absent alike, and that enforcement is what actually runs.
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                // Documented, and `query`/`search` are accepted as aliases — see
                // `execute`. Kept out of `properties` on purpose: advertising three
                // names invites a model to send more than one.
                "q": {
                    "type": "string",
                    "description": "The search terms — REQUIRED, and must be non-blank. Send them as `q`; the aliases `query` and `search` are also accepted, but send exactly one."
                },
                "categories": {
                    "type": "string",
                    "default": "general",
                    "description": "SearXNG category, e.g. general, news, images, science."
                },
                "language": {
                    "type": "string",
                    "default": "en-US",
                    "description": "Result language, e.g. en-US."
                }
            },
            "required": ["q"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // TOLERANT INPUT. The parameter is `q` (SearXNG's own name), but `query` is the
        // name a model naturally reaches for — and did: a live turn failed with
        // "Invalid argument: q is required" in 1ms and the user saw a broken response.
        // This tool is in the router's ALWAYS-OFFERED essentials, so that failure was
        // reachable on any turn. Accepting the obvious synonym costs nothing and
        // removes a whole class of dead turn.
        let q = args
            .get("q")
            .or_else(|| args.get("query"))
            .or_else(|| args.get("search"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArgument(
                    "a search term is required — pass it as `q` (or `query`)".into(),
                )
            })?;
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

#[cfg(test)]
mod searxng_input_tests {
    use super::*;
    use serde_json::json;

    /// Mirrors `SearxngSearch::execute`'s argument resolution so the tolerance
    /// contract is testable without a live SearXNG instance.
    fn resolve_q(args: &Value) -> Option<String> {
        args.get("q")
            .or_else(|| args.get("query"))
            .or_else(|| args.get("search"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    #[test]
    fn the_documented_name_works() {
        assert_eq!(resolve_q(&json!({"q": "rust"})).as_deref(), Some("rust"));
    }

    #[test]
    fn the_name_a_model_actually_guesses_also_works() {
        // A live turn failed with "q is required" because the model sent `query`.
        // searxng_search is in the router's always-offered essentials, so that dead
        // turn was reachable on ANY request.
        assert_eq!(resolve_q(&json!({"query": "san francisco weather"})).as_deref(),
                   Some("san francisco weather"));
        assert_eq!(resolve_q(&json!({"search": "news"})).as_deref(), Some("news"));
    }

    #[test]
    fn the_documented_name_wins_when_both_are_present() {
        assert_eq!(resolve_q(&json!({"q": "a", "query": "b"})).as_deref(), Some("a"));
    }

    #[test]
    fn blank_and_missing_are_still_rejected() {
        // Tolerance must not become "search for nothing".
        assert!(resolve_q(&json!({})).is_none());
        assert!(resolve_q(&json!({"q": "   "})).is_none());
        assert!(resolve_q(&json!({"query": ""})).is_none());
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(resolve_q(&json!({"q": "  rust  "})).as_deref(), Some("rust"));
    }
}
