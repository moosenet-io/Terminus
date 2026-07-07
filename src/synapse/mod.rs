//! Synapse tools — ported from the Python `synapse_tools.py` on the fleet host.
//!
//! Synapse is a fleet-host process that watches for proactive-message
//! candidates ("Pulse") and gates them against config (enabled/strength/
//! quiet hours) before sending. Confirmed live against the fleet host via
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
//!     unreachable from the test host at test time, so we could not observe a
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
//!                           a config file under the fleet host's Synapse directory.
//!   SYNAPSE_LOG_PATH      — local log file path (last-sent marker source),
//!                           default a log file under the fleet host's Synapse directory.
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
    /// Remote synapse script invocation — from `SYNAPSE_SCRIPT`. No
    /// compiled-in default (PII remediation 2026-07): required at runtime.
    pub script: Option<String>,
    /// Local config file path — from `SYNAPSE_CONFIG_PATH`. No compiled-in
    /// default (PII remediation 2026-07): required at runtime.
    pub config_path: Option<String>,
    /// Local log file path — from `SYNAPSE_LOG_PATH`. No compiled-in default
    /// (PII remediation 2026-07): required at runtime.
    pub log_path: Option<String>,
}

impl SynapseConfig {
    pub fn from_env() -> Self {
        SynapseConfig {
            ssh_host: env::var("SYNAPSE_SSH_HOST").ok().filter(|s| !s.is_empty()),
            ssh_user: env::var("SYNAPSE_SSH_USER").unwrap_or_else(|_| "root".into()),
            ssh_key_path: env::var("SYNAPSE_SSH_KEY_PATH").ok().filter(|s| !s.is_empty()),
            script: env::var("SYNAPSE_SCRIPT").ok().filter(|s| !s.is_empty()),
            config_path: env::var("SYNAPSE_CONFIG_PATH").ok().filter(|s| !s.is_empty()),
            log_path: env::var("SYNAPSE_LOG_PATH").ok().filter(|s| !s.is_empty()),
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

    /// PII remediation (2026-07): `SYNAPSE_SCRIPT` no longer has a
    /// compiled-in fleet-host script path fallback.
    fn require_script(&self) -> Result<&str, ToolError> {
        self.script
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SYNAPSE_SCRIPT is not set".into()))
    }

    /// PII remediation (2026-07): `SYNAPSE_CONFIG_PATH` no longer has a
    /// compiled-in fleet-host config path fallback.
    fn require_config_path(&self) -> Result<&str, ToolError> {
        self.config_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SYNAPSE_CONFIG_PATH is not set".into()))
    }

    /// PII remediation (2026-07): `SYNAPSE_LOG_PATH` no longer has a
    /// compiled-in fleet-host log path fallback.
    fn require_log_path(&self) -> Result<&str, ToolError> {
        self.log_path
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("SYNAPSE_LOG_PATH is not set".into()))
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
/// concatenating them the same way).
///
/// Unlike `sentinel::ssh_exec` / `vigil::ssh_exec` (which genericize
/// *connection*-level failures into an opaque error to avoid leaking infra
/// details), this deliberately does NOT do that: the live Python
/// `synapse_trigger`/`synapse_mute` tools shell out to a bare `ssh`
/// subprocess and pass its stderr straight back as the tool's normal text
/// output (`isError: false`) — confirmed live: `"[synapse_trigger DRY
/// RUN]\nssh: connect to host <fleet-host> port 22: No route to host"`. A
/// generic `ToolError` here would surface as an MCP-level `isError: true`,
/// which is NOT what the live server does. So connection/handshake/auth failures are
/// folded into `Ok(String)` as an ssh-style message, matching the real
/// tool's documented and observed payload shape. Only `NotConfigured`
/// (missing `SYNAPSE_SSH_HOST`/`SYNAPSE_SSH_KEY_PATH`) remains a hard
/// error — there is no equivalent Python failure mode since the live
/// server always has its ssh target configured.
fn ssh_exec(config: &SynapseConfig, command: &str, timeout_secs: u64) -> Result<String, ToolError> {
    let host = config.require_host()?;
    let key_path = config.require_key()?;

    let addr = format!("{host}:22");
    let tcp = match TcpStream::connect(&addr) {
        Ok(t) => t,
        Err(e) => {
            warn!("synapse: cannot reach fleet host {host}: {e}");
            return Ok(format!("ssh: connect to host {host} port 22: {e}"));
        }
    };

    let _ = tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs)));

    let mut sess = match Session::new() {
        Ok(s) => s,
        Err(e) => {
            warn!("synapse: session init failed: {e}");
            return Ok(format!("ssh: could not initialize session with {host}: {e}"));
        }
    };
    sess.set_tcp_stream(tcp);
    if let Err(e) = sess.handshake() {
        warn!("synapse: handshake failed with {host}: {e}");
        return Ok(format!("ssh: connect to host {host} port 22: {e}"));
    }

    if let Err(e) = sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None) {
        warn!("synapse: auth failed for {}@{host}: {e}", config.ssh_user);
        return Ok(format!(
            "ssh: {}@{host}: Permission denied (publickey): {e}",
            config.ssh_user
        ));
    }

    if !sess.authenticated() {
        warn!("synapse: authentication failed for {}@{host}", config.ssh_user);
        return Ok(format!(
            "ssh: {}@{host}: Permission denied (publickey)",
            config.ssh_user
        ));
    }

    let mut channel = match sess.channel_session() {
        Ok(c) => c,
        Err(e) => {
            warn!("synapse: channel open failed on {host}: {e}");
            return Ok(format!("ssh: channel open failed on {host}: {e}"));
        }
    };

    debug!("synapse ssh_exec: {command}");
    if let Err(e) = channel.exec(command) {
        warn!("synapse: command exec failed on {host}: {e}");
        return Ok(format!("ssh: command exec failed on {host}: {e}"));
    }

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
        let config_path = self.config.require_config_path()?.to_string();
        let log_path = self.config.require_log_path()?.to_string();
        let (status_cfg, last_sent) = tokio::task::spawn_blocking(move || {
            let status_cfg = load_status_config(&config_path);
            let last_sent = last_sent_marker(&log_path);
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
        let script = self.config.require_script()?;
        let command = format!("{script} trigger {flag}");

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

        let script = self.config.require_script()?;
        let command = format!("{script} mute --hours {hours}");
        let output = run_ssh(Arc::clone(&self.config), command, 30).await?;

        let lower = output.to_lowercase();
        if lower.contains("error")
            || lower.contains("no route to host")
            || lower.contains("connection refused")
            || lower.contains("failed")
            || lower.contains("permission denied")
            || lower.starts_with("ssh:")
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

    /// Test-fixture value standing in for `SYNAPSE_SCRIPT`, which has no
    /// compiled-in default (PII remediation 2026-07).
    const TEST_SCRIPT: &str = "/opt/test-fixture/synapse/synapse.py";

    fn test_config() -> Arc<SynapseConfig> {
        Arc::new(SynapseConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            script: Some(TEST_SCRIPT.into()),
            config_path: Some("/nonexistent/path/config.yaml".into()),
            log_path: Some("/nonexistent/path/pulse.log".into()),
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
        fs::write(&path, "2026-07-01T08:00:00Z sent\n2026-07-02T08:00:00Z sent\n").unwrap(); // pii-test-fixture
        assert_eq!(
            last_sent_marker(path.to_str().unwrap()),
            "2026-07-02T08:00:00Z sent" // pii-test-fixture
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

    #[tokio::test]
    async fn test_synapse_trigger_not_configured_without_script() {
        // PII remediation (2026-07): SYNAPSE_SCRIPT has no compiled-in
        // default — missing it must fail clean with NotConfigured.
        let cfg = Arc::new(SynapseConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/nonexistent/key".into()),
            script: None,
            config_path: Some("/nonexistent/path/config.yaml".into()),
            log_path: Some("/nonexistent/path/pulse.log".into()),
        });
        let tool = SynapseTrigger { config: cfg };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SYNAPSE_SCRIPT")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_synapse_status_not_configured_without_config_path() {
        let cfg = Arc::new(SynapseConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            script: Some(TEST_SCRIPT.into()),
            config_path: None,
            log_path: Some("/nonexistent/path/pulse.log".into()),
        });
        let tool = SynapseStatus { config: cfg };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SYNAPSE_CONFIG_PATH")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_synapse_status_not_configured_without_log_path() {
        let cfg = Arc::new(SynapseConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            script: Some(TEST_SCRIPT.into()),
            config_path: Some("/nonexistent/path/config.yaml".into()),
            log_path: None,
        });
        let tool = SynapseStatus { config: cfg };
        let err = tool.execute(json!({})).await.unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("SYNAPSE_LOG_PATH")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // --- connection-level SSH failures surface as text, not ToolError ------
    //
    // Regression test for a correctness-review finding: the live Python
    // synapse_trigger/synapse_mute embed the underlying ssh failure text in
    // a normal (isError: false) response rather than raising. Confirm this
    // port does the same for a configured-but-unreachable host, instead of
    // converting the connection failure into a generic ToolError.

    fn configured_unreachable_config() -> Arc<SynapseConfig> {
        Arc::new(SynapseConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/nonexistent/key".into()),
            script: Some(TEST_SCRIPT.into()),
            config_path: Some("/nonexistent/path/config.yaml".into()),
            log_path: Some("/nonexistent/path/pulse.log".into()),
        })
    }

    #[tokio::test]
    async fn test_synapse_trigger_connection_failure_is_ok_text_not_error() {
        let tool = SynapseTrigger { config: configured_unreachable_config() };
        let result = tool.execute(json!({"dry_run": true})).await;
        let text = result.expect("connection failure must surface as Ok(text), not Err");
        assert!(text.contains("[synapse_trigger DRY RUN]"));
        assert!(text.to_lowercase().contains("ssh:"));
    }

    #[tokio::test]
    async fn test_synapse_mute_connection_failure_is_ok_text_not_error() {
        let tool = SynapseMute { config: configured_unreachable_config() };
        let result = tool.execute(json!({"hours": 1})).await;
        let text = result.expect("connection failure must surface as Ok(text), not Err");
        assert!(text.starts_with("[synapse_mute] Failed:"));
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
