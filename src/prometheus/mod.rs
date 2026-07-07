//! Prometheus tools — read-only PromQL queries against a LAN Prometheus server.
//!
//! Mirrors the Python prometheus_tools.py on the fleet host exactly. Six tools:
//!   prometheus_status         — health check + target summary
//!   prometheus_query          — arbitrary PromQL instant query
//!   prometheus_query_range    — PromQL range query over a time window
//!   prometheus_targets        — list scrape targets and their health
//!   prometheus_alerts         — list firing/pending alerts
//!   prometheus_health_summary — pre-built cluster health dashboard
//!
//! Required env var:
//!   PROMETHEUS_URL — base URL, e.g. http://prometheus.example:9090 (no auth)
//!
//! Prometheus runs LAN-only without authentication, so no credentials are used.
//! If PROMETHEUS_URL is unset, NotConfigured stubs are registered.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct PrometheusConfig {
    base_url: String,
}

impl PrometheusConfig {
    fn from_env() -> Result<Self, ToolError> {
        let raw = std::env::var("PROMETHEUS_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::NotConfigured("PROMETHEUS_URL not set".into())
            })?;
        Ok(Self {
            base_url: raw.trim_end_matches('/').to_string(),
        })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// GET a Prometheus API path with optional query params, returning the
    /// parsed JSON body. HTTP/network errors map to ToolError::Http.
    async fn api(
        &self,
        client: &reqwest::Client,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ToolError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = client
            .get(&url)
            .header("Accept", "application/json")
            .query(params)
            .send()
            .await
            .map_err(|e| {
                warn!("prometheus: request to {url} failed: {e}");
                ToolError::Http("The metrics service (Prometheus) is unreachable.".into())
            })?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| {
            warn!("prometheus: reading response from {url} failed: {e}");
            ToolError::Http("The metrics service (Prometheus) is unreachable.".into())
        })?;

        if !status.is_success() {
            return Err(ToolError::Http(format!("Prometheus HTTP {status}: {text}")));
        }
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON: {e}")))
    }
}

// ── Duration parsing ────────────────────────────────────────────────────────

/// Parse a duration string like "1h", "6h", "24h", "7d", "90s", "15m" into
/// seconds. Mirrors the Python duration_map; defaults to 3600 (1h) on any
/// parse failure or unknown unit.
fn parse_duration_secs(duration: &str) -> i64 {
    let d = duration.trim();
    if d.is_empty() {
        return 3600;
    }
    let unit = d.chars().last().unwrap_or('h');
    let multiplier = match unit {
        's' => 1_i64,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return 3600,
    };
    let num_part = &d[..d.len() - unit.len_utf8()];
    match num_part.parse::<i64>() {
        Ok(n) => n * multiplier,
        Err(_) => 3600,
    }
}

fn now_unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ── Result formatting helpers ──────────────────────────────────────────────

/// Format a Prometheus instant-query response (`/api/v1/query`) into the same
/// shape the Python tool returns.
fn format_instant_query(query: &str, result: &Value) -> Value {
    let data = result.get("data").cloned().unwrap_or_else(|| json!({}));
    let result_type = data
        .get("resultType")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let raw = data.get("result").and_then(Value::as_array).cloned().unwrap_or_default();

    let mut formatted: Vec<Value> = Vec::new();
    for r in &raw {
        let mut entry = json!({ "labels": r.get("metric").cloned().unwrap_or_else(|| json!({})) });
        match result_type.as_str() {
            "vector" => {
                if let Some(v) = r.get("value").and_then(Value::as_array) {
                    if v.len() >= 2 {
                        entry["timestamp"] = v[0].clone();
                        entry["value"] = v[1].clone();
                    }
                }
            }
            "matrix" => {
                let values: Vec<Value> = r
                    .get("values")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_array())
                            .filter(|v| v.len() >= 2)
                            .map(|v| json!({ "t": v[0], "v": v[1] }))
                            .collect()
                    })
                    .unwrap_or_default();
                entry["values"] = Value::Array(values);
            }
            "scalar" => {
                if let Some(v) = r.as_array() {
                    if v.len() >= 2 {
                        entry["value"] = v[1].clone();
                    }
                } else {
                    entry["value"] = r.clone();
                }
            }
            _ => {}
        }
        formatted.push(entry);
    }

    json!({
        "query": query,
        "result_type": result_type,
        "count": formatted.len(),
        "results": formatted,
    })
}

/// Extract the active targets array from a `/api/v1/targets` response.
fn active_targets(result: &Value) -> Vec<Value> {
    result
        .get("data")
        .and_then(|d| d.get("activeTargets"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Numeric value (index 1) of a "value" pair as f64, if parseable.
fn metric_value_f64(r: &Value) -> Option<f64> {
    let v = r.get("value").and_then(Value::as_array)?;
    let raw = v.get(1)?;
    match raw {
        Value::String(s) => s.parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn instance_of(r: &Value) -> String {
    r.get("metric")
        .and_then(|m| m.get("instance"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

// ── Tool structs ──────────────────────────────────────────────────────────────

struct PrometheusStatus { cfg: PrometheusConfig }
struct PrometheusQuery { cfg: PrometheusConfig }
struct PrometheusQueryRange { cfg: PrometheusConfig }
struct PrometheusTargets { cfg: PrometheusConfig }
struct PrometheusAlerts { cfg: PrometheusConfig }
struct PrometheusHealthSummary { cfg: PrometheusConfig }

#[async_trait]
impl RustTool for PrometheusStatus {
    fn name(&self) -> &str { "prometheus_status" }

    fn description(&self) -> &str {
        "Check Prometheus server health and show target summary. Returns server \
health, total targets, and count of up/down targets."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = PrometheusConfig::client()?;

        // Health check — /-/healthy returns 200 when healthy.
        let healthy = match client
            .get(format!("{}/-/healthy", self.cfg.base_url))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        };

        // Target summary.
        let targets = self.cfg.api(&client, "/api/v1/targets", &[]).await?;
        let active = active_targets(&targets);
        let up_count = active
            .iter()
            .filter(|t| t.get("health").and_then(Value::as_str) == Some("up"))
            .count();
        let down_count = active.len() - up_count;

        let out = json!({
            "healthy": healthy,
            "url": self.cfg.base_url,
            "targets_total": active.len(),
            "targets_up": up_count,
            "targets_down": down_count,
        });
        Ok(out.to_string())
    }
}

#[async_trait]
impl RustTool for PrometheusQuery {
    fn name(&self) -> &str { "prometheus_query" }

    fn description(&self) -> &str {
        "Run a PromQL instant query and return current values. Returns result type \
(vector/scalar/matrix) and metric values. Examples: up, node_load1, \
node_memory_MemAvailable_bytes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "PromQL expression (e.g. 'up', 'node_load1')" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Err(ToolError::InvalidArgument("query is required".into()));
        }
        let client = PrometheusConfig::client()?;
        let result = self
            .cfg
            .api(&client, "/api/v1/query", &[("query", query.clone())])
            .await?;
        Ok(format_instant_query(&query, &result).to_string())
    }
}

#[async_trait]
impl RustTool for PrometheusQueryRange {
    fn name(&self) -> &str { "prometheus_query_range" }

    fn description(&self) -> &str {
        "Run a PromQL range query over a time window. duration: how far back \
(e.g. '1h', '6h', '24h', '7d'). step: resolution (e.g. '15s', '60s', '5m'). \
Returns time series data suitable for charting."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query":    { "type": "string", "description": "PromQL expression (required)" },
                "duration": { "type": "string", "description": "How far back to look (default 1h)" },
                "step":     { "type": "string", "description": "Resolution step (default 60s)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Err(ToolError::InvalidArgument("query is required".into()));
        }
        let duration = args.get("duration").and_then(Value::as_str).unwrap_or("1h").trim().to_string();
        let step = args.get("step").and_then(Value::as_str).unwrap_or("60s").trim().to_string();

        let seconds = parse_duration_secs(&duration);
        let now = now_unix_secs();
        let start = now - seconds as f64;

        let params = vec![
            ("query", query.clone()),
            ("start", start.to_string()),
            ("end", now.to_string()),
            ("step", step.clone()),
        ];
        let result = self.cfg.api(&client_for(&self.cfg).await?, "/api/v1/query_range", &params).await?;

        let data = result.get("data").cloned().unwrap_or_else(|| json!({}));
        let raw = data.get("result").and_then(Value::as_array).cloned().unwrap_or_default();

        let mut formatted: Vec<Value> = Vec::new();
        for r in &raw {
            let values_arr: Vec<Value> = r
                .get("values")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_array())
                        .filter(|v| v.len() >= 2)
                        .map(|v| json!({ "t": v[0], "v": v[1] }))
                        .collect()
                })
                .unwrap_or_default();
            formatted.push(json!({
                "labels": r.get("metric").cloned().unwrap_or_else(|| json!({})),
                "datapoints": values_arr.len(),
                "values": values_arr,
            }));
        }

        let out = json!({
            "query": query,
            "duration": duration,
            "step": step,
            "series_count": formatted.len(),
            "results": formatted,
        });
        Ok(out.to_string())
    }
}

/// Build a reqwest client (small helper so query_range reads cleanly).
async fn client_for(_cfg: &PrometheusConfig) -> Result<reqwest::Client, ToolError> {
    PrometheusConfig::client()
}

#[async_trait]
impl RustTool for PrometheusTargets {
    fn name(&self) -> &str { "prometheus_targets" }

    fn description(&self) -> &str {
        "List all Prometheus scrape targets and their health status. Returns each \
target's job, instance, health, and last scrape time. Down targets sorted first."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = PrometheusConfig::client()?;
        let result = self.cfg.api(&client, "/api/v1/targets", &[]).await?;
        let active = active_targets(&result);

        let mut targets: Vec<Value> = active
            .iter()
            .map(|t| {
                let labels = t.get("labels").cloned().unwrap_or_else(|| json!({}));
                json!({
                    "job":         labels.get("job").and_then(Value::as_str).unwrap_or("unknown"),
                    "instance":    labels.get("instance").and_then(Value::as_str).unwrap_or("unknown"),
                    "health":      t.get("health").and_then(Value::as_str).unwrap_or("unknown"),
                    "last_scrape": t.get("lastScrape").and_then(Value::as_str).unwrap_or(""),
                    "last_error":  t.get("lastError").and_then(Value::as_str).unwrap_or(""),
                })
            })
            .collect();

        // Sort: down first (health != "up"), then by job, then instance.
        targets.sort_by(|a, b| {
            let key = |x: &Value| {
                let up = if x.get("health").and_then(Value::as_str) == Some("up") { 1 } else { 0 };
                (
                    up,
                    x.get("job").and_then(Value::as_str).unwrap_or("").to_string(),
                    x.get("instance").and_then(Value::as_str).unwrap_or("").to_string(),
                )
            };
            key(a).cmp(&key(b))
        });

        let up = targets
            .iter()
            .filter(|t| t.get("health").and_then(Value::as_str) == Some("up"))
            .count();
        let down = targets.len() - up;

        let out = json!({
            "total": targets.len(),
            "up": up,
            "down": down,
            "targets": targets,
        });
        Ok(out.to_string())
    }
}

#[async_trait]
impl RustTool for PrometheusAlerts {
    fn name(&self) -> &str { "prometheus_alerts" }

    fn description(&self) -> &str {
        "List currently firing and pending alerts from Prometheus. Returns alert \
name, state, severity, and affected instance. Requires alerting rules configured."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = PrometheusConfig::client()?;
        let result = self.cfg.api(&client, "/api/v1/alerts", &[]).await?;

        let raw = result
            .get("data")
            .and_then(|d| d.get("alerts"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let alerts: Vec<Value> = raw
            .iter()
            .map(|a| {
                let labels = a.get("labels").cloned().unwrap_or_else(|| json!({}));
                let annotations = a.get("annotations").cloned().unwrap_or_else(|| json!({}));
                json!({
                    "name":         labels.get("alertname").and_then(Value::as_str).unwrap_or("unknown"),
                    "state":        a.get("state").and_then(Value::as_str).unwrap_or("unknown"),
                    "severity":     labels.get("severity").and_then(Value::as_str).unwrap_or("none"),
                    "instance":     labels.get("instance").and_then(Value::as_str).unwrap_or(""),
                    "summary":      annotations.get("summary").and_then(Value::as_str).unwrap_or(""),
                    "active_since": a.get("activeAt").and_then(Value::as_str).unwrap_or(""),
                })
            })
            .collect();

        let firing = alerts
            .iter()
            .filter(|a| a.get("state").and_then(Value::as_str) == Some("firing"))
            .count();
        let pending = alerts
            .iter()
            .filter(|a| a.get("state").and_then(Value::as_str) == Some("pending"))
            .count();

        let out = json!({
            "total": alerts.len(),
            "firing": firing,
            "pending": pending,
            "alerts": alerts,
        });
        Ok(out.to_string())
    }
}

#[async_trait]
impl RustTool for PrometheusHealthSummary {
    fn name(&self) -> &str { "prometheus_health_summary" }

    fn description(&self) -> &str {
        "Pre-built cluster health dashboard. Returns a snapshot of key metrics \
across all monitored nodes: CPU load, memory usage, disk usage, and target health."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = PrometheusConfig::client()?;
        // nodes: instance -> map of fields
        let mut nodes: serde_json::Map<String, Value> = serde_json::Map::new();

        // Node load averages.
        let load = self
            .cfg
            .api(&client, "/api/v1/query", &[("query", "node_load1".to_string())])
            .await?;
        for r in load.get("data").and_then(|d| d.get("result")).and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(v) = metric_value_f64(&r) {
                let instance = instance_of(&r);
                let entry = nodes.entry(instance).or_insert_with(|| json!({}));
                entry["load_1m"] = json!((v * 100.0).round() / 100.0);
            }
        }

        // Memory: total and available (GB).
        for (metric, key) in [
            ("node_memory_MemTotal_bytes", "mem_total_gb"),
            ("node_memory_MemAvailable_bytes", "mem_available_gb"),
        ] {
            let res = self
                .cfg
                .api(&client, "/api/v1/query", &[("query", metric.to_string())])
                .await?;
            for r in res.get("data").and_then(|d| d.get("result")).and_then(Value::as_array).cloned().unwrap_or_default() {
                if let Some(bytes) = metric_value_f64(&r) {
                    let instance = instance_of(&r);
                    let gb = (bytes / 1024_f64.powi(3) * 10.0).round() / 10.0;
                    let entry = nodes.entry(instance).or_insert_with(|| json!({}));
                    entry[key] = json!(gb);
                }
            }
        }

        // Memory used percentage.
        for entry in nodes.values_mut() {
            let total = entry.get("mem_total_gb").and_then(Value::as_f64).unwrap_or(0.0);
            let avail = entry.get("mem_available_gb").and_then(Value::as_f64).unwrap_or(0.0);
            if total > 0.0 {
                let pct = ((1.0 - avail / total) * 100.0 * 10.0).round() / 10.0;
                entry["mem_used_pct"] = json!(pct);
            }
        }

        // Root filesystem usage.
        let fs_avail = self
            .cfg
            .api(&client, "/api/v1/query", &[("query", "node_filesystem_avail_bytes{mountpoint=\"/\"}".to_string())])
            .await?;
        let fs_size = self
            .cfg
            .api(&client, "/api/v1/query", &[("query", "node_filesystem_size_bytes{mountpoint=\"/\"}".to_string())])
            .await?;

        let mut fs_totals: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for r in fs_size.get("data").and_then(|d| d.get("result")).and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(v) = metric_value_f64(&r) {
                fs_totals.insert(instance_of(&r), v);
            }
        }
        for r in fs_avail.get("data").and_then(|d| d.get("result")).and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(avail) = metric_value_f64(&r) {
                let instance = instance_of(&r);
                let total = fs_totals.get(&instance).copied().unwrap_or(0.0);
                if total > 0.0 {
                    let used_pct = ((1.0 - avail / total) * 100.0 * 10.0).round() / 10.0;
                    let avail_gb = (avail / 1024_f64.powi(3) * 10.0).round() / 10.0;
                    let entry = nodes.entry(instance).or_insert_with(|| json!({}));
                    entry["disk_root_used_pct"] = json!(used_pct);
                    entry["disk_root_avail_gb"] = json!(avail_gb);
                }
            }
        }

        // Target health summary.
        let targets = self.cfg.api(&client, "/api/v1/targets", &[]).await?;
        let active = active_targets(&targets);
        let targets_up = active
            .iter()
            .filter(|t| t.get("health").and_then(Value::as_str) == Some("up"))
            .count();
        let targets_down = active.len() - targets_up;

        // Alerts firing.
        let alerts = self.cfg.api(&client, "/api/v1/alerts", &[]).await?;
        let alerts_firing = alerts
            .get("data")
            .and_then(|d| d.get("alerts"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|a| a.get("state").and_then(Value::as_str) == Some("firing"))
                    .count()
            })
            .unwrap_or(0);

        let out = json!({
            "nodes": Value::Object(nodes),
            "cluster": {
                "targets_total": active.len(),
                "targets_up": targets_up,
                "targets_down": targets_down,
                "alerts_firing": alerts_firing,
            }
        });
        Ok(out.to_string())
    }
}

// ── Registration ──────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    match PrometheusConfig::from_env() {
        Ok(cfg) => {
            registry.register_or_replace(Box::new(PrometheusStatus { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(PrometheusQuery { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(PrometheusQueryRange { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(PrometheusTargets { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(PrometheusAlerts { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(PrometheusHealthSummary { cfg }));
        }
        Err(e) => {
            tracing::warn!("Prometheus tools not configured: {e}. Registering stubs.");
            for name in [
                "prometheus_status",
                "prometheus_query",
                "prometheus_query_range",
                "prometheus_targets",
                "prometheus_alerts",
                "prometheus_health_summary",
            ] {
                registry.register_or_replace(Box::new(NotConfiguredStub(name)));
            }
        }
    }
}

struct NotConfiguredStub(&'static str);

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str { self.0 }
    fn description(&self) -> &str { "Prometheus tool (PROMETHEUS_URL not configured)" }
    fn parameters(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured("PROMETHEUS_URL not set".into()))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cfg() -> PrometheusConfig {
        PrometheusConfig { base_url: "http://prom.test:9090".into() }
    }

    // ── duration parsing ────────────────────────────────────────────────────

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration_secs("90s"), 90);
        assert_eq!(parse_duration_secs("15m"), 900);
        assert_eq!(parse_duration_secs("1h"), 3600);
        assert_eq!(parse_duration_secs("6h"), 21_600);
        assert_eq!(parse_duration_secs("24h"), 86_400);
        assert_eq!(parse_duration_secs("7d"), 604_800);
    }

    #[test]
    fn parse_duration_unknown_unit_defaults_1h() {
        assert_eq!(parse_duration_secs("5x"), 3600);
        assert_eq!(parse_duration_secs("garbage"), 3600);
        assert_eq!(parse_duration_secs(""), 3600);
    }

    #[test]
    fn parse_duration_bad_number_defaults_1h() {
        assert_eq!(parse_duration_secs("abch"), 3600);
    }

    // ── config ───────────────────────────────────────────────────────────────

    #[test]
    fn config_trims_trailing_slash() {
        // Build directly to avoid env coupling, then verify the trimming logic
        // separately on a raw string.
        let raw = "http://x:9090/".trim_end_matches('/').to_string();
        assert_eq!(raw, "http://x:9090");
    }

    #[test]
    #[serial]
    fn config_from_env_missing_is_not_configured() {
        let backup = std::env::var("PROMETHEUS_URL").ok();
        std::env::remove_var("PROMETHEUS_URL");
        let r = PrometheusConfig::from_env();
        if let Some(v) = backup { std::env::set_var("PROMETHEUS_URL", v); }
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }

    // ── instant query formatting ──────────────────────────────────────────────

    #[test]
    fn format_instant_vector() {
        let resp = json!({
            "status": "success",
            "data": {
                "resultType": "vector",
                "result": [
                    { "metric": { "__name__": "up", "instance": "node1", "job": "node" },
                      "value": [1717000000.0, "1"] }, // pii-test-fixture
                    { "metric": { "instance": "node2", "job": "node" },
                      "value": [1717000000.0, "0"] } // pii-test-fixture
                ]
            }
        });
        let out = format_instant_query("up", &resp);
        assert_eq!(out["query"], "up");
        assert_eq!(out["result_type"], "vector");
        assert_eq!(out["count"], 2);
        assert_eq!(out["results"][0]["value"], "1");
        assert_eq!(out["results"][0]["labels"]["instance"], "node1");
        assert_eq!(out["results"][1]["value"], "0");
    }

    #[test]
    fn format_instant_matrix() {
        let resp = json!({
            "data": {
                "resultType": "matrix",
                "result": [
                    { "metric": { "instance": "n1" },
                      "values": [[100.0, "0.5"], [160.0, "0.7"]] }
                ]
            }
        });
        let out = format_instant_query("node_load1", &resp);
        assert_eq!(out["result_type"], "matrix");
        assert_eq!(out["count"], 1);
        assert_eq!(out["results"][0]["values"][0]["t"], 100.0);
        assert_eq!(out["results"][0]["values"][0]["v"], "0.5");
        assert_eq!(out["results"][0]["values"][1]["v"], "0.7");
    }

    #[test]
    fn format_instant_empty_results() {
        let resp = json!({ "data": { "resultType": "vector", "result": [] } });
        let out = format_instant_query("up", &resp);
        assert_eq!(out["count"], 0);
        assert!(out["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn format_instant_missing_data_defaults_unknown() {
        let resp = json!({ "status": "error" });
        let out = format_instant_query("foo", &resp);
        assert_eq!(out["result_type"], "unknown");
        assert_eq!(out["count"], 0);
    }

    // ── targets parsing ────────────────────────────────────────────────────────

    #[test]
    fn active_targets_extracts_array() {
        let resp = json!({
            "data": { "activeTargets": [ { "health": "up" }, { "health": "down" } ] }
        });
        let t = active_targets(&resp);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn active_targets_missing_returns_empty() {
        assert!(active_targets(&json!({})).is_empty());
    }

    // ── metric value extraction ──────────────────────────────────────────────

    #[test]
    fn metric_value_parses_string_and_number() {
        let str_val = json!({ "value": [1.0, "3.14"] });
        assert_eq!(metric_value_f64(&str_val), Some(3.14));
        let num_val = json!({ "value": [1.0, 42] });
        assert_eq!(metric_value_f64(&num_val), Some(42.0));
    }

    #[test]
    fn metric_value_missing_returns_none() {
        assert_eq!(metric_value_f64(&json!({})), None);
        assert_eq!(metric_value_f64(&json!({ "value": [1.0] })), None);
    }

    #[test]
    fn instance_of_defaults_unknown() {
        assert_eq!(instance_of(&json!({})), "unknown");
        assert_eq!(instance_of(&json!({ "metric": { "instance": "n1" } })), "n1");
    }

    // ── tool argument validation ──────────────────────────────────────────────

    #[tokio::test]
    async fn query_empty_query_invalid_argument() {
        let tool = PrometheusQuery { cfg: cfg() };
        let r = tool.execute(json!({ "query": "" })).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn query_missing_query_invalid_argument() {
        let tool = PrometheusQuery { cfg: cfg() };
        let r = tool.execute(json!({})).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn query_range_empty_query_invalid_argument() {
        let tool = PrometheusQueryRange { cfg: cfg() };
        let r = tool.execute(json!({ "query": "  " })).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    // ── tool identity ──────────────────────────────────────────────────────────

    #[test]
    fn tool_names_are_stable() {
        let c = cfg();
        assert_eq!(PrometheusStatus { cfg: c.clone() }.name(), "prometheus_status");
        assert_eq!(PrometheusQuery { cfg: c.clone() }.name(), "prometheus_query");
        assert_eq!(PrometheusQueryRange { cfg: c.clone() }.name(), "prometheus_query_range");
        assert_eq!(PrometheusTargets { cfg: c.clone() }.name(), "prometheus_targets");
        assert_eq!(PrometheusAlerts { cfg: c.clone() }.name(), "prometheus_alerts");
        assert_eq!(PrometheusHealthSummary { cfg: c }.name(), "prometheus_health_summary");
    }

    #[test]
    fn tool_parameters_valid_schema() {
        let c = cfg();
        assert_eq!(PrometheusQuery { cfg: c.clone() }.parameters()["type"], "object");
        assert!(PrometheusQuery { cfg: c.clone() }.parameters()["required"].is_array());
        assert!(PrometheusQueryRange { cfg: c.clone() }.parameters()["required"].is_array());
        assert_eq!(PrometheusStatus { cfg: c }.parameters()["type"], "object");
    }

    // ── registration ─────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn register_adds_six_tools_as_stubs_when_unconfigured() {
        let mut reg = ToolRegistry::new();
        let backup = std::env::var("PROMETHEUS_URL").ok();
        std::env::remove_var("PROMETHEUS_URL");
        register(&mut reg);
        if let Some(v) = backup { std::env::set_var("PROMETHEUS_URL", v); }

        assert!(reg.contains("prometheus_status"));
        assert!(reg.contains("prometheus_query"));
        assert!(reg.contains("prometheus_query_range"));
        assert!(reg.contains("prometheus_targets"));
        assert!(reg.contains("prometheus_alerts"));
        assert!(reg.contains("prometheus_health_summary"));
        assert_eq!(reg.len(), 6);
    }

    #[tokio::test]
    async fn stub_returns_not_configured() {
        let stub = NotConfiguredStub("prometheus_status");
        let r = stub.execute(json!({})).await;
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }
}
