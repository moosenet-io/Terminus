//! Synapse tools — ported from the Python `synapse_tools.py` on <host>.
//!
//! Synapse is a fleet-host process that watches for proactive-message
//! candidates ("Pulse") and gates them against config (enabled/strength/
//! quiet hours) before sending. Confirmed live against <host> via
//! `tools/call`:
//!   - `synapse_status` returned instantly and successfully even while the
//!     fleet host was unreachable — text: `"Synapse: DISABLED\nStrength:
//!     moderate | Max/day: 3\nQuiet hours: 22:00 – 08:00\nLast sent: never"`.
//!   - `synapse_trigger` (dry_run default true) returned
//!     `"[synapse_trigger DRY RUN]\nssh: connect to host <fleet-host> port
//!     22: No route to host"` — i.e. the *Python* tool itself shells out via
//!     a bare `ssh` subprocess to the fleet host to run the
//!     scan, and that ssh subprocess's own stderr was captured verbatim into
//!     the tool's text response.
//!   - `synapse_mute` (hours=1, and also tried with hours=0 and hours=100)
//!     returned `"[synapse_mute] Failed: ssh: connect to host <fleet-host>
//!     port 22: No route to host"` in all three cases — the fleet host was
//!     unreachable from <host> at test time, so we could not observe a
//!     success-path response or confirm whether the documented 1-72 hour
//!     bound is enforced client-side (in the Python tool) or only in the
//!     remote script. **This port enforces the bound before attempting the
//!     SSH call**, matching the tool's own docstring contract
//!     ("Hours must be between 1 and 72") — flagged here for human review
//!     since it could not be verified against the live server's actual
//!     validation point.
//!
//! This port:
//! - Uses the `ssh2` crate for typed SSH execution (no `shell=True`, no
//!   subprocess), mirroring `sentinel/mod.rs` and `vigil/mod.rs`'s fleet-host
//!   script convention exactly.
//! - `synapse_status` does NOT go over SSH — the live server answered it
//!   instantly while the SSH-dependent tools failed, implying it reads local
//!   config/log state rather than reaching out to the fleet host (its own
//!   docstring says "Zero cost — reads config + log files"). This port reads
//!   local config/log files directly via `std::fs`.
//!
//! ## Tools (identical names to the Python source)
//!   synapse_status  — show current config + last-sent time (local read)
//!   synapse_trigger — run a scan now (dry_run=true by default; SSH to fleet host)
//!   synapse_mute    — mute Synapse for 1-72 hours (SSH to fleet host)
//!
//! ## Configuration (environment only — no hardcoded hosts/keys)
//!   SYNAPSE_SSH_HOST      — SSH host of the fleet box (e.g. "192.168.0.X").
//!   SYNAPSE_SSH_USER      — SSH user, default "root".
//!   SYNAPSE_SSH_KEY_PATH  — path to the SSH private key file.
//!   SYNAPSE_SCRIPT        — remote synapse script, default mirrors the Python.
//!   SYNAPSE_CONFIG_PATH   — local config file path (YAML), default
//!                           "<path>/synapse/config.yaml".
//!   SYNAPSE_LOG_PATH      — local log file path (last-sent marker source),
//!                           default "<path>/synapse/pulse.log".
//!
//! ## Security model
//! - `hours` is validated to the documented 1-72 range before it is ever
//!   placed into a remote command string.

use std::env;
use std::fs;
use std::io::Read as IoRead;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use ssh2::Session;
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_SCRIPT: &str = "/usr/bin/python3 <path>/synapse/synapse.py";
const DEFAULT_CONFIG_PATH: &str = "<path>/synapse/config.yaml";
const DEFAULT_LOG_PATH: &str = "<path>/synapse/pulse.log";
const MIN_MUTE_HOURS: u64 = 1;
const MAX_MUTE_HOURS: u64 = 72;
const DEFAULT_MUTE_HOURS: u64 = 4;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration sourced entirely from environment variables.
#[derive(Debug, Clone)]
pub struct SynapseConfig {
    /// SSH host of the fleet box — from `SYNAPSE_SSH_HOST`.
    pub ssh_host: Option<String>,
    /// SSH user — from `SYNAPSE_SSH_USER`, default "root".
    pub ssh_user: String,
    /// Path to the SSH private key file — from `SYNAPSE_SSH_KEY_PATH`.
    pub ssh_key_path: Option<String>,
    /// Remote synapse script invocation — from `SYNAPSE_SCRIPT`.
    pub script: String,
    /// Local config file path — from `SYNAPSE_CONFIG_PATH`.
    pub config_path: String,
    /// Local log file path — from `SYNAPSE_LOG_PATH`.
    pub log_path: String,
}

impl SynapseConfig {
    pub fn from_env() -> Self {
        SynapseConfig {
            ssh_host: env::var("SYNAPSE_SSH_HOST").ok().filter(|s| !s.is_empty()),
            ssh_user: env::var("SYNAPSE_SSH_USER").unwrap_or_else(|_| "root".into()),
            ssh_key_path: env::var("SYNAPSE_SSH_KEY_PATH").ok().filter(|s| !s.is_empty()),
            script: env::var("SYNAPSE_SCRIPT")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_SCRIPT.into()),
            config_path: env::var("SYNAPSE_CONFIG_PATH")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_CONFIG_PATH.into()),
            log_path: env::var("SYNAPSE_LOG_PATH")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_LOG_PATH.into()),
        }
    }

    fn require_host(&self) -> Result<&str, ToolError> {
        self.ssh_host
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SYNAPSE_SSH_HOST is not set".into()))
    }

    fn require_key(&self) -> Result<&str, ToolError> {
        self.ssh_key_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SYNAPSE_SSH_KEY_PATH is not set".into()))
    }
}

/// Validate the mute duration against the documented 1-72 hour bound.
fn validate_mute_hours(hours: u64) -> Result<(), ToolError> {
    if (MIN_MUTE_HOURS..=MAX_MUTE_HOURS).contains(&hours) {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "hours must be between {MIN_MUTE_HOURS} and {MAX_MUTE_HOURS} (got {hours})"
        )))
    }
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking for async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session, run a single command, and return combined stdout
/// (the Python original merged stdout/stderr from its ssh subprocess into a
/// single text blob, which this mirrors by reading stdout and stderr and
/// concatenating them the same way). Mirrors `sentinel::ssh_exec` /
/// `vigil::ssh_exec` — generic, non-infra-leaking error messages for
/// *connection*-level failures; remote command output (including the
/// remote's own error text) is returned as-is since that is the tool's
/// actual documented payload.
fn ssh_exec(config: &SynapseConfig, command: &str, timeout_secs: u64) -> Result<String, ToolError> {
    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("synapse: cannot reach fleet host {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = Session::new().map_err(|e| {
        warn!("synapse: session init failed: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("synapse: handshake failed with {host}: {e}");
        ToolError::Execution("The fleet server is unreachable.".into())
    })?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("synapse: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the fleet server.".into())
        })?;

    if !sess.authenticated() {
        warn!("synapse: authentication failed for {}@{host}", config.ssh_user);
        return Err(ToolError::Execution("Could not connect to the fleet server.".into()));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("synapse: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    debug!("synapse ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("synapse: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("synapse: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the operation on the fleet server.".into())
    })?;
    let mut stderr = String::new();
    let _ = channel.stderr().read_to_string(&mut stderr);

    channel.wait_close().ok();

    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&stderr);
    }

    Ok(output)
}

async fn run_ssh(config: Arc<SynapseConfig>, command: String, timeout_secs: u64) -> Result<String, ToolError> {
    tokio::task::spawn_blocking(move || ssh_exec(&config, &command, timeout_secs))
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?
}

// ---------------------------------------------------------------------------
// Local config/log read (synapse_status — no SSH; see module doc)
// ---------------------------------------------------------------------------

/// Parsed Synapse config, mirroring the fields shown in the live
/// `synapse_status` text output.
struct SynapseStatusConfig {
    enabled: bool,
    strength: String,
    max_per_day: i64,
    quiet_start: String,
    quiet_end: String,
}

impl Default for SynapseStatusConfig {
    fn default() -> Self {
        SynapseStatusConfig {
            enabled: false,
            strength: "moderate".into(),
            max_per_day: 3,
            quiet_start: "22:00".into(),
            quiet_end: "08:00".into(),
        }
    }
}

fn load_status_config(path: &str) -> SynapseStatusConfig {
    let mut cfg = SynapseStatusConfig::default();
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return cfg, // missing file -> documented defaults
    };
    let parsed: Value = match serde_yaml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return cfg,
    };
    if let Some(b) = parsed.get("enabled").and_then(|v| v.as_bool()) {
        cfg.enabled = b;
    }
    if let Some(s) = parsed.get("strength").and_then(|v| v.as_str()) {
        cfg.strength = s.to_string();
    }
    if let Some(n) = parsed.get("max_per_day").and_then(|v| v.as_i64()) {
        cfg.max_per_day = n;
    }
    if let Some(s) = parsed.get("quiet_start").and_then(|v| v.as_str()) {
        cfg.quiet_start = s.to_string();
    }
    if let Some(s) = parsed.get("quiet_end").and_then(|v| v.as_str()) {
        cfg.quiet_end = s.to_string();
    }
    cfg
}

/// Read the last non-empty line of the log file as the "last sent" marker,
/// falling back to "never" when the file is missing/empty — matching the
/// live server's "Last sent: never" output with no prior sends.
fn last_sent_marker(path: &str) -> String {
    match fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .unwrap_or_else(|| "never".into()),
        Err(_) => "never".into(),
    }
}

fn format_status(cfg: &SynapseStatusConfig, last_sent: &str) -> String {
    format!(
        "Synapse: {}\nStrength: {} | Max/day: {}\nQuiet hours: {} \u{2013} {}\nLast sent: {}",
        if cfg.enabled { "ENABLED" } else { "DISABLED" },
        cfg.strength,
        cfg.max_per_day,
        cfg.quiet_start,
        cfg.quiet_end,
        last_sent
    )
}

// ---------------------------------------------------------------------------
// Tool: synapse_status
// ---------------------------------------------------------------------------

pub struct SynapseStatus {
    config: Arc<SynapseConfig>,
}

#[async_trait]
impl RustTool for SynapseStatus {
    fn name(&self) -> &str {
        "synapse_status"
    }

    fn description(&self) -> &str {
        "Show Synapse current config (enabled, strength, quiet hours) and when the \
         last message was sent. Zero cost — reads config + log files."
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
        let (status_cfg, last_sent) = tokio::task::spawn_blocking(move || {
            let status_cfg = load_status_config(&cfg.config_path);
            let last_sent = last_sent_marker(&cfg.log_path);
            (status_cfg, last_sent)
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

        Ok(format_status(&status_cfg, &last_sent))
    }
}

// ---------------------------------------------------------------------------
// Tool: synapse_trigger
// ---------------------------------------------------------------------------

pub struct SynapseTrigger {
    config: Arc<SynapseConfig>,
}

#[async_trait]
impl RustTool for SynapseTrigger {
    fn name(&self) -> &str {
        "synapse_trigger"
    }

    fn description(&self) -> &str {
        "Run a Synapse scan manually right now. dry_run=True (default): shows what \
         would be sent without sending. dry_run=False: actually sends the message if \
         a candidate passes the gate. Returns scan output."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "dry_run": {
                    "type": "boolean",
                    "description": "Show what would be sent without sending (default true)",
                    "default": true
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let dry_run = args["dry_run"].as_bool().unwrap_or(true);

        let flag = if dry_run { "--dry-run" } else { "--live" };
        let command = format!("{} trigger {flag}", self.config.script);

        let output = run_ssh(Arc::clone(&self.config), command, 60).await?;

        let label = if dry_run { "DRY RUN" } else { "LIVE" };
        Ok(format!("[synapse_trigger {label}]\n{}", output.trim_end()))
    }
}

// ---------------------------------------------------------------------------
// Tool: synapse_mute
// ---------------------------------------------------------------------------

pub struct SynapseMute {
    config: Arc<SynapseConfig>,
}

#[async_trait]
impl RustTool for SynapseMute {
    fn name(&self) -> &str {
        "synapse_mute"
    }

    fn description(&self) -> &str {
        "Mute Synapse for the next N hours (default 4). Does this by writing a Pulse \
         marker 'synapse_muted_until' with a future timestamp. The gate checks this \
         marker before sending. Hours must be between 1 and 72."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "hours": {
                    "type": "integer",
                    "description": "Number of hours to mute for (1-72, default 4)",
                    "default": DEFAULT_MUTE_HOURS
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let hours = args["hours"].as_u64().unwrap_or(DEFAULT_MUTE_HOURS);
        validate_mute_hours(hours)?;

        let command = format!("{} mute --hours {hours}", self.config.script);
        let output = run_ssh(Arc::clone(&self.config), command, 30).await?;

        if output.to_lowercase().contains("error")
            || output.to_lowercase().contains("no route to host")
            || output.to_lowercase().contains("connection refused")
            || output.to_lowercase().contains("failed")
        {
            return Ok(format!("[synapse_mute] Failed: {}", output.trim_end()));
        }

        Ok(format!(
            "[synapse_mute] Synapse muted for {hours} hour(s).\n{}",
            output.trim_end()
        ))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Synapse tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(SynapseConfig::from_env());

    let _ = registry.register(Box::new(SynapseStatus { config: Arc::clone(&config) }));
    let _ = registry.register(Box::new(SynapseTrigger { config: Arc::clone(&config) }));
    let _ = registry.register(Box::new(SynapseMute { config }));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config() -> Arc<SynapseConfig> {
        Arc::new(SynapseConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            script: DEFAULT_SCRIPT.into(),
            config_path: "/nonexistent/path/config.yaml".into(),
            log_path: "/nonexistent/path/pulse.log".into(),
        })
    }

    // --- validate_mute_hours -----------------------------------------------

    #[test]
    fn test_validate_mute_hours_accepts_bounds() {
        assert!(validate_mute_hours(1).is_ok());
        assert!(validate_mute_hours(72).is_ok());
        assert!(validate_mute_hours(4).is_ok());
    }

    #[test]
    fn test_validate_mute_hours_rejects_out_of_range() {
        assert!(validate_mute_hours(0).is_err());
        assert!(validate_mute_hours(73).is_err());
        assert!(validate_mute_hours(100).is_err());
    }

    // --- load_status_config / last_sent_marker (local fs, no ssh) ---------

    #[test]
    fn test_load_status_config_defaults_when_missing() {
        let cfg = load_status_config("/nonexistent/path/config.yaml");
        assert!(!cfg.enabled);
        assert_eq!(cfg.strength, "moderate");
        assert_eq!(cfg.max_per_day, 3);
        assert_eq!(cfg.quiet_start, "22:00");
        assert_eq!(cfg.quiet_end, "08:00");
    }

    #[test]
    fn test_load_status_config_reads_real_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(
            &path,
            "enabled: true\nstrength: aggressive\nmax_per_day: 7\nquiet_start: '23:00'\nquiet_end: '06:00'\n",
        )
        .unwrap();
        let cfg = load_status_config(path.to_str().unwrap());
        assert!(cfg.enabled);
        assert_eq!(cfg.strength, "aggressive");
        assert_eq!(cfg.max_per_day, 7);
        assert_eq!(cfg.quiet_start, "23:00");
        assert_eq!(cfg.quiet_end, "06:00");
    }

    #[test]
    fn test_last_sent_marker_never_when_missing() {
        assert_eq!(last_sent_marker("/nonexistent/path/pulse.log"), "never");
    }

    #[test]
    fn test_last_sent_marker_never_when_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pulse.log");
        fs::write(&path, "\n\n").unwrap();
        assert_eq!(last_sent_marker(path.to_str().unwrap()), "never");
    }

    #[test]
    fn test_last_sent_marker_returns_last_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pulse.log");
        fs::write(&path, "2026-07-01T08:00:00Z sent\n2026-07-02T08:00:00Z sent\n").unwrap();
        assert_eq!(
            last_sent_marker(path.to_str().unwrap()),
            "2026-07-02T08:00:00Z sent"
        );
    }

    #[test]
    fn test_format_status_matches_live_shape() {
        let cfg = SynapseStatusConfig::default();
        let formatted = format_status(&cfg, "never");
        assert_eq!(
            formatted,
            "Synapse: DISABLED\nStrength: moderate | Max/day: 3\nQuiet hours: 22:00 \u{2013} 08:00\nLast sent: never"
        );
    }

    // --- tool metadata -------------------------------------------------------

    #[test]
    fn test_synapse_status_metadata() {
        let tool = SynapseStatus { config: test_config() };
        assert_eq!(tool.name(), "synapse_status");
    }

    #[test]
    fn test_synapse_trigger_metadata() {
        let tool = SynapseTrigger { config: test_config() };
        assert_eq!(tool.name(), "synapse_trigger");
    }

    #[test]
    fn test_synapse_mute_metadata() {
        let tool = SynapseMute { config: test_config() };
        assert_eq!(tool.name(), "synapse_mute");
    }

    // --- execute: synapse_status never needs SSH config ---------------------

    #[tokio::test]
    async fn test_synapse_status_works_without_ssh_config() {
        let tool = SynapseStatus { config: test_config() };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Synapse: DISABLED"));
        assert!(result.contains("Last sent: never"));
    }

    // --- execute: invalid arguments (no network needed) ----------------------

    #[tokio::test]
    async fn test_synapse_mute_rejects_zero_hours() {
        let tool = SynapseMute { config: test_config() };
        let err = tool.execute(json!({"hours": 0})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_synapse_mute_rejects_over_72_hours() {
        let tool = SynapseMute { config: test_config() };
        let err = tool.execute(json!({"hours": 100})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_synapse_trigger_not_configured_without_host() {
        let tool = SynapseTrigger { config: test_config() };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SYNAPSE_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_synapse_mute_not_configured_without_host() {
        let tool = SynapseMute { config: test_config() };
        let err = tool.execute(json!({"hours": 4})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SYNAPSE_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- registration ----------------------------------------------------

    #[test]
    fn test_register_adds_three_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 3);
        assert!(registry.contains("synapse_status"));
        assert!(registry.contains("synapse_trigger"));
        assert!(registry.contains("synapse_mute"));
    }
}
