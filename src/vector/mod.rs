//! Vector tools — autonomous dev loop management via parameterized SQL (sqlx).
//!
//! All 11 tools interact with a Postgres database identified by
//! `VECTOR_DATABASE_URL`. Project names are strictly validated before any
//! database operation to prevent injection. Shell commands are **never** used.
//!
//! Tools:
//!   vector_submit, vector_status, vector_list, vector_halt, vector_resume,
//!   vector_logs, vector_clear_done, vector_projects, vector_queue_depth,
//!   vector_last_error, vector_history

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Project-name validation
// ---------------------------------------------------------------------------

/// Validate a project name: `^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$`
///
/// Rejects empty names, names starting with `_` or `-`, names longer than 64
/// characters, and names containing any character outside `[a-zA-Z0-9_-]`.
pub fn validate_project_name(name: &str) -> Result<(), ToolError> {
    if name.is_empty() {
        return Err(ToolError::InvalidArgument(
            "project name must not be empty".into(),
        ));
    }
    if name.len() > 64 {
        return Err(ToolError::InvalidArgument(format!(
            "project name too long ({} chars, max 64)",
            name.len()
        )));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_alphanumeric() {
        return Err(ToolError::InvalidArgument(
            "project name must start with a letter or digit".into(),
        ));
    }
    for ch in chars {
        if !matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-') {
            return Err(ToolError::InvalidArgument(format!(
                "project name contains invalid character: {:?}",
                ch
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper — grab VECTOR_DATABASE_URL
// ---------------------------------------------------------------------------

fn vector_db_url() -> Result<String, ToolError> {
    std::env::var("VECTOR_DATABASE_URL").map_err(|_| {
        ToolError::NotConfigured("VECTOR_DATABASE_URL not set".into())
    })
}

// ---------------------------------------------------------------------------
// Tool: vector_submit
// ---------------------------------------------------------------------------

pub struct VectorSubmit;

#[async_trait]
impl RustTool for VectorSubmit {
    fn name(&self) -> &str { "vector_submit" }

    fn description(&self) -> &str {
        "Submit a new dev-loop task for a Vector project. \
         Returns the new task_id."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project":  {"type": "string", "description": "Project name (alphanumeric, hyphens, underscores)"},
                "task":     {"type": "string", "description": "Task description"},
                "priority": {"type": "integer", "description": "Priority 1-10 (default 5)", "default": 5}
            },
            "required": ["project", "task"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = args["project"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'project'".into()))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'task'".into()))?;
        let priority = args["priority"].as_i64().unwrap_or(5) as i32;

        validate_project_name(project)?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let row: (i64,) = sqlx::query_as(
            "INSERT INTO vector_tasks (project, task, priority, status, created_at) \
             VALUES ($1, $2, $3, 'pending', NOW()) RETURNING id",
        )
        .bind(project)
        .bind(task)
        .bind(priority)
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        Ok(format!("Task submitted. task_id={}", row.0))
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_status
// ---------------------------------------------------------------------------

pub struct VectorStatus;

#[async_trait]
impl RustTool for VectorStatus {
    fn name(&self) -> &str { "vector_status" }

    fn description(&self) -> &str {
        "Return the current status of a Vector task by task_id."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "integer", "description": "Task ID returned by vector_submit"}
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task_id = args["task_id"]
            .as_i64()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'task_id'".into()))?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let row: Option<(String, String, i32)> = sqlx::query_as(
            "SELECT project, status, priority FROM vector_tasks WHERE id = $1",
        )
        .bind(task_id)
        .fetch_optional(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        match row {
            Some((project, status, priority)) => Ok(format!(
                "task_id={task_id} project={project} status={status} priority={priority}"
            )),
            None => Err(ToolError::NotFound(format!("task_id={task_id} not found"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_list
// ---------------------------------------------------------------------------

pub struct VectorList;

#[async_trait]
impl RustTool for VectorList {
    fn name(&self) -> &str { "vector_list" }

    fn description(&self) -> &str {
        "List all tasks for a Vector project, ordered by priority then creation time."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Project name"}
            },
            "required": ["project"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = args["project"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'project'".into()))?;
        validate_project_name(project)?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let rows: Vec<(i64, String, i32, String)> = sqlx::query_as(
            "SELECT id, task, priority, status FROM vector_tasks \
             WHERE project = $1 ORDER BY priority DESC, created_at ASC LIMIT 50",
        )
        .bind(project)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        if rows.is_empty() {
            return Ok(format!("No tasks found for project '{project}'"));
        }

        let lines: Vec<String> = rows
            .into_iter()
            .map(|(id, task, prio, status)| {
                format!("  [{id}] ({status}, p{prio}) {task}")
            })
            .collect();

        Ok(format!("Tasks for '{project}':\n{}", lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_halt
// ---------------------------------------------------------------------------

pub struct VectorHalt;

#[async_trait]
impl RustTool for VectorHalt {
    fn name(&self) -> &str { "vector_halt" }

    fn description(&self) -> &str {
        "Halt (pause) a running Vector task by task_id."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "integer", "description": "Task ID to halt"}
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task_id = args["task_id"]
            .as_i64()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'task_id'".into()))?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let rows_affected = sqlx::query(
            "UPDATE vector_tasks SET status = 'halted', updated_at = NOW() \
             WHERE id = $1 AND status NOT IN ('done', 'error')",
        )
        .bind(task_id)
        .execute(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?
        .rows_affected();

        if rows_affected == 0 {
            Err(ToolError::NotFound(format!(
                "task_id={task_id} not found or already terminal"
            )))
        } else {
            Ok(format!("task_id={task_id} halted"))
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_resume
// ---------------------------------------------------------------------------

pub struct VectorResume;

#[async_trait]
impl RustTool for VectorResume {
    fn name(&self) -> &str { "vector_resume" }

    fn description(&self) -> &str {
        "Resume a halted Vector task, setting its status back to running."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "integer", "description": "Task ID to resume"}
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task_id = args["task_id"]
            .as_i64()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'task_id'".into()))?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let rows_affected = sqlx::query(
            "UPDATE vector_tasks SET status = 'running', updated_at = NOW() \
             WHERE id = $1 AND status = 'halted'",
        )
        .bind(task_id)
        .execute(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?
        .rows_affected();

        if rows_affected == 0 {
            Err(ToolError::NotFound(format!(
                "task_id={task_id} not found or not in halted state"
            )))
        } else {
            Ok(format!("task_id={task_id} resumed (status=running)"))
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_logs
// ---------------------------------------------------------------------------

pub struct VectorLogs;

#[async_trait]
impl RustTool for VectorLogs {
    fn name(&self) -> &str { "vector_logs" }

    fn description(&self) -> &str {
        "Retrieve the most recent log entries for a Vector task."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "integer", "description": "Task ID"},
                "lines":   {"type": "integer", "description": "Number of log lines to return (1-200, default 50)", "default": 50}
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task_id = args["task_id"]
            .as_i64()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'task_id'".into()))?;

        let lines = args["lines"].as_i64().unwrap_or(50).clamp(1, 200) as i64;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT logged_at::text, message FROM vector_logs \
             WHERE task_id = $1 ORDER BY logged_at DESC LIMIT $2",
        )
        .bind(task_id)
        .bind(lines)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        if rows.is_empty() {
            return Ok(format!("No log entries for task_id={task_id}"));
        }

        // Return in chronological order (reversed from the DESC query)
        let lines: Vec<String> = rows
            .into_iter()
            .rev()
            .map(|(ts, msg)| format!("[{ts}] {msg}"))
            .collect();

        Ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_clear_done
// ---------------------------------------------------------------------------

pub struct VectorClearDone;

#[async_trait]
impl RustTool for VectorClearDone {
    fn name(&self) -> &str { "vector_clear_done" }

    fn description(&self) -> &str {
        "Delete all completed tasks for a Vector project."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Project name"}
            },
            "required": ["project"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = args["project"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'project'".into()))?;
        validate_project_name(project)?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let rows_affected = sqlx::query(
            "DELETE FROM vector_tasks WHERE project = $1 AND status = 'done'",
        )
        .bind(project)
        .execute(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?
        .rows_affected();

        Ok(format!(
            "Cleared {rows_affected} completed task(s) from project '{project}'"
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_projects
// ---------------------------------------------------------------------------

pub struct VectorProjects;

#[async_trait]
impl RustTool for VectorProjects {
    fn name(&self) -> &str { "vector_projects" }

    fn description(&self) -> &str {
        "List all distinct projects that have Vector tasks."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT project FROM vector_tasks ORDER BY project ASC",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        if rows.is_empty() {
            return Ok("No projects found".into());
        }

        let names: Vec<String> = rows.into_iter().map(|(p,)| p).collect();
        Ok(format!("Projects:\n{}", names.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_queue_depth
// ---------------------------------------------------------------------------

pub struct VectorQueueDepth;

#[async_trait]
impl RustTool for VectorQueueDepth {
    fn name(&self) -> &str { "vector_queue_depth" }

    fn description(&self) -> &str {
        "Return the count of pending tasks in a Vector project queue."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Project name"}
            },
            "required": ["project"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = args["project"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'project'".into()))?;
        validate_project_name(project)?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM vector_tasks WHERE project = $1 AND status = 'pending'",
        )
        .bind(project)
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        Ok(format!("project='{project}' pending_tasks={count}"))
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_last_error
// ---------------------------------------------------------------------------

pub struct VectorLastError;

#[async_trait]
impl RustTool for VectorLastError {
    fn name(&self) -> &str { "vector_last_error" }

    fn description(&self) -> &str {
        "Return the most recent error task for a Vector project."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Project name"}
            },
            "required": ["project"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = args["project"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'project'".into()))?;
        validate_project_name(project)?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT id, task, updated_at::text FROM vector_tasks \
             WHERE project = $1 AND status = 'error' \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(project)
        .fetch_optional(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        match row {
            Some((id, task, updated_at)) => Ok(format!(
                "Last error in '{project}': task_id={id} at={updated_at}\n  {task}"
            )),
            None => Ok(format!("No error tasks found for project '{project}'")),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: vector_history
// ---------------------------------------------------------------------------

pub struct VectorHistory;

#[async_trait]
impl RustTool for VectorHistory {
    fn name(&self) -> &str { "vector_history" }

    fn description(&self) -> &str {
        "Return task history for a Vector project over the last N days."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Project name"},
                "days":    {"type": "integer", "description": "Number of days to look back (1-90, default 7)", "default": 7}
            },
            "required": ["project"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = args["project"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'project'".into()))?;
        let days = args["days"].as_i64().unwrap_or(7).clamp(1, 90) as i64;
        validate_project_name(project)?;

        let db_url = vector_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT id, task, status, created_at::text FROM vector_tasks \
             WHERE project = $1 AND created_at >= NOW() - ($2::bigint * INTERVAL '1 day') \
             ORDER BY created_at DESC LIMIT 100",
        )
        .bind(project)
        .bind(days)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        if rows.is_empty() {
            return Ok(format!(
                "No tasks in the last {days} day(s) for project '{project}'"
            ));
        }

        let lines: Vec<String> = rows
            .into_iter()
            .map(|(id, task, status, created_at)| {
                format!("  [{id}] {created_at} ({status}) {task}")
            })
            .collect();

        Ok(format!(
            "History for '{project}' (last {days} days):\n{}",
            lines.join("\n")
        ))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all 11 Vector tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(VectorSubmit),
        Box::new(VectorStatus),
        Box::new(VectorList),
        Box::new(VectorHalt),
        Box::new(VectorResume),
        Box::new(VectorLogs),
        Box::new(VectorClearDone),
        Box::new(VectorProjects),
        Box::new(VectorQueueDepth),
        Box::new(VectorLastError),
        Box::new(VectorHistory),
    ];
    for tool in tools {
        registry.register_or_replace(tool);
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // --- project name validation -------------------------------------------

    #[test]
    fn test_valid_project_name_simple() {
        assert!(validate_project_name("myproject").is_ok());
    }

    #[test]
    fn test_valid_project_name_with_hyphen() {
        assert!(validate_project_name("my-project").is_ok());
    }

    #[test]
    fn test_valid_project_name_with_underscore() {
        assert!(validate_project_name("my_project").is_ok());
    }

    #[test]
    fn test_valid_project_name_alphanumeric_mix() {
        assert!(validate_project_name("proj123-abc_def").is_ok());
    }

    #[test]
    fn test_valid_project_name_single_char() {
        assert!(validate_project_name("a").is_ok());
    }

    #[test]
    fn test_invalid_project_name_empty() {
        let err = validate_project_name("").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_invalid_project_name_starts_with_hyphen() {
        let err = validate_project_name("-bad").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_invalid_project_name_starts_with_underscore() {
        let err = validate_project_name("_bad").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_invalid_project_name_too_long() {
        let long = "a".repeat(65);
        let err = validate_project_name(&long).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_valid_project_name_max_length() {
        let max = format!("a{}", "b".repeat(63));
        assert_eq!(max.len(), 64);
        assert!(validate_project_name(&max).is_ok());
    }

    #[test]
    fn test_invalid_project_name_shell_metachar_semicolon() {
        let err = validate_project_name("proj;rm -rf /").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_invalid_project_name_shell_metachar_pipe() {
        let err = validate_project_name("proj|cat /etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_invalid_project_name_space() {
        let err = validate_project_name("proj name").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_invalid_project_name_dollar_sign() {
        let err = validate_project_name("$HOME").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // --- NotConfigured when DB URL is missing --------------------------------

    #[tokio::test]
    #[serial]
    async fn test_vector_submit_not_configured_without_env() {
        // Ensure the env var is not set in this test process
        // (It won't be in CI — if it happens to be set, skip gracefully)
        if std::env::var("VECTOR_DATABASE_URL").is_ok() {
            return; // skip — real DB available, not testing NotConfigured
        }
        let tool = VectorSubmit;
        let result = tool
            .execute(json!({"project": "alpha", "task": "build", "priority": 5}))
            .await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_vector_status_not_configured_without_env() {
        if std::env::var("VECTOR_DATABASE_URL").is_ok() {
            return;
        }
        let tool = VectorStatus;
        let result = tool.execute(json!({"task_id": 1})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_vector_list_not_configured_without_env() {
        if std::env::var("VECTOR_DATABASE_URL").is_ok() {
            return;
        }
        let tool = VectorList;
        let result = tool.execute(json!({"project": "alpha"})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    // --- Input validation rejects bad project names --------------------------

    #[tokio::test]
    async fn test_vector_submit_rejects_invalid_project() {
        let tool = VectorSubmit;
        let result = tool
            .execute(json!({"project": "bad;project", "task": "x"}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn test_vector_list_rejects_invalid_project() {
        let tool = VectorList;
        let result = tool.execute(json!({"project": "-invalid"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn test_vector_queue_depth_rejects_invalid_project() {
        let tool = VectorQueueDepth;
        let result = tool.execute(json!({"project": "has spaces"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    // --- Registration --------------------------------------------------------

    #[test]
    fn test_vector_registers_11_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 11);
    }

    #[test]
    fn test_vector_tool_names_present() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        let expected = [
            "vector_submit", "vector_status", "vector_list", "vector_halt",
            "vector_resume", "vector_logs", "vector_clear_done", "vector_projects",
            "vector_queue_depth", "vector_last_error", "vector_history",
        ];
        for name in &expected {
            assert!(registry.contains(name), "missing tool: {name}");
        }
    }

    // --- Parameter schema completeness -------------------------------------

    #[test]
    fn test_vector_logs_parameters_include_lines() {
        let tool = VectorLogs;
        let params = tool.parameters();
        assert!(params["properties"]["lines"].is_object());
    }

    #[test]
    fn test_vector_history_parameters_include_days() {
        let tool = VectorHistory;
        let params = tool.parameters();
        assert!(params["properties"]["days"].is_object());
    }
}
