//! Sentinel tools — ported from the Python `sentinel_tools.py` on the fleet host.
//!
//! Sentinel triggers operational checks and logging on the fleet host and
//! refreshes the live MooseNet status page. The Python original shelled out
//! via `ssh ... '<cmd>'` with `subprocess.run(shell=True)`, and its
//! `sentinel_status` tool additionally SSHed to the fleet host to run an
//! inline <secret-manager>-auth + `curl` pipeline against the Gitea contents API. // pii-test-fixture
//!
//! This port:
//! - Uses the `ssh2` crate for typed SSH execution (no `shell=True`, no
//!   subprocess), mirroring `gateway/mod.rs` and `ansible/mod.rs`.
//! - Replaces the Python `sentinel_status` SSH+<secret-manager>+curl chain with a // pii-test-fixture
//!   direct call into this crate's own `gitea` module. Terminus already holds
//!   `GITEA_URL`/`GITEA_TOKEN` locally (see `gitea/mod.rs`) — the Python
//!   version only detoured through fleet-host because the *Python* MCP
//!   process didn't have those credentials. The Rust process does, so the
//!   extra SSH hop plus an inline shell script that logs in to <secret-manager> and // pii-test-fixture
//!   shells out to `curl`/`python3 -c` is unnecessary — and is exactly the
//!   kind of subprocess/shell chain the `RustTool` contract forbids. Same
//!   end result (latest check content from Gitea), simpler and testable.
//!
//! ## Tools (identical names to the Python source)
//!   sentinel_run            — run an operational check/logging task
//!   sentinel_status         — check status of a check (or list all)
//!   sentinel_refresh_status — force a status page regeneration
//!
//! ## Configuration (environment only — no hardcoded hosts/keys)
//!   SENTINEL_SSH_HOST      — SSH host of the fleet box (e.g. "192.168.0.X").
//!   SENTINEL_SSH_USER      — SSH user, default "root".
//!   SENTINEL_SSH_KEY_PATH  — path to the SSH private key file.
//!   SENTINEL_SCRIPT        — remote ops script, default mirrors the Python.
//!   SENTINEL_STATUS_GENERATOR_CMD — remote command for the status generator.
//!   SENTINEL_STATUS_PAGE_URL      — URL of the live status page (optional).
//!   SENTINEL_REPO                 — Gitea repo Sentinel writes to, default
//!                                    "lumina-sentinel".
//!
//! ## Security model
//! - `operation` is validated against the fixed `VALID_OPS` allowlist before
//!   it is ever placed into a remote command string.
//! - `args` (only meaningful for `commute-tracker`) is restricted to
//!   `[A-Za-z0-9_-]` — no shell metacharacters can reach the remote command.

use std::env;
use std::io::Read as IoRead;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use ssh2::Session;
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::gitea::GiteaClient;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Constants (identical to the Python source)
// ---------------------------------------------------------------------------

/// Operations that trigger a status page refresh after completion.
const STATUS_TRIGGERING_OPS: &[&str] = &["system-snapshot", "self-health", "plex-health"];

/// All valid Sentinel operations.
const VALID_OPS: &[&str] = &[
    "plex-health",
    "self-health",
    "vm901-watchdog",
    "gitea-health",
    "system-snapshot",
    "commute-tracker",
    "daily-log",
    "reflection",
    "tool-usage-log",
    "memory-curation",
];

/// Operations whose latest result lives under `checks/` rather than `logs/`.
const CHECK_CATEGORY_OPS: &[&str] = &[
    "plex-health",
    "self-health",
    "vm901-watchdog",
    "gitea-health",
    "system-snapshot",
    "commute-tracker",
];

const DEFAULT_REPO: &str = "lumina-sentinel";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration sourced entirely from environment variables.
#[derive(Debug, Clone)]
pub struct SentinelConfig {
    /// SSH host of the fleet box — from `SENTINEL_SSH_HOST`.
    pub ssh_host: Option<String>,
    /// SSH user — from `SENTINEL_SSH_USER`, default "root".
    pub ssh_user: String,
    /// Path to the SSH private key file — from `SENTINEL_SSH_KEY_PATH`.
    pub ssh_key_path: Option<String>,
    /// Remote ops script invocation — from `SENTINEL_SCRIPT`. No compiled-in
    /// default (PII remediation 2026-07): required at runtime, see
    /// [`SentinelConfig::require_script`].
    pub script: Option<String>,
    /// Remote command used by `sentinel_refresh_status` — from
    /// `SENTINEL_STATUS_GENERATOR_CMD`. No compiled-in default (PII
    /// remediation 2026-07): required at runtime, see
    /// [`SentinelConfig::require_status_generator_cmd`].
    pub status_generator_cmd: Option<String>,
    /// Live status page URL — from `SENTINEL_STATUS_PAGE_URL` (optional).
    pub status_page_url: Option<String>,
    /// Gitea repo Sentinel results live in — from `SENTINEL_REPO`.
    pub repo: String,
}

impl SentinelConfig {
    pub fn from_env() -> Self {
        SentinelConfig {
            ssh_host: env::var("SENTINEL_SSH_HOST").ok().filter(|s| !s.is_empty()),
            ssh_user: env::var("SENTINEL_SSH_USER").unwrap_or_else(|_| "root".into()),
            ssh_key_path: env::var("SENTINEL_SSH_KEY_PATH").ok().filter(|s| !s.is_empty()),
            script: env::var("SENTINEL_SCRIPT").ok().filter(|s| !s.is_empty()),
            status_generator_cmd: env::var("SENTINEL_STATUS_GENERATOR_CMD")
                .ok()
                .filter(|s| !s.is_empty()),
            status_page_url: env::var("SENTINEL_STATUS_PAGE_URL").ok().filter(|s| !s.is_empty()),
            repo: env::var("SENTINEL_REPO")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_REPO.into()),
        }
    }

    fn require_host(&self) -> Result<&str, ToolError> {
        self.ssh_host
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SENTINEL_SSH_HOST is not set".into()))
    }

    fn require_key(&self) -> Result<&str, ToolError> {
        self.ssh_key_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SENTINEL_SSH_KEY_PATH is not set".into()))
    }

    /// PII remediation (2026-07): `SENTINEL_SCRIPT` no longer has a
    /// compiled-in fleet-host script path fallback.
    fn require_script(&self) -> Result<&str, ToolError> {
        self.script
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SENTINEL_SCRIPT is not set".into()))
    }

    /// PII remediation (2026-07): `SENTINEL_STATUS_GENERATOR_CMD` no longer
    /// has a compiled-in fallback command (which embedded real fleet paths
    /// and secret env var names).
    fn require_status_generator_cmd(&self) -> Result<&str, ToolError> {
        self.status_generator_cmd.as_deref().ok_or_else(|| {
            ToolError::NotConfigured("SENTINEL_STATUS_GENERATOR_CMD is not set".into())
        })
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate `args` contains only characters safe to place in a remote shell
/// command (alphanumeric, `_`, `-`). Empty is always allowed.
fn validate_args(args: &str) -> Result<(), ToolError> {
    if args.is_empty() {
        return Ok(());
    }
    if args.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(
            "'args' may only contain letters, digits, '_' and '-'".into(),
        ))
    }
}

/// The Gitea sub-path category ("checks" or "logs") for a given operation.
fn category_for(operation: &str) -> &'static str {
    if CHECK_CATEGORY_OPS.contains(&operation) {
        "checks"
    } else {
        "logs"
    }
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking for async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session, run a single command, and return stdout. Mirrors
/// `gateway::ssh_exec` — generic, non-infra-leaking error messages.
fn ssh_exec(config: &SentinelConfig, command: &str, timeout_secs: u64) -> Result<String, ToolError> {
    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("sentinel: cannot reach fleet host {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new().map_err(|e| {
        warn!("sentinel: session init failed: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("sentinel: handshake failed with {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("sentinel: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the fleet server.".into())
        })?;

    if !sess.authenticated() {
        warn!("sentinel: authentication failed for {}@{host}", config.ssh_user);
        return Err(ToolError::Execution("Could not connect to the fleet server.".into()));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("sentinel: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    debug!("sentinel ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("sentinel: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("sentinel: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);
    if exit_status != 0 {
        warn!("sentinel ssh_exec exit status {exit_status} for: {command}");
        return Err(ToolError::Execution(format!(
            "Remote command exited with status {exit_status}"
        )));
    }

    Ok(output)
}

/// Fire the status page refresh in the background — mirrors the Python
/// `_trigger_status_page`, which backgrounds the command and ignores the
/// result. We spawn a blocking task and detach it (do not await).
fn trigger_status_page(config: Arc<SentinelConfig>) {
    let cmd = match config.require_status_generator_cmd() {
        Ok(cmd) => cmd.to_string(),
        Err(e) => {
            warn!("sentinel: background status refresh skipped: {e}");
            return;
        }
    };
    tokio::task::spawn_blocking(move || {
        if let Err(e) = ssh_exec(&config, &cmd, 10) {
            warn!("sentinel: background status refresh failed: {e}");
        }
    });
}

// ---------------------------------------------------------------------------
// Tool: sentinel_run
// ---------------------------------------------------------------------------

pub struct SentinelRun {
    config: Arc<SentinelConfig>,
}

#[async_trait]
impl RustTool for SentinelRun {
    fn name(&self) -> &str {
        "sentinel_run"
    }

    fn description(&self) -> &str {
        "Run an operational check or logging task via Sentinel on the fleet host. \
         Operations: plex-health, self-health, vm901-watchdog, gitea-health, \
         system-snapshot, commute-tracker, daily-log, reflection, tool-usage-log, \
         memory-curation. For commute-tracker, pass args='morning' or args='afternoon'. \
         After health checks (system-snapshot, self-health, plex-health) the live \
         status page is automatically refreshed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Operation to run",
                    "enum": VALID_OPS
                },
                "args": {
                    "type": "string",
                    "description": "Optional operation arguments (e.g. 'morning'/'afternoon' for commute-tracker)",
                    "default": ""
                }
            },
            "required": ["operation"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let operation = args["operation"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'operation' must be a string".into()))?;
        let op_args = args["args"].as_str().unwrap_or("");

        if !VALID_OPS.contains(&operation) {
            return Err(ToolError::InvalidArgument(format!(
                "Unknown operation: {operation}. Valid operations: {}",
                VALID_OPS.join(", ")
            )));
        }
        validate_args(op_args)?;

        let script = self.config.require_script()?;
        let mut command = format!("{script} {operation}");
        if !op_args.is_empty() {
            command.push(' ');
            command.push_str(op_args);
        }

        let cfg = Arc::clone(&self.config);
        let output = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command, 120))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

        let category = category_for(operation);
        let latest_path = format!("{category}/latest-{operation}.md");

        let mut response = json!({
            "status": "complete",
            "operation": operation,
            "output": output.trim(),
            "latest_path": latest_path,
            "repo": format!("moosenet/{}", self.config.repo),
        });

        if STATUS_TRIGGERING_OPS.contains(&operation) {
            trigger_status_page(Arc::clone(&self.config));
            if let Some(url) = &self.config.status_page_url {
                response["status_page"] = json!(url);
            }
            response["status_page_refreshed"] = json!(true);
        }

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: sentinel_status
// ---------------------------------------------------------------------------

pub struct SentinelStatus {
    config: Arc<SentinelConfig>,
}

#[async_trait]
impl RustTool for SentinelStatus {
    fn name(&self) -> &str {
        "sentinel_status"
    }

    fn description(&self) -> &str {
        "Check the status of operational checks. If operation is specified, returns \
         the latest result for that check from Gitea. If empty, returns a summary of \
         all available latest checks. Also returns the live status page URL."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Operation to check (optional — leave empty for a summary)",
                    "default": ""
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let operation = args["operation"].as_str().unwrap_or("");

        if operation.is_empty() {
            let mut response = json!({
                "message": "Specify an operation to check status",
                "valid_operations": VALID_OPS,
            });
            if let Some(url) = &self.config.status_page_url {
                response["status_page"] = json!(url);
            }
            return serde_json::to_string_pretty(&response)
                .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")));
        }

        if !VALID_OPS.contains(&operation) {
            return Err(ToolError::InvalidArgument(format!(
                "Unknown operation: {operation}. Valid operations: {}",
                VALID_OPS.join(", ")
            )));
        }

        let category = category_for(operation);
        let path = format!("{category}/latest-{operation}.md");

        let client = GiteaClient::from_env()?;
        let content = client.fetch_file_text(&self.config.repo, &path).await;

        let mut response = json!({ "operation": operation });
        if let Some(url) = &self.config.status_page_url {
            response["status_page"] = json!(url);
        }
        match content {
            Ok(text) => response["content"] = json!(text),
            Err(ToolError::NotFound(_)) => response["content"] = json!("No data found"),
            Err(e) => return Err(e),
        }

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: sentinel_refresh_status
// ---------------------------------------------------------------------------

pub struct SentinelRefreshStatus {
    config: Arc<SentinelConfig>,
}

#[async_trait]
impl RustTool for SentinelRefreshStatus {
    fn name(&self) -> &str {
        "sentinel_refresh_status"
    }

    fn description(&self) -> &str {
        "Force a refresh of the MooseNet live status page. Runs all health checks \
         and regenerates the HTML dashboard. Returns the status page URL and a \
         summary of service states."
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
        let cmd = cfg.require_status_generator_cmd()?.to_string();
        let result = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &cmd, 60))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

        let mut response = json!({});
        match result {
            Ok(output) => {
                response["status"] = json!("refreshed");
                response["output"] = json!(output.trim());
                response["error"] = json!("");
            }
            Err(e) => {
                response["status"] = json!("refreshed");
                response["output"] = json!("");
                response["error"] = json!(e.to_string());
            }
        }
        if let Some(url) = &self.config.status_page_url {
            response["status_page"] = json!(url);
        }

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Sentinel tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(SentinelConfig::from_env());

    let _ = registry.register(Box::new(SentinelRun { config: Arc::clone(&config) }));
    let _ = registry.register(Box::new(SentinelStatus { config: Arc::clone(&config) }));
    let _ = registry.register(Box::new(SentinelRefreshStatus { config }));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-fixture values standing in for `SENTINEL_SCRIPT` /
    /// `SENTINEL_STATUS_GENERATOR_CMD`, which have no compiled-in default
    /// (PII remediation 2026-07).
    const TEST_SCRIPT: &str = "/opt/test-fixture/sentinel/ops.py";
    const TEST_STATUS_GENERATOR_CMD: &str = "/opt/test-fixture/sentinel/status_generator.sh";

    fn test_config() -> Arc<SentinelConfig> {
        Arc::new(SentinelConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            script: Some(TEST_SCRIPT.into()),
            status_generator_cmd: Some(TEST_STATUS_GENERATOR_CMD.into()),
            status_page_url: None,
            repo: DEFAULT_REPO.into(),
        })
    }

    // --- validation ---------------------------------------------------

    #[test]
    fn test_validate_args_allows_empty() {
        assert!(validate_args("").is_ok());
    }

    #[test]
    fn test_validate_args_allows_alnum() {
        assert!(validate_args("morning").is_ok());
        assert!(validate_args("afternoon_2").is_ok());
        assert!(validate_args("v1-beta").is_ok());
    }

    #[test]
    fn test_validate_args_rejects_shell_metacharacters() {
        assert!(validate_args("morning; rm -rf /").is_err());
        assert!(validate_args("$(whoami)").is_err());
        assert!(validate_args("a && b").is_err());
    }

    #[test]
    fn test_category_for_checks_vs_logs() {
        assert_eq!(category_for("self-health"), "checks");
        assert_eq!(category_for("commute-tracker"), "checks");
        assert_eq!(category_for("daily-log"), "logs");
        assert_eq!(category_for("memory-curation"), "logs");
    }

    // --- tool metadata --------------------------------------------------

    #[test]
    fn test_sentinel_run_metadata() {
        let tool = SentinelRun { config: test_config() };
        assert_eq!(tool.name(), "sentinel_run");
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "operation"));
    }

    #[test]
    fn test_sentinel_status_metadata() {
        let tool = SentinelStatus { config: test_config() };
        assert_eq!(tool.name(), "sentinel_status");
    }

    #[test]
    fn test_sentinel_refresh_status_metadata() {
        let tool = SentinelRefreshStatus { config: test_config() };
        assert_eq!(tool.name(), "sentinel_refresh_status");
    }

    // --- execute: invalid arguments (no network needed) -----------------

    #[tokio::test]
    async fn test_sentinel_run_unknown_operation_rejected() {
        let tool = SentinelRun { config: test_config() };
        let err = tool
            .execute(json!({"operation": "delete-everything"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("Unknown operation"));
    }

    #[tokio::test]
    async fn test_sentinel_run_missing_operation_rejected() {
        let tool = SentinelRun { config: test_config() };
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_sentinel_run_bad_args_rejected() {
        let tool = SentinelRun { config: test_config() };
        let err = tool
            .execute(json!({"operation": "commute-tracker", "args": "morning; id"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_sentinel_run_not_configured_without_ssh_host() {
        let tool = SentinelRun { config: test_config() };
        let err = tool
            .execute(json!({"operation": "self-health"}))
            .await
            .unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SENTINEL_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_sentinel_status_unknown_operation_rejected() {
        let tool = SentinelStatus { config: test_config() };
        let err = tool
            .execute(json!({"operation": "not-a-real-op"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_sentinel_status_empty_operation_lists_valid_ops() {
        let tool = SentinelStatus { config: test_config() };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("valid_operations"));
    }

    #[tokio::test]
    async fn test_sentinel_run_not_configured_without_script() {
        // PII remediation (2026-07): SENTINEL_SCRIPT has no compiled-in
        // default — missing it must fail clean with NotConfigured.
        let cfg = Arc::new(SentinelConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            script: None,
            status_generator_cmd: Some(TEST_STATUS_GENERATOR_CMD.into()),
            status_page_url: None,
            repo: DEFAULT_REPO.into(),
        });
        let tool = SentinelRun { config: cfg };
        let err = tool
            .execute(json!({"operation": "self-health"}))
            .await
            .unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SENTINEL_SCRIPT")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_sentinel_refresh_status_not_configured_without_generator_cmd() {
        // PII remediation (2026-07): SENTINEL_STATUS_GENERATOR_CMD has no
        // compiled-in default — missing it must fail clean with NotConfigured.
        let cfg = Arc::new(SentinelConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            script: Some(TEST_SCRIPT.into()),
            status_generator_cmd: None,
            status_page_url: None,
            repo: DEFAULT_REPO.into(),
        });
        let tool = SentinelRefreshStatus { config: cfg };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SENTINEL_STATUS_GENERATOR_CMD")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- infra-leak genericization (mirrors gateway's test) --------------

    #[test]
    fn test_ssh_exec_unreachable_error_is_generic() {
        let cfg = SentinelConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            script: Some(TEST_SCRIPT.into()),
            status_generator_cmd: Some(TEST_STATUS_GENERATOR_CMD.into()),
            status_page_url: None,
            repo: DEFAULT_REPO.into(),
        };
        let msg = match ssh_exec(&cfg, "true", 2).expect_err("unroutable host must error") {
            ToolError::Execution(m) => m,
            other => panic!("expected Execution error, got {other:?}"),
        };
        let lower = msg.to_lowercase();
        assert!(!lower.contains("ssh"), "leaked 'ssh': {msg}");
        assert!(!msg.contains(":22"), "leaked port 22: {msg}");
        assert!(!msg.contains("127.0.0.1"), "leaked target IP: {msg}");
    }

    // --- registration -----------------------------------------------------

    #[test]
    fn test_register_adds_three_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 3);
        assert!(registry.contains("sentinel_run"));
        assert!(registry.contains("sentinel_status"));
        assert!(registry.contains("sentinel_refresh_status"));
    }
}
