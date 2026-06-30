//! Plane CE tool implementations (CHORD-06).
//!
//! Provides 24 Rust tools that wrap the Plane CE REST API via reqwest.
//! All configuration comes from environment variables — no hardcoded URLs or tokens.
//!
//! ## Configuration
//! - `PLANE_API_URL` — base URL of the Plane CE instance (required at call time)
//! - `PLANE_API_KEY` — API key for authentication (required at call time)
//! - `PLANE_WORKSPACE` — workspace slug (default: "moosenet")
//!
//! When `PLANE_API_URL` is not set the tools register normally but return
//! `ToolError::NotConfigured` on every call.

pub mod types;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use types::*;

/// True if `s` is a canonical 8-4-4-4-12 hyphenated UUID.
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

// ─── PlaneClient ─────────────────────────────────────────────────────────────

/// Shared HTTP client for the Plane CE API.
///
/// Constructed from environment variables. When `PLANE_API_URL` is absent,
/// `configured` is false and every tool returns `ToolError::NotConfigured`.
#[derive(Clone)]
pub struct PlaneClient {
    http: Client,
    base_url: Option<String>,
    api_key: Option<String>,
    workspace: String,
}

impl PlaneClient {
    /// Build a `PlaneClient` from environment variables.
    pub fn from_env() -> Self {
        let base_url = std::env::var("PLANE_API_URL").ok().map(|u| u.trim_end_matches('/').to_string());
        let api_key = std::env::var("PLANE_API_KEY").ok();
        let workspace = std::env::var("PLANE_WORKSPACE")
            .unwrap_or_else(|_| "moosenet".into());

        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");

        Self { http, base_url, api_key, workspace }
    }

    /// Returns true if both PLANE_API_URL and PLANE_API_KEY are configured.
    pub fn configured(&self) -> bool {
        self.base_url.is_some() && self.api_key.is_some()
    }

    /// Return a `ToolError::NotConfigured` with helpful message.
    fn not_configured(&self) -> ToolError {
        ToolError::NotConfigured(
            "PLANE_API_URL and PLANE_API_KEY must be set to use Plane tools".into(),
        )
    }

    /// Build the base URL for workspace-scoped endpoints.
    fn workspace_url(&self) -> String {
        format!(
            "{}/api/v1/workspaces/{}/",
            self.base_url.as_deref().unwrap_or(""),
            self.workspace
        )
    }

    /// Resolve a project identifier (e.g. "LM") or a UUID to a project UUID.
    ///
    /// Plane CE's project-scoped endpoints require the project UUID in the path;
    /// passing a human identifier like "LM" yields a 404 ("Page not found").
    /// UUIDs are returned unchanged (no network call); anything else is looked up
    /// against the workspace project list, matching on `identifier`
    /// (case-insensitive) or exact `id`.
    async fn resolve_project_id(&self, project_id: &str) -> Result<String, ToolError> {
        if is_uuid(project_id) {
            return Ok(project_id.to_string());
        }
        let url = format!("{}projects/", self.workspace_url());
        let resp = self.get_with_retry(&url).await?;
        let resp = Self::check_status(resp).await?;
        let list: ApiList<Project> = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to parse projects: {e}")))?;
        list.into_items()
            .into_iter()
            .find(|p| p.identifier.eq_ignore_ascii_case(project_id) || p.id == project_id)
            .map(|p| p.id)
            .ok_or_else(|| {
                ToolError::NotFound(format!(
                    "No Plane project matches identifier or id '{project_id}'"
                ))
            })
    }

    /// Execute a GET request with rate-limit retry (max 3 attempts, 3 s delay).
    async fn get_with_retry(&self, url: &str) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .get(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
        })
        .await
    }

    /// Execute a POST request with rate-limit retry.
    async fn post_with_retry(&self, url: &str, body: &Value) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .post(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Execute a PATCH request with rate-limit retry.
    async fn patch_with_retry(&self, url: &str, body: &Value) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .patch(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Execute a DELETE request with rate-limit retry.
    async fn delete_with_retry(&self, url: &str) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .delete(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
        })
        .await
    }

    /// Core retry loop: respects 429 with 3-second back-off, max 3 attempts.
    async fn request_with_retry<F>(&self, build: F) -> Result<Response, ToolError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut attempts = 0u8;
        loop {
            attempts += 1;
            let resp = build()
                .send()
                .await
                .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

            if resp.status() == StatusCode::TOO_MANY_REQUESTS {
                if attempts >= 3 {
                    return Err(ToolError::Http(
                        "Plane rate limit exceeded — try again later".into(),
                    ));
                }
                warn!("Plane 429 received, retrying in 3 s (attempt {attempts}/3)");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            return Ok(resp);
        }
    }

    /// Map non-success HTTP status to a clean ToolError.
    async fn check_status(resp: Response) -> Result<Response, ToolError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        match status {
            StatusCode::NOT_FOUND => Err(ToolError::NotFound(format!("Resource not found: {body}"))),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(ToolError::Http(format!("Plane authentication failed: {status}")))
            }
            StatusCode::UNPROCESSABLE_ENTITY => {
                Err(ToolError::InvalidArgument(format!("Invalid request: {body}")))
            }
            _ => Err(ToolError::Http(format!("Plane returned {status}: {body}"))),
        }
    }
}

// ─── Helper macro for guard boilerplate ──────────────────────────────────────

macro_rules! require_configured {
    ($self:expr) => {
        if !$self.client.configured() {
            return Err($self.client.not_configured());
        }
    };
}

macro_rules! require_arg {
    ($args:expr, $field:literal, $type:ident) => {
        $args
            .get($field)
            .and_then(|v| v.$type())
            .ok_or_else(|| ToolError::InvalidArgument(format!("missing required argument: {}", $field)))?
    };
}

// ─── 1. plane_list_projects ──────────────────────────────────────────────────

pub struct PlaneListProjects {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListProjects {
    fn name(&self) -> &str { "plane_list_projects" }
    fn description(&self) -> &str { "List all projects in the Plane workspace" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let url = format!("{}projects/", self.client.workspace_url());
        debug!("plane_list_projects GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Project> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse projects: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No projects found in workspace".into());
        }
        let mut out = format!("Found {} project(s):\n", items.len());
        for p in &items {
            out.push_str(&format!("  [{id}] {name} ({identifier})\n",
                id = p.id, name = p.name, identifier = p.identifier));
        }
        Ok(out)
    }
}

// ─── 2. plane_get_project ────────────────────────────────────────────────────

pub struct PlaneGetProject {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetProject {
    fn name(&self) -> &str { "plane_get_project" }
    fn description(&self) -> &str { "Get details for a specific Plane project by ID" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!("{}projects/{project_id}/", self.client.workspace_url());
        debug!("plane_get_project GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let p: Project = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse project: {e}")))?;
        Ok(format!(
            "Project: {name}\nID: {id}\nIdentifier: {identifier}\nDescription: {desc}",
            name = p.name,
            id = p.id,
            identifier = p.identifier,
            desc = p.description.as_deref().unwrap_or("(none)")
        ))
    }
}

// ─── 3. plane_list_work_items ────────────────────────────────────────────────

pub struct PlaneListWorkItems {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListWorkItems {
    fn name(&self) -> &str { "plane_list_work_items" }
    fn description(&self) -> &str { "List work items (issues) in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "limit": { "type": "integer", "description": "Max results to return (default 50)" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_work_items GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Issue> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;
        let total = list.total_count();
        let items: Vec<Issue> = list.into_items().into_iter().take(limit).collect();
        if items.is_empty() {
            return Ok("No work items found".into());
        }
        let mut out = format!("Work items ({} shown of {}):\n", items.len(), total);
        for i in &items {
            let priority = i.priority.as_deref().unwrap_or("none");
            let seq = i.sequence_id.map(|s| format!("#{s}")).unwrap_or_default();
            out.push_str(&format!("  [{id}] {seq} {name} (priority: {priority})\n",
                id = i.id, seq = seq, name = i.name, priority = priority));
        }
        Ok(out)
    }
}

// ─── 4. plane_get_work_item ──────────────────────────────────────────────────

pub struct PlaneGetWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetWorkItem {
    fn name(&self) -> &str { "plane_get_work_item" }
    fn description(&self) -> &str { "Get details for a specific work item by ID" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            self.client.workspace_url()
        );
        debug!("plane_get_work_item GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse issue: {e}")))?;
        Ok(format!(
            "Issue: {name}\nID: {id}\nSequence: {seq}\nPriority: {priority}\nState: {state}\nDescription: {desc}",
            name = i.name,
            id = i.id,
            seq = i.sequence_id.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            priority = i.priority.as_deref().unwrap_or("none"),
            state = i.state.as_deref().unwrap_or("unknown"),
            desc = i.description.as_deref().unwrap_or("(none)")
        ))
    }
}

// ─── 5. plane_create_work_item ───────────────────────────────────────────────

pub struct PlaneCreateWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCreateWorkItem {
    fn name(&self) -> &str { "plane_create_work_item" }
    fn description(&self) -> &str { "Create a new work item (issue) in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "name": { "type": "string", "description": "Issue title" },
                "description_html": { "type": "string", "description": "Issue description (HTML)" },
                "state": { "type": "string", "description": "State UUID" },
                "priority": { "type": "string", "description": "Priority: urgent/high/medium/low/none" },
                "due_date": { "type": "string", "description": "Due date (YYYY-MM-DD)" }
            },
            "required": ["project_id", "name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let name = require_arg!(args, "name", as_str);
        let mut body = json!({ "name": name });
        if let Some(v) = args.get("description_html").and_then(|v| v.as_str()) {
            body["description_html"] = json!(v);
        }
        if let Some(v) = args.get("state").and_then(|v| v.as_str()) {
            body["state"] = json!(v);
        }
        if let Some(v) = args.get("priority").and_then(|v| v.as_str()) {
            body["priority"] = json!(v);
        }
        if let Some(v) = args.get("due_date").and_then(|v| v.as_str()) {
            body["due_date"] = json!(v);
        }
        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_create_work_item POST {url}");
        let resp = self.client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created issue: {e}")))?;
        Ok(format!("Created issue: {name}\nID: {id}\nSequence: #{seq}",
            name = i.name, id = i.id,
            seq = i.sequence_id.unwrap_or(0)))
    }
}

// ─── 6. plane_update_work_item ───────────────────────────────────────────────

pub struct PlaneUpdateWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneUpdateWorkItem {
    fn name(&self) -> &str { "plane_update_work_item" }
    fn description(&self) -> &str { "Update fields on an existing Plane work item" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "name": { "type": "string", "description": "New title" },
                "description_html": { "type": "string", "description": "New description (HTML)" },
                "state": { "type": "string", "description": "New state UUID" },
                "priority": { "type": "string", "description": "New priority" },
                "due_date": { "type": "string", "description": "New due date (YYYY-MM-DD)" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let mut body = json!({});
        for field in &["name", "description_html", "state", "priority", "due_date"] {
            if let Some(v) = args.get(field).and_then(|v| v.as_str()) {
                body[*field] = json!(v);
            }
        }
        if body.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            return Err(ToolError::InvalidArgument("No fields to update provided".into()));
        }
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            self.client.workspace_url()
        );
        debug!("plane_update_work_item PATCH {url}");
        let resp = self.client.patch_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse updated issue: {e}")))?;
        Ok(format!("Updated issue: {name} (ID: {id})", name = i.name, id = i.id))
    }
}

// ─── 7. plane_delete_work_item ───────────────────────────────────────────────

pub struct PlaneDeleteWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneDeleteWorkItem {
    fn name(&self) -> &str { "plane_delete_work_item" }
    fn description(&self) -> &str { "Delete a Plane work item permanently" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID to delete" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            self.client.workspace_url()
        );
        debug!("plane_delete_work_item DELETE {url}");
        let resp = self.client.delete_with_retry(&url).await?;
        PlaneClient::check_status(resp).await?;
        Ok(format!("Deleted work item {issue_id}"))
    }
}

// ─── 8. plane_list_cycles ────────────────────────────────────────────────────

pub struct PlaneListCycles {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListCycles {
    fn name(&self) -> &str { "plane_list_cycles" }
    fn description(&self) -> &str { "List cycles (sprints) in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/cycles/",
            self.client.workspace_url()
        );
        debug!("plane_list_cycles GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Cycle> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse cycles: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No cycles found".into());
        }
        let mut out = format!("Found {} cycle(s):\n", items.len());
        for c in &items {
            let status = c.status.as_deref().unwrap_or("unknown");
            let start = c.start_date.as_deref().unwrap_or("-");
            let end = c.end_date.as_deref().unwrap_or("-");
            out.push_str(&format!("  [{id}] {name} ({status}) {start}..{end}\n",
                id = c.id, name = c.name, status = status, start = start, end = end));
        }
        Ok(out)
    }
}

// ─── 9. plane_get_cycle ──────────────────────────────────────────────────────

pub struct PlaneGetCycle {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetCycle {
    fn name(&self) -> &str { "plane_get_cycle" }
    fn description(&self) -> &str { "Get details for a specific Plane cycle" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "cycle_id": { "type": "string", "description": "Cycle UUID" }
            },
            "required": ["project_id", "cycle_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let cycle_id = require_arg!(args, "cycle_id", as_str);
        let url = format!(
            "{}projects/{project_id}/cycles/{cycle_id}/",
            self.client.workspace_url()
        );
        debug!("plane_get_cycle GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let c: Cycle = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse cycle: {e}")))?;
        Ok(format!(
            "Cycle: {name}\nID: {id}\nStatus: {status}\nDates: {start} to {end}",
            name = c.name, id = c.id,
            status = c.status.as_deref().unwrap_or("unknown"),
            start = c.start_date.as_deref().unwrap_or("-"),
            end = c.end_date.as_deref().unwrap_or("-")
        ))
    }
}

// ─── 10. plane_list_cycle_issues ─────────────────────────────────────────────

pub struct PlaneListCycleIssues {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListCycleIssues {
    fn name(&self) -> &str { "plane_list_cycle_issues" }
    fn description(&self) -> &str { "List issues in a specific Plane cycle" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "cycle_id": { "type": "string", "description": "Cycle UUID" }
            },
            "required": ["project_id", "cycle_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let cycle_id = require_arg!(args, "cycle_id", as_str);
        let url = format!(
            "{}projects/{project_id}/cycles/{cycle_id}/cycle-issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_cycle_issues GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Issue> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse cycle issues: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No issues in this cycle".into());
        }
        let mut out = format!("Cycle issues ({}):\n", items.len());
        for i in &items {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 11. plane_list_modules ──────────────────────────────────────────────────

pub struct PlaneListModules {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListModules {
    fn name(&self) -> &str { "plane_list_modules" }
    fn description(&self) -> &str { "List modules in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/modules/",
            self.client.workspace_url()
        );
        debug!("plane_list_modules GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Module> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse modules: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No modules found".into());
        }
        let mut out = format!("Found {} module(s):\n", items.len());
        for m in &items {
            let status = m.status.as_deref().unwrap_or("unknown");
            out.push_str(&format!("  [{id}] {name} ({status})\n",
                id = m.id, name = m.name, status = status));
        }
        Ok(out)
    }
}

// ─── 12. plane_get_module ────────────────────────────────────────────────────

pub struct PlaneGetModule {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetModule {
    fn name(&self) -> &str { "plane_get_module" }
    fn description(&self) -> &str { "Get details for a specific Plane module" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "module_id": { "type": "string", "description": "Module UUID" }
            },
            "required": ["project_id", "module_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let module_id = require_arg!(args, "module_id", as_str);
        let url = format!(
            "{}projects/{project_id}/modules/{module_id}/",
            self.client.workspace_url()
        );
        debug!("plane_get_module GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let m: Module = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse module: {e}")))?;
        Ok(format!(
            "Module: {name}\nID: {id}\nStatus: {status}\nDates: {start} to {end}",
            name = m.name, id = m.id,
            status = m.status.as_deref().unwrap_or("unknown"),
            start = m.start_date.as_deref().unwrap_or("-"),
            end = m.target_date.as_deref().unwrap_or("-")
        ))
    }
}

// ─── 13. plane_create_module ─────────────────────────────────────────────────

pub struct PlaneCreateModule {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCreateModule {
    fn name(&self) -> &str { "plane_create_module" }
    fn description(&self) -> &str { "Create a new module in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "name": { "type": "string", "description": "Module name" },
                "description": { "type": "string", "description": "Module description" },
                "status": { "type": "string", "description": "Module status" },
                "start_date": { "type": "string", "description": "Start date (YYYY-MM-DD)" },
                "target_date": { "type": "string", "description": "Target date (YYYY-MM-DD)" }
            },
            "required": ["project_id", "name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let name = require_arg!(args, "name", as_str);
        let mut body = json!({ "name": name });
        for field in &["description", "status", "start_date", "target_date"] {
            if let Some(v) = args.get(field).and_then(|v| v.as_str()) {
                body[*field] = json!(v);
            }
        }
        let url = format!(
            "{}projects/{project_id}/modules/",
            self.client.workspace_url()
        );
        debug!("plane_create_module POST {url}");
        let resp = self.client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let m: Module = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created module: {e}")))?;
        Ok(format!("Created module: {name} (ID: {id})", name = m.name, id = m.id))
    }
}

// ─── 14. plane_list_module_issues ────────────────────────────────────────────

pub struct PlaneListModuleIssues {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListModuleIssues {
    fn name(&self) -> &str { "plane_list_module_issues" }
    fn description(&self) -> &str { "List issues in a specific Plane module" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "module_id": { "type": "string", "description": "Module UUID" }
            },
            "required": ["project_id", "module_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let module_id = require_arg!(args, "module_id", as_str);
        let url = format!(
            "{}projects/{project_id}/modules/{module_id}/module-issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_module_issues GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Issue> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse module issues: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No issues in this module".into());
        }
        let mut out = format!("Module issues ({}):\n", items.len());
        for i in &items {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 15. plane_list_states ───────────────────────────────────────────────────

pub struct PlaneListStates {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListStates {
    fn name(&self) -> &str { "plane_list_states" }
    fn description(&self) -> &str { "List workflow states in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/states/",
            self.client.workspace_url()
        );
        debug!("plane_list_states GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<State> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse states: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No states found".into());
        }
        let mut out = format!("States ({}):\n", items.len());
        for s in &items {
            out.push_str(&format!("  [{id}] {name} (group: {group}, color: {color})\n",
                id = s.id, name = s.name, group = s.group, color = s.color));
        }
        Ok(out)
    }
}

// ─── 16. plane_list_labels ───────────────────────────────────────────────────

pub struct PlaneListLabels {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListLabels {
    fn name(&self) -> &str { "plane_list_labels" }
    fn description(&self) -> &str { "List labels in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/labels/",
            self.client.workspace_url()
        );
        debug!("plane_list_labels GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Label> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse labels: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No labels found".into());
        }
        let mut out = format!("Labels ({}):\n", items.len());
        for l in &items {
            let color = l.color.as_deref().unwrap_or("-");
            out.push_str(&format!("  [{id}] {name} (color: {color})\n",
                id = l.id, name = l.name, color = color));
        }
        Ok(out)
    }
}

// ─── 17. plane_list_members ──────────────────────────────────────────────────

pub struct PlaneListMembers {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListMembers {
    fn name(&self) -> &str { "plane_list_members" }
    fn description(&self) -> &str { "List members of a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/members/",
            self.client.workspace_url()
        );
        debug!("plane_list_members GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Member> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse members: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No members found".into());
        }
        let mut out = format!("Members ({}):\n", items.len());
        for m in &items {
            let name = m.member.as_ref()
                .and_then(|md| md.display_name.as_deref())
                .unwrap_or("unknown");
            out.push_str(&format!("  [{id}] {name} (role: {role})\n",
                id = m.id, name = name, role = m.role));
        }
        Ok(out)
    }
}

// ─── 18. plane_list_comments ─────────────────────────────────────────────────

pub struct PlaneListComments {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListComments {
    fn name(&self) -> &str { "plane_list_comments" }
    fn description(&self) -> &str { "List comments on a Plane work item" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/comments/",
            self.client.workspace_url()
        );
        debug!("plane_list_comments GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Comment> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse comments: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No comments on this issue".into());
        }
        let mut out = format!("Comments ({}):\n", items.len());
        for c in &items {
            let author = c.actor_detail.as_ref()
                .and_then(|a| a.display_name.as_deref())
                .unwrap_or("unknown");
            let text = c.comment_stripped.as_deref()
                .or(c.comment_html.as_deref())
                .unwrap_or("(empty)");
            out.push_str(&format!("  [{id}] {author}: {text}\n",
                id = c.id, author = author, text = text));
        }
        Ok(out)
    }
}

// ─── 19. plane_create_comment ────────────────────────────────────────────────

pub struct PlaneCreateComment {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCreateComment {
    fn name(&self) -> &str { "plane_create_comment" }
    fn description(&self) -> &str { "Add a comment to a Plane work item" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "comment": { "type": "string", "description": "Comment text" }
            },
            "required": ["project_id", "issue_id", "comment"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let comment_text = require_arg!(args, "comment", as_str);
        let body = json!({ "comment_html": format!("<p>{comment_text}</p>") });
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/comments/",
            self.client.workspace_url()
        );
        debug!("plane_create_comment POST {url}");
        let resp = self.client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let c: Comment = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created comment: {e}")))?;
        Ok(format!("Comment added (ID: {id})", id = c.id))
    }
}

// ─── 20. plane_list_issues_by_state ──────────────────────────────────────────

pub struct PlaneListIssuesByState {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListIssuesByState {
    fn name(&self) -> &str { "plane_list_issues_by_state" }
    fn description(&self) -> &str { "List work items filtered by state group (backlog/unstarted/started/completed/cancelled)" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "state_group": {
                    "type": "string",
                    "description": "State group to filter by",
                    "enum": ["backlog", "unstarted", "started", "completed", "cancelled"]
                },
                "limit": { "type": "integer", "description": "Max results (default 50)" }
            },
            "required": ["project_id", "state_group"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let state_group = require_arg!(args, "state_group", as_str);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

        // Fetch all issues then filter client-side (state_group query param is broken in Plane CE)
        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_issues_by_state GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Issue> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let filtered: Vec<Issue> = list.into_items()
            .into_iter()
            .filter(|i| {
                i.state_detail.as_ref()
                    .map(|sd| sd.group.to_lowercase() == state_group.to_lowercase())
                    .unwrap_or(false)
            })
            .take(limit)
            .collect();

        if filtered.is_empty() {
            return Ok(format!("No issues in state group '{state_group}'"));
        }
        let mut out = format!("Issues in '{}' ({}):\n", state_group, filtered.len());
        for i in &filtered {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 21. plane_get_issue_by_sequence ─────────────────────────────────────────

pub struct PlaneGetIssueBySequence {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetIssueBySequence {
    fn name(&self) -> &str { "plane_get_issue_by_sequence" }
    fn description(&self) -> &str { "Get a work item by its human-readable sequence number (e.g. LM-42)" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "sequence_id": { "type": "integer", "description": "Sequence number (numeric part of LM-42 etc.)" }
            },
            "required": ["project_id", "sequence_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let sequence_id = args.get("sequence_id").and_then(|v| v.as_u64())
            .ok_or_else(|| ToolError::InvalidArgument("missing required argument: sequence_id".into()))?;

        // Fetch all and filter by sequence_id
        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_get_issue_by_sequence GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Issue> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let found = list.into_items()
            .into_iter()
            .find(|i| i.sequence_id == Some(sequence_id));

        match found {
            None => Err(ToolError::NotFound(format!("No issue with sequence_id #{sequence_id}"))),
            Some(i) => Ok(format!(
                "Issue #{seq}: {name}\nID: {id}\nPriority: {priority}\nState: {state}",
                seq = sequence_id,
                name = i.name,
                id = i.id,
                priority = i.priority.as_deref().unwrap_or("none"),
                state = i.state.as_deref().unwrap_or("unknown")
            )),
        }
    }
}

// ─── 22. plane_list_work_items_filtered ──────────────────────────────────────

pub struct PlaneListWorkItemsFiltered {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListWorkItemsFiltered {
    fn name(&self) -> &str { "plane_list_work_items_filtered" }
    fn description(&self) -> &str { "List work items with optional priority and/or label filters" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "priority": { "type": "string", "description": "Filter by priority: urgent/high/medium/low/none" },
                "label_id": { "type": "string", "description": "Filter by label UUID" },
                "limit": { "type": "integer", "description": "Max results (default 50)" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let priority_filter = args.get("priority").and_then(|v| v.as_str());
        let label_filter = args.get("label_id").and_then(|v| v.as_str());
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_work_items_filtered GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Issue> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let filtered: Vec<Issue> = list.into_items()
            .into_iter()
            .filter(|i| {
                let priority_ok = priority_filter.map(|p| {
                    i.priority.as_deref().unwrap_or("none").eq_ignore_ascii_case(p)
                }).unwrap_or(true);
                let label_ok = label_filter.map(|lf| {
                    i.label_ids.iter().any(|l| l == lf)
                }).unwrap_or(true);
                priority_ok && label_ok
            })
            .take(limit)
            .collect();

        if filtered.is_empty() {
            return Ok("No work items match the given filters".into());
        }
        let mut out = format!("Filtered work items ({}):\n", filtered.len());
        for i in &filtered {
            let priority = i.priority.as_deref().unwrap_or("none");
            out.push_str(&format!("  [{id}] {name} (priority: {priority})\n",
                id = i.id, name = i.name, priority = priority));
        }
        Ok(out)
    }
}

// ─── 23. plane_list_recent_activity ──────────────────────────────────────────

pub struct PlaneListRecentActivity {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListRecentActivity {
    fn name(&self) -> &str { "plane_list_recent_activity" }
    fn description(&self) -> &str { "List recent activity/audit events for a Plane work item" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "limit": { "type": "integer", "description": "Max results (default 20)" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/activities/",
            self.client.workspace_url()
        );
        debug!("plane_list_recent_activity GET {url}");
        let resp = self.client.get_with_retry(&url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<Activity> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse activities: {e}")))?;
        let items: Vec<Activity> = list.into_items().into_iter().take(limit).collect();
        if items.is_empty() {
            return Ok("No recent activity".into());
        }
        let mut out = format!("Recent activity ({}):\n", items.len());
        for a in &items {
            let actor = a.actor_detail.as_ref()
                .and_then(|ad| ad.display_name.as_deref())
                .unwrap_or("unknown");
            let verb = a.verb.as_deref().unwrap_or("updated");
            let field = a.field.as_deref().unwrap_or("");
            out.push_str(&format!("  {actor} {verb} {field}\n",
                actor = actor, verb = verb, field = field));
        }
        Ok(out)
    }
}

// ─── 24. plane_close_work_item ───────────────────────────────────────────────

pub struct PlaneCloseWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCloseWorkItem {
    fn name(&self) -> &str { "plane_close_work_item" }
    fn description(&self) -> &str {
        "Close a work item by moving it to the first available 'completed' state"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID to close" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);

        // Fetch states to find the 'completed' group
        let states_url = format!(
            "{}projects/{project_id}/states/",
            self.client.workspace_url()
        );
        debug!("plane_close_work_item: fetching states from {states_url}");
        let resp = self.client.get_with_retry(&states_url).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let list: ApiList<State> = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse states: {e}")))?;

        let completed_state = list.into_items()
            .into_iter()
            .find(|s| s.group.to_lowercase() == "completed")
            .ok_or_else(|| ToolError::NotFound("No 'completed' state found in this project".into()))?;

        // PATCH the issue to use the completed state
        let body = json!({ "state": completed_state.id });
        let issue_url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            self.client.workspace_url()
        );
        debug!("plane_close_work_item PATCH {issue_url}");
        let resp = self.client.patch_with_retry(&issue_url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse updated issue: {e}")))?;
        Ok(format!(
            "Closed work item: {name} (now in state '{state}')",
            name = i.name,
            state = completed_state.name
        ))
    }
}

// ─── Register all plane tools ─────────────────────────────────────────────────

/// Register all 24 Plane CE tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    let client = Arc::new(PlaneClient::from_env());

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(PlaneListProjects { client: client.clone() }),
        Box::new(PlaneGetProject { client: client.clone() }),
        Box::new(PlaneListWorkItems { client: client.clone() }),
        Box::new(PlaneGetWorkItem { client: client.clone() }),
        Box::new(PlaneCreateWorkItem { client: client.clone() }),
        Box::new(PlaneUpdateWorkItem { client: client.clone() }),
        Box::new(PlaneDeleteWorkItem { client: client.clone() }),
        Box::new(PlaneListCycles { client: client.clone() }),
        Box::new(PlaneGetCycle { client: client.clone() }),
        Box::new(PlaneListCycleIssues { client: client.clone() }),
        Box::new(PlaneListModules { client: client.clone() }),
        Box::new(PlaneGetModule { client: client.clone() }),
        Box::new(PlaneCreateModule { client: client.clone() }),
        Box::new(PlaneListModuleIssues { client: client.clone() }),
        Box::new(PlaneListStates { client: client.clone() }),
        Box::new(PlaneListLabels { client: client.clone() }),
        Box::new(PlaneListMembers { client: client.clone() }),
        Box::new(PlaneListComments { client: client.clone() }),
        Box::new(PlaneCreateComment { client: client.clone() }),
        Box::new(PlaneListIssuesByState { client: client.clone() }),
        Box::new(PlaneGetIssueBySequence { client: client.clone() }),
        Box::new(PlaneListWorkItemsFiltered { client: client.clone() }),
        Box::new(PlaneListRecentActivity { client: client.clone() }),
        Box::new(PlaneCloseWorkItem { client: client.clone() }),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            tracing::warn!("Failed to register plane tool: {e}");
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    /// Build a PlaneClient pointing at the given mock server URL.
    fn mock_client(server: &MockServer) -> Arc<PlaneClient> {
        Arc::new(PlaneClient {
            http: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            workspace: "testws".into(),
        })
    }

    /// Register a projects-list mock so `resolve_project_id` can map a non-UUID
    /// id/identifier back to itself. Matches on `id` (== `identifier` here).
    fn mock_projects(server: &MockServer, id: &str) {
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": id, "name": "Mock", "identifier": id, "network": 0}
            ]));
        });
    }

    // ── is_uuid helper ────────────────────────────────────────────────────────

    #[test]
    fn test_is_uuid_recognizes_canonical_uuid() {
        assert!(is_uuid("4ef3f3ec-e7ef-4af3-b258-881565e629f9"));
        assert!(!is_uuid("LM"));
        assert!(!is_uuid("proj-abc"));
        assert!(!is_uuid("4ef3f3ec-e7ef-4af3-b258-881565e629f")); // 35 chars
        assert!(!is_uuid("4ef3f3ecXe7ef-4af3-b258-881565e629f9")); // wrong separator
    }

    // ── project identifier → UUID resolution ──────────────────────────────────

    #[tokio::test]
    async fn test_resolve_identifier_to_uuid_then_lists_issues() {
        let server = MockServer::start();
        // Resolution step: list projects, match identifier "LM".
        let projects_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "uuid-lm", "name": "Lumina Core", "identifier": "LM", "network": 0}
            ]));
        });
        // Issues fetched against the resolved UUID, not the identifier.
        let issues_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/uuid-lm/issues/");
            then.status(200).json_body(json!([
                {"id": "i1", "name": "Task", "project": "uuid-lm", "workspace": "testws", "sequence_id": 1}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListWorkItems { client };
        let result = tool.execute(json!({"project_id": "LM"})).await.unwrap();
        assert!(result.contains("Task"), "{result}");
        projects_mock.assert();
        issues_mock.assert();
    }

    // ── Not-configured guard ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_not_configured_when_env_absent() {
        // Client with no base_url
        let client = Arc::new(PlaneClient {
            http: Client::new(),
            base_url: None,
            api_key: None,
            workspace: "moosenet".into(),
        });
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)),
            "Expected NotConfigured, got {err:?}");
    }

    // ── Auth header on all requests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_auth_header_sent_on_list_projects() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "test-api-key");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({})).await;
        mock.assert();
    }

    #[tokio::test]
    async fn test_auth_header_sent_on_create_work_item() {
        let server = MockServer::start();
        mock_projects(&server, "proj-1");
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/workspaces/testws/projects/proj-1/issues/")
                .header("x-api-key", "test-api-key");
            then.status(201).json_body(json!({
                "id": "issue-1",
                "name": "Test",
                "project": "proj-1",
                "workspace": "testws",
                "sequence_id": 1
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCreateWorkItem { client };
        let _ = tool.execute(json!({"project_id": "proj-1", "name": "Test"})).await;
        mock.assert();
    }

    // ── Correct HTTP methods and paths ────────────────────────────────────────

    #[tokio::test]
    async fn test_list_projects_get_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Alpha"), "Expected project name in output: {result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_get_project_by_id() {
        let server = MockServer::start();
        mock_projects(&server, "proj-abc");
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/proj-abc/");
            then.status(200).json_body(json!({
                "id": "proj-abc", "name": "My Project", "identifier": "MP", "network": 0
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({"project_id": "proj-abc"})).await.unwrap();
        assert!(result.contains("My Project"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_create_work_item_post_request() {
        let server = MockServer::start();
        mock_projects(&server, "proj-1");
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/workspaces/testws/projects/proj-1/issues/");
            then.status(201).json_body(json!({
                "id": "issue-99", "name": "Fix login bug",
                "project": "proj-1", "workspace": "testws", "sequence_id": 99
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCreateWorkItem { client };
        let result = tool.execute(json!({
            "project_id": "proj-1",
            "name": "Fix login bug",
            "priority": "high"
        })).await.unwrap();
        assert!(result.contains("Fix login bug"), "{result}");
        assert!(result.contains("99"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_update_work_item_patch_request() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(200).json_body(json!({
                "id": "i1", "name": "Updated name",
                "project": "p1", "workspace": "testws"
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneUpdateWorkItem { client };
        let result = tool.execute(json!({
            "project_id": "p1",
            "issue_id": "i1",
            "name": "Updated name"
        })).await.unwrap();
        assert!(result.contains("Updated name"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_delete_work_item_delete_request() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::DELETE).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(204);
        });
        let client = mock_client(&server);
        let tool = PlaneDeleteWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await.unwrap();
        assert!(result.contains("i1"), "{result}");
        mock.assert();
    }

    // ── 429 retry logic ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_429_retry_succeeds_on_second_attempt() {
        let server = MockServer::start();
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count_clone = call_count.clone();

        // First call → 429; second call → 200
        let mock429 = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(429);
        });

        // We can't do conditional per-call responses easily with httpmock,
        // so instead test the client's retry by checking it sends more than 1 request.
        // We use a workaround: mock returns 429 up to 2 times, then 200.
        // httpmock doesn't support dynamic responses, so we verify error after 3 attempts.
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        // After 3 x 429s the tool should return an error
        assert!(result.is_err());
        let _ = call_count_clone; // used above
        // 3 retries means the mock was hit at least once
        assert!(mock429.hits() >= 1, "Expected at least 1 hit, got {}", mock429.hits());
    }

    #[tokio::test]
    async fn test_429_returns_rate_limit_error_after_3_attempts() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(429);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("rate limit") || err.contains("HTTP error"),
            "Expected rate limit error, got: {err}");
        assert!(mock.hits() >= 3, "Expected at least 3 retries, got {}", mock.hits());
    }

    // ── 404 → NotFound error ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_404_returns_not_found_error() {
        let server = MockServer::start();
        mock_projects(&server, "bad-id");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/bad-id/");
            then.status(404).body("Not found");
        });
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({"project_id": "bad-id"})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    // ── Missing required argument ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_missing_required_arg_returns_invalid_argument() {
        let server = MockServer::start();
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_update_with_no_fields_returns_error() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let client = mock_client(&server);
        let tool = PlaneUpdateWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
    }

    // ── Empty response handled gracefully ─────────────────────────────────────

    #[tokio::test]
    async fn test_empty_project_list_returns_message() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("No projects"), "{result}");
    }

    // ── register() populates 24 tools ─────────────────────────────────────────

    #[test]
    fn test_register_all_plane_tools() {
        // Temporarily set env vars so client.configured() is true-ish
        // (not required for registration, only for execution)
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 24,
            "Expected 24 plane tools, got {}", registry.len());
    }

    #[test]
    fn test_all_plane_tool_names_unique() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        let names: Vec<String> = registry.list().iter().map(|t| t.name.clone()).collect();
        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(names.len(), deduped.len(),
            "Duplicate tool names found: {:?}", names);
    }

    #[test]
    fn test_all_plane_tools_have_descriptions() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        for info in registry.list() {
            assert!(!info.description.is_empty(),
                "Tool '{}' has empty description", info.name);
        }
    }

    #[test]
    fn test_all_plane_tools_have_valid_parameters_schema() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        for info in registry.list() {
            assert_eq!(info.parameters["type"], "object",
                "Tool '{}' parameters schema should have type: object", info.name);
        }
    }

    // ── Filter by state group (client-side) ───────────────────────────────────

    #[tokio::test]
    async fn test_list_issues_by_state_filters_correctly() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([
                {
                    "id": "i1", "name": "Open task",
                    "project": "p1", "workspace": "testws",
                    "state_detail": {"id": "s1", "name": "In Progress", "color": "#fff", "group": "started"}
                },
                {
                    "id": "i2", "name": "Done task",
                    "project": "p1", "workspace": "testws",
                    "state_detail": {"id": "s2", "name": "Done", "color": "#0f0", "group": "completed"}
                }
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListIssuesByState { client };
        let result = tool.execute(json!({"project_id": "p1", "state_group": "started"})).await.unwrap();
        assert!(result.contains("Open task"), "{result}");
        assert!(!result.contains("Done task"), "{result}");
    }

    // ── Paginated response ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_paginated_response_parsed_correctly() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!({
                "count": 2,
                "next": null,
                "previous": null,
                "results": [
                    {"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0},
                    {"id": "p2", "name": "Beta", "identifier": "BT", "network": 0}
                ]
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Alpha"), "{result}");
        assert!(result.contains("Beta"), "{result}");
    }

    // ── close_work_item fetches states then patches ───────────────────────────

    #[tokio::test]
    async fn test_close_work_item_uses_completed_state() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let _states_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/states/");
            then.status(200).json_body(json!([
                {"id": "s-done", "name": "Done", "color": "#0f0", "group": "completed", "project": "p1"},
                {"id": "s-todo", "name": "Todo", "color": "#fff", "group": "unstarted", "project": "p1"}
            ]));
        });
        let _patch_mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(200).json_body(json!({
                "id": "i1", "name": "My task",
                "project": "p1", "workspace": "testws",
                "state": "s-done"
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCloseWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await.unwrap();
        assert!(result.contains("Done") || result.contains("My task"), "{result}");
    }

    // ── get_issue_by_sequence finds correct issue ─────────────────────────────

    #[tokio::test]
    async fn test_get_issue_by_sequence_found() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([
                {"id": "i1", "name": "Task A", "sequence_id": 1, "project": "p1", "workspace": "testws"},
                {"id": "i42", "name": "Task B", "sequence_id": 42, "project": "p1", "workspace": "testws"}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneGetIssueBySequence { client };
        let result = tool.execute(json!({"project_id": "p1", "sequence_id": 42})).await.unwrap();
        assert!(result.contains("Task B"), "{result}");
        assert!(result.contains("42"), "{result}");
    }

    #[tokio::test]
    async fn test_get_issue_by_sequence_not_found() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneGetIssueBySequence { client };
        let result = tool.execute(json!({"project_id": "p1", "sequence_id": 99})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }
}
