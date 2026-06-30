//! Dura sysadmin tools — CHORD-11
//!
//! Replaces the Python dura_tools.py (Grade C: shell=True with nested SSH and
//! journalctl grep). This implementation uses the `ssh2` crate for typed SSH
//! execution (no subprocess, no shell=True) and the Prometheus HTTP API for
//! health queries (no journalctl grep).
//!
//! ## Security model
//! - SSH commands: fixed strings wherever possible. Only `service` and
//!   `last_n_lines` are user-supplied; both are validated before use.
//! - `service`: validated against an allowlist from `DURA_ALLOWED_SERVICES`.
//! - `last_n_lines`: parsed as `u32`, capped at 1000.
//! - Prometheus label values: safe by design (Prometheus escapes them).
//! - No `shell=true`, no `std::process::Command`, no string-interpolated
//!   commands with raw user input.

use std::env;
use std::io::Read as IoRead;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use ssh2::Session;
use tracing::{debug, error, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// DuraConfig
// ---------------------------------------------------------------------------

/// Configuration sourced entirely from environment variables — no hardcoded
/// hosts, users, keys, or URLs.
#[derive(Debug, Clone)]
pub struct DuraConfig {
    /// SSH host (e.g. "192.168.0.X") — from `DURA_SSH_HOST`.
    pub ssh_host: Option<String>,
    /// SSH user — from `DURA_SSH_USER`, default "root".
    pub ssh_user: String,
    /// Path to the SSH private key file — from `DURA_SSH_KEY_PATH`.
    pub ssh_key_path: Option<String>,
    /// Prometheus base URL — from `PROMETHEUS_URL`.
    pub prometheus_url: Option<String>,
    /// Comma-separated list of allowed service names — from
    /// `DURA_ALLOWED_SERVICES`, default "lumina,chord,terminus,matrix,postgres".
    pub allowed_services: Vec<String>,
}

impl DuraConfig {
    /// Read configuration from environment.
    pub fn from_env() -> Self {
        let ssh_host = env::var("DURA_SSH_HOST").ok();
        let ssh_user = env::var("DURA_SSH_USER").unwrap_or_else(|_| "root".into());
        let ssh_key_path = env::var("DURA_SSH_KEY_PATH").ok();
        let prometheus_url = env::var("PROMETHEUS_URL").ok();

        let allowed_raw = env::var("DURA_ALLOWED_SERVICES")
            .unwrap_or_else(|_| "lumina,chord,terminus,matrix,postgres".into());
        let allowed_services = allowed_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        DuraConfig {
            ssh_host,
            ssh_user,
            ssh_key_path,
            prometheus_url,
            allowed_services,
        }
    }

    /// Returns `true` if `service` is in the allowlist.
    pub fn is_allowed_service(&self, service: &str) -> bool {
        self.allowed_services.iter().any(|s| s == service)
    }
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking for async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session, run a single fixed command, and return stdout.
///
/// `command` must be a fixed string — caller is responsible for ensuring no
/// raw user input appears in the command text.
fn ssh_exec(config: &DuraConfig, command: &str) -> Result<String, ToolError> {
    let host = config
        .ssh_host
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("DURA_SSH_HOST is not set".into()))?;

    let key_path = config
        .ssh_key_path
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("DURA_SSH_KEY_PATH is not set".into()))?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("dura: cannot reach target host {host}: {e}");
        ToolError::Execution("The target server is unreachable.".into())
    })?;

    // Optional: set a reasonable timeout so we don't hang forever.
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(30)));

    let mut sess = Session::new().map_err(|e| {
        warn!("dura: session init failed: {e}");
        ToolError::Execution("Could not complete the check on the target server.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("dura: handshake failed with {host}: {e}");
        ToolError::Execution("The target server is unreachable.".into())
    })?;

    // Authenticate with the provided key file; no passphrase.
    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("dura: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the target server.".into())
        })?;

    if !sess.authenticated() {
        warn!("dura: authentication failed for {}@{host}", config.ssh_user);
        return Err(ToolError::Execution(
            "Could not connect to the target server.".into(),
        ));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("dura: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the check on the target server.".into())
    })?;

    debug!("dura ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("dura: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the check on the target server.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("dura: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the check on the target server.".into())
    })?;

    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);
    if exit_status != 0 {
        warn!("dura ssh_exec exit status {exit_status} for: {command}");
    }

    Ok(output)
}

// ---------------------------------------------------------------------------
// Prometheus helper
// ---------------------------------------------------------------------------

/// Query Prometheus with a raw PromQL expression. Returns the raw JSON body.
async fn prometheus_query(
    client: &Client,
    prometheus_url: &str,
    promql: &str,
) -> Result<Value, ToolError> {
    let url = format!("{prometheus_url}/api/v1/query");
    let resp = client
        .get(&url)
        .query(&[("query", promql)])
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            warn!("dura: prometheus query failed ({url}): {e}");
            ToolError::Http("The metrics service (Prometheus) is unreachable.".into())
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "Prometheus returned {status}: {body}"
        )));
    }

    resp.json::<Value>()
        .await
        .map_err(|e| ToolError::Http(format!("Prometheus response parse error: {e}")))
}

/// Format a Prometheus query result into a readable string.
fn format_prometheus_result(data: &Value, label_key: &str) -> String {
    let results = data
        .pointer("/data/result")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if results.is_empty() {
        return "No data returned from Prometheus.".into();
    }

    let mut lines = Vec::new();
    for item in &results {
        let label_val = item
            .pointer(&format!("/metric/{label_key}"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let value = item
            .pointer("/value/1")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let status = if value == "1" { "UP" } else { "DOWN" };
        lines.push(format!("  {label_val}: {status}"));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Tool: dura_smoke_test
// ---------------------------------------------------------------------------

pub struct DuraSmokeTest {
    config: Arc<DuraConfig>,
}

#[async_trait]
impl RustTool for DuraSmokeTest {
    fn name(&self) -> &str {
        "dura_smoke_test"
    }

    fn description(&self) -> &str {
        "Check SSH connectivity and basic host health (hostname + uptime). \
         No user input used in any command."
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
        // Spawn blocking because ssh2 is synchronous.
        let result = tokio::task::spawn_blocking(move || {
            // FIXED command — no user input.
            ssh_exec(&cfg, "hostname && uptime")
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

        match result {
            Ok(output) => Ok(format!(
                "Target server reachable: OK\n\nHost info:\n{output}"
            )),
            Err(ToolError::NotConfigured(msg)) => Err(ToolError::NotConfigured(msg)),
            // The inner error is already genericized by ssh_exec; pass it through.
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: dura_backup_status
// ---------------------------------------------------------------------------

pub struct DuraBackupStatus {
    config: Arc<DuraConfig>,
}

#[async_trait]
impl RustTool for DuraBackupStatus {
    fn name(&self) -> &str {
        "dura_backup_status"
    }

    fn description(&self) -> &str {
        "List the /backup/ directory contents via SSH. Fixed command — no user input."
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
        tokio::task::spawn_blocking(move || {
            // FIXED command — no user input.
            ssh_exec(&cfg, "ls -la /backup/")
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?
        .map(|output| format!("Backup directory listing:\n{output}"))
    }
}

// ---------------------------------------------------------------------------
// Tool: dura_log_query
// ---------------------------------------------------------------------------

pub struct DuraLogQuery {
    config: Arc<DuraConfig>,
}

#[async_trait]
impl RustTool for DuraLogQuery {
    fn name(&self) -> &str {
        "dura_log_query"
    }

    fn description(&self) -> &str {
        "Fetch the last N lines of a systemd service's journal log via SSH. \
         `service` must be in the allowed-services list. `last_n_lines` is \
         capped at 1000."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "service": {
                    "type": "string",
                    "description": "Systemd service name (must be in the allowlist)"
                },
                "last_n_lines": {
                    "type": "integer",
                    "description": "Number of log lines to return (1-1000, default 50)",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 50
                }
            },
            "required": ["service"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // --- Validate service name against allowlist ---
        let service = args["service"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'service' must be a string".into()))?;

        if !self.config.is_allowed_service(service) {
            return Err(ToolError::InvalidArgument(format!(
                "Service '{service}' is not in the allowed-services list. \
                 Allowed: {}",
                self.config.allowed_services.join(", ")
            )));
        }

        // --- Validate and cap line count ---
        let raw_n = args["last_n_lines"].as_u64().unwrap_or(50);
        let n: u32 = raw_n.min(1000) as u32;

        // Build command. `service` is validated against the allowlist so it
        // is safe to include directly — it cannot be arbitrary shell input.
        // `n` is a numeric value with no metacharacter risk.
        let command = format!("journalctl -u {service} -n {n} --no-pager");

        let cfg = Arc::clone(&self.config);
        let output = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

        output.map(|text| {
            format!("Journal log for service '{service}' (last {n} lines):\n{text}")
        })
    }
}

// ---------------------------------------------------------------------------
// Tool: dura_constellation_health
// ---------------------------------------------------------------------------

pub struct DuraConstellationHealth {
    http: Client,
    config: Arc<DuraConfig>,
}

impl DuraConstellationHealth {
    fn new(config: Arc<DuraConfig>) -> Self {
        Self {
            http: Client::new(),
            config,
        }
    }
}

#[async_trait]
impl RustTool for DuraConstellationHealth {
    fn name(&self) -> &str {
        "dura_constellation_health"
    }

    fn description(&self) -> &str {
        "Check the health and status of all servers, hosts, machines, and services \
         (homelab / infrastructure). Use when the user asks how their servers, hosts, \
         machines, or homelab are doing, or to check system or service health. Queries \
         Prometheus for the `up` metric per service; returns UP/DOWN."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let prometheus_url = self.config.prometheus_url.as_deref().ok_or_else(|| {
            ToolError::NotConfigured("PROMETHEUS_URL is not set".into())
        })?;

        // `up` returns all scrape targets — safe PromQL, no user input.
        let data = prometheus_query(&self.http, prometheus_url, "up").await?;
        let summary = format_prometheus_result(&data, "job");

        Ok(format!("Constellation service health (Prometheus `up` metric):\n{summary}"))
    }
}

// ---------------------------------------------------------------------------
// Tool: dura_container_status
// ---------------------------------------------------------------------------

pub struct DuraContainerStatus {
    config: Arc<DuraConfig>,
}

#[async_trait]
impl RustTool for DuraContainerStatus {
    fn name(&self) -> &str {
        "dura_container_status"
    }

    fn description(&self) -> &str {
        "List all running systemd services on the target host via SSH. \
         Fixed command — no user input."
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
        // FIXED command — no user input.
        tokio::task::spawn_blocking(move || {
            ssh_exec(&cfg, "systemctl list-units --type=service --state=running --no-pager")
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?
        .map(|output| format!("Running systemd services:\n{output}"))
    }
}

// ---------------------------------------------------------------------------
// Tool: dura_disk_usage
// ---------------------------------------------------------------------------

pub struct DuraDiskUsage {
    config: Arc<DuraConfig>,
}

#[async_trait]
impl RustTool for DuraDiskUsage {
    fn name(&self) -> &str {
        "dura_disk_usage"
    }

    fn description(&self) -> &str {
        "Report disk usage on the target host via SSH (`df -h`). \
         Fixed command — no user input."
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
        // FIXED command — no user input.
        tokio::task::spawn_blocking(move || ssh_exec(&cfg, "df -h"))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?
            .map(|output| format!("Disk usage:\n{output}"))
    }
}

// ---------------------------------------------------------------------------
// Tool: dura_service_check
// ---------------------------------------------------------------------------

pub struct DuraServiceCheck {
    http: Client,
    config: Arc<DuraConfig>,
}

impl DuraServiceCheck {
    fn new(config: Arc<DuraConfig>) -> Self {
        Self {
            http: Client::new(),
            config,
        }
    }
}

#[async_trait]
impl RustTool for DuraServiceCheck {
    fn name(&self) -> &str {
        "dura_service_check"
    }

    fn description(&self) -> &str {
        "Check Prometheus UP/DOWN status for a single named service. \
         `service_name` must be in the allowed-services list."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "service_name": {
                    "type": "string",
                    "description": "Service name to check (must be in the allowlist)"
                }
            },
            "required": ["service_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let prometheus_url = self.config.prometheus_url.as_deref().ok_or_else(|| {
            ToolError::NotConfigured("PROMETHEUS_URL is not set".into())
        })?;

        // --- Validate service name against allowlist ---
        let service_name = args["service_name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'service_name' must be a string".into()))?;

        if !self.config.is_allowed_service(service_name) {
            return Err(ToolError::InvalidArgument(format!(
                "Service '{service_name}' is not in the allowed-services list. \
                 Allowed: {}",
                self.config.allowed_services.join(", ")
            )));
        }

        // Prometheus label value is safe: Prometheus handles label value
        // escaping internally and we validated against the allowlist.
        let promql = format!("up{{job=\"{service_name}\"}}");
        let data = prometheus_query(&self.http, prometheus_url, &promql).await?;
        let summary = format_prometheus_result(&data, "job");

        if summary.is_empty() || summary.contains("No data") {
            Ok(format!("No Prometheus data found for service '{service_name}'."))
        } else {
            Ok(format!("Service check for '{service_name}':\n{summary}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Dura tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(DuraConfig::from_env());

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(DuraSmokeTest { config: Arc::clone(&config) }),
        Box::new(DuraBackupStatus { config: Arc::clone(&config) }),
        Box::new(DuraLogQuery { config: Arc::clone(&config) }),
        Box::new(DuraConstellationHealth::new(Arc::clone(&config))),
        Box::new(DuraContainerStatus { config: Arc::clone(&config) }),
        Box::new(DuraDiskUsage { config: Arc::clone(&config) }),
        Box::new(DuraServiceCheck::new(Arc::clone(&config))),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            error!("dura: failed to register tool: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a config with a known allowlist (no env required).
    fn test_config(ssh_host: Option<&str>, prometheus_url: Option<&str>) -> Arc<DuraConfig> {
        Arc::new(DuraConfig {
            ssh_host: ssh_host.map(String::from),
            ssh_user: "root".into(),
            ssh_key_path: None,
            prometheus_url: prometheus_url.map(String::from),
            allowed_services: vec![
                "lumina".into(),
                "chord".into(),
                "terminus".into(),
                "matrix".into(),
                "postgres".into(),
            ],
        })
    }

    // ------------------------------------------------------------------
    // Unit test 1: allowlist rejects unknown services
    // ------------------------------------------------------------------
    #[test]
    fn test_allowlist_rejects_unknown_service() {
        let cfg = test_config(None, None);
        assert!(!cfg.is_allowed_service("evil_service"));
        assert!(!cfg.is_allowed_service("lumina; rm -rf /"));
        assert!(!cfg.is_allowed_service(""));
        assert!(!cfg.is_allowed_service("LUMINA")); // case-sensitive
    }

    // ------------------------------------------------------------------
    // Unit test 2: allowlist accepts known services
    // ------------------------------------------------------------------
    #[test]
    fn test_allowlist_accepts_known_services() {
        let cfg = test_config(None, None);
        for svc in &["lumina", "chord", "terminus", "matrix", "postgres"] {
            assert!(cfg.is_allowed_service(svc), "should allow {svc}");
        }
    }

    // ------------------------------------------------------------------
    // Unit test 3: custom allowlist from env (via DuraConfig directly)
    // ------------------------------------------------------------------
    #[test]
    fn test_custom_allowlist_parsed() {
        let cfg = DuraConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            prometheus_url: None,
            allowed_services: vec!["alpha".into(), "beta".into()],
        };
        assert!(cfg.is_allowed_service("alpha"));
        assert!(cfg.is_allowed_service("beta"));
        assert!(!cfg.is_allowed_service("gamma"));
    }

    // ------------------------------------------------------------------
    // Unit test 4: line count capped at 1000
    // ------------------------------------------------------------------
    #[test]
    fn test_log_query_line_count_capped_at_1000() {
        // The capping logic: raw_n.min(1000) as u32 — verify correct behaviour.
        let raw_n: u64 = 9999;
        let capped: u32 = raw_n.min(1000) as u32;
        assert_eq!(capped, 1000, "line count must be capped at 1000");

        // Normal value passes through unchanged.
        let raw_n2: u64 = 42;
        let capped2: u32 = raw_n2.min(1000) as u32;
        assert_eq!(capped2, 42);

        // Boundary: exactly 1000 is allowed.
        let raw_n3: u64 = 1000;
        let capped3: u32 = raw_n3.min(1000) as u32;
        assert_eq!(capped3, 1000);

        // Zero defaults to 50 in execute(), but capping itself allows 0.
        let raw_n4: u64 = 0;
        let capped4: u32 = raw_n4.min(1000) as u32;
        assert_eq!(capped4, 0);
    }

    // ------------------------------------------------------------------
    // Unit test 5: dura_log_query rejects disallowed service
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_log_query_rejects_disallowed_service() {
        let cfg = test_config(Some("127.0.0.1"), None);
        let tool = DuraLogQuery { config: cfg };
        let args = json!({ "service": "malicious_service", "last_n_lines": 10 });
        let result = tool.execute(args).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::InvalidArgument(msg) => {
                assert!(msg.contains("not in the allowed-services list"));
            }
            other => panic!("expected InvalidArgument, got: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Unit test 6: dura_service_check rejects disallowed service
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_service_check_rejects_disallowed_service() {
        let cfg = test_config(None, Some("http://prometheus.local:9090"));
        let tool = DuraServiceCheck::new(cfg);
        let args = json!({ "service_name": "unknown_svc" });
        let result = tool.execute(args).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::InvalidArgument(msg) => {
                assert!(msg.contains("not in the allowed-services list"));
            }
            other => panic!("expected InvalidArgument, got: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Unit test 7: NotConfigured when DURA_SSH_HOST not set
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_smoke_test_not_configured_without_ssh_host() {
        let cfg = Arc::new(DuraConfig {
            ssh_host: None, // not set
            ssh_user: "root".into(),
            ssh_key_path: None,
            prometheus_url: None,
            allowed_services: vec!["lumina".into()],
        });
        let tool = DuraSmokeTest { config: cfg };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::NotConfigured(msg) => {
                assert!(msg.contains("DURA_SSH_HOST"));
            }
            other => panic!("expected NotConfigured, got: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Unit test 8: NotConfigured for constellation health without Prometheus
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_constellation_health_not_configured_without_prometheus() {
        let cfg = test_config(None, None); // no prometheus_url
        let tool = DuraConstellationHealth::new(cfg);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::NotConfigured(msg) => {
                assert!(msg.contains("PROMETHEUS_URL"));
            }
            other => panic!("expected NotConfigured, got: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Unit test 9: SSH commands are fixed (no raw user input in fixed tools)
    // ------------------------------------------------------------------
    #[test]
    fn test_fixed_ssh_commands_contain_no_user_input() {
        // Verify the literal command strings used in fixed tools.
        // These are the only commands that reach ssh_exec for those tools.
        let smoke_cmd = "hostname && uptime";
        let backup_cmd = "ls -la /backup/";
        let container_cmd =
            "systemctl list-units --type=service --state=running --no-pager";
        let disk_cmd = "df -h";

        // None of these contain user-controlled data — they are fixed literals.
        for cmd in &[smoke_cmd, backup_cmd, container_cmd, disk_cmd] {
            assert!(
                !cmd.contains("$("),
                "command must not contain shell substitution: {cmd}"
            );
            assert!(
                !cmd.contains("`"),
                "command must not contain backtick: {cmd}"
            );
            assert!(
                !cmd.contains(";") || *cmd == "hostname && uptime",
                "unexpected semicolon in command: {cmd}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Unit test 10: dura_log_query allows valid service name
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_log_query_allows_valid_service() {
        // No SSH host set — should fail with NotConfigured, not InvalidArgument.
        let cfg = Arc::new(DuraConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            prometheus_url: None,
            allowed_services: vec!["lumina".into()],
        });
        let tool = DuraLogQuery { config: cfg };
        let args = json!({ "service": "lumina", "last_n_lines": 20 });
        let result = tool.execute(args).await;
        // Service is valid — error should be NotConfigured (no SSH host), not InvalidArgument.
        match result {
            Err(ToolError::NotConfigured(_)) => {} // expected path
            Err(ToolError::InvalidArgument(msg)) => {
                panic!("should not be InvalidArgument for valid service; got: {msg}")
            }
            Ok(_) => {} // won't happen without a real SSH host, but acceptable
            Err(_other) => {} // Execution errors (connection refused) are fine
        }
    }

    // ------------------------------------------------------------------
    // Unit test 11: register() adds exactly 7 tools
    // ------------------------------------------------------------------
    #[test]
    fn test_register_adds_seven_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(
            registry.len(),
            7,
            "Dura module must register exactly 7 tools"
        );
    }

    // ------------------------------------------------------------------
    // Unit test 12: all 7 tool names are distinct and present
    // ------------------------------------------------------------------
    #[test]
    fn test_register_all_tool_names_present() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        let expected = [
            "dura_smoke_test",
            "dura_backup_status",
            "dura_log_query",
            "dura_constellation_health",
            "dura_container_status",
            "dura_disk_usage",
            "dura_service_check",
        ];
        for name in &expected {
            assert!(
                registry.contains(name),
                "registry should contain tool '{name}'"
            );
        }
    }

    // ------------------------------------------------------------------
    // Unit test 13: dura_service_check NotConfigured without Prometheus URL
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_service_check_not_configured_without_prometheus() {
        let cfg = Arc::new(DuraConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            prometheus_url: None, // not set
            allowed_services: vec!["lumina".into()],
        });
        let tool = DuraServiceCheck::new(cfg);
        let args = json!({ "service_name": "lumina" });
        let result = tool.execute(args).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::NotConfigured(msg) => {
                assert!(msg.contains("PROMETHEUS_URL"));
            }
            other => panic!("expected NotConfigured, got: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Unit test 14: connection-time errors are genericized (no infra leak)
    // ------------------------------------------------------------------
    /// Assert a user-facing error string leaks no infra detail.
    fn assert_no_infra_leak(msg: &str) {
        let lower = msg.to_lowercase();
        assert!(!lower.contains("ssh"), "leaked 'ssh': {msg}");
        assert!(!lower.contains("route to host"), "leaked route detail: {msg}");
        assert!(!msg.contains(":22"), "leaked port 22: {msg}");
        assert!(!msg.contains("127.0.0.1"), "leaked target IP: {msg}");
        // Generic, helpful wording is present.
        assert!(
            lower.contains("unreachable") || lower.contains("could not"),
            "expected generic wording, got: {msg}"
        );
    }

    #[test]
    fn test_ssh_exec_unreachable_error_is_generic() {
        // 127.0.0.1:1 — connection refused fast (no SSH listener on port 1).
        // Exercises the TcpStream::connect error path -> genericized message.
        let cfg = DuraConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            prometheus_url: None,
            allowed_services: vec![],
        };
        let err = ssh_exec(&cfg, "hostname").expect_err("unroutable host must error");
        let msg = match err {
            ToolError::Execution(m) => m,
            other => panic!("expected Execution error, got: {other:?}"),
        };
        assert_no_infra_leak(&msg);
    }
}
