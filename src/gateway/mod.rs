//! Gateway tools — ported from the Python `gateway_tools.py` on <host>.
//!
//! These tools surface the Lumina API Gateway / dashboard that runs on the
//! fleet host (<host>). The Python original shelled out via `ssh ... 'curl ...'`
//! with `shell=True`. This Rust port uses the `ssh2` crate for typed SSH
//! execution (no `shell=True`, no string-interpolated user input) and runs a
//! fixed `curl` command against the gateway's localhost HTTP endpoints.
//!
//! ## Tools (identical names to the Python source)
//!   dashboard_status    — GET /api/health
//!   dashboard_calendar  — GET /api/calendar
//!   dashboard_tasks     — GET /api/tasks
//!   dashboard_insights  — GET /api/insights
//!   dashboard_inbox     — GET /api/inbox
//!   dashboard_refresh   — trigger the dashboard composer run
//!
//! ## Configuration (environment only — no hardcoded hosts/keys)
//!   GATEWAY_SSH_HOST     — SSH host of the gateway box (e.g. "192.168.0.X").
//!   GATEWAY_SSH_USER     — SSH user, default "root".
//!   GATEWAY_SSH_KEY_PATH — path to the SSH private key file.
//!   GATEWAY_URL          — base URL of the gateway, default "http://localhost:8080".
//!   DASHBOARD_API_KEY    — value sent as the `x-api-key` header (same name as Python).
//!   GATEWAY_COMPOSER_CMD — command run for dashboard_refresh. Default mirrors the
//!                          Python composer invocation.
//!
//! ## Security model
//! - All SSH commands are built from fixed templates. The only variable parts
//!   are the gateway endpoint path (chosen from a fixed internal set, never
//!   user input) and the API key (sent as an HTTP header, single-quoted, with
//!   single quotes rejected so it cannot break out of the quoting).
//! - No `shell=true` semantics with raw user input; tools take no user-supplied
//!   arguments at all.

use std::env;
use std::io::Read as IoRead;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use ssh2::Session;
use tracing::{debug, error, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const DEFAULT_GATEWAY_URL: &str = "http://localhost:8080";
const DEFAULT_COMPOSER_CMD: &str = "set -a && . <path>/axon/.env && set +a && \
     python3 <path>/dashboard/composer.py 2>&1 | tail -10";

/// Configuration sourced entirely from environment variables.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// SSH host of the gateway box — from `GATEWAY_SSH_HOST`.
    pub ssh_host: Option<String>,
    /// SSH user — from `GATEWAY_SSH_USER`, default "root".
    pub ssh_user: String,
    /// Path to the SSH private key file — from `GATEWAY_SSH_KEY_PATH`.
    pub ssh_key_path: Option<String>,
    /// Gateway base URL — from `GATEWAY_URL`, default "http://localhost:8080".
    pub gateway_url: String,
    /// API key sent as `x-api-key` — from `DASHBOARD_API_KEY`.
    pub api_key: Option<String>,
    /// Command used by `dashboard_refresh` — from `GATEWAY_COMPOSER_CMD`.
    pub composer_cmd: String,
}

impl GatewayConfig {
    /// Read configuration from environment.
    pub fn from_env() -> Self {
        let ssh_host = env::var("GATEWAY_SSH_HOST").ok().filter(|s| !s.is_empty());
        let ssh_user = env::var("GATEWAY_SSH_USER").unwrap_or_else(|_| "root".into());
        let ssh_key_path = env::var("GATEWAY_SSH_KEY_PATH").ok().filter(|s| !s.is_empty());
        let gateway_url = env::var("GATEWAY_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GATEWAY_URL.into());
        let api_key = env::var("DASHBOARD_API_KEY").ok().filter(|s| !s.is_empty());
        let composer_cmd = env::var("GATEWAY_COMPOSER_CMD")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_COMPOSER_CMD.into());

        GatewayConfig {
            ssh_host,
            ssh_user,
            ssh_key_path,
            gateway_url,
            api_key,
            composer_cmd,
        }
    }

    /// Resolve the SSH host or return a `NotConfigured` error.
    fn require_host(&self) -> Result<&str, ToolError> {
        self.ssh_host
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("GATEWAY_SSH_HOST is not set".into()))
    }

    /// Resolve the SSH key path or return a `NotConfigured` error.
    fn require_key(&self) -> Result<&str, ToolError> {
        self.ssh_key_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("GATEWAY_SSH_KEY_PATH is not set".into()))
    }

    /// Resolve the API key or return a `NotConfigured` error.
    /// Mirrors the Python `_gateway_key()` which raises if `DASHBOARD_API_KEY`
    /// is unset.
    fn require_api_key(&self) -> Result<&str, ToolError> {
        self.api_key
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("DASHBOARD_API_KEY is not set".into()))
    }
}

// ---------------------------------------------------------------------------
// Command building
// ---------------------------------------------------------------------------

/// Build the remote `curl` command for a fixed gateway endpoint.
///
/// `endpoint` is always one of a small internal set of literals (never user
/// input). `api_key` is validated to contain no single quote so it cannot
/// break out of the single-quoted header argument.
fn build_curl_command(
    gateway_url: &str,
    endpoint: &str,
    api_key: &str,
) -> Result<String, ToolError> {
    if api_key.contains('\'') {
        return Err(ToolError::InvalidArgument(
            "DASHBOARD_API_KEY must not contain a single quote".into(),
        ));
    }
    Ok(format!(
        "curl -s -H 'x-api-key: {api_key}' {gateway_url}{endpoint}"
    ))
}

/// Parse the gateway's response body as JSON, mirroring the Python `_gw`
/// helper: on JSON decode failure it returns an error object rather than
/// failing the tool call.
fn parse_gateway_response(stdout: &str) -> Value {
    match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(v) => v,
        Err(_) => {
            let preview: String = stdout.chars().take(100).collect();
            json!({ "error": format!("invalid JSON: {preview}") })
        }
    }
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking for async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session, run a single command, and return stdout.
///
/// `command` is built from fixed templates by the callers in this module; no
/// raw user input reaches this function.
fn ssh_exec(config: &GatewayConfig, command: &str, timeout_secs: u64) -> Result<String, ToolError> {
    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("gateway: cannot reach gateway host {host}: {e}");
        ToolError::Execution("The gateway server is unreachable.".into())
    })?;

    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new().map_err(|e| {
        warn!("gateway: session init failed: {e}");
        ToolError::Execution("Could not complete the check on the gateway server.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("gateway: handshake failed with {host}: {e}");
        ToolError::Execution("The gateway server is unreachable.".into())
    })?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("gateway: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the gateway server.".into())
        })?;

    if !sess.authenticated() {
        warn!("gateway: authentication failed for {}@{host}", config.ssh_user);
        return Err(ToolError::Execution(
            "Could not connect to the gateway server.".into(),
        ));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("gateway: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the check on the gateway server.".into())
    })?;

    debug!("gateway ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("gateway: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the check on the gateway server.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("gateway: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the check on the gateway server.".into())
    })?;

    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);
    if exit_status != 0 {
        warn!("gateway ssh_exec exit status {exit_status} for: {command}");
    }

    Ok(output)
}

/// Run a fixed gateway endpoint call via SSH and return the parsed JSON,
/// rendered as a pretty string. Mirrors the Python `_gw(endpoint)`.
async fn call_endpoint(config: Arc<GatewayConfig>, endpoint: &'static str) -> Result<String, ToolError> {
    // Resolve API key up front so a missing key surfaces as NotConfigured.
    let api_key = config.require_api_key()?.to_string();
    let command = build_curl_command(&config.gateway_url, endpoint, &api_key)?;

    let cfg = Arc::clone(&config);
    let stdout = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command, 10))
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

    let value = parse_gateway_response(&stdout);
    serde_json::to_string_pretty(&value)
        .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
}

// ---------------------------------------------------------------------------
// Endpoint tools
// ---------------------------------------------------------------------------

/// A simple gateway endpoint tool: fixed name/description/endpoint, no params.
struct DashboardEndpointTool {
    config: Arc<GatewayConfig>,
    tool_name: &'static str,
    desc: &'static str,
    endpoint: &'static str,
}

#[async_trait]
impl RustTool for DashboardEndpointTool {
    fn name(&self) -> &str {
        self.tool_name
    }

    fn description(&self) -> &str {
        self.desc
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        call_endpoint(Arc::clone(&self.config), self.endpoint).await
    }
}

// ---------------------------------------------------------------------------
// Tool: dashboard_refresh
// ---------------------------------------------------------------------------

pub struct DashboardRefresh {
    config: Arc<GatewayConfig>,
}

#[async_trait]
impl RustTool for DashboardRefresh {
    fn name(&self) -> &str {
        "dashboard_refresh"
    }

    fn description(&self) -> &str {
        "Trigger an immediate dashboard composer run (bypasses the 2 AM schedule). \
         Regenerates the Homepage YAML config from current module state."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let cfg = Arc::clone(&self.config);
        let command = cfg.composer_cmd.clone();
        // 60s timeout to match the Python composer invocation.
        let stdout = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command, 60))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

        // Match Python: {'status': 'triggered', 'output': last 300 chars}.
        let chars: Vec<char> = stdout.chars().collect();
        let tail: String = if chars.len() > 300 {
            chars[chars.len() - 300..].iter().collect()
        } else {
            stdout
        };
        let value = json!({ "status": "triggered", "output": tail });
        serde_json::to_string_pretty(&value)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all gateway tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(GatewayConfig::from_env());

    let endpoint_tools: Vec<(&'static str, &'static str, &'static str)> = vec![
        (
            "dashboard_status",
            "Get health of all Lumina dashboard gateway endpoints.",
            "/api/health",
        ),
        (
            "dashboard_calendar",
            "Get today's calendar events from the Lumina gateway.",
            "/api/calendar",
        ),
        (
            "dashboard_tasks",
            "Get urgent/high priority Plane tasks from the Lumina gateway.",
            "/api/tasks",
        ),
        (
            "dashboard_insights",
            "Get current rotating insights shown on the Lumina dashboard.",
            "/api/insights",
        ),
        (
            "dashboard_inbox",
            "Get Nexus inbox pending message count from the Lumina gateway.",
            "/api/inbox",
        ),
    ];

    for (tool_name, desc, endpoint) in endpoint_tools {
        let tool = Box::new(DashboardEndpointTool {
            config: Arc::clone(&config),
            tool_name,
            desc,
            endpoint,
        });
        if let Err(e) = registry.register(tool) {
            error!("gateway: failed to register tool {tool_name}: {e}");
        }
    }

    if let Err(e) = registry.register(Box::new(DashboardRefresh {
        config: Arc::clone(&config),
    })) {
        error!("gateway: failed to register dashboard_refresh: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tests (no network / no SSH)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<GatewayConfig> {
        Arc::new(GatewayConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            gateway_url: DEFAULT_GATEWAY_URL.into(),
            api_key: None,
            composer_cmd: DEFAULT_COMPOSER_CMD.into(),
        })
    }

    // --- Command building -------------------------------------------------

    #[test]
    fn test_build_curl_command_shape() {
        let cmd = build_curl_command("http://localhost:8080", "/api/health", "secret123").unwrap();
        assert_eq!(
            cmd,
            "curl -s -H 'x-api-key: secret123' http://localhost:8080/api/health"
        );
    }

    #[test]
    fn test_build_curl_command_each_endpoint() {
        for ep in &[
            "/api/health",
            "/api/calendar",
            "/api/tasks",
            "/api/insights",
            "/api/inbox",
        ] {
            let cmd = build_curl_command("http://localhost:8080", ep, "k").unwrap();
            assert!(cmd.contains(ep), "command should contain endpoint {ep}");
            assert!(cmd.starts_with("curl -s -H 'x-api-key: k'"));
        }
    }

    #[test]
    fn test_build_curl_command_rejects_quote_in_key() {
        let result = build_curl_command("http://localhost:8080", "/api/health", "ev'il");
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("single quote")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn test_build_curl_respects_custom_gateway_url() {
        let cmd = build_curl_command("http://<internal-ip>:9000", "/api/tasks", "abc").unwrap();
        assert!(cmd.contains("http://<internal-ip>:9000/api/tasks"));
    }

    // --- Response parsing -------------------------------------------------

    #[test]
    fn test_parse_gateway_response_valid_json() {
        let v = parse_gateway_response("{\"status\":\"ok\",\"count\":3}");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["count"], 3);
    }

    #[test]
    fn test_parse_gateway_response_invalid_json() {
        let v = parse_gateway_response("not json at all <<<");
        assert!(v["error"].as_str().unwrap().starts_with("invalid JSON:"));
    }

    #[test]
    fn test_parse_gateway_response_invalid_json_truncates_to_100() {
        let long = "x".repeat(500);
        let v = parse_gateway_response(&long);
        let err = v["error"].as_str().unwrap();
        // "invalid JSON: " prefix + at most 100 chars of preview.
        let preview = err.strip_prefix("invalid JSON: ").unwrap();
        assert_eq!(preview.chars().count(), 100);
    }

    #[test]
    fn test_parse_gateway_response_trims_whitespace() {
        let v = parse_gateway_response("  \n {\"ok\":true}\n  ");
        assert_eq!(v["ok"], true);
    }

    // --- Config -----------------------------------------------------------

    #[test]
    fn test_config_defaults() {
        let cfg = GatewayConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            gateway_url: DEFAULT_GATEWAY_URL.into(),
            api_key: None,
            composer_cmd: DEFAULT_COMPOSER_CMD.into(),
        };
        assert_eq!(cfg.gateway_url, "http://localhost:8080");
        assert!(cfg.composer_cmd.contains("composer.py"));
        assert_eq!(cfg.ssh_user, "root");
    }

    #[test]
    fn test_require_api_key_not_configured() {
        let cfg = test_config();
        let result = cfg.require_api_key();
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::NotConfigured(msg) => assert!(msg.contains("DASHBOARD_API_KEY")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn test_require_host_not_configured() {
        let cfg = test_config();
        match cfg.require_host().unwrap_err() {
            ToolError::NotConfigured(msg) => assert!(msg.contains("GATEWAY_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn test_require_key_not_configured() {
        let cfg = test_config();
        match cfg.require_key().unwrap_err() {
            ToolError::NotConfigured(msg) => assert!(msg.contains("GATEWAY_SSH_KEY_PATH")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- Tool execute: NotConfigured paths (no network reached) -----------

    #[tokio::test]
    async fn test_endpoint_tool_not_configured_without_api_key() {
        // api_key is None -> NotConfigured before any SSH attempt.
        let tool = DashboardEndpointTool {
            config: test_config(),
            tool_name: "dashboard_status",
            desc: "x",
            endpoint: "/api/health",
        };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::NotConfigured(msg) => assert!(msg.contains("DASHBOARD_API_KEY")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_endpoint_tool_not_configured_without_ssh_host() {
        // api_key present, but no SSH host -> NotConfigured from ssh_exec.
        let cfg = Arc::new(GatewayConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/key".into()),
            gateway_url: DEFAULT_GATEWAY_URL.into(),
            api_key: Some("k".into()),
            composer_cmd: DEFAULT_COMPOSER_CMD.into(),
        });
        let tool = DashboardEndpointTool {
            config: cfg,
            tool_name: "dashboard_status",
            desc: "x",
            endpoint: "/api/health",
        };
        match tool.execute(json!({})).await.unwrap_err() {
            ToolError::NotConfigured(msg) => assert!(msg.contains("GATEWAY_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_refresh_not_configured_without_ssh_host() {
        let tool = DashboardRefresh {
            config: test_config(),
        };
        match tool.execute(json!({})).await.unwrap_err() {
            ToolError::NotConfigured(msg) => assert!(msg.contains("GATEWAY_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- Infra-leak genericization ----------------------------------------

    #[test]
    fn test_ssh_exec_unreachable_error_is_generic() {
        // 127.0.0.1:22 — fails fast at connect/handshake/auth; no real key.
        let cfg = GatewayConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            gateway_url: DEFAULT_GATEWAY_URL.into(),
            api_key: Some("k".into()),
            composer_cmd: DEFAULT_COMPOSER_CMD.into(),
        };
        let msg = match ssh_exec(&cfg, "true", 2).expect_err("unroutable host must error") {
            ToolError::Execution(m) => m,
            other => panic!("expected Execution error, got {other:?}"),
        };
        let lower = msg.to_lowercase();
        assert!(!lower.contains("ssh"), "leaked 'ssh': {msg}");
        assert!(!lower.contains("route to host"), "leaked route detail: {msg}");
        assert!(!msg.contains(":22"), "leaked port 22: {msg}");
        assert!(!msg.contains("127.0.0.1"), "leaked target IP: {msg}");
        assert!(
            lower.contains("unreachable") || lower.contains("could not"),
            "expected generic wording, got: {msg}"
        );
    }

    // --- Tool metadata ----------------------------------------------------

    #[test]
    fn test_endpoint_tool_params_empty() {
        let tool = DashboardEndpointTool {
            config: test_config(),
            tool_name: "dashboard_status",
            desc: "x",
            endpoint: "/api/health",
        };
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["required"].as_array().unwrap().is_empty());
    }

    // --- Registration -----------------------------------------------------

    #[test]
    fn test_register_adds_six_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 6, "gateway must register exactly 6 tools");
    }

    #[test]
    fn test_register_all_tool_names_present() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        for name in &[
            "dashboard_status",
            "dashboard_calendar",
            "dashboard_tasks",
            "dashboard_insights",
            "dashboard_inbox",
            "dashboard_refresh",
        ] {
            assert!(registry.contains(name), "registry should contain '{name}'");
        }
    }
}
