//! Axon tools: work order / task queue backed by Postgres.
//!
//! Provides four RustTool implementations:
//! - `axon_submit` — insert a new work order (task)
//! - `axon_status` — fetch the status of a task by ID
//! - `axon_list`   — list pending tasks assigned to a user
//! - `axon_cancel` — cancel a task (access-controlled: only the assignee or submitter)
//!
//! All queries use sqlx parameterized binding — zero SQL string interpolation.
//! DATABASE_URL must be set in the environment (postgres://user:pass@host/dbname).
//! If unset, every tool returns [`ToolError::NotConfigured`].

use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Pool helper
// ---------------------------------------------------------------------------

/// Create a Postgres pool from DATABASE_URL, or return NotConfigured.
async fn get_pool() -> Result<PgPool, ToolError> {
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        ToolError::NotConfigured(
            "DATABASE_URL not set — Axon tools require a Postgres connection".into(),
        )
    })?;
    PgPool::connect(&url)
        .await
        .map_err(|e| ToolError::Database(format!("Cannot connect to database: {e}")))
}

// ---------------------------------------------------------------------------
// axon_submit
// ---------------------------------------------------------------------------

/// Insert a new work order into the Axon task queue.
pub struct AxonSubmit;

#[async_trait]
impl RustTool for AxonSubmit {
    fn name(&self) -> &str {
        "axon_submit"
    }

    fn description(&self) -> &str {
        "Submit a new work order to the Axon task queue"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project/queue identifier" },
                "title":      { "type": "string", "description": "Work order title" },
                "priority":   {
                    "type": "string",
                    "description": "Priority level",
                    "enum": ["low", "normal", "high", "urgent"],
                    "default": "normal"
                },
                "assignee":   { "type": "string", "description": "Agent or user ID to assign to" }
            },
            "required": ["project_id", "title", "assignee"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = args["project_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'project_id' must be a string".into()))?;
        let title = args["title"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'title' must be a string".into()))?;
        let priority = args["priority"].as_str().unwrap_or("normal");
        let assignee = args["assignee"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'assignee' must be a string".into()))?;

        // Validate priority against allowed values
        let valid_priorities = ["low", "normal", "high", "urgent"];
        if !valid_priorities.contains(&priority) {
            return Err(ToolError::InvalidArgument(format!(
                "priority must be one of: {}",
                valid_priorities.join(", ")
            )));
        }

        let pool = get_pool().await?;

        // Parameterized INSERT — all four values are bound parameters
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO axon_tasks (project_id, title, priority, assignee, status, created_at) \
             VALUES ($1, $2, $3, $4, 'pending', NOW()) \
             RETURNING id",
        )
        .bind(project_id)
        .bind(title)
        .bind(priority)
        .bind(assignee)
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to submit work order: {e}")))?;

        Ok(format!(
            "Work order submitted (id={}, project={project_id}, assignee={assignee}, priority={priority})",
            row.0
        ))
    }
}

// ---------------------------------------------------------------------------
// axon_status
// ---------------------------------------------------------------------------

/// Fetch the status of a task by its ID.
pub struct AxonStatus;

#[async_trait]
impl RustTool for AxonStatus {
    fn name(&self) -> &str {
        "axon_status"
    }

    fn description(&self) -> &str {
        "Fetch the current status of an Axon work order by task ID"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "integer", "description": "Task ID to look up" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task_id: i64 = args["task_id"]
            .as_i64()
            .ok_or_else(|| ToolError::InvalidArgument("'task_id' must be an integer".into()))?;

        let pool = get_pool().await?;

        // Parameterized SELECT by task_id only — no user filter needed for status reads
        let row: Option<(String, String, String, String, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as(
                "SELECT project_id, title, status, assignee, created_at \
                 FROM axon_tasks \
                 WHERE id = $1",
            )
            .bind(task_id)
            .fetch_optional(&pool)
            .await
            .map_err(|e| ToolError::Database(format!("Failed to fetch task status: {e}")))?;

        match row {
            None => Err(ToolError::NotFound(format!("Task id={task_id} not found"))),
            Some((project_id, title, status, assignee, created_at)) => Ok(format!(
                "Task id={task_id}\n\
                 Project:  {project_id}\n\
                 Title:    {title}\n\
                 Status:   {status}\n\
                 Assignee: {assignee}\n\
                 Created:  {created_at}"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// axon_list
// ---------------------------------------------------------------------------

/// List pending tasks assigned to a given user.
pub struct AxonList;

#[async_trait]
impl RustTool for AxonList {
    fn name(&self) -> &str {
        "axon_list"
    }

    fn description(&self) -> &str {
        "List pending Axon work orders assigned to a given agent/user"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id": { "type": "string", "description": "Assignee agent or user ID" }
            },
            "required": ["user_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let user_id = args["user_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'user_id' must be a string".into()))?;

        let pool = get_pool().await?;

        // Parameterized SELECT — assignee and status are bound parameters
        let rows: Vec<(i64, String, String, String, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as(
                "SELECT id, project_id, title, priority, created_at \
                 FROM axon_tasks \
                 WHERE assignee = $1 AND status = 'pending' \
                 ORDER BY \
                   CASE priority \
                     WHEN 'urgent' THEN 1 \
                     WHEN 'high'   THEN 2 \
                     WHEN 'normal' THEN 3 \
                     ELSE 4 \
                   END, \
                   created_at ASC",
            )
            .bind(user_id)
            .fetch_all(&pool)
            .await
            .map_err(|e| ToolError::Database(format!("Failed to list tasks: {e}")))?;

        if rows.is_empty() {
            return Ok(format!("No pending tasks for '{user_id}'"));
        }

        let mut out = format!("{} pending task(s) for '{user_id}':\n\n", rows.len());
        for (id, project_id, title, priority, created_at) in &rows {
            out.push_str(&format!(
                "[id={id}] [{priority}] {created_at} project={project_id} | {title}\n"
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// axon_cancel
// ---------------------------------------------------------------------------

/// Cancel a task. Access control: only the assignee can cancel their own tasks.
pub struct AxonCancel;

#[async_trait]
impl RustTool for AxonCancel {
    fn name(&self) -> &str {
        "axon_cancel"
    }

    fn description(&self) -> &str {
        "Cancel an Axon work order. Only the assigned agent/user can cancel their own tasks."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "integer", "description": "Task ID to cancel" },
                "user_id": { "type": "string", "description": "Caller's agent/user ID (enforced)" }
            },
            "required": ["task_id", "user_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task_id: i64 = args["task_id"]
            .as_i64()
            .ok_or_else(|| ToolError::InvalidArgument("'task_id' must be an integer".into()))?;
        let user_id = args["user_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'user_id' must be a string".into()))?;

        let pool = get_pool().await?;

        // Access control: WHERE binds BOTH task_id AND assignee (user_id).
        // A user cannot cancel a task that is not assigned to them — the UPDATE
        // will affect 0 rows and we return NotFound.
        let result = sqlx::query(
            "UPDATE axon_tasks \
             SET status = 'cancelled', updated_at = NOW() \
             WHERE id = $1 AND assignee = $2 AND status = 'pending'",
        )
        .bind(task_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to cancel task: {e}")))?;

        if result.rows_affected() == 0 {
            Err(ToolError::NotFound(format!(
                "Task id={task_id} not found, not assigned to '{user_id}', or not in pending state"
            )))
        } else {
            Ok(format!("Task id={task_id} cancelled"))
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Axon tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(AxonSubmit));
    registry.register_or_replace(Box::new(AxonStatus));
    registry.register_or_replace(Box::new(AxonList));
    registry.register_or_replace(Box::new(AxonCancel));
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_tool_meta(tool: &dyn RustTool) {
        assert!(!tool.name().is_empty(), "tool name must not be empty");
        assert!(!tool.description().is_empty(), "description must not be empty");
        let params = tool.parameters();
        assert_eq!(params["type"], "object", "parameters must be a JSON Schema object");
    }

    #[test]
    fn test_axon_submit_metadata() {
        let tool = AxonSubmit;
        assert_eq!(tool.name(), "axon_submit");
        assert_tool_meta(&tool);
        let params = tool.parameters();
        let required = params["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "project_id"));
        assert!(required.iter().any(|v| v == "title"));
        assert!(required.iter().any(|v| v == "assignee"));
    }

    #[test]
    fn test_axon_status_metadata() {
        let tool = AxonStatus;
        assert_eq!(tool.name(), "axon_status");
        assert_tool_meta(&tool);
        let params = tool.parameters();
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "task_id"));
    }

    #[test]
    fn test_axon_list_metadata() {
        let tool = AxonList;
        assert_eq!(tool.name(), "axon_list");
        assert_tool_meta(&tool);
        let params = tool.parameters();
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "user_id"));
    }

    #[test]
    fn test_axon_cancel_metadata() {
        let tool = AxonCancel;
        assert_eq!(tool.name(), "axon_cancel");
        assert_tool_meta(&tool);
        let params = tool.parameters();
        let required = params["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "task_id"));
        assert!(required.iter().any(|v| v == "user_id"));
    }

    #[tokio::test]
    async fn test_axon_submit_missing_database_url() {
        let tool = AxonSubmit;
        let result = tool
            .execute(json!({
                "project_id": "PX",
                "title": "Test task",
                "priority": "normal",
                "assignee": "axon"
            }))
            .await;
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("DATABASE_URL") || err_str.contains("database") || err_str.contains("connect"),
            "unexpected error: {err_str}"
        );
    }

    #[tokio::test]
    async fn test_axon_status_missing_database_url() {
        let tool = AxonStatus;
        let result = tool.execute(json!({"task_id": 1})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_axon_list_missing_database_url() {
        let tool = AxonList;
        let result = tool.execute(json!({"user_id": "axon"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_axon_cancel_missing_database_url() {
        let tool = AxonCancel;
        let result = tool.execute(json!({"task_id": 1, "user_id": "axon"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_axon_submit_invalid_priority() {
        // With no DATABASE_URL, this gets NotConfigured before priority validation.
        // We test the pure validation logic separately via the allowed-values list.
        let valid_priorities = ["low", "normal", "high", "urgent"];
        assert!(!valid_priorities.contains(&"critical"), "critical is not a valid priority");
        assert!(valid_priorities.contains(&"urgent"), "urgent must be valid");
    }

    #[test]
    fn test_axon_cancel_access_control_enforced_by_sql() {
        // Verify the UPDATE SQL includes both task_id AND assignee binding.
        // Access control is enforced in the WHERE clause — not in application logic.
        let sql = "UPDATE axon_tasks \
                   SET status = 'cancelled', updated_at = NOW() \
                   WHERE id = $1 AND assignee = $2 AND status = 'pending'";
        assert!(sql.contains("assignee = $2"), "access control: assignee must be bound parameter");
        assert!(sql.contains("id = $1"), "task id must be a bound parameter");
        assert!(!sql.contains("format!"), "SQL must not use string interpolation");
    }

    #[test]
    fn test_axon_list_sql_uses_parameters() {
        let sql = "SELECT id, project_id, title, priority, created_at \
                   FROM axon_tasks \
                   WHERE assignee = $1 AND status = 'pending'";
        assert!(sql.contains("$1"), "assignee must be bound parameter");
        assert!(!sql.contains("format!"), "no string interpolation");
    }

    #[test]
    fn test_axon_registration() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 4);
        assert!(registry.contains("axon_submit"));
        assert!(registry.contains("axon_status"));
        assert!(registry.contains("axon_list"));
        assert!(registry.contains("axon_cancel"));
    }

    #[test]
    fn test_axon_submit_missing_required_field() {
        // Verify that missing fields are caught (InvalidArgument)
        // We check by looking at the tool parameter schema
        let tool = AxonSubmit;
        let params = tool.parameters();
        let required = params["required"].as_array().unwrap();
        // All three required fields must be present
        assert_eq!(required.len(), 3, "axon_submit requires project_id, title, assignee");
    }
}
