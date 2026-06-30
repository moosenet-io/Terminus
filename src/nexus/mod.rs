//! Nexus tools: agent inter-inbox messaging backed by Postgres.
//!
//! Provides five RustTool implementations:
//! - `nexus_send`    — insert a message into the inbox
//! - `nexus_check`   — count pending messages for a user
//! - `nexus_read`    — fetch latest N pending messages for a user
//! - `nexus_ack`     — mark a specific message as read (access-controlled by user_id)
//! - `nexus_history` — fetch recent messages (all statuses) for a user
//!
//! All queries use sqlx parameterized binding — zero SQL string interpolation.
//! DATABASE_URL must be set in the environment (postgres://user:pass@host/dbname).
//! If unset, every tool returns [`ToolError::NotConfigured`].
//!
//! Real table schema (inbox_messages):
//!   id uuid, from_agent varchar(32), to_agent varchar(32),
//!   message_type varchar(32), priority varchar(16) default 'normal',
//!   payload jsonb, status varchar(16) default 'pending',
//!   created_at timestamptz, read_at timestamptz nullable

use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Pool helper
// ---------------------------------------------------------------------------

async fn get_pool() -> Result<PgPool, ToolError> {
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        ToolError::NotConfigured(
            "DATABASE_URL not set — Nexus tools require a Postgres connection".into(),
        )
    })?;
    PgPool::connect(&url)
        .await
        .map_err(|e| ToolError::Database(format!("Cannot connect to database: {e}")))
}

// ---------------------------------------------------------------------------
// nexus_send
// ---------------------------------------------------------------------------

pub struct NexusSend;

#[async_trait]
impl RustTool for NexusSend {
    fn name(&self) -> &str { "nexus_send" }

    fn description(&self) -> &str {
        "Send a message from one agent to another via the Nexus inbox"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from":         { "type": "string", "description": "Sender agent ID" },
                "to":           { "type": "string", "description": "Recipient agent ID" },
                "body":         { "type": "string", "description": "Message body text" },
                "message_type": { "type": "string", "description": "Message type (default: message)" },
                "priority":     { "type": "string", "description": "Priority: low, normal, high, urgent, critical (default: normal)" }
            },
            "required": ["from", "to", "body"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let from = args["from"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'from' must be a string".into()))?;
        let to = args["to"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'to' must be a string".into()))?;
        let body = args["body"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'body' must be a string".into()))?;
        let msg_type = args["message_type"].as_str().unwrap_or("message");
        let priority = args["priority"].as_str().unwrap_or("normal");

        // Wrap body in JSONB payload
        let payload = serde_json::to_string(&json!({"body": body}))
            .map_err(|e| ToolError::Database(format!("Payload serialization failed: {e}")))?;

        let pool = get_pool().await?;

        let row: (String,) = sqlx::query_as(
            "INSERT INTO inbox_messages (from_agent, to_agent, message_type, priority, payload) \
             VALUES ($1, $2, $3, $4, $5::jsonb) \
             RETURNING id::text",
        )
        .bind(from)
        .bind(to)
        .bind(msg_type)
        .bind(priority)
        .bind(payload)
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to send message: {e}")))?;

        Ok(format!("Message sent (id={})", row.0))
    }
}

// ---------------------------------------------------------------------------
// nexus_check
// ---------------------------------------------------------------------------

pub struct NexusCheck;

#[async_trait]
impl RustTool for NexusCheck {
    fn name(&self) -> &str { "nexus_check" }

    fn description(&self) -> &str {
        "Count pending messages in the Nexus inbox for a given agent/user"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id": { "type": "string", "description": "Agent or user ID to check inbox for" }
            },
            "required": ["user_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let user_id = args["user_id"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'user_id' must be a string".into()))?;

        let pool = get_pool().await?;

        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM inbox_messages WHERE to_agent = $1 AND status = 'pending'")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .map_err(|e| ToolError::Database(format!("Failed to check inbox: {e}")))?;

        let count = row.0;
        if count == 0 {
            Ok("No pending messages".into())
        } else {
            Ok(format!("{count} pending message(s) in inbox"))
        }
    }
}

// ---------------------------------------------------------------------------
// nexus_read
// ---------------------------------------------------------------------------

pub struct NexusRead;

#[async_trait]
impl RustTool for NexusRead {
    fn name(&self) -> &str { "nexus_read" }

    fn description(&self) -> &str {
        "Fetch the latest pending messages from the Nexus inbox for a given agent/user"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id": { "type": "string", "description": "Agent or user ID" },
                "limit":   { "type": "integer", "description": "Max messages to return (default 20)", "default": 20 }
            },
            "required": ["user_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let user_id = args["user_id"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'user_id' must be a string".into()))?;
        let limit: i64 = args["limit"].as_i64().unwrap_or(20).clamp(1, 100);

        let pool = get_pool().await?;

        let rows: Vec<(String, String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT id::text, from_agent, message_type, payload::text, created_at \
             FROM inbox_messages \
             WHERE to_agent = $1 AND status = 'pending' \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to read inbox: {e}")))?;

        if rows.is_empty() {
            return Ok("No pending messages".into());
        }

        let mut out = format!("{} pending message(s):\n\n", rows.len());
        for (id, from, msg_type, payload_str, ts) in &rows {
            let body = serde_json::from_str::<Value>(payload_str)
                .ok()
                .and_then(|v| v["body"].as_str().map(str::to_string))
                .unwrap_or_else(|| payload_str.clone());
            out.push_str(&format!(
                "[id={id}] {ts} from={from} type={msg_type}\n{body}\n---\n"
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// nexus_ack
// ---------------------------------------------------------------------------

pub struct NexusAck;

#[async_trait]
impl RustTool for NexusAck {
    fn name(&self) -> &str { "nexus_ack" }

    fn description(&self) -> &str {
        "Mark a Nexus inbox message as read. Only the recipient can acknowledge their own messages."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id":    { "type": "string", "description": "Recipient agent/user ID (enforced)" },
                "message_id": { "type": "string", "description": "UUID of the message to acknowledge" }
            },
            "required": ["user_id", "message_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let user_id = args["user_id"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'user_id' must be a string".into()))?;
        let message_id = args["message_id"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'message_id' must be a UUID string".into()))?;

        let pool = get_pool().await?;

        let result = sqlx::query(
            "UPDATE inbox_messages \
             SET status = 'read', read_at = NOW() \
             WHERE id = $1::uuid AND to_agent = $2 AND status = 'pending'",
        )
        .bind(message_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to acknowledge message: {e}")))?;

        if result.rows_affected() == 0 {
            Err(ToolError::NotFound(format!(
                "Message id={message_id} not found for user '{user_id}', or already acknowledged"
            )))
        } else {
            Ok(format!("Message id={message_id} acknowledged"))
        }
    }
}

// ---------------------------------------------------------------------------
// nexus_history
// ---------------------------------------------------------------------------

pub struct NexusHistory;

#[async_trait]
impl RustTool for NexusHistory {
    fn name(&self) -> &str { "nexus_history" }

    fn description(&self) -> &str {
        "Fetch recent Nexus inbox messages (all statuses) for a given agent/user"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id": { "type": "string", "description": "Agent or user ID" },
                "limit":   { "type": "integer", "description": "Max messages to return (default 20)", "default": 20 }
            },
            "required": ["user_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let user_id = args["user_id"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'user_id' must be a string".into()))?;
        let limit: i64 = args["limit"].as_i64().unwrap_or(20).clamp(1, 100);

        let pool = get_pool().await?;

        let rows: Vec<(String, String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT id::text, from_agent, message_type, status, created_at \
             FROM inbox_messages \
             WHERE to_agent = $1 \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to fetch history: {e}")))?;

        if rows.is_empty() {
            return Ok("No messages in history".into());
        }

        let mut out = format!("Last {} message(s):\n\n", rows.len());
        for (id, from, msg_type, status, ts) in &rows {
            out.push_str(&format!(
                "[id={id}] [{status}] {ts} from={from} type={msg_type}\n"
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(NexusSend));
    registry.register_or_replace(Box::new(NexusCheck));
    registry.register_or_replace(Box::new(NexusRead));
    registry.register_or_replace(Box::new(NexusAck));
    registry.register_or_replace(Box::new(NexusHistory));
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_tool_meta(tool: &dyn RustTool) {
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
    }

    #[test]
    fn test_nexus_send_metadata() {
        let tool = NexusSend;
        assert_eq!(tool.name(), "nexus_send");
        assert_tool_meta(&tool);
        let params = tool.parameters();
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "from"));
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "to"));
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "body"));
    }

    #[test]
    fn test_nexus_check_metadata() {
        let tool = NexusCheck;
        assert_eq!(tool.name(), "nexus_check");
        assert_tool_meta(&tool);
    }

    #[test]
    fn test_nexus_read_metadata() {
        let tool = NexusRead;
        assert_eq!(tool.name(), "nexus_read");
        assert_tool_meta(&tool);
    }

    #[test]
    fn test_nexus_ack_metadata() {
        let tool = NexusAck;
        assert_eq!(tool.name(), "nexus_ack");
        assert_tool_meta(&tool);
        let params = tool.parameters();
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "user_id"));
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "message_id"));
    }

    #[test]
    fn test_nexus_history_metadata() {
        let tool = NexusHistory;
        assert_eq!(tool.name(), "nexus_history");
        assert_tool_meta(&tool);
    }

    #[tokio::test]
    async fn test_nexus_send_missing_database_url() {
        let tool = NexusSend;
        let result = tool.execute(json!({"from":"lumina","to":"axon","body":"hello"})).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("DATABASE_URL") || err.contains("database") || err.contains("connect"),
            "unexpected: {err}");
    }

    #[tokio::test]
    async fn test_nexus_check_missing_database_url() {
        let tool = NexusCheck;
        let result = tool.execute(json!({"user_id":"lumina"})).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_nexus_ack_message_id_is_string() {
        // message_id should be UUID string, not integer
        let params = NexusAck.parameters();
        assert_eq!(params["properties"]["message_id"]["type"], "string");
    }

    #[test]
    fn test_nexus_registration() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("nexus_send"));
        assert!(registry.contains("nexus_check"));
        assert!(registry.contains("nexus_read"));
        assert!(registry.contains("nexus_ack"));
        assert!(registry.contains("nexus_history"));
        assert_eq!(registry.len(), 5);
    }
}
