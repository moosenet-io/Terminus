//! Network diagnostic tools — ported from the Python `network_tools.py` on <host>.
//!
//! Mirrors the five Python tools with identical names and parameters:
//!   net_ping         — ping a host and report latency (SSH-based, fixed command)
//!   net_port_check   — check whether a TCP port is open (pure-Rust TcpStream)
//!   net_dns_lookup   — resolve a hostname to IPs (pure-Rust ToSocketAddrs)
//!   net_subnet_scan  — parallel ping sweep of an IP range (SSH-based)
//!   net_check_services — TCP reachability of known MooseNet services (pure-Rust)
//!
//! ## Why SSH for ping / subnet scan
//! The `RustTool` contract forbids `std::process::Command`/shell subprocesses, and
//! ICMP echo (`ping`) requires either raw sockets (root) or shelling out. To keep
//! parity with the Python behaviour without violating the contract, the ICMP-based
//! tools execute a *fixed-form* `ping` command on a configured diagnostics host via
//! the `ssh2` crate (the same pattern as `dura/mod.rs`). Only validated, non-shell
//! tokens (verified IPs / numeric octets) are interpolated into those commands.
//!
//! The TCP / DNS tools (`net_port_check`, `net_dns_lookup`, `net_check_services`)
//! need no shell and run in pure Rust.
//!
//! ## Configuration (env vars — no hardcoded hosts or credentials)
//!   NET_SSH_HOST       — host to run ping/subnet-scan commands on (required for
//!                        net_ping and net_subnet_scan)
//!   NET_SSH_USER       — SSH user, default "root"
//!   NET_SSH_KEY_PATH   — path to the SSH private key (required for SSH tools)
//!   NET_SERVICES       — comma-separated `name=host:port` list used by
//!                        net_check_services (e.g. "Gitea=<internal-ip>:3000,..").
//!                        If unset, net_check_services returns NotConfigured.

use std::env;
use std::io::Read as IoRead;
use std::net::{TcpStream, ToSocketAddrs};
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

/// A single named service target for `net_check_services`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceTarget {
    name: String,
    host: String,
    port: u16,
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// SSH host that runs `ping`/subnet-scan commands — from `NET_SSH_HOST`.
    ssh_host: Option<String>,
    /// SSH user — from `NET_SSH_USER`, default "root".
    ssh_user: String,
    /// SSH private key path — from `NET_SSH_KEY_PATH`.
    ssh_key_path: Option<String>,
    /// Service targets for `net_check_services` — from `NET_SERVICES`.
    services: Vec<ServiceTarget>,
}

impl NetworkConfig {
    pub fn from_env() -> Self {
        let ssh_host = env::var("NET_SSH_HOST").ok().filter(|s| !s.is_empty());
        let ssh_user = env::var("NET_SSH_USER").unwrap_or_else(|_| "root".into());
        let ssh_key_path = env::var("NET_SSH_KEY_PATH").ok().filter(|s| !s.is_empty());
        let services = env::var("NET_SERVICES")
            .ok()
            .map(|raw| parse_services(&raw))
            .unwrap_or_default();

        Self {
            ssh_host,
            ssh_user,
            ssh_key_path,
            services,
        }
    }
}

/// Parse a `name=host:port,name=host:port` list into service targets.
/// Malformed entries are skipped.
fn parse_services(raw: &str) -> Vec<ServiceTarget> {
    raw.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (name, hostport) = entry.split_once('=')?;
            let (host, port) = hostport.rsplit_once(':')?;
            let name = name.trim();
            let host = host.trim();
            let port: u16 = port.trim().parse().ok()?;
            if name.is_empty() || host.is_empty() {
                return None;
            }
            Some(ServiceTarget {
                name: name.to_string(),
                host: host.to_string(),
                port,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Input validation (no shell metacharacters ever reach an SSH command)
// ---------------------------------------------------------------------------

/// Validate a host token destined for an SSH-executed command. Accepts only
/// characters that are safe in DNS hostnames / IP literals: ASCII alphanumerics,
/// `.`, `-`, and `:` (for IPv6). Rejects anything containing shell metacharacters,
/// whitespace, or control characters.
fn validate_host_token(host: &str) -> Result<(), ToolError> {
    if host.is_empty() {
        return Err(ToolError::InvalidArgument("host must not be empty".into()));
    }
    if host.len() > 253 {
        return Err(ToolError::InvalidArgument("host is too long".into()));
    }
    let ok = host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':');
    if !ok {
        return Err(ToolError::InvalidArgument(format!(
            "host '{host}' contains disallowed characters (only alphanumerics, '.', '-', ':' permitted)"
        )));
    }
    Ok(())
}

/// Validate a subnet prefix like "10.0.0" — between one and three dotted
/// numeric octets, each 0-255, no metacharacters.
fn validate_subnet_prefix(prefix: &str) -> Result<(), ToolError> {
    if prefix.is_empty() {
        return Err(ToolError::InvalidArgument(
            "subnet_prefix must not be empty".into(),
        ));
    }
    let parts: Vec<&str> = prefix.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(ToolError::InvalidArgument(
            "subnet_prefix must be one to three dotted octets (e.g. '10.0.0')".into(),
        ));
    }
    for part in parts {
        let octet: u32 = part.parse().map_err(|_| {
            ToolError::InvalidArgument(format!("subnet_prefix octet '{part}' is not a number"))
        })?;
        if octet > 255 {
            return Err(ToolError::InvalidArgument(format!(
                "subnet_prefix octet '{part}' exceeds 255"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SSH helper (synchronous — wrapped in spawn_blocking by async callers)
// ---------------------------------------------------------------------------

/// Open an SSH session to the configured diagnostics host, run a single command,
/// and return (stdout, exit_status). The `command` must be built only from
/// validated tokens — callers are responsible for ensuring no raw user input.
fn ssh_exec(config: &NetworkConfig, command: &str) -> Result<(String, i32), ToolError> {
    let host = config
        .ssh_host
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("NET_SSH_HOST is not set".into()))?;
    let key_path = config
        .ssh_key_path
        .as_deref()
        .ok_or_else(|| ToolError::NotConfigured("NET_SSH_KEY_PATH is not set".into()))?;

    let addr = format!("{host}:22");
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        warn!("network: cannot reach diagnostics host {host}: {e}");
        ToolError::Execution("The diagnostics server is unreachable.".into())
    })?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(40)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(40)));

    let mut sess = Session::new().map_err(|e| {
        warn!("network: session init failed: {e}");
        ToolError::Execution("Could not complete the diagnostic check.".into())
    })?;
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        warn!("network: handshake failed with {host}: {e}");
        ToolError::Execution("The diagnostics server is unreachable.".into())
    })?;

    sess.userauth_pubkey_file(&config.ssh_user, None, key_path.as_ref(), None)
        .map_err(|e| {
            warn!("network: auth failed for {}@{host}: {e}", config.ssh_user);
            ToolError::Execution("Could not connect to the diagnostics server.".into())
        })?;

    if !sess.authenticated() {
        warn!("network: authentication failed for {}@{host}", config.ssh_user);
        return Err(ToolError::Execution(
            "Could not connect to the diagnostics server.".into(),
        ));
    }

    let mut channel = sess.channel_session().map_err(|e| {
        warn!("network: channel open failed on {host}: {e}");
        ToolError::Execution("Could not complete the diagnostic check.".into())
    })?;

    debug!("network ssh_exec: {command}");
    channel.exec(command).map_err(|e| {
        warn!("network: command exec failed on {host}: {e}");
        ToolError::Execution("Could not complete the diagnostic check.".into())
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        warn!("network: read failed from {host}: {e}");
        ToolError::Execution("Could not complete the diagnostic check.".into())
    })?;

    channel.wait_close().ok();
    let exit_status = channel.exit_status().unwrap_or(-1);
    if exit_status != 0 {
        warn!("network ssh_exec exit status {exit_status} for: {command}");
    }
    Ok((output, exit_status))
}

// ---------------------------------------------------------------------------
// ping output parsing
// ---------------------------------------------------------------------------

/// Extract the "packets transmitted" stats line and the rtt summary line from
/// `ping` output, matching the Python parsing.
fn parse_ping_output(stdout: &str) -> (String, String) {
    let mut stats_line = String::new();
    let mut rtt_line = String::new();
    for line in stdout.lines() {
        if line.contains("packets transmitted") {
            stats_line = line.trim().to_string();
        }
        if line.contains("rtt") || line.contains("round-trip") {
            rtt_line = line.trim().to_string();
        }
    }
    (stats_line, rtt_line)
}

/// Parse the host lines emitted by the subnet-scan command. Each non-empty line
/// is an IP that responded. Results are sorted by final octet (matching Python).
fn parse_subnet_hosts(stdout: &str) -> Vec<String> {
    let mut hosts: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    hosts.sort_by_key(|ip| {
        ip.rsplit_once('.')
            .and_then(|(_, last)| last.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    hosts.dedup();
    hosts
}

// ---------------------------------------------------------------------------
// Pure-Rust TCP port check
// ---------------------------------------------------------------------------

/// Attempt a TCP connection to `host:port` with the given timeout.
/// Returns Ok(true) if the port is open, Ok(false) if closed/filtered, and
/// Err only when the host cannot be resolved.
fn tcp_port_open(host: &str, port: u16, timeout: Duration) -> Result<bool, ToolError> {
    let addr = format!("{host}:{port}");
    let mut addrs = match addr.to_socket_addrs() {
        Ok(a) => a,
        Err(_) => return Err(ToolError::InvalidArgument("DNS resolution failed".into())),
    };
    let sock_addr = match addrs.next() {
        Some(a) => a,
        None => return Err(ToolError::InvalidArgument("DNS resolution failed".into())),
    };
    match TcpStream::connect_timeout(&sock_addr, timeout) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// Tool: net_ping
// ---------------------------------------------------------------------------

pub struct NetPing {
    config: Arc<NetworkConfig>,
}

#[async_trait]
impl RustTool for NetPing {
    fn name(&self) -> &str {
        "net_ping"
    }

    fn description(&self) -> &str {
        "Ping a host and return latency statistics. Args: host (IP or hostname), \
         count (number of pings, default 3, max 10). Executes a fixed ping command \
         on the configured diagnostics host via SSH."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "description": "IP address or hostname to ping" },
                "count": { "type": "integer", "description": "Number of pings (default 3, max 10)", "default": 3 }
            },
            "required": ["host"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let host = args
            .get("host")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("host is required".into()))?
            .to_string();
        validate_host_token(&host)?;

        let count = args.get("count").and_then(Value::as_u64).unwrap_or(3);
        let count = count.clamp(1, 10);

        // Fixed-form command; host validated above, count is numeric.
        let command = format!("ping -c {count} -W 3 {host}");

        let cfg = Arc::clone(&self.config);
        let host_for_result = host.clone();
        let (stdout, exit) = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

        let (stats, rtt) = parse_ping_output(&stdout);
        let result = json!({
            "host": host_for_result,
            "reachable": exit == 0,
            "stats": stats,
            "rtt": rtt,
            "output": stdout.trim(),
        });
        Ok(result.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: net_port_check
// ---------------------------------------------------------------------------

pub struct NetPortCheck;

#[async_trait]
impl RustTool for NetPortCheck {
    fn name(&self) -> &str {
        "net_port_check"
    }

    fn description(&self) -> &str {
        "Check if a TCP port is open on a host. Args: host (IP or hostname), \
         port (TCP port number), timeout (connection timeout seconds, default 3)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "description": "IP address or hostname" },
                "port": { "type": "integer", "description": "TCP port number" },
                "timeout": { "type": "integer", "description": "Connection timeout in seconds (default 3)", "default": 3 }
            },
            "required": ["host", "port"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let host = args
            .get("host")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("host is required".into()))?
            .to_string();

        let port_raw = args
            .get("port")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::InvalidArgument("port is required".into()))?;
        if port_raw == 0 || port_raw > 65535 {
            return Err(ToolError::InvalidArgument(
                "port must be between 1 and 65535".into(),
            ));
        }
        let port = port_raw as u16;

        let timeout_secs = args.get("timeout").and_then(Value::as_u64).unwrap_or(3);
        let timeout = Duration::from_secs(timeout_secs.clamp(1, 60));

        let host_blocking = host.clone();
        let result = tokio::task::spawn_blocking(move || {
            tcp_port_open(&host_blocking, port, timeout)
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

        match result {
            Ok(open) => Ok(json!({
                "host": host,
                "port": port,
                "open": open,
                "status": if open { "open" } else { "closed/filtered" },
            })
            .to_string()),
            Err(ToolError::InvalidArgument(_)) => Ok(json!({
                "host": host,
                "port": port,
                "open": false,
                "error": "DNS resolution failed",
            })
            .to_string()),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: net_dns_lookup
// ---------------------------------------------------------------------------

pub struct NetDnsLookup;

#[async_trait]
impl RustTool for NetDnsLookup {
    fn name(&self) -> &str {
        "net_dns_lookup"
    }

    fn description(&self) -> &str {
        "Resolve a hostname via the system DNS resolver. Args: hostname. \
         Returns all resolved IP addresses, sorted and de-duplicated."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "hostname": { "type": "string", "description": "Hostname to resolve (e.g. 'search.example.com')" }
            },
            "required": ["hostname"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let hostname = args
            .get("hostname")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("hostname is required".into()))?
            .to_string();

        let hostname_blocking = hostname.clone();
        // Port 0 lets getaddrinfo resolve name-only (matches Python getaddrinfo(host, None)).
        let resolve = tokio::task::spawn_blocking(move || {
            (hostname_blocking.as_str(), 0u16)
                .to_socket_addrs()
                .map(|iter| {
                    let mut ips: Vec<String> =
                        iter.map(|sa| sa.ip().to_string()).collect();
                    ips.sort();
                    ips.dedup();
                    ips
                })
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

        match resolve {
            Ok(addresses) => Ok(json!({
                "hostname": hostname,
                "resolved": true,
                "addresses": addresses,
            })
            .to_string()),
            Err(e) => Ok(json!({
                "hostname": hostname,
                "resolved": false,
                "error": e.to_string(),
            })
            .to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: net_subnet_scan
// ---------------------------------------------------------------------------

pub struct NetSubnetScan {
    config: Arc<NetworkConfig>,
}

#[async_trait]
impl RustTool for NetSubnetScan {
    fn name(&self) -> &str {
        "net_subnet_scan"
    }

    fn description(&self) -> &str {
        "Quick parallel ping sweep of an IP range to find responding hosts. \
         Args: subnet_prefix (first three octets, default '10.0.0'), \
         start (first host octet, default 1), end (last host octet, default 254). \
         Runs on the configured diagnostics host via SSH. Limited to 254 hosts."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subnet_prefix": { "type": "string", "description": "First three octets (default '10.0.0')", "default": "10.0.0" },
                "start": { "type": "integer", "description": "First host octet to scan (default 1)", "default": 1 },
                "end": { "type": "integer", "description": "Last host octet to scan (default 254)", "default": 254 }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let subnet_prefix = args
            .get("subnet_prefix")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("10.0.0")
            .to_string();
        validate_subnet_prefix(&subnet_prefix)?;

        let start = args.get("start").and_then(Value::as_u64).unwrap_or(1);
        let mut end = args.get("end").and_then(Value::as_u64).unwrap_or(254);

        if start > 255 || end > 255 {
            return Err(ToolError::InvalidArgument(
                "start and end must be between 0 and 255".into(),
            ));
        }
        if start > end {
            return Err(ToolError::InvalidArgument("start must be <= end".into()));
        }
        // Match Python's cap of 254 hosts per scan.
        if end - start > 254 {
            end = start + 254;
        }

        // All tokens numeric / validated — safe to interpolate.
        let command = format!(
            "for i in $(seq {start} {end}); do (ping -c 1 -W 1 {subnet_prefix}.$i >/dev/null 2>&1 && echo {subnet_prefix}.$i) & done; wait"
        );

        let cfg = Arc::clone(&self.config);
        let range_label = format!("{subnet_prefix}.{start}-{end}");
        let (stdout, _exit) = tokio::task::spawn_blocking(move || ssh_exec(&cfg, &command))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))??;

        let hosts = parse_subnet_hosts(&stdout);
        Ok(json!({
            "subnet": range_label,
            "hosts_up": hosts.len(),
            "hosts": hosts,
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: net_check_services
// ---------------------------------------------------------------------------

pub struct NetCheckServices {
    config: Arc<NetworkConfig>,
}

#[async_trait]
impl RustTool for NetCheckServices {
    fn name(&self) -> &str {
        "net_check_services"
    }

    fn description(&self) -> &str {
        "Quick health check of known MooseNet services via TCP reachability. \
         Service targets come from the NET_SERVICES env var \
         (format: 'Name=host:port,Name2=host:port'). Returns up/down per service."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        if self.config.services.is_empty() {
            return Err(ToolError::NotConfigured(
                "NET_SERVICES is not set (format: 'Name=host:port,Name2=host:port')".into(),
            ));
        }

        let services = self.config.services.clone();
        let results = tokio::task::spawn_blocking(move || {
            let timeout = Duration::from_secs(2);
            let mut out = Vec::with_capacity(services.len());
            for svc in &services {
                let ok = tcp_port_open(&svc.host, svc.port, timeout).unwrap_or(false);
                out.push(json!({
                    "service": svc.name,
                    "host": format!("{}:{}", svc.host, svc.port),
                    "status": if ok { "up" } else { "down" },
                }));
            }
            out
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {e}")))?;

        let up = results
            .iter()
            .filter(|r| r.get("status").and_then(Value::as_str) == Some("up"))
            .count();
        let total = results.len();

        Ok(json!({
            "total": total,
            "up": up,
            "down": total - up,
            "services": results,
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(NetworkConfig::from_env());

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(NetPing { config: Arc::clone(&config) }),
        Box::new(NetPortCheck),
        Box::new(NetDnsLookup),
        Box::new(NetSubnetScan { config: Arc::clone(&config) }),
        Box::new(NetCheckServices { config: Arc::clone(&config) }),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            error!("network: failed to register tool: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (no network / no SSH — validation, parsing, building, registration)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_config() -> Arc<NetworkConfig> {
        Arc::new(NetworkConfig {
            ssh_host: None,
            ssh_user: "root".into(),
            ssh_key_path: None,
            services: Vec::new(),
        })
    }

    // ── infra-leak genericization ───────────────────────────────────────────

    #[test]
    fn test_ssh_exec_unreachable_error_is_generic() {
        // 127.0.0.1:22 — fails fast at connect/handshake/auth; no real key.
        let cfg = NetworkConfig {
            ssh_host: Some("127.0.0.1".into()),
            ssh_user: "root".into(),
            ssh_key_path: Some("/tmp/nonexistent-key".into()),
            services: Vec::new(),
        };
        let msg = match ssh_exec(&cfg, "true").expect_err("unroutable host must error") {
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

    // ── host token validation ───────────────────────────────────────────────

    #[test]
    fn validate_host_token_accepts_ip_and_hostname() {
        assert!(validate_host_token("<internal-ip>").is_ok());
        assert!(validate_host_token("search.example.com").is_ok());
        assert!(validate_host_token("host-1").is_ok());
        assert!(validate_host_token("fe80::1").is_ok());
    }

    #[test]
    fn validate_host_token_rejects_shell_metacharacters() {
        assert!(validate_host_token("1.1.1.1; rm -rf /").is_err());
        assert!(validate_host_token("$(whoami)").is_err());
        assert!(validate_host_token("a`b`").is_err());
        assert!(validate_host_token("a b").is_err());
        assert!(validate_host_token("a|b").is_err());
        assert!(validate_host_token("a&b").is_err());
        assert!(validate_host_token("").is_err());
    }

    // ── subnet prefix validation ────────────────────────────────────────────

    #[test]
    fn validate_subnet_prefix_accepts_valid() {
        assert!(validate_subnet_prefix("10.0.0").is_ok());
        assert!(validate_subnet_prefix("10.0.0").is_ok());
        assert!(validate_subnet_prefix("172").is_ok());
        assert!(validate_subnet_prefix("10.1").is_ok());
    }

    #[test]
    fn validate_subnet_prefix_rejects_invalid() {
        assert!(validate_subnet_prefix("<internal-ip>").is_err()); // 4 octets
        assert!(validate_subnet_prefix("999.0.0").is_err()); // >255
        assert!(validate_subnet_prefix("10.0.x").is_err()); // non-numeric
        assert!(validate_subnet_prefix("10.0.0; ls").is_err());
        assert!(validate_subnet_prefix("").is_err());
    }

    // ── service list parsing ────────────────────────────────────────────────

    #[test]
    fn parse_services_parses_well_formed_list() {
        let svcs = parse_services("Gitea=<internal-ip>:3000,Plane=<internal-ip>:80");
        assert_eq!(svcs.len(), 2);
        assert_eq!(svcs[0].name, "Gitea");
        assert_eq!(svcs[0].host, "<internal-ip>");
        assert_eq!(svcs[0].port, 3000);
        assert_eq!(svcs[1].name, "Plane");
        assert_eq!(svcs[1].port, 80);
    }

    #[test]
    fn parse_services_skips_malformed_entries() {
        let svcs = parse_services("Good=<internal-ip>:22, ,Bad,NoPort=<internal-ip>,BadPort=<internal-ip>:abc");
        assert_eq!(svcs.len(), 1);
        assert_eq!(svcs[0].name, "Good");
        assert_eq!(svcs[0].port, 22);
    }

    #[test]
    fn parse_services_handles_whitespace() {
        let svcs = parse_services(" Web = <internal-ip> : 8080 ");
        assert_eq!(svcs.len(), 1);
        assert_eq!(svcs[0].name, "Web");
        assert_eq!(svcs[0].host, "<internal-ip>");
        assert_eq!(svcs[0].port, 8080);
    }

    // ── ping output parsing ─────────────────────────────────────────────────

    #[test]
    fn parse_ping_output_extracts_stats_and_rtt() {
        let sample = "\
PING <internal-ip> (<internal-ip>) 56(84) bytes of data.
64 bytes from <internal-ip>: icmp_seq=1 ttl=64 time=0.45 ms
64 bytes from <internal-ip>: icmp_seq=2 ttl=64 time=0.50 ms

--- <internal-ip> ping statistics ---
3 packets transmitted, 3 received, 0% packet loss, time 2003ms
rtt min/avg/max/mdev = 0.450/0.475/0.500/0.025 ms";
        let (stats, rtt) = parse_ping_output(sample);
        assert!(stats.contains("3 packets transmitted"));
        assert!(rtt.contains("rtt min/avg/max"));
    }

    #[test]
    fn parse_ping_output_handles_unreachable() {
        let sample = "\
PING <internal-ip> (<internal-ip>) 56(84) bytes of data.

--- <internal-ip> ping statistics ---
3 packets transmitted, 0 received, 100% packet loss, time 2034ms";
        let (stats, rtt) = parse_ping_output(sample);
        assert!(stats.contains("100% packet loss"));
        assert_eq!(rtt, "");
    }

    // ── subnet host parsing ─────────────────────────────────────────────────

    #[test]
    fn parse_subnet_hosts_sorts_by_last_octet() {
        let sample = "<internal-ip>\n<internal-ip>\n<internal-ip>\n\n<internal-ip>\n";
        let hosts = parse_subnet_hosts(sample);
        assert_eq!(
            hosts,
            vec![
                "<internal-ip>",
                "<internal-ip>",
                "<internal-ip>",
                "<internal-ip>",
            ]
        );
    }

    #[test]
    fn parse_subnet_hosts_empty_input() {
        assert!(parse_subnet_hosts("\n\n  \n").is_empty());
    }

    // ── command building (no SSH execution) ─────────────────────────────────

    #[test]
    fn ping_command_is_fixed_form() {
        let host = "<internal-ip>";
        let count = 5u64;
        let cmd = format!("ping -c {count} -W 3 {host}");
        assert_eq!(cmd, "ping -c 5 -W 3 <internal-ip>");
        assert!(!cmd.contains(';'));
        assert!(!cmd.contains("$("));
        assert!(!cmd.contains('`'));
    }

    #[test]
    fn subnet_command_uses_validated_tokens() {
        let prefix = "10.0.0";
        let (start, end) = (1u64, 5u64);
        let cmd = format!(
            "for i in $(seq {start} {end}); do (ping -c 1 -W 1 {prefix}.$i >/dev/null 2>&1 && echo {prefix}.$i) & done; wait"
        );
        assert!(cmd.contains("seq 1 5"));
        assert!(cmd.contains("10.0.0.$i"));
    }

    // ── net_ping arg validation (no SSH host → NotConfigured for valid host) ─

    #[tokio::test]
    async fn net_ping_missing_host_is_invalid_argument() {
        let tool = NetPing { config: empty_config() };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn net_ping_malicious_host_rejected_before_ssh() {
        let tool = NetPing { config: empty_config() };
        let result = tool.execute(json!({"host": "1.1.1.1; rm -rf /"})).await;
        match result {
            Err(ToolError::InvalidArgument(msg)) => assert!(msg.contains("disallowed")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn net_ping_valid_host_no_ssh_config_is_not_configured() {
        // Valid host passes validation; then SSH config is missing → NotConfigured.
        let tool = NetPing { config: empty_config() };
        let result = tool.execute(json!({"host": "<internal-ip>", "count": 2})).await;
        match result {
            Err(ToolError::NotConfigured(msg)) => assert!(msg.contains("NET_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // ── net_port_check arg validation ───────────────────────────────────────

    #[tokio::test]
    async fn net_port_check_missing_host_is_invalid() {
        let tool = NetPortCheck;
        let result = tool.execute(json!({"port": 80})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn net_port_check_missing_port_is_invalid() {
        let tool = NetPortCheck;
        let result = tool.execute(json!({"host": "<internal-ip>"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn net_port_check_out_of_range_port_is_invalid() {
        let tool = NetPortCheck;
        let result = tool.execute(json!({"host": "<internal-ip>", "port": 70000})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
        let result0 = tool.execute(json!({"host": "<internal-ip>", "port": 0})).await;
        assert!(matches!(result0, Err(ToolError::InvalidArgument(_))));
    }

    // ── net_dns_lookup arg validation ───────────────────────────────────────

    #[tokio::test]
    async fn net_dns_lookup_missing_hostname_is_invalid() {
        let tool = NetDnsLookup;
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn net_dns_lookup_localhost_resolves() {
        // "localhost" resolves without any network — validates the success-shape
        // mapping (sorted, deduped addresses) deterministically.
        let tool = NetDnsLookup;
        let result = tool
            .execute(json!({"hostname": "localhost"}))
            .await
            .expect("localhost resolution should not error");
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["hostname"], "localhost");
        assert_eq!(v["resolved"], true);
        let addrs = v["addresses"].as_array().expect("addresses array");
        assert!(!addrs.is_empty(), "localhost should resolve to at least one address");
        // Result is sorted and de-duplicated.
        let strs: Vec<String> = addrs.iter().map(|a| a.as_str().unwrap().to_string()).collect();
        let mut sorted = strs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(strs, sorted, "addresses must be sorted and deduped");
    }

    // ── net_subnet_scan arg validation ──────────────────────────────────────

    #[tokio::test]
    async fn net_subnet_scan_bad_prefix_is_invalid() {
        let tool = NetSubnetScan { config: empty_config() };
        let result = tool.execute(json!({"subnet_prefix": "<internal-ip>"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn net_subnet_scan_start_gt_end_is_invalid() {
        let tool = NetSubnetScan { config: empty_config() };
        let result = tool
            .execute(json!({"subnet_prefix": "10.0.0", "start": 100, "end": 5}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn net_subnet_scan_valid_no_ssh_is_not_configured() {
        let tool = NetSubnetScan { config: empty_config() };
        let result = tool
            .execute(json!({"subnet_prefix": "10.0.0", "start": 1, "end": 5}))
            .await;
        match result {
            Err(ToolError::NotConfigured(msg)) => assert!(msg.contains("NET_SSH_HOST")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // ── net_check_services ──────────────────────────────────────────────────

    #[tokio::test]
    async fn net_check_services_no_config_is_not_configured() {
        let tool = NetCheckServices { config: empty_config() };
        let result = tool.execute(json!({})).await;
        match result {
            Err(ToolError::NotConfigured(msg)) => assert!(msg.contains("NET_SERVICES")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // ── registration ────────────────────────────────────────────────────────

    #[test]
    fn register_adds_five_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 5);
    }

    #[test]
    fn register_all_expected_names_present() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        for name in [
            "net_ping",
            "net_port_check",
            "net_dns_lookup",
            "net_subnet_scan",
            "net_check_services",
        ] {
            assert!(reg.contains(name), "missing tool {name}");
        }
    }

    #[test]
    fn tool_parameters_are_objects() {
        let cfg = empty_config();
        assert_eq!(NetPing { config: Arc::clone(&cfg) }.parameters()["type"], "object");
        assert_eq!(NetPortCheck.parameters()["type"], "object");
        assert_eq!(NetDnsLookup.parameters()["type"], "object");
        assert_eq!(NetSubnetScan { config: Arc::clone(&cfg) }.parameters()["type"], "object");
        assert_eq!(NetCheckServices { config: cfg }.parameters()["type"], "object");
    }

    #[test]
    fn tool_names_are_stable() {
        let cfg = empty_config();
        assert_eq!(NetPing { config: Arc::clone(&cfg) }.name(), "net_ping");
        assert_eq!(NetPortCheck.name(), "net_port_check");
        assert_eq!(NetDnsLookup.name(), "net_dns_lookup");
        assert_eq!(NetSubnetScan { config: Arc::clone(&cfg) }.name(), "net_subnet_scan");
        assert_eq!(NetCheckServices { config: cfg }.name(), "net_check_services");
    }
}
