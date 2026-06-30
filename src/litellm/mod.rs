//! LiteLLM tools — read-only status and model queries against the LiteLLM
//! proxy (Refractor, CT215). Ports the Python litellm_tools.py on CT322 exactly.
//!
//! Three tools, identical names and params to the Python source:
//!   litellm_list_models   — list all configured model routes
//!   litellm_model_status  — per-model health check (healthy/unhealthy)
//!   litellm_request_log   — recent request/spend logs (with global-spend fallback)
//!
//! All endpoints require master-key authentication.
//!
//! Required env vars:
//!   LITELLM_URL          — base URL, e.g. http://litellm.example:4000
//!   LITELLM_MASTER_KEY   — master key sent as `Authorization: Bearer <key>`
//!
//! If either var is unset, registration installs no-op stubs that return a
//! clear NotConfigured error rather than failing at call time.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct LiteLLMConfig {
    base_url: String,
    master_key: String,
}

impl LiteLLMConfig {
    fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("LITELLM_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("LITELLM_URL not set".into()))?;
        let master_key = std::env::var("LITELLM_MASTER_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("LITELLM_MASTER_KEY not set".into()))?;
        Ok(Self { base_url, master_key })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(40))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// Authenticated GET against the LiteLLM API. Mirrors the Python `_litellm_api`:
    /// on an HTTP error it returns an `{ "error": true, ... }` JSON value rather
    /// than erroring, so callers can replicate the Python's error-passthrough.
    async fn api_get(
        &self,
        client: &reqwest::Client,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, ToolError> {
        let url = format!("{}{path}", self.base_url);
        let resp = client
            .get(&url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", self.master_key))
            .query(query)
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;

        let status = resp.status();
        let raw = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;

        if !status.is_success() {
            return Ok(json!({
                "error": true,
                "status": status.as_u16(),
                "message": raw,
            }));
        }

        if raw.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&raw)
            .map_err(|e| ToolError::Http(format!("Invalid JSON from LiteLLM: {e}")))
    }
}

// ── Parsing helpers (no network) ──────────────────────────────────────────────

/// Build the litellm_list_models response from a `/v1/models` body.
fn parse_models(result: &Value) -> Value {
    if result.get("error").is_some() {
        return result.clone();
    }
    let mut models: Vec<Value> = result
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|m| {
                    json!({
                        "id": m.get("id").and_then(Value::as_str).unwrap_or("unknown"),
                        "owned_by": m.get("owned_by").and_then(Value::as_str).unwrap_or("unknown"),
                        "created": m.get("created").and_then(Value::as_i64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    models.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("id").and_then(Value::as_str).unwrap_or(""))
    });

    json!({
        "count": models.len(),
        "models": models,
    })
}

/// Build the litellm_model_status response from a `/health` body.
fn parse_health(result: &Value) -> Value {
    if result.get("error").is_some() {
        return result.clone();
    }

    let healthy_count = result.get("healthy_count").and_then(Value::as_i64).unwrap_or(0);
    let unhealthy_count = result.get("unhealthy_count").and_then(Value::as_i64).unwrap_or(0);

    let healthy: Vec<Value> = result
        .get("healthy_endpoints")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|h| {
                    json!({
                        "model": h.get("model").and_then(Value::as_str).unwrap_or("unknown"),
                        "api_base": h.get("api_base").and_then(Value::as_str).unwrap_or("unknown"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let unhealthy: Vec<Value> = result
        .get("unhealthy_endpoints")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|u| {
                    // Truncate long error strings like the Python (>200 chars → +"...").
                    let error = match u.get("error") {
                        Some(Value::String(s)) if s.len() > 200 => {
                            let truncated: String = s.chars().take(200).collect();
                            Value::String(format!("{truncated}..."))
                        }
                        Some(v) => v.clone(),
                        None => Value::String("unknown".into()),
                    };
                    json!({
                        "model": u.get("model").and_then(Value::as_str).unwrap_or("unknown"),
                        "api_base": u.get("api_base").and_then(Value::as_str).unwrap_or("unknown"),
                        "error": error,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "healthy_count": healthy_count,
        "unhealthy_count": unhealthy_count,
        "healthy": healthy,
        "unhealthy": unhealthy,
    })
}

/// Build the litellm_request_log response from a `/spend/logs` body.
fn parse_spend_logs(result: &Value, limit: usize) -> Value {
    // Entries may be a bare array, or `{data: [...]}` / `{logs: [...]}`.
    let entries: Vec<Value> = if let Some(arr) = result.as_array() {
        arr.clone()
    } else if let Some(arr) = result.get("data").and_then(Value::as_array) {
        arr.clone()
    } else if let Some(arr) = result.get("logs").and_then(Value::as_array) {
        arr.clone()
    } else {
        Vec::new()
    };

    let logs: Vec<Value> = entries
        .iter()
        .take(limit)
        .map(|entry| {
            json!({
                "model": entry.get("model").and_then(Value::as_str).unwrap_or("unknown"),
                "tokens": entry.get("total_tokens").and_then(Value::as_i64).unwrap_or(0),
                "cost": entry.get("spend").cloned().unwrap_or(json!(0)),
                "status": entry.get("status").and_then(Value::as_str).unwrap_or("unknown"),
                "timestamp": entry
                    .get("startTime")
                    .or_else(|| entry.get("created_at"))
                    .cloned()
                    .unwrap_or(Value::String(String::new())),
                "api_key_alias": entry.get("api_key_alias").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect();

    json!({
        "count": logs.len(),
        "logs": logs,
    })
}

// ── Tools ─────────────────────────────────────────────────────────────────────

struct LitellmListModels { cfg: LiteLLMConfig }
struct LitellmModelStatus { cfg: LiteLLMConfig }
struct LitellmRequestLog { cfg: LiteLLMConfig }

#[async_trait]
impl RustTool for LitellmListModels {
    fn name(&self) -> &str { "litellm_list_models" }

    fn description(&self) -> &str {
        "List all model routes configured in LiteLLM. Returns each model's ID, the \
backend provider (owned_by), and creation timestamp. Useful for discovering what \
models are available for inference."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = LiteLLMConfig::client()?;
        let result = self.cfg.api_get(&client, "/v1/models", &[]).await?;
        Ok(parse_models(&result).to_string())
    }
}

#[async_trait]
impl RustTool for LitellmModelStatus {
    fn name(&self) -> &str { "litellm_model_status" }

    fn description(&self) -> &str {
        "Check health of all configured models. Runs LiteLLM's internal health check \
against each model endpoint and returns healthy models, unhealthy models with error \
details, and summary counts. Health checks can be slow (10-30s) as LiteLLM tests each \
model with a real inference call; Ollama models will time out if Ollama is not running."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = LiteLLMConfig::client()?;
        let result = self.cfg.api_get(&client, "/health", &[]).await?;
        Ok(parse_health(&result).to_string())
    }
}

#[async_trait]
impl RustTool for LitellmRequestLog {
    fn name(&self) -> &str { "litellm_request_log" }

    fn description(&self) -> &str {
        "View recent LiteLLM request logs: model used, tokens consumed, cost, and \
status. Useful for monitoring spend and debugging routing issues. Requires LiteLLM's \
database logging to be enabled; falls back to a global-spend summary otherwise."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Number of recent requests to return (default 20, max 100)" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mut limit = args.get("limit").and_then(Value::as_i64).unwrap_or(20);
        if limit > 100 {
            limit = 100;
        }
        if limit < 0 {
            limit = 0;
        }
        let limit = limit as usize;

        let client = LiteLLMConfig::client()?;
        let result = self
            .cfg
            .api_get(&client, "/spend/logs", &[("limit", limit.to_string())])
            .await?;

        if result.get("error").is_some() {
            // Fallback: global spend summary, matching the Python.
            let spend = self.cfg.api_get(&client, "/global/spend", &[]).await?;
            if spend.get("error").is_some() {
                return Ok(json!({
                    "message": "Request logging may not be enabled in LiteLLM.",
                    "error": spend,
                })
                .to_string());
            }
            return Ok(json!({
                "message": "Detailed request logs not available. Global spend summary:",
                "global_spend": spend,
            })
            .to_string());
        }

        Ok(parse_spend_logs(&result, limit).to_string())
    }
}

// ── Registration ────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    match LiteLLMConfig::from_env() {
        Ok(cfg) => {
            registry.register_or_replace(Box::new(LitellmListModels { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(LitellmModelStatus { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(LitellmRequestLog { cfg }));
        }
        Err(e) => {
            tracing::warn!("LiteLLM tools not configured: {e}. Registering stubs.");
            registry.register_or_replace(Box::new(NotConfiguredStub("litellm_list_models")));
            registry.register_or_replace(Box::new(NotConfiguredStub("litellm_model_status")));
            registry.register_or_replace(Box::new(NotConfiguredStub("litellm_request_log")));
        }
    }
}

struct NotConfiguredStub(&'static str);

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str { self.0 }
    fn description(&self) -> &str {
        "LiteLLM tool (LITELLM_URL / LITELLM_MASTER_KEY not configured)"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured(
            "LITELLM_URL and LITELLM_MASTER_KEY must be set".into(),
        ))
    }
}

// ── Tests (no network) ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cfg() -> LiteLLMConfig {
        LiteLLMConfig {
            base_url: "http://litellm.test:4000".into(),
            master_key: "sk-test".into(),
        }
    }

    // ── config ──────────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn from_env_missing_url_errors() {
        let url = std::env::var("LITELLM_URL").ok();
        let key = std::env::var("LITELLM_MASTER_KEY").ok();
        std::env::remove_var("LITELLM_URL");
        std::env::set_var("LITELLM_MASTER_KEY", "k");
        let r = LiteLLMConfig::from_env();
        if let Some(u) = url { std::env::set_var("LITELLM_URL", u); }
        if let Some(k) = key { std::env::set_var("LITELLM_MASTER_KEY", k); } else { std::env::remove_var("LITELLM_MASTER_KEY"); }
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }

    #[test]
    #[serial]
    fn from_env_strips_trailing_slash() {
        let url = std::env::var("LITELLM_URL").ok();
        let key = std::env::var("LITELLM_MASTER_KEY").ok();
        std::env::set_var("LITELLM_URL", "http://x:4000/");
        std::env::set_var("LITELLM_MASTER_KEY", "k");
        let c = LiteLLMConfig::from_env().unwrap();
        assert_eq!(c.base_url, "http://x:4000");
        assert_eq!(c.master_key, "k");
        if let Some(u) = url { std::env::set_var("LITELLM_URL", u); } else { std::env::remove_var("LITELLM_URL"); }
        if let Some(k) = key { std::env::set_var("LITELLM_MASTER_KEY", k); } else { std::env::remove_var("LITELLM_MASTER_KEY"); }
    }

    // ── parse_models ──────────────────────────────────────────────────────────

    #[test]
    fn parse_models_sorts_and_counts() {
        let body = json!({
            "data": [
                { "id": "zephyr", "owned_by": "ollama", "created": 100 },
                { "id": "claude", "owned_by": "anthropic", "created": 200 },
            ]
        });
        let out = parse_models(&body);
        assert_eq!(out["count"], 2);
        assert_eq!(out["models"][0]["id"], "claude");
        assert_eq!(out["models"][1]["id"], "zephyr");
        assert_eq!(out["models"][0]["owned_by"], "anthropic");
        assert_eq!(out["models"][0]["created"], 200);
    }

    #[test]
    fn parse_models_handles_missing_fields() {
        let body = json!({ "data": [ {} ] });
        let out = parse_models(&body);
        assert_eq!(out["count"], 1);
        assert_eq!(out["models"][0]["id"], "unknown");
        assert_eq!(out["models"][0]["owned_by"], "unknown");
        assert_eq!(out["models"][0]["created"], 0);
    }

    #[test]
    fn parse_models_empty_when_no_data() {
        let out = parse_models(&json!({}));
        assert_eq!(out["count"], 0);
        assert_eq!(out["models"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parse_models_passthrough_error() {
        let err = json!({ "error": true, "status": 401, "message": "bad key" });
        let out = parse_models(&err);
        assert_eq!(out["error"], true);
        assert_eq!(out["status"], 401);
    }

    // ── parse_health ──────────────────────────────────────────────────────────

    #[test]
    fn parse_health_counts_and_endpoints() {
        let body = json!({
            "healthy_count": 1,
            "unhealthy_count": 1,
            "healthy_endpoints": [ { "model": "claude", "api_base": "https://api.anthropic.com" } ],
            "unhealthy_endpoints": [ { "model": "ollama/qwen", "api_base": "http://gpu:11434", "error": "timeout" } ]
        });
        let out = parse_health(&body);
        assert_eq!(out["healthy_count"], 1);
        assert_eq!(out["unhealthy_count"], 1);
        assert_eq!(out["healthy"][0]["model"], "claude");
        assert_eq!(out["unhealthy"][0]["model"], "ollama/qwen");
        assert_eq!(out["unhealthy"][0]["error"], "timeout");
    }

    #[test]
    fn parse_health_truncates_long_error() {
        let long = "x".repeat(250);
        let body = json!({
            "unhealthy_endpoints": [ { "model": "m", "api_base": "b", "error": long } ]
        });
        let out = parse_health(&body);
        let err = out["unhealthy"][0]["error"].as_str().unwrap();
        assert_eq!(err.len(), 203); // 200 chars + "..."
        assert!(err.ends_with("..."));
    }

    #[test]
    fn parse_health_defaults_when_empty() {
        let out = parse_health(&json!({}));
        assert_eq!(out["healthy_count"], 0);
        assert_eq!(out["unhealthy_count"], 0);
        assert_eq!(out["healthy"].as_array().unwrap().len(), 0);
        assert_eq!(out["unhealthy"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parse_health_passthrough_error() {
        let err = json!({ "error": true, "message": "down" });
        let out = parse_health(&err);
        assert_eq!(out["error"], true);
    }

    // ── parse_spend_logs ──────────────────────────────────────────────────────

    #[test]
    fn parse_spend_logs_bare_array() {
        let body = json!([
            { "model": "claude", "total_tokens": 1500, "spend": 0.045, "status": "success", "startTime": "2026-06-08T07:00:00Z", "api_key_alias": "lumina" },
            { "model": "qwen", "total_tokens": 200, "spend": 0.0, "status": "success" }
        ]);
        let out = parse_spend_logs(&body, 20);
        assert_eq!(out["count"], 2);
        assert_eq!(out["logs"][0]["model"], "claude");
        assert_eq!(out["logs"][0]["tokens"], 1500);
        assert_eq!(out["logs"][0]["cost"], 0.045);
        assert_eq!(out["logs"][0]["timestamp"], "2026-06-08T07:00:00Z");
        assert_eq!(out["logs"][0]["api_key_alias"], "lumina");
        // missing fields default
        assert_eq!(out["logs"][1]["api_key_alias"], "");
        assert_eq!(out["logs"][1]["timestamp"], "");
    }

    #[test]
    fn parse_spend_logs_data_wrapper_and_limit() {
        let body = json!({
            "data": [
                { "model": "a" }, { "model": "b" }, { "model": "c" }
            ]
        });
        let out = parse_spend_logs(&body, 2);
        assert_eq!(out["count"], 2);
        assert_eq!(out["logs"][0]["model"], "a");
        assert_eq!(out["logs"][1]["model"], "b");
    }

    #[test]
    fn parse_spend_logs_created_at_fallback() {
        let body = json!([ { "model": "m", "created_at": "2026-01-01" } ]);
        let out = parse_spend_logs(&body, 20);
        assert_eq!(out["logs"][0]["timestamp"], "2026-01-01");
    }

    #[test]
    fn parse_spend_logs_empty() {
        let out = parse_spend_logs(&json!({}), 20);
        assert_eq!(out["count"], 0);
        assert_eq!(out["logs"].as_array().unwrap().len(), 0);
    }

    // ── tool arg handling / metadata ──────────────────────────────────────────

    #[tokio::test]
    async fn list_models_param_schema_is_object() {
        let t = LitellmListModels { cfg: cfg() };
        assert_eq!(t.parameters()["type"], "object");
        assert_eq!(t.name(), "litellm_list_models");
    }

    #[test]
    fn request_log_default_and_cap() {
        // Replicate the limit-handling logic the tool uses.
        let parse = |args: Value| -> usize {
            let mut limit = args.get("limit").and_then(Value::as_i64).unwrap_or(20);
            if limit > 100 { limit = 100; }
            if limit < 0 { limit = 0; }
            limit as usize
        };
        assert_eq!(parse(json!({})), 20);
        assert_eq!(parse(json!({ "limit": 5 })), 5);
        assert_eq!(parse(json!({ "limit": 999 })), 100);
        assert_eq!(parse(json!({ "limit": -3 })), 0);
    }

    #[test]
    fn tool_names_are_stable() {
        assert_eq!(LitellmListModels { cfg: cfg() }.name(), "litellm_list_models");
        assert_eq!(LitellmModelStatus { cfg: cfg() }.name(), "litellm_model_status");
        assert_eq!(LitellmRequestLog { cfg: cfg() }.name(), "litellm_request_log");
    }

    #[tokio::test]
    async fn stub_returns_not_configured() {
        let stub = NotConfiguredStub("litellm_list_models");
        let r = stub.execute(json!({})).await;
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
        assert_eq!(stub.name(), "litellm_list_models");
    }

    // ── registration ──────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn register_adds_three_tools_stub_path() {
        let mut reg = ToolRegistry::new();
        let url = std::env::var("LITELLM_URL").ok();
        let key = std::env::var("LITELLM_MASTER_KEY").ok();
        std::env::remove_var("LITELLM_URL");
        std::env::remove_var("LITELLM_MASTER_KEY");

        register(&mut reg);

        if let Some(u) = url { std::env::set_var("LITELLM_URL", u); }
        if let Some(k) = key { std::env::set_var("LITELLM_MASTER_KEY", k); }

        assert!(reg.contains("litellm_list_models"));
        assert!(reg.contains("litellm_model_status"));
        assert!(reg.contains("litellm_request_log"));
    }

    #[test]
    #[serial]
    fn register_adds_three_tools_configured_path() {
        let mut reg = ToolRegistry::new();
        let url = std::env::var("LITELLM_URL").ok();
        let key = std::env::var("LITELLM_MASTER_KEY").ok();
        std::env::set_var("LITELLM_URL", "http://x:4000");
        std::env::set_var("LITELLM_MASTER_KEY", "sk-test");

        register(&mut reg);

        if let Some(u) = url { std::env::set_var("LITELLM_URL", u); } else { std::env::remove_var("LITELLM_URL"); }
        if let Some(k) = key { std::env::set_var("LITELLM_MASTER_KEY", k); } else { std::env::remove_var("LITELLM_MASTER_KEY"); }

        assert!(reg.contains("litellm_list_models"));
        assert!(reg.contains("litellm_model_status"));
        assert!(reg.contains("litellm_request_log"));
    }
}
