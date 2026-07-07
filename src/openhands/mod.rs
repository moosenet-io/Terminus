//! OpenHands tools — ported from the Python `openhands_tools.py` on the fleet host.
//!
//! Three tools driving the OpenHands agent runtime over its HTTP API:
//!   openhands_run_task          — start a conversation/task, return its conversation_id
//!   openhands_get_status        — poll a conversation by id (optional recent events)
//!   openhands_list_conversations — list recent conversations
//!
//! Every tool here is GUARDED: each `execute()` first calls the shared approval
//! [`gate`]. Starting/inspecting agent runs can mutate filesystems and run builds,
//! so the operator must approve each call out of band. Without `DATABASE_URL`
//! (the gate's Postgres) the gate denies the call before any HTTP is performed.
//!
//! Required env var:
//!   OPENHANDS_URL — base URL of the OpenHands API (e.g. http://openhands.example:3000)
//!
//! Mirrors the Python source exactly: same tool names, same params, same default
//! working_dir, and the same response field shapes.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::approval::{gate, Gate};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// PII remediation note (2026-07): real, functional default working
// directory on the dev workstation — left unchanged rather than
// guess-redacted; flagged for operator review before any public release.
const DEFAULT_WORKING_DIR: &str = "<path>/arcade";

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct OpenHandsConfig {
    base_url: Option<String>,
}

impl OpenHandsConfig {
    fn from_env() -> Self {
        let base_url = std::env::var("OPENHANDS_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        Self { base_url }
    }

    fn url(&self) -> Result<&str, ToolError> {
        self.base_url
            .as_deref()
            .ok_or_else(|| ToolError::NotConfigured("OPENHANDS_URL not set".into()))
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("MooseNet-MCP/1.0")
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

/// Build the full URL from a base + endpoint, mirroring the Python `f"{URL}{ep}"`.
fn build_url(base: &str, endpoint: &str) -> String {
    format!("{base}{endpoint}")
}

async fn oh_get(client: &reqwest::Client, base: &str, endpoint: &str) -> Result<Value, ToolError> {
    let url = build_url(base, endpoint);
    let resp = client
        .get(&url)
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(ToolError::Http(format!("HTTP {}: {}", status.as_u16(), text)));
    }
    serde_json::from_str(&text)
        .map_err(|e| ToolError::Http(format!("invalid JSON from {url}: {e}")))
}

async fn oh_post(
    client: &reqwest::Client,
    base: &str,
    endpoint: &str,
    body: &Value,
) -> Result<Value, ToolError> {
    let url = build_url(base, endpoint);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(ToolError::Http(format!("HTTP {}: {}", status.as_u16(), text)));
    }
    serde_json::from_str(&text)
        .map_err(|e| ToolError::Http(format!("invalid JSON from {url}: {e}")))
}

// ── Response shaping (matches the Python dict construction) ────────────────────

/// Build the full task string: prepend the working dir line only when it differs
/// from the default, exactly as the Python does.
fn build_full_task(task: &str, working_dir: &str) -> String {
    if !working_dir.is_empty() && working_dir != DEFAULT_WORKING_DIR {
        format!("Working directory: {working_dir}\n\n{task}")
    } else {
        task.to_string()
    }
}

fn shape_run_result(result: &Value) -> Value {
    json!({
        "conversation_id": result.get("conversation_id").cloned().unwrap_or(Value::Null),
        "status": result.get("conversation_status").and_then(Value::as_str).unwrap_or("STARTING"),
        "message": result.get("message").cloned().unwrap_or(Value::Null),
        "poll_with": "openhands_get_status",
    })
}

fn shape_status(
    conversation_id: &str,
    conv: &Value,
    include_events: bool,
    events: Option<&Value>,
) -> Value {
    let status = conv.get("status").cloned().unwrap_or(Value::Null);
    let complete = conv.get("status").and_then(Value::as_str) == Some("STOPPED");
    let mut result = json!({
        "conversation_id": conversation_id,
        "status": status,
        "runtime_status": conv.get("runtime_status").cloned().unwrap_or(Value::Null),
        "title": conv.get("title").cloned().unwrap_or(Value::Null),
        "last_updated": conv.get("last_updated_at").cloned().unwrap_or(Value::Null),
        "complete": complete,
    });
    if include_events {
        let recent = shape_recent_events(events.unwrap_or(&Value::Null));
        if let Some(obj) = result.as_object_mut() {
            obj.insert("recent_events".into(), recent);
        }
    }
    result
}

/// Take the last 5 of the returned events and project them, matching Python.
fn shape_recent_events(events_resp: &Value) -> Value {
    let events = events_resp
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let start = events.len().saturating_sub(5);
    let projected: Vec<Value> = events[start..]
        .iter()
        .map(|e| {
            let message = e.get("message").and_then(Value::as_str).unwrap_or("");
            let truncated: String = message.chars().take(200).collect();
            json!({
                "source": e.get("source").cloned().unwrap_or(Value::Null),
                "observation": e.get("observation").cloned().unwrap_or(Value::Null),
                "message": truncated,
            })
        })
        .collect();
    Value::Array(projected)
}

fn shape_conversations(result: &Value, limit: usize) -> Value {
    let convs = result
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let projected: Vec<Value> = convs
        .iter()
        .take(limit)
        .map(|c| {
            json!({
                "conversation_id": c.get("conversation_id").cloned().unwrap_or(Value::Null),
                "title": c.get("title").cloned().unwrap_or(Value::Null),
                "status": c.get("status").cloned().unwrap_or(Value::Null),
                "runtime_status": c.get("runtime_status").cloned().unwrap_or(Value::Null),
                "last_updated": c.get("last_updated_at").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({
        "conversations": projected,
        "total": projected.len(),
    })
}

// ── Tool structs ──────────────────────────────────────────────────────────────

struct OpenHandsRunTask {
    config: OpenHandsConfig,
}
struct OpenHandsGetStatus {
    config: OpenHandsConfig,
}
struct OpenHandsListConversations {
    config: OpenHandsConfig,
}

#[async_trait]
impl RustTool for OpenHandsRunTask {
    fn name(&self) -> &str {
        "openhands_run_task"
    }

    fn description(&self) -> &str {
        "Send a task to OpenHands and return the conversation_id. OpenHands handles \
scaffolding, file gen, builds, and format conversion. working_dir: directory context \
for the task (default: <path>/arcade). model: optional model override. Returns \
conversation_id for polling via openhands_get_status. GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task":        { "type": "string", "description": "The task to send to OpenHands (required)" },
                "working_dir": { "type": "string", "description": "Directory context for the task (default: <path>/arcade)" },
                "model":       { "type": "string", "description": "Optional model override (default: OpenHands default-coder via LiteLLM)" }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let working_dir = args
            .get("working_dir")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_WORKING_DIR)
            .to_string();

        let summary = format!(
            "OpenHands: start a new task in '{}' — task: {}",
            working_dir,
            task.chars().take(120).collect::<String>()
        );
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        if task.is_empty() {
            return Err(ToolError::InvalidArgument("task is required".into()));
        }

        let base = self.config.url()?;
        let client = OpenHandsConfig::client()?;

        let full_task = build_full_task(&task, &working_dir);
        let body = json!({ "initial_user_msg": full_task });

        let result = oh_post(&client, base, "/api/conversations", &body).await?;
        Ok(shape_run_result(&result).to_string())
    }
}

#[async_trait]
impl RustTool for OpenHandsGetStatus {
    fn name(&self) -> &str {
        "openhands_get_status"
    }

    fn description(&self) -> &str {
        "Get the status of an OpenHands task by conversation_id. Returns status \
(STARTING/RUNNING/STOPPED), runtime_status, and optionally recent events. Poll until \
status is STOPPED to get the final result. include_events: if True, returns recent \
events for progress detail. GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "conversation_id": { "type": "string",  "description": "The conversation id to inspect (required)" },
                "include_events":  { "type": "boolean", "description": "If true, include the last few events (default false)" }
            },
            "required": ["conversation_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let conversation_id = args
            .get("conversation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let include_events = args
            .get("include_events")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let summary = format!("OpenHands: get status of conversation '{conversation_id}'");
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        if conversation_id.is_empty() {
            return Err(ToolError::InvalidArgument("conversation_id is required".into()));
        }

        let base = self.config.url()?;
        let client = OpenHandsConfig::client()?;

        let conv = oh_get(
            &client,
            base,
            &format!("/api/conversations/{conversation_id}"),
        )
        .await?;

        let events = if include_events {
            Some(
                oh_get(
                    &client,
                    base,
                    &format!("/api/conversations/{conversation_id}/events?limit=10"),
                )
                .await?,
            )
        } else {
            None
        };

        Ok(shape_status(&conversation_id, &conv, include_events, events.as_ref()).to_string())
    }
}

#[async_trait]
impl RustTool for OpenHandsListConversations {
    fn name(&self) -> &str {
        "openhands_list_conversations"
    }

    fn description(&self) -> &str {
        "List recent OpenHands conversations/tasks. Returns conversation_id, title, \
status, and last_updated for each. GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max conversations to return (default 10)" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;

        let summary = format!("OpenHands: list up to {limit} recent conversations");
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        let base = self.config.url()?;
        let client = OpenHandsConfig::client()?;

        let result = oh_get(&client, base, "/api/conversations").await?;
        Ok(shape_conversations(&result, limit).to_string())
    }
}

// ── Registration ──────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    let config = OpenHandsConfig::from_env();
    if config.base_url.is_none() {
        tracing::warn!(
            "OpenHands tools not configured: OPENHANDS_URL is unset. \
Registering tools that will return NotConfigured until set."
        );
    }
    registry.register_or_replace(Box::new(OpenHandsRunTask {
        config: config.clone(),
    }));
    registry.register_or_replace(Box::new(OpenHandsGetStatus {
        config: config.clone(),
    }));
    registry.register_or_replace(Box::new(OpenHandsListConversations { config }));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cfg(url: Option<&str>) -> OpenHandsConfig {
        OpenHandsConfig {
            base_url: url.map(str::to_string),
        }
    }

    // ── config / url building ────────────────────────────────────────────────

    #[test]
    #[serial]
    fn config_url_strips_trailing_slash_via_from_env() {
        std::env::set_var("OPENHANDS_URL", "http://example.test:3000/");
        let c = OpenHandsConfig::from_env();
        assert_eq!(c.base_url.as_deref(), Some("http://example.test:3000"));
        std::env::remove_var("OPENHANDS_URL");
    }

    #[test]
    #[serial]
    fn config_url_unset_is_none() {
        std::env::remove_var("OPENHANDS_URL");
        let c = OpenHandsConfig::from_env();
        assert!(c.base_url.is_none());
        assert!(matches!(c.url(), Err(ToolError::NotConfigured(_))));
    }

    #[test]
    fn build_url_concatenates() {
        assert_eq!(
            build_url("http://h:3000", "/api/conversations"),
            "http://h:3000/api/conversations"
        );
        assert_eq!(
            build_url("http://h:3000", "/api/conversations/abc/events?limit=10"),
            "http://h:3000/api/conversations/abc/events?limit=10"
        );
    }

    // ── full-task construction (matches Python default-dir logic) ─────────────

    #[test]
    fn full_task_default_dir_is_passthrough() {
        let out = build_full_task("do the thing", DEFAULT_WORKING_DIR);
        assert_eq!(out, "do the thing");
    }

    #[test]
    fn full_task_custom_dir_prepends_line() {
        let out = build_full_task("do the thing", "/srv/proj");
        assert_eq!(out, "Working directory: /srv/proj\n\ndo the thing");
    }

    #[test]
    fn full_task_empty_dir_is_passthrough() {
        let out = build_full_task("task", "");
        assert_eq!(out, "task");
    }

    // ── response shaping on samples ──────────────────────────────────────────

    #[test]
    fn shape_run_result_uses_status_default() {
        let sample = json!({ "conversation_id": "c1" });
        let out = shape_run_result(&sample);
        assert_eq!(out["conversation_id"], "c1");
        assert_eq!(out["status"], "STARTING");
        assert_eq!(out["poll_with"], "openhands_get_status");
    }

    #[test]
    fn shape_run_result_reads_conversation_status() {
        let sample = json!({
            "conversation_id": "c2",
            "conversation_status": "RUNNING",
            "message": "started"
        });
        let out = shape_run_result(&sample);
        assert_eq!(out["status"], "RUNNING");
        assert_eq!(out["message"], "started");
    }

    #[test]
    fn shape_status_complete_when_stopped() {
        let conv = json!({
            "status": "STOPPED",
            "runtime_status": "ready",
            "title": "T",
            "last_updated_at": "2026-06-08T00:00:00Z" // pii-test-fixture
        });
        let out = shape_status("cid", &conv, false, None);
        assert_eq!(out["conversation_id"], "cid");
        assert_eq!(out["status"], "STOPPED");
        assert_eq!(out["complete"], true);
        assert_eq!(out["title"], "T");
        assert!(out.get("recent_events").is_none());
    }

    #[test]
    fn shape_status_not_complete_when_running() {
        let conv = json!({ "status": "RUNNING" });
        let out = shape_status("cid", &conv, false, None);
        assert_eq!(out["complete"], false);
    }

    #[test]
    fn shape_status_includes_last_five_events_truncated() {
        let mut events = Vec::new();
        for i in 0..7 {
            events.push(json!({
                "source": "agent",
                "observation": format!("obs{i}"),
                "message": "x".repeat(250)
            }));
        }
        let events_resp = json!({ "events": events });
        let conv = json!({ "status": "RUNNING" });
        let out = shape_status("cid", &conv, true, Some(&events_resp));
        let recent = out["recent_events"].as_array().unwrap();
        // Python takes events[-5:] of the returned list.
        assert_eq!(recent.len(), 5);
        // First of the recent five corresponds to index 2 (obs2).
        assert_eq!(recent[0]["observation"], "obs2");
        // Message truncated to 200 chars.
        assert_eq!(recent[0]["message"].as_str().unwrap().chars().count(), 200);
    }

    #[test]
    fn shape_status_handles_no_events_key() {
        let conv = json!({ "status": "RUNNING" });
        let out = shape_status("cid", &conv, true, Some(&json!({})));
        assert_eq!(out["recent_events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn shape_conversations_limits_and_projects() {
        let mut results = Vec::new();
        for i in 0..5 {
            results.push(json!({
                "conversation_id": format!("c{i}"),
                "title": format!("T{i}"),
                "status": "STOPPED",
                "runtime_status": "ready",
                "last_updated_at": "2026-06-08" // pii-test-fixture
            }));
        }
        let body = json!({ "results": results });
        let out = shape_conversations(&body, 3);
        let convs = out["conversations"].as_array().unwrap();
        assert_eq!(convs.len(), 3);
        assert_eq!(out["total"], 3);
        assert_eq!(convs[0]["conversation_id"], "c0");
        assert_eq!(convs[2]["title"], "T2");
    }

    #[test]
    fn shape_conversations_empty_results() {
        let out = shape_conversations(&json!({}), 10);
        assert_eq!(out["total"], 0);
        assert_eq!(out["conversations"].as_array().unwrap().len(), 0);
    }

    // ── approval gate is enforced before any action ──────────────────────────
    //
    // With DATABASE_URL unset the gate cannot reach Postgres, so it must Deny
    // and the tool must return that message verbatim (NOT perform HTTP, NOT
    // return a NotConfigured/InvalidArgument error from the real action path).

    #[tokio::test]
    #[serial]
    async fn run_task_blocked_by_gate_without_db() {
        std::env::remove_var("DATABASE_URL");
        let tool = OpenHandsRunTask {
            config: cfg(Some("http://example.test:3000")),
        };
        let out = tool.execute(json!({ "task": "build a thing" })).await.unwrap();
        assert!(
            out.contains("unavailable") || out.contains("DATABASE_URL") || out.contains("APPROVAL")
        );
    }

    #[tokio::test]
    #[serial]
    async fn get_status_blocked_by_gate_without_db() {
        std::env::remove_var("DATABASE_URL");
        let tool = OpenHandsGetStatus {
            config: cfg(Some("http://example.test:3000")),
        };
        let out = tool
            .execute(json!({ "conversation_id": "abc" }))
            .await
            .unwrap();
        assert!(
            out.contains("unavailable") || out.contains("DATABASE_URL") || out.contains("APPROVAL")
        );
    }

    #[tokio::test]
    #[serial]
    async fn list_conversations_blocked_by_gate_without_db() {
        std::env::remove_var("DATABASE_URL");
        let tool = OpenHandsListConversations {
            config: cfg(Some("http://example.test:3000")),
        };
        let out = tool.execute(json!({ "limit": 5 })).await.unwrap();
        assert!(
            out.contains("unavailable") || out.contains("DATABASE_URL") || out.contains("APPROVAL")
        );
    }

    // ── registration ─────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn register_adds_three_tools() {
        let mut reg = ToolRegistry::new();
        let backup = std::env::var("OPENHANDS_URL").ok();
        std::env::remove_var("OPENHANDS_URL");
        register(&mut reg);
        if let Some(v) = backup {
            std::env::set_var("OPENHANDS_URL", v);
        }
        assert!(reg.contains("openhands_run_task"));
        assert!(reg.contains("openhands_get_status"));
        assert!(reg.contains("openhands_list_conversations"));
        assert_eq!(reg.len(), 3);
    }

    #[test]
    fn tool_names_are_stable() {
        let c = cfg(None);
        assert_eq!(
            OpenHandsRunTask { config: c.clone() }.name(),
            "openhands_run_task"
        );
        assert_eq!(
            OpenHandsGetStatus { config: c.clone() }.name(),
            "openhands_get_status"
        );
        assert_eq!(
            OpenHandsListConversations { config: c }.name(),
            "openhands_list_conversations"
        );
    }

    #[test]
    fn tool_parameters_are_valid_schema() {
        let c = cfg(None);
        let r = OpenHandsRunTask { config: c.clone() }.parameters();
        let s = OpenHandsGetStatus { config: c.clone() }.parameters();
        let l = OpenHandsListConversations { config: c }.parameters();
        assert_eq!(r["type"], "object");
        assert_eq!(s["type"], "object");
        assert_eq!(l["type"], "object");
        assert_eq!(r["required"][0], "task");
        assert_eq!(s["required"][0], "conversation_id");
    }
}
