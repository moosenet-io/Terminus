//! Ansible tools — ANS-01 (Tier-2 migration)
//!
//! Ported from the Python `ansible_tools.py` on <host>. The Python version shelled
//! out to `ssh` via `subprocess`. This implementation uses the `ssh2` crate for
//! typed SSH execution (no subprocess, no shell=True), mirroring `dura/mod.rs`.
//!
//! ## Tools (identical names + params to the Python source)
//! - `ansible_run_playbook(playbook_name)` — run an allowlisted playbook on the
//!   ansible host via `ansible-playbook`.
//! - `ansible_list_playbooks()` — list `*.yml` playbooks on the host with their
//!   allowlist status.
//! - `ansible_last_run_status()` — return the result of the last run this process
//!   performed (in-memory; resets on restart).
//! - `ansible_view_run_log()` — return stdout/stderr of the last run.
//!
//! ## Security model
//! - Every tool is GUARDED: each `execute()` begins with the shared approval gate
//!   (`crate::approval::gate`). The real action only runs on `Gate::Granted`.
//! - `ansible_run_playbook` enforces the SAME playbook allowlist as the Python
//!   source — only allowlisted names may be run, and the name is validated against
//!   the allowlist before it is ever interpolated into a command. A non-allowlisted
//!   name returns the same refusal shape the Python returns (`allowed: false`).
//! - All host/user/key/path values come from environment variables — no hardcoded
//!   IPs, users, or credentials.

use std::env;
use std::io::Read as IoRead;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use ssh2::Session;
use tracing::{debug, error, warn};

use crate::approval::{gate, Gate};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Allowlist (identical to the Python PLAYBOOK_ALLOWLIST)
// ---------------------------------------------------------------------------

/// The default allowlist — byte-for-byte the same set the Python source ships.
/// Overridable via `ANSIBLE_PLAYBOOK_ALLOWLIST` (comma-separated) for ops, but the
/// default exactly matches the Python so behaviour is identical out of the box.
const DEFAULT_ALLOWLIST: &[&str] = &[
    "deploy-<host>-db",
    "deploy-<host>-ct",
    "ping",
    "deploy-<secret-manager>",
    "install-node-exporter",
    "generate-prometheus-targets",
    "rotate-<matrix-server>-token",
    "deploy-plane",
    "restart-<host>",
];

// ---------------------------------------------------------------------------
// AnsibleConfig
// ---------------------------------------------------------------------------

/// Configuration sourced entirely from environment variables — no hardcoded
/// hosts, users, keys, or paths.
#[derive(Debug, Clone)]
pub struct AnsibleConfig {
    /// SSH host of the ansible control node — from `ANSIBLE_HOST`.
    pub host: Option<String>,
    /// SSH user — from `ANSIBLE_USER`, default "root".
    pub user: String,
    /// Path to the SSH private key file — from `ANSIBLE_SSH_KEY`.
    pub ssh_key: Option<String>,
    /// Directory holding the playbooks on the host — from `ANSIBLE_PLAYBOOK_ROOT`.
    pub playbook_root: Option<String>,
    /// Inventory file path on the host — from `ANSIBLE_INVENTORY_PATH`.
    pub inventory_path: Option<String>,
    /// Names of allowlisted playbooks. Defaults to [`DEFAULT_ALLOWLIST`]; can be
    /// overridden via `ANSIBLE_PLAYBOOK_ALLOWLIST` (comma-separated).
    pub allowlist: Vec<String>,
}

impl AnsibleConfig {
    /// Read configuration from the environment.
    pub fn from_env() -> Self {
        let host = env::var("ANSIBLE_HOST").ok();
        let user = env::var("ANSIBLE_USER").unwrap_or_else(|_| "root".into());
        let ssh_key = env::var("ANSIBLE_SSH_KEY").ok();
        let playbook_root = env::var("ANSIBLE_PLAYBOOK_ROOT").ok();
        let inventory_path = env::var("ANSIBLE_INVENTORY_PATH").ok();

        let allowlist = match env::var("ANSIBLE_PLAYBOOK_ALLOWLIST") {
            Ok(raw) if !raw.trim().is_empty() => raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            _ => DEFAULT_ALLOWLIST.iter().map(|s| s.to_string()).collect(),
        };

        AnsibleConfig {
            host,
            user,
            ssh_key,
            playbook_root,
            inventory_path,
            allowlist,
        }
    }

    /// Returns `true` if `name` is an allowlisted playbook.
    pub fn is_allowed(&self, name: &str) -> bool {
        self.allowlist.iter().any(|p| p == name)
    }
}

// ---------------------------------------------------------------------------
// Last-run state (in-memory, like the Python _last_run dict)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct LastRun {
    playbook: Option<String>,
    returncode: i32,
    stdout: String,
    stderr: String,
    timestamp: String,
}

/// ISO-8601 UTC timestamp to seconds with a trailing `Z`, matching the Python
/// `datetime.utcnow().replace(microsecond=0).isoformat() + "Z"`. Implemented with
/// the standard library only (no chrono dependency added).
fn utc_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days/civil-time conversion (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, m, d, hh, mm, ss
    )
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking for async callers)
// ---------------------------------------------------------------------------

/// Result of an SSH command, mirroring the Python `_ssh_cmd` return shape.
struct SshResult {
    returncode: i32,
    stdout: String,
    stderr: String,
}

/// Open an SSH session, run a single command, and return its stdout/stderr/exit.
///
/// `command` is built by the caller from validated/fixed inputs only — no raw
/// user-supplied text reaches this function.
fn ssh_exec(
    config: &AnsibleConfig,
    command: &str,
    timeout_secs: u64,
) -> Result<SshResult, ToolError> {
    let host = config
        .host
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("ANSIBLE_HOST is not set".into()))?;
    let key_path = config
        .ssh_key
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("ANSIBLE_SSH_KEY is not set".into()))?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr)
        .map_err(|e| ToolError::Execution(format!("Cannot reach ansible host {host}: {e}")))?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

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

    let mut channel = sess
        .channel_session()
        .map_err(|e| ToolError::Execution(e.to_string()))?;

    debug!("ansible ssh_exec: {command}");
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
        warn!("ansible ssh_exec exit status {returncode} for: {command}");
    }

    Ok(SshResult {
        returncode,
        stdout: stdout.trim().to_string(),
        stderr: stderr.trim().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Tool: ansible_run_playbook
// ---------------------------------------------------------------------------

pub struct AnsibleRunPlaybook {
    config: Arc<AnsibleConfig>,
    last_run: Arc<Mutex<LastRun>>,
}

#[async_trait]
impl RustTool for AnsibleRunPlaybook {
    fn name(&self) -> &str {
        "ansible_run_playbook"
    }

    fn description(&self) -> &str {
        "Run an allowlisted Ansible playbook on the ansible control host via SSH. \
         GUARDED: requires operator approval. 'playbook_name' must be on the allowlist."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "playbook_name": {
                    "type": "string",
                    "description": "Playbook name without .yml extension (e.g. \"ping\"). Must be on the allowlist."
                }
            },
            "required": ["playbook_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // --- Validate the argument first so we can build a precise summary ---
        let playbook_name = args["playbook_name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'playbook_name' must be a string".into()))?
            .to_string();

        // --- APPROVAL GATE (must run before any real work) ---
        let summary = format!(
            "Run Ansible playbook '{playbook_name}' on the ansible control host via ansible-playbook"
        );
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        // --- Enforce the allowlist (same refusal shape as Python) ---
        if !self.config.is_allowed(&playbook_name) {
            return Ok(json!({
                "allowed": false,
                "playbook": playbook_name,
                "returncode": Value::Null,
                "stdout": "",
                "stderr": format!(
                    "Playbook '{}' is not on the allowlist: {:?}",
                    playbook_name, self.config.allowlist
                ),
            })
            .to_string());
        }

        let playbook_root = self
            .config
            .playbook_root
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("ANSIBLE_PLAYBOOK_ROOT is not set".into()))?;
        let inventory_path = self
            .config
            .inventory_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("ANSIBLE_INVENTORY_PATH is not set".into()))?;

        // playbook_name is validated against the allowlist, so it is a known-safe
        // literal and cannot be arbitrary shell input.
        let playbook_path = format!("{playbook_root}/{playbook_name}.yml");
        let command = format!("ansible-playbook {playbook_path} -i {inventory_path}");

        let cfg = Arc::clone(&self.config);
        let cmd = command.clone();
        let exec = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &cmd, 120))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

        let result = exec?;

        // --- Record last-run state (in-memory) ---
        {
            let mut lr = self
                .last_run
                .lock()
                .map_err(|e| ToolError::Execution(format!("last_run lock poisoned: {e}")))?;
            *lr = LastRun {
                playbook: Some(playbook_name.clone()),
                returncode: result.returncode,
                stdout: result.stdout.clone(),
                stderr: result.stderr.clone(),
                timestamp: utc_now_iso(),
            };
        }

        Ok(json!({
            "allowed": true,
            "playbook": playbook_name,
            "returncode": result.returncode,
            "stdout": result.stdout,
            "stderr": result.stderr,
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: ansible_list_playbooks
// ---------------------------------------------------------------------------

pub struct AnsibleListPlaybooks {
    config: Arc<AnsibleConfig>,
}

/// Parse the output of `ls -1 <root>/*.yml` into structured playbook records.
/// Pulled out so it can be unit-tested on sample output with no network.
fn parse_playbook_listing(stdout: &str, allowlist: &[String]) -> Vec<Value> {
    let allowed = |name: &str| allowlist.iter().any(|p| p == name);

    let mut playbooks: Vec<(bool, String, Value)> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let file = line.rsplit('/').next().unwrap_or(line);
        let name = file.strip_suffix(".yml").unwrap_or(file).to_string();
        let is_allowed = allowed(&name);
        playbooks.push((
            is_allowed,
            name.clone(),
            json!({ "name": name, "allowed": is_allowed, "path": line }),
        ));
    }

    // Sort: allowed first (matching Python `(not allowed, name)`), then by name.
    playbooks.sort_by(|a, b| (!a.0).cmp(&(!b.0)).then_with(|| a.1.cmp(&b.1)));

    playbooks.into_iter().map(|(_, _, v)| v).collect()
}

#[async_trait]
impl RustTool for AnsibleListPlaybooks {
    fn name(&self) -> &str {
        "ansible_list_playbooks"
    }

    fn description(&self) -> &str {
        "List all *.yml playbooks on the ansible host and their allowlist status. \
         GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // --- APPROVAL GATE ---
        let summary = "List Ansible playbooks on the ansible control host".to_string();
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        let playbook_root = self
            .config
            .playbook_root
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("ANSIBLE_PLAYBOOK_ROOT is not set".into()))?;

        // Fixed command shape — only the configured root path (not user input).
        let command = format!("ls -1 {playbook_root}/*.yml 2>/dev/null");

        let cfg = Arc::clone(&self.config);
        let cmd = command.clone();
        let exec = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &cmd, 30))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;
        let result = exec?;

        if result.returncode != 0 {
            return Ok(json!({
                "error": result.stderr,
                "playbooks": [],
            })
            .to_string());
        }

        let playbooks = parse_playbook_listing(&result.stdout, &self.config.allowlist);
        let allowed_count = playbooks
            .iter()
            .filter(|p| p["allowed"].as_bool().unwrap_or(false))
            .count();

        Ok(json!({
            "total": playbooks.len(),
            "allowed_count": allowed_count,
            "allowlist": self.config.allowlist,
            "playbooks": playbooks,
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: ansible_last_run_status
// ---------------------------------------------------------------------------

pub struct AnsibleLastRunStatus {
    last_run: Arc<Mutex<LastRun>>,
}

#[async_trait]
impl RustTool for AnsibleLastRunStatus {
    fn name(&self) -> &str {
        "ansible_last_run_status"
    }

    fn description(&self) -> &str {
        "Check the result of the last playbook run this process performed (in-memory; \
         resets on restart). GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // --- APPROVAL GATE ---
        let summary = "Read the last Ansible playbook run status (in-memory)".to_string();
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        let lr = self
            .last_run
            .lock()
            .map_err(|e| ToolError::Execution(format!("last_run lock poisoned: {e}")))?;

        if lr.playbook.is_none() {
            return Ok(json!({
                "message": "No playbook has been run since the MCP server started."
            })
            .to_string());
        }

        Ok(json!({
            "playbook": lr.playbook,
            "returncode": lr.returncode,
            "success": lr.returncode == 0,
            "timestamp": lr.timestamp,
            "has_output": !lr.stdout.is_empty() || !lr.stderr.is_empty(),
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: ansible_view_run_log
// ---------------------------------------------------------------------------

pub struct AnsibleViewRunLog {
    last_run: Arc<Mutex<LastRun>>,
}

#[async_trait]
impl RustTool for AnsibleViewRunLog {
    fn name(&self) -> &str {
        "ansible_view_run_log"
    }

    fn description(&self) -> &str {
        "Retrieve stdout/stderr from the last playbook run this process performed \
         (in-memory; resets on restart). GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // --- APPROVAL GATE ---
        let summary =
            "Read the last Ansible playbook run log (stdout/stderr, in-memory)".to_string();
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        let lr = self
            .last_run
            .lock()
            .map_err(|e| ToolError::Execution(format!("last_run lock poisoned: {e}")))?;

        if lr.playbook.is_none() {
            return Ok(json!({
                "message": "No playbook has been run since the MCP server started."
            })
            .to_string());
        }

        Ok(json!({
            "playbook": lr.playbook,
            "returncode": lr.returncode,
            "timestamp": lr.timestamp,
            "stdout": lr.stdout,
            "stderr": lr.stderr,
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Ansible tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(AnsibleConfig::from_env());
    // Shared in-memory last-run state across the run/status/log tools.
    let last_run = Arc::new(Mutex::new(LastRun::default()));

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(AnsibleRunPlaybook {
            config: Arc::clone(&config),
            last_run: Arc::clone(&last_run),
        }),
        Box::new(AnsibleListPlaybooks {
            config: Arc::clone(&config),
        }),
        Box::new(AnsibleLastRunStatus {
            last_run: Arc::clone(&last_run),
        }),
        Box::new(AnsibleViewRunLog {
            last_run: Arc::clone(&last_run),
        }),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            error!("ansible: failed to register tool: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (no network / no SSH — arg validation, parsing, allowlist, gate)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn test_config() -> Arc<AnsibleConfig> {
        Arc::new(AnsibleConfig {
            host: None,
            user: "root".into(),
            ssh_key: None,
            playbook_root: None,
            inventory_path: None,
            allowlist: DEFAULT_ALLOWLIST.iter().map(|s| s.to_string()).collect(),
        })
    }

    // Ensure approval-gate tests don't see a real DATABASE_URL.
    fn clear_db_url() {
        std::env::remove_var("DATABASE_URL");
    }

    // ------------------------------------------------------------------
    // 1: default allowlist matches the Python source exactly
    // ------------------------------------------------------------------
    #[test]
    fn test_default_allowlist_matches_python() {
        let expected = [
            "deploy-<host>-db",
            "deploy-<host>-ct",
            "ping",
            "deploy-<secret-manager>",
            "install-node-exporter",
            "generate-prometheus-targets",
            "rotate-<matrix-server>-token",
            "deploy-plane",
            "restart-<host>",
        ];
        assert_eq!(DEFAULT_ALLOWLIST, &expected);
    }

    // ------------------------------------------------------------------
    // 2: allowlist membership checks
    // ------------------------------------------------------------------
    #[test]
    fn test_is_allowed() {
        let cfg = test_config();
        assert!(cfg.is_allowed("ping"));
        assert!(cfg.is_allowed("restart-<host>"));
        assert!(!cfg.is_allowed("rm-rf"));
        assert!(!cfg.is_allowed("ping; rm -rf /"));
        assert!(!cfg.is_allowed(""));
        assert!(!cfg.is_allowed("PING")); // case-sensitive
    }

    // ------------------------------------------------------------------
    // 3: custom allowlist via field
    // ------------------------------------------------------------------
    #[test]
    fn test_custom_allowlist_from_field() {
        let cfg = AnsibleConfig {
            host: None,
            user: "root".into(),
            ssh_key: None,
            playbook_root: None,
            inventory_path: None,
            allowlist: vec!["alpha".into(), "beta".into()],
        };
        assert!(cfg.is_allowed("alpha"));
        assert!(cfg.is_allowed("beta"));
        assert!(!cfg.is_allowed("ping"));
    }

    // ------------------------------------------------------------------
    // 4: parse_playbook_listing on sample `ls` output
    // ------------------------------------------------------------------
    #[test]
    fn test_parse_playbook_listing() {
        let allowlist: Vec<String> = vec!["ping".into(), "deploy-plane".into()];
        let sample = "\
/opt/ansible/playbooks/zeta-unknown.yml
/opt/ansible/playbooks/ping.yml
/opt/ansible/playbooks/deploy-plane.yml

   /opt/ansible/playbooks/another-unlisted.yml
";
        let parsed = parse_playbook_listing(sample, &allowlist);
        assert_eq!(parsed.len(), 4);

        // Allowed ones sort first, alphabetically among themselves.
        assert_eq!(parsed[0]["name"], "deploy-plane");
        assert_eq!(parsed[0]["allowed"], true);
        assert_eq!(parsed[1]["name"], "ping");
        assert_eq!(parsed[1]["allowed"], true);

        // Then the disallowed ones, alphabetically.
        assert_eq!(parsed[2]["name"], "another-unlisted");
        assert_eq!(parsed[2]["allowed"], false);
        assert_eq!(parsed[3]["name"], "zeta-unknown");
        assert_eq!(parsed[3]["allowed"], false);

        // Path is preserved (trimmed).
        assert_eq!(parsed[1]["path"], "/opt/ansible/playbooks/ping.yml");
    }

    #[test]
    fn test_parse_playbook_listing_empty() {
        let parsed = parse_playbook_listing("", &[]);
        assert!(parsed.is_empty());
    }

    // ------------------------------------------------------------------
    // 5: utc_now_iso format shape
    // ------------------------------------------------------------------
    #[test]
    fn test_utc_now_iso_shape() {
        let ts = utc_now_iso();
        // e.g. 2026-06-09T01:43:41Z
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
        // Year is plausible (>= 2020).
        let year: i64 = ts[0..4].parse().unwrap();
        assert!(year >= 2020, "year was {year}");
    }

    // ------------------------------------------------------------------
    // 6: run_playbook requires playbook_name string
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn test_run_playbook_missing_arg() {
        let tool = AnsibleRunPlaybook {
            config: test_config(),
            last_run: Arc::new(Mutex::new(LastRun::default())),
        };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("playbook_name")),
            other => panic!("expected InvalidArgument, got: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // 7: GATE — run_playbook without DATABASE_URL returns approval message,
    //    NOT the real action (no SSH attempted, no last_run mutation).
    // ------------------------------------------------------------------
    #[tokio::test]
    #[serial]
    async fn test_run_playbook_gate_denies_without_db() {
        clear_db_url();
        let last_run = Arc::new(Mutex::new(LastRun::default()));
        let tool = AnsibleRunPlaybook {
            config: test_config(),
            last_run: Arc::clone(&last_run),
        };
        // Valid arg so we get past validation and hit the gate.
        let result = tool.execute(json!({ "playbook_name": "ping" })).await;
        // Gate returns Ok(message) (Denied/Pending are returned verbatim as Ok).
        let msg = result.expect("gate returns Ok with a message");
        assert!(
            msg.contains("unavailable")
                || msg.contains("DATABASE_URL")
                || msg.contains("APPROVAL"),
            "expected approval/unavailable message, got: {msg}"
        );
        // The real action must NOT have run: last_run untouched.
        assert!(
            last_run.lock().unwrap().playbook.is_none(),
            "guarded action ran without approval"
        );
    }

    // ------------------------------------------------------------------
    // 8: GATE — list_playbooks without DATABASE_URL returns approval message
    // ------------------------------------------------------------------
    #[tokio::test]
    #[serial]
    async fn test_list_playbooks_gate_denies_without_db() {
        clear_db_url();
        let tool = AnsibleListPlaybooks {
            config: test_config(),
        };
        let msg = tool.execute(json!({})).await.expect("gate returns Ok");
        assert!(
            msg.contains("unavailable")
                || msg.contains("DATABASE_URL")
                || msg.contains("APPROVAL"),
            "expected approval/unavailable message, got: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // 9: GATE — last_run_status without DATABASE_URL returns approval message
    //    (does NOT leak state).
    // ------------------------------------------------------------------
    #[tokio::test]
    #[serial]
    async fn test_last_run_status_gate_denies_without_db() {
        clear_db_url();
        let tool = AnsibleLastRunStatus {
            last_run: Arc::new(Mutex::new(LastRun {
                playbook: Some("ping".into()),
                returncode: 0,
                stdout: "ok".into(),
                stderr: String::new(),
                timestamp: "2026-06-09T00:00:00Z".into(),
            })),
        };
        let msg = tool.execute(json!({})).await.expect("gate returns Ok");
        assert!(
            msg.contains("unavailable")
                || msg.contains("DATABASE_URL")
                || msg.contains("APPROVAL"),
            "expected approval/unavailable message, got: {msg}"
        );
        // Must not have leaked the stored playbook name.
        assert!(
            !msg.contains("\"playbook\""),
            "state leaked through gate: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // 10: GATE — view_run_log without DATABASE_URL returns approval message
    // ------------------------------------------------------------------
    #[tokio::test]
    #[serial]
    async fn test_view_run_log_gate_denies_without_db() {
        clear_db_url();
        let tool = AnsibleViewRunLog {
            last_run: Arc::new(Mutex::new(LastRun::default())),
        };
        let msg = tool.execute(json!({})).await.expect("gate returns Ok");
        assert!(
            msg.contains("unavailable")
                || msg.contains("DATABASE_URL")
                || msg.contains("APPROVAL"),
            "expected approval/unavailable message, got: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // 11: register() adds exactly 4 tools, all distinct expected names
    // ------------------------------------------------------------------
    #[test]
    fn test_register_adds_four_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 4, "ansible must register exactly 4 tools");
        for name in &[
            "ansible_run_playbook",
            "ansible_list_playbooks",
            "ansible_last_run_status",
            "ansible_view_run_log",
        ] {
            assert!(registry.contains(name), "registry should contain '{name}'");
        }
    }

    // ------------------------------------------------------------------
    // 12: run_playbook parameter schema is well-formed
    // ------------------------------------------------------------------
    #[test]
    fn test_run_playbook_parameters_schema() {
        let tool = AnsibleRunPlaybook {
            config: test_config(),
            last_run: Arc::new(Mutex::new(LastRun::default())),
        };
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"]["playbook_name"].is_object());
        assert_eq!(params["required"][0], "playbook_name");
    }
}
