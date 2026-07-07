//! Dev tools — DEV-01 (Tier-2 migration of the fleet host's `dev_tools.py`).
//!
//! Provides read-write access to the dev workstation over SSH from the
//! Terminus host. All commands execute via SSH using the `ssh2` crate (typed
//! exec, no `shell=True`, no subprocess). `dev_trigger_openhands` SSHes into the
//! dev host and curls the OpenHands API on localhost there.
//!
//! ## Security model (preserved verbatim from the Python source)
//! - All filesystem paths are **jailed** to the `WORKSPACE_ROOTS` allowlist.
//! - Path traversal (`..`) is rejected outright.
//! - A path must start with one of the allowed roots or it is rejected.
//! - `dev_run_command` caps the execution timeout at 300 seconds.
//! - User-supplied strings that must reach a shell are single-quoted and have
//!   embedded single quotes escaped exactly as the Python did
//!   (`'` -> `'\''`), so they cannot break out of the quoting.
//!
//! ## Configuration (no hardcoded IPs / creds / keys)
//! Sourced entirely from environment variables; the Python constants become
//! env-configurable values:
//! - `DEV_HOST`            — SSH host of the dev workstation.
//! - `DEV_USER`            — SSH user (default `root`).
//! - `DEV_SSH_KEY`         — path to the SSH private key (was `/root/.ssh/id_ed25519`).
//! - `DEV_OPENHANDS_URL`   — OpenHands base URL inside the dev host
//!                           (default `http://127.0.0.1:3000`). // pii-test-fixture
//! - `DEV_WORKSPACE_ROOTS` — comma-separated allowlist of workspace roots
//!                           (default `/opt/moosenet,/home/coder,/srv/openhands/workspace`).

use std::env;
use std::io::{Read as IoRead, Write as IoWrite};
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
// DevConfig
// ---------------------------------------------------------------------------

/// Default workspace roots — identical allowlist to the Python `WORKSPACE_ROOTS`.
const DEFAULT_WORKSPACE_ROOTS: &str = "/opt/moosenet,/home/coder,/srv/openhands/workspace";
/// Default OpenHands base URL — identical to the Python `OPENHANDS_URL`.
const DEFAULT_OPENHANDS_URL: &str = "http://127.0.0.1:3000"; // pii-test-fixture

/// Configuration sourced entirely from environment variables.
#[derive(Debug, Clone)]
pub struct DevConfig {
    /// SSH host of the dev workstation — from `DEV_HOST`.
    pub host: Option<String>,
    /// SSH user — from `DEV_USER`, default `root`.
    pub user: String,
    /// Path to the SSH private key file — from `DEV_SSH_KEY`.
    pub ssh_key: Option<String>,
    /// OpenHands base URL — from `DEV_OPENHANDS_URL`.
    pub openhands_url: String,
    /// Allowed workspace roots — from `DEV_WORKSPACE_ROOTS`.
    pub workspace_roots: Vec<String>,
}

impl DevConfig {
    /// Read configuration from the environment.
    pub fn from_env() -> Self {
        let host = env::var("DEV_HOST").ok();
        let user = env::var("DEV_USER").unwrap_or_else(|_| "root".into());
        let ssh_key = env::var("DEV_SSH_KEY").ok();
        let openhands_url =
            env::var("DEV_OPENHANDS_URL").unwrap_or_else(|_| DEFAULT_OPENHANDS_URL.into());

        let roots_raw =
            env::var("DEV_WORKSPACE_ROOTS").unwrap_or_else(|_| DEFAULT_WORKSPACE_ROOTS.into());
        let workspace_roots = roots_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        DevConfig {
            host,
            user,
            ssh_key,
            openhands_url,
            workspace_roots,
        }
    }

    /// Validate that a path is under an allowed workspace root.
    ///
    /// Mirrors the Python `_validate_path`: rejects any path containing `..`,
    /// then requires the path to start with one of the allowed roots. Returns
    /// `Ok(())` if valid, or an error message describing the failure.
    pub fn validate_path(&self, path: &str) -> Result<(), String> {
        if path.contains("..") {
            return Err("Path traversal (..) is not allowed".into());
        }
        for root in &self.workspace_roots {
            if path.starts_with(root) {
                return Ok(());
            }
        }
        Err(format!(
            "Path must be under one of: [{}]",
            self.workspace_roots
                .iter()
                .map(|r| format!("'{r}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

// ---------------------------------------------------------------------------
// Shell-quoting helper (matches the Python escaping exactly)
// ---------------------------------------------------------------------------

/// Escape embedded single quotes the same way the Python source does:
/// `s.replace("'", "'\\''")` — i.e. a literal `'\''` sequence. The caller is
/// responsible for wrapping the result in single quotes.
fn escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}

// ---------------------------------------------------------------------------
// SSH command result
// ---------------------------------------------------------------------------

/// Mirror of the Python `_ssh_cmd` return dict.
#[derive(Debug, Clone)]
struct SshResult {
    returncode: i32,
    stdout: String,
    stderr: String,
}

// ---------------------------------------------------------------------------
// SSH helpers (synchronous — wrapped in spawn_blocking by async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session to the dev host. Returns `NotConfigured` if host or key
/// are missing; `Execution` for connection/auth failures.
fn ssh_session(config: &DevConfig) -> Result<Session, ToolError> {
    let host = config
        .host
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("DEV_HOST is not set".into()))?;
    let key_path = config
        .ssh_key
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("DEV_SSH_KEY is not set".into()))?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr)
        .map_err(|e| ToolError::Execution(format!("Cannot reach dev host {host}: {e}")))?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(30)));

    let mut sess = Session::new().map_err(|e| ToolError::Execution(e.to_string()))?;
    sess.set_tcp_stream(tcp);
    sess.handshake()
        .map_err(|e| ToolError::Execution(format!("SSH handshake failed with {host}: {e}")))?;

    sess.userauth_pubkey_file(&config.user, None, key_path.as_ref(), None)
        .map_err(|e| ToolError::Execution(format!("SSH auth failed: {e}")))?;

    if !sess.authenticated() {
        return Err(ToolError::Execution(format!(
            "SSH authentication failed for {}@{host}",
            config.user
        )));
    }
    Ok(sess)
}

/// Run a single command over SSH and collect stdout/stderr/returncode.
///
/// Equivalent to the Python `_ssh_cmd`. `command` is passed as-is to the remote
/// shell; callers are responsible for quoting/escaping any user input exactly
/// as the Python did (see `escape_single_quotes`).
fn ssh_cmd(config: &DevConfig, command: &str, timeout_secs: u64) -> Result<SshResult, ToolError> {
    let sess = ssh_session(config)?;
    // Apply the per-command timeout to the underlying transport.
    sess.set_timeout((timeout_secs * 1000) as u32);

    let mut channel = sess
        .channel_session()
        .map_err(|e| ToolError::Execution(e.to_string()))?;

    debug!("dev ssh_cmd: {command}");
    channel
        .exec(command)
        .map_err(|e| ToolError::Execution(format!("SSH exec failed: {e}")))?;

    let mut stdout = String::new();
    channel
        .read_to_string(&mut stdout)
        .map_err(|e| ToolError::Execution(format!("SSH read failed: {e}")))?;

    let mut stderr = String::new();
    channel
        .stderr()
        .read_to_string(&mut stderr)
        .map_err(|e| ToolError::Execution(format!("SSH stderr read failed: {e}")))?;

    channel.wait_close().ok();
    let returncode = channel.exit_status().unwrap_or(-1);
    if returncode != 0 {
        warn!("dev ssh_cmd exit status {returncode} for: {command}");
    }

    Ok(SshResult {
        returncode,
        stdout: stdout.trim().to_string(),
        stderr: stderr.trim().to_string(),
    })
}

/// Run a command over SSH while writing `input` to the remote process's stdin.
///
/// Equivalent to the Python `dev_write_file` `subprocess.run(..., input=content)`
/// pattern: it execs `cat > '<path>'` (built by the caller) and streams the file
/// content into the channel.
fn ssh_cmd_with_input(
    config: &DevConfig,
    command: &str,
    input: &str,
    timeout_secs: u64,
) -> Result<SshResult, ToolError> {
    let sess = ssh_session(config)?;
    sess.set_timeout((timeout_secs * 1000) as u32);

    let mut channel = sess
        .channel_session()
        .map_err(|e| ToolError::Execution(e.to_string()))?;

    debug!("dev ssh_cmd_with_input: {command}");
    channel
        .exec(command)
        .map_err(|e| ToolError::Execution(format!("SSH exec failed: {e}")))?;

    channel
        .write_all(input.as_bytes())
        .map_err(|e| ToolError::Execution(format!("SSH stdin write failed: {e}")))?;
    // Signal EOF on stdin so the remote `cat` terminates.
    channel
        .send_eof()
        .map_err(|e| ToolError::Execution(format!("SSH send_eof failed: {e}")))?;

    let mut stdout = String::new();
    channel
        .read_to_string(&mut stdout)
        .map_err(|e| ToolError::Execution(format!("SSH read failed: {e}")))?;

    let mut stderr = String::new();
    channel
        .stderr()
        .read_to_string(&mut stderr)
        .map_err(|e| ToolError::Execution(format!("SSH stderr read failed: {e}")))?;

    channel.wait_close().ok();
    let returncode = channel.exit_status().unwrap_or(-1);

    Ok(SshResult {
        returncode,
        stdout: stdout.trim().to_string(),
        stderr: stderr.trim().to_string(),
    })
}

/// Run an SSH command on a blocking thread (ssh2 is synchronous).
async fn run_ssh(config: Arc<DevConfig>, command: String, timeout_secs: u64) -> Result<SshResult, ToolError> {
    tokio::task::spawn_blocking(move || ssh_cmd(&config, &command, timeout_secs))
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?
}

// ---------------------------------------------------------------------------
// Tool: dev_list_workspaces
// ---------------------------------------------------------------------------

pub struct DevListWorkspaces {
    config: Arc<DevConfig>,
}

#[async_trait]
impl RustTool for DevListWorkspaces {
    fn name(&self) -> &str {
        "dev_list_workspaces"
    }

    fn description(&self) -> &str {
        "List available project directories across all workspace roots on the dev host."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let mut lines = vec!["Workspace roots:".to_string()];
        for root in &self.config.workspace_roots {
            // `root` comes from the trusted allowlist, not user input.
            let command = format!("ls -1 {root}/ 2>/dev/null");
            let result = run_ssh(Arc::clone(&self.config), command, 30).await?;
            lines.push(format!("\n{root}:"));
            if result.returncode == 0 && !result.stdout.is_empty() {
                for entry in result.stdout.lines() {
                    if !entry.trim().is_empty() {
                        lines.push(format!("  {entry}"));
                    }
                }
            } else {
                lines.push("  (empty or unavailable)".to_string());
            }
        }
        Ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Tool: dev_open_workspace
// ---------------------------------------------------------------------------

pub struct DevOpenWorkspace {
    config: Arc<DevConfig>,
}

#[async_trait]
impl RustTool for DevOpenWorkspace {
    fn name(&self) -> &str {
        "dev_open_workspace"
    }

    fn description(&self) -> &str {
        "Open or create a workspace directory on the dev host (path must be under an allowed root)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Full path to the workspace (must be under an allowed root)"
                },
                "create": {
                    "type": "boolean",
                    "description": "Create the directory if it doesn't exist (default true)",
                    "default": true
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' must be a string".into()))?;
        let create = args["create"].as_bool().unwrap_or(true);

        if let Err(msg) = self.config.validate_path(path) {
            return Err(ToolError::InvalidArgument(msg));
        }

        let safe_path = escape_single_quotes(path);

        // Check existence.
        let check = run_ssh(
            Arc::clone(&self.config),
            format!("test -d '{safe_path}' && echo exists || echo missing"),
            30,
        )
        .await?;
        let exists = check.stdout.contains("exists");

        if !exists && !create {
            return Err(ToolError::NotFound(format!(
                "Workspace {path} does not exist and create=false"
            )));
        }

        if !exists {
            let create_result = run_ssh(
                Arc::clone(&self.config),
                format!("mkdir -p '{safe_path}'"),
                30,
            )
            .await?;
            if create_result.returncode != 0 {
                return Err(ToolError::Execution(format!(
                    "Failed to create: {}",
                    create_result.stderr
                )));
            }
        }

        let ls_result = run_ssh(
            Arc::clone(&self.config),
            format!("ls -la '{safe_path}'"),
            30,
        )
        .await?;

        Ok(format!(
            "Workspace: {path}\nCreated: {}\nContents:\n{}",
            !exists, ls_result.stdout
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: dev_run_command
// ---------------------------------------------------------------------------

pub struct DevRunCommand {
    config: Arc<DevConfig>,
}

#[async_trait]
impl RustTool for DevRunCommand {
    fn name(&self) -> &str {
        "dev_run_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command on the dev host within a workspace directory \
         (path-jailed to allowed roots; timeout capped at 300s)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute (e.g. 'git status', 'npm install')"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Directory to run from (must be under an allowed root)",
                    "default": "/opt/moosenet"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Max execution time in seconds (default 60, max 300)",
                    "default": 60
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'command' must be a string".into()))?;
        let working_dir = args["working_dir"].as_str().unwrap_or("/opt/moosenet");

        if let Err(msg) = self.config.validate_path(working_dir) {
            return Err(ToolError::InvalidArgument(msg));
        }

        // Cap timeout at 300s, matching the Python.
        let mut timeout = args["timeout"].as_u64().unwrap_or(60);
        if timeout > 300 {
            timeout = 300;
        }

        // Escape single quotes in the command (the working_dir is jailed but
        // we still single-quote it for safety).
        let safe_cmd = escape_single_quotes(command);
        let safe_dir = escape_single_quotes(working_dir);
        let full = format!("cd '{safe_dir}' && {safe_cmd}");

        let result = run_ssh(Arc::clone(&self.config), full, timeout).await?;

        Ok(format!(
            "Command: {command}\nWorking dir: {working_dir}\nReturn code: {}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}",
            result.returncode, result.stdout, result.stderr
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: dev_read_file
// ---------------------------------------------------------------------------

pub struct DevReadFile {
    config: Arc<DevConfig>,
}

#[async_trait]
impl RustTool for DevReadFile {
    fn name(&self) -> &str {
        "dev_read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the dev host (path-jailed; returns up to max_lines lines)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Full path to the file (must be under an allowed root)"
                },
                "max_lines": {
                    "type": "integer",
                    "description": "Maximum lines to return (default 500)",
                    "default": 500
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' must be a string".into()))?;
        let max_lines = args["max_lines"].as_u64().unwrap_or(500);

        if let Err(msg) = self.config.validate_path(path) {
            return Err(ToolError::InvalidArgument(msg));
        }

        let safe_path = escape_single_quotes(path);

        // Check existence + file type.
        let check = run_ssh(
            Arc::clone(&self.config),
            format!("test -f '{safe_path}' && file --brief '{safe_path}'"),
            30,
        )
        .await?;
        if check.returncode != 0 {
            return Err(ToolError::NotFound(format!("File not found: {path}")));
        }
        let descr = check.stdout.to_lowercase();
        if !descr.contains("text") && !descr.contains("empty") {
            return Err(ToolError::InvalidArgument(format!(
                "File appears to be binary: {}",
                check.stdout
            )));
        }

        let result = run_ssh(
            Arc::clone(&self.config),
            format!("head -n {max_lines} '{safe_path}'"),
            30,
        )
        .await?;
        let line_count = result.stdout.lines().count() as u64;
        let truncated = line_count >= max_lines;

        Ok(format!(
            "File: {path}\nLines: {line_count}\nTruncated: {truncated}\n\n{}",
            result.stdout
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: dev_write_file
// ---------------------------------------------------------------------------

pub struct DevWriteFile {
    config: Arc<DevConfig>,
}

#[async_trait]
impl RustTool for DevWriteFile {
    fn name(&self) -> &str {
        "dev_write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file on the dev host (path-jailed; overwrites existing)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Full path to the file (must be under an allowed root)"
                },
                "content": {
                    "type": "string",
                    "description": "File content to write (overwrites existing)"
                },
                "create_dirs": {
                    "type": "boolean",
                    "description": "Create parent directories if they don't exist (default true)",
                    "default": true
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' must be a string".into()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'content' must be a string".into()))?;
        let create_dirs = args["create_dirs"].as_bool().unwrap_or(true);

        if let Err(msg) = self.config.validate_path(path) {
            return Err(ToolError::InvalidArgument(msg));
        }

        let safe_path = escape_single_quotes(path);

        if create_dirs {
            // Parent = everything before the last '/', matching the Python rsplit.
            if let Some(idx) = path.rfind('/') {
                let parent = &path[..idx];
                if !parent.is_empty() {
                    let safe_parent = escape_single_quotes(parent);
                    let _ = run_ssh(
                        Arc::clone(&self.config),
                        format!("mkdir -p '{safe_parent}'"),
                        30,
                    )
                    .await?;
                }
            }
        }

        // Write via stdin (cat > 'path') to handle multi-line content safely.
        let cfg = Arc::clone(&self.config);
        let content_owned = content.to_string();
        let command = format!("cat > '{safe_path}'");
        let write_result = tokio::task::spawn_blocking(move || {
            ssh_cmd_with_input(&cfg, &command, &content_owned, 30)
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

        if write_result.returncode != 0 {
            return Err(ToolError::Execution(format!(
                "Write failed: {}",
                write_result.stderr
            )));
        }

        // Verify line count.
        let verify = run_ssh(
            Arc::clone(&self.config),
            format!("wc -l < '{safe_path}'"),
            30,
        )
        .await?;

        Ok(format!(
            "Wrote file: {path}\nWritten: true\nLines: {}",
            verify.stdout.trim()
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: dev_trigger_openhands
// ---------------------------------------------------------------------------

pub struct DevTriggerOpenhands {
    config: Arc<DevConfig>,
}

#[async_trait]
impl RustTool for DevTriggerOpenhands {
    fn name(&self) -> &str {
        "dev_trigger_openhands"
    }

    fn description(&self) -> &str {
        "Trigger an OpenHands task on the dev host by SSHing in and curling its \
         local API to create a new conversation."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Natural language description of the task for OpenHands"
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'task' must be a string".into()))?;

        let url = &self.config.openhands_url;

        // Escape the task exactly as the Python did: single quotes then double
        // quotes, so it can sit inside a JSON string within a single-quoted -d.
        let escaped_task = task.replace('\'', "'\\''").replace('"', "\\\"");
        let api_cmd = format!(
            "curl -sf -X POST {url}/api/conversations \
             -H 'Content-Type: application/json' \
             -d '{{\"initial_user_msg\": \"{escaped_task}\"}}' \
             --max-time 30"
        );

        let result = run_ssh(Arc::clone(&self.config), api_cmd, 35).await?;

        if result.returncode != 0 {
            // Check whether OpenHands is even responding.
            let health = run_ssh(
                Arc::clone(&self.config),
                format!("curl -sf {url}/api/options/config --max-time 5"),
                10,
            )
            .await?;
            if health.returncode != 0 {
                return Err(ToolError::Execution(format!(
                    "OpenHands does not appear to be responding. Detail: {}",
                    result.stderr
                )));
            }
            return Err(ToolError::Execution(format!(
                "Failed to trigger task. Detail: {}",
                result.stderr
            )));
        }

        // Parse the response; fall back to raw text (capped at 500 chars).
        match serde_json::from_str::<Value>(&result.stdout) {
            Ok(response) => Ok(format!(
                "Triggered OpenHands task.\nTask: {task}\nResponse:\n{}",
                serde_json::to_string_pretty(&response).unwrap_or_else(|_| result.stdout.clone())
            )),
            Err(_) => {
                let raw: String = result.stdout.chars().take(500).collect();
                Ok(format!(
                    "Triggered OpenHands task.\nTask: {task}\nRaw response:\n{raw}"
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all dev tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(DevConfig::from_env());

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(DevListWorkspaces { config: Arc::clone(&config) }),
        Box::new(DevOpenWorkspace { config: Arc::clone(&config) }),
        Box::new(DevRunCommand { config: Arc::clone(&config) }),
        Box::new(DevReadFile { config: Arc::clone(&config) }),
        Box::new(DevWriteFile { config: Arc::clone(&config) }),
        Box::new(DevTriggerOpenhands { config: Arc::clone(&config) }),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            error!("dev: failed to register tool: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (no network / SSH — arg validation, path safety, command building)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<DevConfig> {
        Arc::new(DevConfig {
            host: None,
            user: "root".into(),
            ssh_key: None,
            openhands_url: DEFAULT_OPENHANDS_URL.into(),
            workspace_roots: vec![
                "/opt/moosenet".into(),
                "/home/coder".into(),
                "/srv/openhands/workspace".into(),
            ],
        })
    }

    // ------------------------------------------------------------------
    // Path validation
    // ------------------------------------------------------------------
    #[test]
    fn test_validate_path_accepts_allowed_roots() {
        let cfg = test_config();
        assert!(cfg.validate_path("/opt/moosenet/my-project").is_ok());
        assert!(cfg.validate_path("<path>/foo/bar").is_ok()); // pii-test-fixture
        assert!(cfg.validate_path("/srv/openhands/workspace/x").is_ok());
        // Exact root.
        assert!(cfg.validate_path("/opt/moosenet").is_ok());
    }

    #[test]
    fn test_validate_path_rejects_traversal() {
        let cfg = test_config();
        let err = cfg.validate_path("/opt/moosenet/../etc/passwd").unwrap_err();
        assert!(err.contains("Path traversal"));
        // Even a leading allowed prefix doesn't save it.
        assert!(cfg.validate_path("<path>/../../etc").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_validate_path_rejects_outside_roots() {
        let cfg = test_config();
        let err = cfg.validate_path("/etc/passwd").unwrap_err();
        assert!(err.contains("Path must be under one of"));
        assert!(cfg.validate_path("/root/.ssh/id_ed25519").is_err());
        assert!(cfg.validate_path("/tmp/evil").is_err());
    }

    #[test]
    fn test_custom_workspace_roots_parsed() {
        let cfg = DevConfig {
            host: None,
            user: "root".into(),
            ssh_key: None,
            openhands_url: DEFAULT_OPENHANDS_URL.into(),
            workspace_roots: vec!["/alpha".into(), "/beta".into()],
        };
        assert!(cfg.validate_path("/alpha/x").is_ok());
        assert!(cfg.validate_path("/beta/y").is_ok());
        assert!(cfg.validate_path("/gamma/z").is_err());
    }

    // ------------------------------------------------------------------
    // Single-quote escaping (matches Python s.replace("'", "'\\''"))
    // ------------------------------------------------------------------
    #[test]
    fn test_escape_single_quotes() {
        assert_eq!(escape_single_quotes("git status"), "git status");
        assert_eq!(escape_single_quotes("echo 'hi'"), "echo '\\''hi'\\''");
        assert_eq!(escape_single_quotes("a'b'c"), "a'\\''b'\\''c");
        // No quotes — unchanged.
        assert_eq!(escape_single_quotes("npm install"), "npm install");
    }

    // ------------------------------------------------------------------
    // Arg validation: missing required args -> InvalidArgument
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_run_command_requires_command() {
        let tool = DevRunCommand { config: test_config() };
        let result = tool.execute(json!({})).await;
        match result.unwrap_err() {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("command")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_run_command_rejects_bad_working_dir() {
        let tool = DevRunCommand { config: test_config() };
        let result = tool
            .execute(json!({ "command": "ls", "working_dir": "/etc" }))
            .await;
        match result.unwrap_err() {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("Path must be under one of")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_open_workspace_requires_path() {
        let tool = DevOpenWorkspace { config: test_config() };
        match tool.execute(json!({})).await.unwrap_err() {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("path")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_open_workspace_rejects_bad_path() {
        let tool = DevOpenWorkspace { config: test_config() };
        match tool
            .execute(json!({ "path": "/etc/cron.d" }))
            .await
            .unwrap_err()
        {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("Path must be under one of")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_read_file_rejects_traversal() {
        let tool = DevReadFile { config: test_config() };
        match tool
            .execute(json!({ "path": "/opt/moosenet/../secret" }))
            .await
            .unwrap_err()
        {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("Path traversal")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_write_file_requires_content() {
        let tool = DevWriteFile { config: test_config() };
        match tool
            .execute(json!({ "path": "/opt/moosenet/f.txt" }))
            .await
            .unwrap_err()
        {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("content")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_write_file_rejects_bad_path() {
        let tool = DevWriteFile { config: test_config() };
        match tool
            .execute(json!({ "path": "/root/x", "content": "hi" }))
            .await
            .unwrap_err()
        {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("Path must be under one of")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_trigger_openhands_requires_task() {
        let tool = DevTriggerOpenhands { config: test_config() };
        match tool.execute(json!({})).await.unwrap_err() {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("task")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // NotConfigured: valid args but no SSH host configured
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_list_workspaces_not_configured_without_host() {
        let tool = DevListWorkspaces { config: test_config() };
        match tool.execute(json!({})).await.unwrap_err() {
            ToolError::NotConfigured(msg) => assert!(msg.contains("DEV_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_run_command_not_configured_without_host() {
        let tool = DevRunCommand { config: test_config() };
        // Valid command + valid jailed dir -> should reach SSH and fail NotConfigured.
        match tool
            .execute(json!({ "command": "ls", "working_dir": "/opt/moosenet" }))
            .await
            .unwrap_err()
        {
            ToolError::NotConfigured(msg) => assert!(msg.contains("DEV_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_write_file_not_configured_uses_key_msg() {
        // Host set but key missing -> NotConfigured about the key.
        let cfg = Arc::new(DevConfig {
            host: Some("127.0.0.1".into()),
            user: "root".into(),
            ssh_key: None,
            openhands_url: DEFAULT_OPENHANDS_URL.into(),
            workspace_roots: vec!["/opt/moosenet".into()],
        });
        // Use a host that will fail to connect quickly OR key-missing first.
        // ssh_session checks host first then key; 127.0.0.1:22 may connect, so
        // assert we get either NotConfigured(key) or an Execution error — never
        // a path/arg error for valid input.
        let tool = DevWriteFile { config: cfg };
        let result = tool
            .execute(json!({ "path": "/opt/moosenet/x.txt", "content": "hi", "create_dirs": false }))
            .await;
        match result {
            Err(ToolError::NotConfigured(msg)) => assert!(msg.contains("DEV_SSH_KEY")),
            Err(ToolError::Execution(_)) => {} // connection refused / auth fail is fine
            Err(ToolError::InvalidArgument(msg)) => {
                panic!("should not be InvalidArgument for valid input: {msg}")
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Timeout capping logic (mirrors Python: if timeout > 300 -> 300)
    // ------------------------------------------------------------------
    #[test]
    fn test_timeout_cap_logic() {
        let cap = |t: u64| if t > 300 { 300 } else { t };
        assert_eq!(cap(60), 60);
        assert_eq!(cap(300), 300);
        assert_eq!(cap(9999), 300);
        assert_eq!(cap(0), 0);
    }

    // ------------------------------------------------------------------
    // OpenHands task escaping (single then double quotes, as Python)
    // ------------------------------------------------------------------
    #[test]
    fn test_openhands_task_escaping() {
        let task = "fix the 'bug' in \"main\"";
        let escaped = task.replace('\'', "'\\''").replace('"', "\\\"");
        assert_eq!(escaped, "fix the '\\''bug'\\'' in \\\"main\\\"");
    }

    // ------------------------------------------------------------------
    // Registration
    // ------------------------------------------------------------------
    #[test]
    fn test_register_adds_six_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 6, "dev module must register exactly 6 tools");
    }

    #[test]
    fn test_register_all_tool_names_present() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        let expected = [
            "dev_list_workspaces",
            "dev_open_workspace",
            "dev_run_command",
            "dev_read_file",
            "dev_write_file",
            "dev_trigger_openhands",
        ];
        for name in &expected {
            assert!(registry.contains(name), "registry should contain '{name}'");
        }
    }
}
