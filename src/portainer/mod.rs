//! <container-mgr> tools — read-only Docker container management queries via the // pii-test-fixture
//! <container-mgr> API on an internal host (self-signed TLS). // pii-test-fixture
//!
//! Four tools mirroring the Python portainer_tools.py on the MCP hub host exactly:
//!   portainer_status            — <container-mgr> server health and version // pii-test-fixture
//!   portainer_list_environments — list managed Docker endpoints
//!   portainer_list_containers   — list containers in an environment
//!   portainer_container_logs    — tail logs from a specific container
//!
//! Required env vars:
//!   PORTAINER_URL        — e.g. https://<container-mgr>.example:9443 (self-signed cert) // pii-test-fixture
//!   PORTAINER_API_TOKEN  — <container-mgr> access token (sent as X-API-Key header) // pii-test-fixture
//!
//! If either is unset, register() registers stub tools that return a clear
//! NotConfigured error.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct PortainerConfig {
    url: String,
    token: String,
}

impl PortainerConfig {
    fn from_env() -> Result<Self, ToolError> {
        let url = std::env::var("PORTAINER_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("PORTAINER_URL not set".into()))?;
        let token = std::env::var("PORTAINER_API_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("PORTAINER_API_TOKEN not set".into()))?;
        Ok(Self { url, token })
    }

    /// <container-mgr> runs with a self-signed cert on an internal host, so we accept // pii-test-fixture
    /// invalid certs for this client only (matches the Python ssl.CERT_NONE).
    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// Authenticated GET against the <container-mgr> API. `path` begins with `/` // pii-test-fixture
    /// (e.g. "/status") and is appended after the "/api" prefix.
    async fn api_get(
        &self,
        client: &reqwest::Client,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, ToolError> {
        let url = format!("{}/api{}", self.url, path);
        let resp = client
            .get(&url)
            .header("Accept", "application/json")
            .header("X-API-Key", &self.token)
            .query(query)
            .send()
            .await
            .map_err(|e| {
                warn!("<container-mgr>: request to {url} failed: {e}"); // pii-test-fixture
                ToolError::Http("The container service (<container-mgr>) is unreachable.".into()) // pii-test-fixture
            })?;

        let status = resp.status();
        let raw = resp.text().await.map_err(|e| {
            warn!("<container-mgr>: reading response from {url} failed: {e}"); // pii-test-fixture
            ToolError::Http("The container service (<container-mgr>) is unreachable.".into()) // pii-test-fixture
        })?;
        if !status.is_success() {
            return Err(ToolError::Http(format!(
                "<container-mgr> HTTP {status}: {}", // pii-test-fixture
                raw.chars().take(200).collect::<String>()
            )));
        }
        if raw.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&raw).map_err(|e| ToolError::Http(format!("Bad JSON: {e}")))
    }
}

// ── Parsing helpers (network-free, unit-tested) ─────────────────────────────────

/// Parse the /status response into the trimmed shape the Python returns.
fn parse_status(body: &Value) -> Value {
    json!({
        "version":     body.get("Version").and_then(Value::as_str).unwrap_or("unknown"),
        "instance_id": body.get("InstanceID").and_then(Value::as_str).unwrap_or(""),
        "healthy":     true,
    })
}

/// Parse the /endpoints array into the trimmed environment list.
fn parse_environments(body: &Value) -> Result<Value, ToolError> {
    let arr = body
        .as_array()
        .ok_or_else(|| ToolError::Http("Unexpected response format".into()))?;
    let envs: Vec<Value> = arr
        .iter()
        .map(|e| {
            json!({
                "id":        e.get("Id"),
                "name":      e.get("Name").and_then(Value::as_str).unwrap_or("unknown"),
                "url":       e.get("URL").and_then(Value::as_str).unwrap_or(""),
                "type":      e.get("Type").and_then(Value::as_u64).unwrap_or(0),
                "status":    if e.get("Status").and_then(Value::as_u64) == Some(1) { "up" } else { "down" },
                "snapshots": e.get("Snapshots").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
            })
        })
        .collect();
    Ok(json!({ "count": envs.len(), "environments": envs }))
}

/// Extract the first endpoint Id from an /endpoints array (default 1).
fn first_environment_id(body: &Value) -> i64 {
    body.as_array()
        .and_then(|a| a.first())
        .and_then(|e| e.get("Id"))
        .and_then(Value::as_i64)
        .unwrap_or(1)
}

/// Parse a docker `containers/json` array into the trimmed, sorted summary.
fn parse_containers(environment_id: i64, body: &Value) -> Result<Value, ToolError> {
    let arr = body
        .as_array()
        .ok_or_else(|| ToolError::Http("Unexpected response format".into()))?;

    let mut containers: Vec<Value> = arr
        .iter()
        .map(|c| {
            let names: Vec<String> = c
                .get("Names")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|n| n.trim_start_matches('/').to_string())
                        .collect()
                })
                .unwrap_or_default();

            let id = c.get("Id").and_then(Value::as_str).unwrap_or("");
            let short_id: String = id.chars().take(12).collect();

            let ports: Vec<String> = c
                .get("Ports")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|p| {
                            let public = p.get("PublicPort").and_then(Value::as_u64)?;
                            let private = p.get("PrivatePort").and_then(Value::as_u64).unwrap_or(0);
                            let typ = p.get("Type").and_then(Value::as_str).unwrap_or("tcp");
                            Some(format!("{public}->{private}/{typ}"))
                        })
                        .collect()
                })
                .unwrap_or_default();

            let name = names
                .first()
                .cloned()
                .unwrap_or_else(|| short_id.clone());

            json!({
                "name":   name,
                "image":  c.get("Image").and_then(Value::as_str).unwrap_or("unknown"),
                "state":  c.get("State").and_then(Value::as_str).unwrap_or("unknown"),
                "status": c.get("Status").and_then(Value::as_str).unwrap_or(""),
                "ports":  ports,
                "id":     short_id,
            })
        })
        .collect();

    // running first, then by name (matches the Python sort key).
    containers.sort_by(|a, b| {
        let ak = if a["state"] == "running" { 0 } else { 1 };
        let bk = if b["state"] == "running" { 0 } else { 1 };
        ak.cmp(&bk).then_with(|| {
            a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
        })
    });

    let running = containers.iter().filter(|c| c["state"] == "running").count();
    Ok(json!({
        "environment_id": environment_id,
        "total":          containers.len(),
        "running":        running,
        "stopped":        containers.len() - running,
        "containers":     containers,
    }))
}

/// Clean a Docker multiplexed log stream into plain text lines, stripping the
/// 8-byte frame header when present (matches the Python heuristic, which drops
/// the leading 8 chars of any line longer than 8 chars whose first byte is a
/// stream marker 0x00/0x01/0x02).
fn parse_logs(container_id: &str, tail: u64, raw: &str) -> Value {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let cleaned = line.trim();
        if cleaned.is_empty() {
            continue;
        }
        let out = if cleaned.chars().count() > 8 {
            let first = cleaned.as_bytes()[0];
            if first == 0x00 || first == 0x01 || first == 0x02 {
                cleaned.chars().skip(8).collect::<String>()
            } else {
                cleaned.to_string()
            }
        } else {
            cleaned.to_string()
        };
        lines.push(out);
    }
    let tail_n = tail as usize;
    let start = lines.len().saturating_sub(tail_n);
    let tailed = &lines[start..];
    json!({
        "container": container_id,
        "lines":     tailed.len(),
        "tail":      tail,
        "logs":      tailed.join("\n"),
    })
}

// ── Tool structs ────────────────────────────────────────────────────────────────

struct PortainerStatus           { cfg: PortainerConfig }
struct PortainerListEnvironments { cfg: PortainerConfig }
struct PortainerListContainers   { cfg: PortainerConfig }
struct PortainerContainerLogs    { cfg: PortainerConfig }

#[async_trait]
impl RustTool for PortainerStatus {
    fn name(&self) -> &str { "portainer_status" }

    fn description(&self) -> &str {
        "Check <container-mgr> server health and version." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = PortainerConfig::client()?;
        let body = self.cfg.api_get(&client, "/status", &[]).await?;
        Ok(parse_status(&body).to_string())
    }
}

#[async_trait]
impl RustTool for PortainerListEnvironments {
    fn name(&self) -> &str { "portainer_list_environments" }

    fn description(&self) -> &str {
        "List all Docker environments (endpoints) managed by <container-mgr>. Returns each environment's name, ID, URL, and status." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = PortainerConfig::client()?;
        let body = self.cfg.api_get(&client, "/endpoints", &[]).await?;
        Ok(parse_environments(&body)?.to_string())
    }
}

#[async_trait]
impl RustTool for PortainerListContainers {
    fn name(&self) -> &str { "portainer_list_containers" }

    fn description(&self) -> &str {
        "List Docker containers in a <container-mgr> environment. environment_id is the <container-mgr> endpoint ID (0 = auto-detect the first environment). all_containers (default true) includes stopped containers. Returns name, image, state, status, ports." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "environment_id": { "type": "integer", "description": "<container-mgr> endpoint ID; 0 = auto-detect first environment (default 0)" }, // pii-test-fixture
                "all_containers": { "type": "boolean", "description": "Include stopped containers (default true)" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = PortainerConfig::client()?;
        let mut environment_id = args.get("environment_id").and_then(Value::as_i64).unwrap_or(0);
        let all_containers = args.get("all_containers").and_then(Value::as_bool).unwrap_or(true);

        if environment_id == 0 {
            let envs = self.cfg.api_get(&client, "/endpoints", &[]).await?;
            environment_id = first_environment_id(&envs);
        }

        let path = format!("/endpoints/{environment_id}/docker/containers/json");
        let query: &[(&str, &str)] = if all_containers { &[("all", "true")] } else { &[] };
        let body = self.cfg.api_get(&client, &path, query).await?;
        Ok(parse_containers(environment_id, &body)?.to_string())
    }
}

#[async_trait]
impl RustTool for PortainerContainerLogs {
    fn name(&self) -> &str { "portainer_container_logs" }

    fn description(&self) -> &str {
        "Get logs from a Docker container via <container-mgr>. container_id is the container ID or name. environment_id (0 = auto-detect first). tail = number of log lines from the end (default 100)." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "container_id":   { "type": "string",  "description": "Container ID or name (required)" },
                "environment_id": { "type": "integer", "description": "<container-mgr> endpoint ID; 0 = auto-detect first (default 0)" }, // pii-test-fixture
                "tail":           { "type": "integer", "description": "Number of log lines from the end (default 100)" }
            },
            "required": ["container_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let container_id = args
            .get("container_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("container_id is required".into()))?;
        let tail = args.get("tail").and_then(Value::as_u64).unwrap_or(100);
        let mut environment_id = args.get("environment_id").and_then(Value::as_i64).unwrap_or(0);

        let client = PortainerConfig::client()?;
        if environment_id == 0 {
            let envs = self.cfg.api_get(&client, "/endpoints", &[]).await?;
            environment_id = first_environment_id(&envs);
        }

        // Logs are a raw multiplexed stream, not JSON — fetch the text directly.
        let tail_s = tail.to_string();
        let path = format!("/endpoints/{environment_id}/docker/containers/{container_id}/logs");
        let url = format!("{}/api{}", self.cfg.url, path);
        let resp = client
            .get(&url)
            .header("X-API-Key", &self.cfg.token)
            .query(&[("stdout", "true"), ("stderr", "true"), ("tail", tail_s.as_str())])
            .send()
            .await
            .map_err(|e| {
                warn!("<container-mgr>: request to {url} failed: {e}"); // pii-test-fixture
                ToolError::Http("The container service (<container-mgr>) is unreachable.".into()) // pii-test-fixture
            })?;

        let status = resp.status();
        let raw = resp.text().await.map_err(|e| {
            warn!("<container-mgr>: reading response from {url} failed: {e}"); // pii-test-fixture
            ToolError::Http("The container service (<container-mgr>) is unreachable.".into()) // pii-test-fixture
        })?;
        if !status.is_success() {
            return Err(ToolError::Http(format!(
                "<container-mgr> HTTP {status}: {}", // pii-test-fixture
                raw.chars().take(200).collect::<String>()
            )));
        }
        Ok(parse_logs(container_id, tail, &raw).to_string())
    }
}

// ── Registration ────────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    match PortainerConfig::from_env() {
        Ok(cfg) => {
            registry.register_or_replace(Box::new(PortainerStatus { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(PortainerListEnvironments { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(PortainerListContainers { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(PortainerContainerLogs { cfg }));
        }
        Err(e) => {
            tracing::warn!("<container-mgr> tools not configured: {e}. Registering stubs."); // pii-test-fixture
            registry.register_or_replace(Box::new(NotConfiguredStub("portainer_status")));
            registry.register_or_replace(Box::new(NotConfiguredStub("portainer_list_environments")));
            registry.register_or_replace(Box::new(NotConfiguredStub("portainer_list_containers")));
            registry.register_or_replace(Box::new(NotConfiguredStub("portainer_container_logs")));
        }
    }
}

struct NotConfiguredStub(&'static str);

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str { self.0 }
    fn description(&self) -> &str { "<container-mgr> tool (PORTAINER_URL / PORTAINER_API_TOKEN not configured)" } // pii-test-fixture
    fn parameters(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured(
            "PORTAINER_URL and PORTAINER_API_TOKEN must be set".into(),
        ))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cfg() -> PortainerConfig {
        PortainerConfig { url: "https://<container-mgr>.test:9443".into(), token: "tok".into() } // pii-test-fixture
    }

    // ── config ─────────────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn from_env_missing_returns_not_configured() {
        let url = std::env::var("PORTAINER_URL").ok();
        let tok = std::env::var("PORTAINER_API_TOKEN").ok();
        std::env::remove_var("PORTAINER_URL");
        std::env::remove_var("PORTAINER_API_TOKEN");
        let r = PortainerConfig::from_env();
        if let Some(v) = url { std::env::set_var("PORTAINER_URL", v); }
        if let Some(v) = tok { std::env::set_var("PORTAINER_API_TOKEN", v); }
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }

    // ── status parsing ───────────────────────────────────────────────────────────

    #[test]
    fn parse_status_extracts_version() {
        let body = json!({ "Version": "2.19.4", "InstanceID": "abc-123" });
        let out = parse_status(&body);
        assert_eq!(out["version"], "2.19.4");
        assert_eq!(out["instance_id"], "abc-123");
        assert_eq!(out["healthy"], true);
    }

    #[test]
    fn parse_status_defaults_when_missing() {
        let out = parse_status(&json!({}));
        assert_eq!(out["version"], "unknown");
        assert_eq!(out["instance_id"], "");
    }

    // ── environments parsing ───────────────────────────────────────────────────────

    #[test]
    fn parse_environments_maps_fields_and_status() {
        let body = json!([
            { "Id": 1, "Name": "local", "URL": "unix://", "Type": 1, "Status": 1, "Snapshots": [{}] },
            { "Id": 2, "Name": "remote", "URL": "tcp://x", "Type": 2, "Status": 2, "Snapshots": [] }
        ]);
        let out = parse_environments(&body).unwrap();
        assert_eq!(out["count"], 2);
        assert_eq!(out["environments"][0]["name"], "local");
        assert_eq!(out["environments"][0]["status"], "up");
        assert_eq!(out["environments"][0]["snapshots"], 1);
        assert_eq!(out["environments"][1]["status"], "down");
        assert_eq!(out["environments"][1]["id"], 2);
    }

    #[test]
    fn parse_environments_rejects_non_array() {
        assert!(matches!(parse_environments(&json!({})), Err(ToolError::Http(_))));
    }

    #[test]
    fn first_environment_id_picks_first_or_default() {
        assert_eq!(first_environment_id(&json!([{ "Id": 7 }, { "Id": 9 }])), 7);
        assert_eq!(first_environment_id(&json!([])), 1);
        assert_eq!(first_environment_id(&json!({})), 1);
    }

    // ── containers parsing ─────────────────────────────────────────────────────────

    #[test]
    fn parse_containers_summary_and_sort() {
        let body = json!([
            {
                "Id": "zzzzzzzzzzzz0000",
                "Names": ["/stopped-b"],
                "Image": "img:1",
                "State": "exited",
                "Status": "Exited (0)",
                "Ports": []
            },
            {
                "Id": "aaaaaaaaaaaa1111",
                "Names": ["/running-a"],
                "Image": "img:2",
                "State": "running",
                "Status": "Up 2 hours",
                "Ports": [{ "PublicPort": 8080, "PrivatePort": 80, "Type": "tcp" }]
            }
        ]);
        let out = parse_containers(3, &body).unwrap();
        assert_eq!(out["environment_id"], 3);
        assert_eq!(out["total"], 2);
        assert_eq!(out["running"], 1);
        assert_eq!(out["stopped"], 1);
        // running sorts first
        assert_eq!(out["containers"][0]["name"], "running-a");
        assert_eq!(out["containers"][0]["ports"][0], "8080->80/tcp");
        assert_eq!(out["containers"][0]["id"], "aaaaaaaaaaaa");
        assert_eq!(out["containers"][1]["name"], "stopped-b");
    }

    #[test]
    fn parse_containers_falls_back_to_short_id_when_no_name() {
        let body = json!([
            { "Id": "abcdef1234567890", "Names": [], "State": "running" } // pii-test-fixture
        ]);
        let out = parse_containers(1, &body).unwrap();
        assert_eq!(out["containers"][0]["name"], "abcdef123456");
        assert_eq!(out["containers"][0]["image"], "unknown");
    }

    #[test]
    fn parse_containers_skips_ports_without_public_port() {
        let body = json!([
            {
                "Id": "x", "Names": ["/c"], "State": "running",
                "Ports": [{ "PrivatePort": 80, "Type": "tcp" }, { "PublicPort": 443, "PrivatePort": 443, "Type": "tcp" }]
            }
        ]);
        let out = parse_containers(1, &body).unwrap();
        let ports = out["containers"][0]["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0], "443->443/tcp");
    }

    #[test]
    fn parse_containers_rejects_non_array() {
        assert!(matches!(parse_containers(1, &json!({})), Err(ToolError::Http(_))));
    }

    // ── logs parsing ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_logs_tails_and_counts() {
        let raw = "line1\nline2\nline3\nline4";
        let out = parse_logs("c1", 2, raw);
        assert_eq!(out["container"], "c1");
        assert_eq!(out["tail"], 2);
        assert_eq!(out["lines"], 2);
        assert_eq!(out["logs"], "line3\nline4");
    }

    #[test]
    fn parse_logs_skips_blank_lines() {
        let raw = "a\n\n  \nb\n";
        let out = parse_logs("c", 100, raw);
        assert_eq!(out["lines"], 2);
        assert_eq!(out["logs"], "a\nb");
    }

    #[test]
    fn parse_logs_strips_stream_frame_header() {
        // First byte 0x01 (stdout marker) on a >8-char line → leading 8 chars
        // dropped (header byte + 7 more), matching the Python cleaned[8:] slice.
        // chars: [0x01,H,e,l,l,o,W,o,r,l,d] → skip(8) → "rld".
        let line = format!("{}HelloWorld", '\u{0001}');
        let out = parse_logs("c", 100, &line);
        assert_eq!(out["logs"], "rld");
    }

    #[test]
    fn parse_logs_keeps_plain_lines_unchanged() {
        let raw = "a plain log line over eight chars";
        let out = parse_logs("c", 100, raw);
        assert_eq!(out["logs"], "a plain log line over eight chars");
    }

    // ── tool metadata ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn container_logs_requires_container_id() {
        let t = PortainerContainerLogs { cfg: cfg() };
        assert!(matches!(
            t.execute(json!({})).await,
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            t.execute(json!({ "container_id": "  " })).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[test]
    fn tool_names_are_stable() {
        assert_eq!(PortainerStatus { cfg: cfg() }.name(), "portainer_status");
        assert_eq!(PortainerListEnvironments { cfg: cfg() }.name(), "portainer_list_environments");
        assert_eq!(PortainerListContainers { cfg: cfg() }.name(), "portainer_list_containers");
        assert_eq!(PortainerContainerLogs { cfg: cfg() }.name(), "portainer_container_logs");
    }

    #[test]
    fn tool_parameters_are_valid_json_schema() {
        assert_eq!(PortainerStatus { cfg: cfg() }.parameters()["type"], "object");
        assert_eq!(PortainerListEnvironments { cfg: cfg() }.parameters()["type"], "object");
        let lc = PortainerListContainers { cfg: cfg() }.parameters();
        assert_eq!(lc["type"], "object");
        let logs = PortainerContainerLogs { cfg: cfg() }.parameters();
        assert_eq!(logs["required"][0], "container_id");
    }

    #[test]
    #[serial]
    fn register_adds_four_tools_stub_path() {
        let mut reg = ToolRegistry::new();
        let url = std::env::var("PORTAINER_URL").ok();
        let tok = std::env::var("PORTAINER_API_TOKEN").ok();
        std::env::remove_var("PORTAINER_URL");
        std::env::remove_var("PORTAINER_API_TOKEN");
        register(&mut reg);
        if let Some(v) = url { std::env::set_var("PORTAINER_URL", v); }
        if let Some(v) = tok { std::env::set_var("PORTAINER_API_TOKEN", v); }
        assert!(reg.contains("portainer_status"));
        assert!(reg.contains("portainer_list_environments"));
        assert!(reg.contains("portainer_list_containers"));
        assert!(reg.contains("portainer_container_logs"));
    }
}
