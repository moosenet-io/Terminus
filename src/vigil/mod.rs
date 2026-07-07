//! Vigil tools — ported from the Python `vigil_tools.py` on the fleet host.
//!
//! Vigil generates morning/afternoon briefings on the fleet host and reports
//! whether the latest briefing is ready. The Python original shelled out via
//! `ssh ... '<cmd>'` with `subprocess.run(shell=True)`, and its `vigil_status`
//! tool additionally SSHed to the fleet host to run an inline <secret-manager>-auth // pii-test-fixture
//! + `curl` pipeline against the Gitea contents API just to check whether a
//! file exists.
//!
//! This port:
//! - Uses the `ssh2` crate for typed SSH execution (no `shell=True`, no
//!   subprocess), mirroring `sentinel/mod.rs`, `gateway/mod.rs`, and
//!   `ansible/mod.rs`.
//! - Replaces the Python `vigil_status` SSH+<secret-manager>+curl chain with a // pii-test-fixture
//!   direct call into this crate's own `gitea` module (see
//!   `sentinel/mod.rs` for the identical rationale: Terminus already holds
//!   `GITEA_URL`/`GITEA_TOKEN` locally, so the extra SSH hop plus an inline
//!   shell script is unnecessary and is exactly the kind of subprocess/shell
//!   chain the `RustTool` contract forbids).
//!
//! ## Tools (identical names to the Python source)
//!   vigil_generate — trigger briefing generation on the fleet host
//!   vigil_status   — check whether the latest briefing is available
//!
//! ## Configuration (environment only — no hardcoded hosts/keys)
//!   VIGIL_SSH_HOST     — SSH host of the fleet box (e.g. "192.168.0.X").
//!   VIGIL_SSH_USER     — SSH user, default "root".
//!   VIGIL_SSH_KEY_PATH — path to the SSH private key file.
//!   VIGIL_SCRIPT       — remote briefing script, default mirrors the Python.
//!   VIGIL_REPO         — Gitea repo Vigil writes to, default "lumina-vigil".
//!
//! ## Security model
//! - `briefing_type` is validated against `{"morning", "afternoon"}` before
//!   it is ever placed into a remote command string or a Gitea path.

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
// Constants
// ---------------------------------------------------------------------------

const VALID_BRIEFING_TYPES: &[&str] = &["morning", "afternoon"];
const DEFAULT_REPO: &str = "lumina-vigil";

fn validate_briefing_type(briefing_type: &str) -> Result<(), ToolError> {
    if VALID_BRIEFING_TYPES.contains(&briefing_type) {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "Invalid briefing type: {briefing_type}. Use 'morning' or 'afternoon'."
        )))
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration sourced entirely from environment variables.
#[derive(Debug, Clone)]
pub struct VigilConfig {
    /// SSH host of the fleet box — from `VIGIL_SSH_HOST`.
    pub ssh_host: Option<String>,
    /// SSH user — from `VIGIL_SSH_USER`, default "root".
    pub ssh_user: String,
    /// Path to the SSH private key file — from `VIGIL_SSH_KEY_PATH`.
    pub ssh_key_path: Option<String>,
    /// Remote briefing script invocation — from `VIGIL_SCRIPT`. No compiled-in
    /// default (PII remediation 2026-07): required at runtime, see
    /// [`VigilConfig::require_script`].
    pub script: Option<String>,
    /// Gitea repo Vigil briefings live in — from `VIGIL_REPO`.
    pub repo: String,
}

impl VigilConfig {
    pub fn from_env() -> Self {
        VigilConfig {
            ssh_host: env::var("VIGIL_SSH_HOST").ok().filter(|s| !s.is_empty()),
            ssh_user: env::var("VIGIL_SSH_USER").unwrap_or_else(|_| "root".into()),
            ssh_key_path: env::var("VIGIL_SSH_KEY_PATH").ok().filter(|s| !s.is_empty()),
            script: env::var("VIGIL_SCRIPT").ok().filter(|s| !s.is_empty()),
            repo: env::var("VIGIL_REPO")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_REPO.into()),
        }
    }

    fn require_host(&self) -> Result<&str, ToolError> {
        self.ssh_host
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("VIGIL_SSH_HOST is not set".into()))
    }

    fn require_key(&self) -> Result<&str, ToolError> {
        self.ssh_key_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("VIGIL_SSH_KEY_PATH is not set".into()))
    }

    /// PII remediation (2026-07): `VIGIL_SCRIPT` no longer has a compiled-in
    /// fleet-host script path fallback — it must be set, or this fails clean.
    fn require_script(&self) -> Result<&str, ToolError> {
        self.script
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("VIGIL_SCRIPT is not set".into()))
    }
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking for async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session, run a single command, and return stdout. Mirrors
/// `sentinel::ssh_exec` / `gateway::ssh_exec` — generic, non-infra-leaking
/// error messages.
fn ssh_exec(config: &VigilConfig, command: &str, timeout_secs: u64) -> Result<String, ToolError> {
    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("vigil: cannot reach fleet host {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new().map_err(|e| {
        warn!("vigil: session init failed: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("vigil: handshake failed with {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("vigil: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the fleet server.".into())
        })?;

    if !sess.authenticated() {
        warn!("vigil: authentication failed for {}@{host}", config.ssh_user);
        return Err(ToolError::Execution("Could not connect to the fleet server.".into()));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("vigil: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    debug!("vigil ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("vigil: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("vigil: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);
    if exit_status != 0 {
        warn!("vigil ssh_exec exit status {exit_status} for: {command}");
        return Err(ToolError::Execution(format!(
            "Remote command exited with status {exit_status}"
        )));
    }

    Ok(output)
}

// ---------------------------------------------------------------------------
// Tool: vigil_generate
// ---------------------------------------------------------------------------

pub struct VigilGenerate {
    config: Arc<VigilConfig>,
}

#[async_trait]
impl RustTool for VigilGenerate {
    fn name(&self) -> &str {
        "vigil_generate"
    }

    fn description(&self) -> &str {
        "Generate a briefing by triggering Vigil on the fleet host. briefing_type: \
         'morning' or 'afternoon'. Gathers live data (news, weather, commute, crypto, \
         sports), formats it, and writes to the Gitea lumina-vigil repo. Returns the \
         Gitea path to the finished briefing. Takes ~30-60 seconds to complete."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "briefing_type": {
                    "type": "string",
                    "description": "Which briefing to generate",
                    "enum": VALID_BRIEFING_TYPES,
                    "default": "morning"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let briefing_type = args["briefing_type"].as_str().unwrap_or("morning");
        validate_briefing_type(briefing_type)?;

        let script = self.config.require_script()?;
        let command = format!("{script} {briefing_type}");
        let cfg = Arc::clone(&self.config);
        let output = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command, 120))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

        let latest_path = format!("briefings/latest-{briefing_type}.md");
        let response = json!({
            "status": "ready",
            "briefing_type": briefing_type,
            "latest_path": latest_path,
            "repo": format!("moosenet/{}", self.config.repo),
            "message": format!(
                "Briefing is ready. Read it from Gitea: moosenet/{}/{}",
                self.config.repo, latest_path
            ),
            "output": output.trim(),
        });

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: vigil_status
// ---------------------------------------------------------------------------

pub struct VigilStatus {
    config: Arc<VigilConfig>,
}

#[async_trait]
impl RustTool for VigilStatus {
    fn name(&self) -> &str {
        "vigil_status"
    }

    fn description(&self) -> &str {
        "Check if the latest briefing is available on Gitea. Returns the file path \
         and size if found. Use this for light polling instead of regenerating."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "briefing_type": {
                    "type": "string",
                    "description": "Which briefing to check",
                    "enum": VALID_BRIEFING_TYPES,
                    "default": "morning"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let briefing_type = args["briefing_type"].as_str().unwrap_or("morning");
        validate_briefing_type(briefing_type)?;

        let path = format!("briefings/latest-{briefing_type}.md");
        let client = GiteaClient::from_env()?;

        match client.fetch_file_text(&self.config.repo, &path).await {
            Ok(text) => {
                // Mirrors the Python source's `file_info`, which also reports the
                // first 8 chars of the file's SHA alongside size/exists.
                let sha8: String = match client.get_file_sha(&self.config.repo, &path).await {
                    Ok(sha) => sha.chars().take(8).collect(),
                    Err(_) => String::new(),
                };
                let response = json!({
                    "status": "ready",
                    "briefing_type": briefing_type,
                    "latest_path": path,
                    "repo": format!("moosenet/{}", self.config.repo),
                    "file_info": { "exists": true, "size": text.len(), "sha": sha8 },
                });
                serde_json::to_string_pretty(&response)
                    .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
            }
            Err(ToolError::NotFound(_)) => {
                let response = json!({
                    "status": "not_found",
                    "briefing_type": briefing_type,
                    "message": "No briefing found. Run vigil_generate first.",
                });
                serde_json::to_string_pretty(&response)
                    .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
            }
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Vigil tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(VigilConfig::from_env());

    let _ = registry.register(Box::new(VigilGenerate { config: Arc::clone(&config) }));
    let _ = registry.register(Box::new(VigilStatus { config }));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Test-fixture script path — stands in for the real `VIGIL_SCRIPT` value,
    /// which has no compiled-in default (PII remediation 2026-07).
    const TEST_SCRIPT: &str = "/opt/test-fixture/vigil/briefing.py";

    fn test_config() -> Arc<VigilConfig> {
        Arc::new(VigilConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            script: Some(TEST_SCRIPT.into()),
            repo: DEFAULT_REPO.into(),
        })
    }

    // --- validation ---------------------------------------------------

    #[test]
    fn test_validate_briefing_type_accepts_morning_and_afternoon() {
        assert!(validate_briefing_type("morning").is_ok());
        assert!(validate_briefing_type("afternoon").is_ok());
    }

    #[test]
    fn test_validate_briefing_type_rejects_other_values() {
        let err = validate_briefing_type("evening").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("morning"));
    }

    #[test]
    fn test_validate_briefing_type_rejects_injection_attempt() {
        assert!(validate_briefing_type("morning; rm -rf /").is_err());
    }

    // --- tool metadata --------------------------------------------------

    #[test]
    fn test_vigil_generate_metadata() {
        let tool = VigilGenerate { config: test_config() };
        assert_eq!(tool.name(), "vigil_generate");
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
    }

    #[test]
    fn test_vigil_status_metadata() {
        let tool = VigilStatus { config: test_config() };
        assert_eq!(tool.name(), "vigil_status");
    }

    // --- execute: invalid arguments (no network needed) -----------------

    #[tokio::test]
    async fn test_vigil_generate_invalid_type_rejected() {
        let tool = VigilGenerate { config: test_config() };
        let err = tool
            .execute(json!({"briefing_type": "midday"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_vigil_status_invalid_type_rejected() {
        let tool = VigilStatus { config: test_config() };
        let err = tool
            .execute(json!({"briefing_type": "midday"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // --- vigil_status: happy path against a mocked Gitea (env-configured) ---

    #[tokio::test]
    #[serial]
    async fn test_vigil_status_ready_includes_truncated_sha() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine};
        use httpmock::prelude::*;

        let server = MockServer::start();
        let content = B64.encode("briefing body");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/moosenet/lumina-vigil/contents/briefings/latest-morning.md");
            then.status(200).json_body(json!({
                "type": "file",
                "encoding": "base64",
                "size": 13,
                "name": "latest-morning.md",
                "path": "briefings/latest-morning.md",
                "content": content,
                "sha": "0123456789abcdef", // pii-test-fixture
                "url": "http://example.com",
                "html_url": "http://example.com"
            }));
        });

        std::env::set_var("GITEA_URL", server.base_url());
        std::env::set_var("GITEA_TOKEN", "test-token");
        std::env::set_var("GITEA_OWNER", "moosenet");

        let tool = VigilStatus { config: test_config() };
        let result = tool.execute(json!({"briefing_type": "morning"})).await.unwrap();

        std::env::remove_var("GITEA_URL");
        std::env::remove_var("GITEA_TOKEN");
        std::env::remove_var("GITEA_OWNER");

        mock.assert_hits(2); // fetch_file_text + get_file_sha
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["status"], "ready");
        assert_eq!(v["file_info"]["exists"], true);
        assert_eq!(v["file_info"]["size"], 13);
        assert_eq!(v["file_info"]["sha"], "01234567");
    }

    #[tokio::test]
    async fn test_vigil_generate_defaults_to_morning() {
        // No SSH host configured -> NotConfigured, but only *after* validation passes,
        // proving the default briefing_type ("morning") was accepted.
        let tool = VigilGenerate { config: test_config() };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("VIGIL_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_vigil_generate_not_configured_without_ssh_host() {
        let tool = VigilGenerate { config: test_config() };
        let err = tool
            .execute(json!({"briefing_type": "afternoon"}))
            .await
            .unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("VIGIL_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_vigil_generate_not_configured_without_script() {
        // PII remediation (2026-07): VIGIL_SCRIPT has no compiled-in default —
        // missing it must fail clean with NotConfigured, not panic or run a
        // guessed command.
        let cfg = Arc::new(VigilConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            script: None,
            repo: DEFAULT_REPO.into(),
        });
        let tool = VigilGenerate { config: cfg };
        let err = tool
            .execute(json!({"briefing_type": "morning"}))
            .await
            .unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("VIGIL_SCRIPT")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- infra-leak genericization ----------------------------------------

    #[test]
    fn test_ssh_exec_unreachable_error_is_generic() {
        let cfg = VigilConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            script: Some(TEST_SCRIPT.into()),
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
    fn test_register_adds_two_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 2);
        assert!(registry.contains("vigil_generate"));
        assert!(registry.contains("vigil_status"));
    }
}
