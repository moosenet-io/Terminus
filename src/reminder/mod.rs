//! Reminder tools: one-shot scheduled alerts backed by Postgres.
//!
//! Four `RustTool` implementations:
//! - `reminder_set`    — parse a natural-language time → UTC, INSERT, confirm
//! - `reminder_list`   — list pending (not fired, not cancelled) reminders
//! - `reminder_cancel` — mark a reminder cancelled
//! - `reminder_poll`   — (internal) atomically claim due reminders and mark them
//!                       fired, returning `[{id, message}]` JSON for the scheduler
//!
//! DB ownership lives entirely here (terminus-rs has sqlx + chrono). lumina-core
//! holds the Matrix connection and calls `reminder_poll` over the chord proxy;
//! it never touches the database.
//!
//! Table (created out-of-band, see deploy notes):
//! ```sql
//! CREATE TABLE IF NOT EXISTS reminders (
//!   id TEXT PRIMARY KEY, user_id TEXT, message TEXT NOT NULL,
//!   fire_at TIMESTAMPTZ NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
//!   fired BOOLEAN NOT NULL DEFAULT false, cancelled BOOLEAN NOT NULL DEFAULT false);
//! ```
//!
//! `reminder_poll` uses a single transaction (SELECT ... FOR UPDATE SKIP LOCKED,
//! then UPDATE ... fired=true) to claim and mark due reminders atomically.
//! Crash window: a reminder is marked fired inside the same transaction that
//! returns it, so a crash AFTER the transaction commits but BEFORE the scheduler
//! delivers the Matrix message would drop that one reminder. This is the
//! documented "mark-and-return" tradeoff and is acceptable for a best-effort
//! one-shot alert.

mod parse;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub use parse::{parse_time, ParseError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn get_pool() -> Result<PgPool, ToolError> {
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        ToolError::NotConfigured(
            "DATABASE_URL not set — reminder tools require a Postgres connection".into(),
        )
    })?;
    PgPool::connect(&url)
        .await
        .map_err(|e| ToolError::Database(format!("Cannot connect to database: {e}")))
}

/// Resolve the effective timezone: explicit arg → `LUMINA_TIMEZONE` env →
/// "America/Los_Angeles" default.
fn resolve_timezone(explicit: Option<&str>) -> Result<Tz, ToolError> {
    let name = explicit
        .map(str::to_string)
        .or_else(|| std::env::var("LUMINA_TIMEZONE").ok())
        .unwrap_or_else(|| "America/Los_Angeles".to_string());
    name.parse::<Tz>()
        .map_err(|_| ToolError::InvalidArgument(format!("unknown timezone '{name}'")))
}

// ---------------------------------------------------------------------------
// reminder_set
// ---------------------------------------------------------------------------

pub struct ReminderSet;

#[async_trait]
impl RustTool for ReminderSet {
    fn name(&self) -> &str { "reminder_set" }

    fn description(&self) -> &str {
        "Set a reminder — schedule a one-time alert/notification at a specific time. \
         Use when the user says remind me, don't let me forget, alert me at, wake me up at, set a timer."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message":  { "type": "string", "description": "What to be reminded about" },
                "time":     { "type": "string", "description": "When to fire, in natural language (e.g. 'in 30 minutes', 'tomorrow at 9am', 'at 3pm')" },
                "timezone": { "type": "string", "description": "IANA timezone (default: LUMINA_TIMEZONE env or America/Los_Angeles)" }
            },
            "required": ["message", "time"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let message = args["message"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'message' must be a string".into()))?;
        let time_phrase = args["time"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'time' must be a string".into()))?;
        let tz = resolve_timezone(args["timezone"].as_str())?;

        let now = Utc::now();
        let fire_at = parse_time(time_phrase, tz, now)
            .map_err(|e| ToolError::InvalidArgument(format!("could not understand time '{time_phrase}': {e}")))?;

        let id = uuid::Uuid::new_v4().to_string();
        let pool = get_pool().await?;

        // Retry once on a transient DB error before reporting failure — the
        // insert is idempotent on the freshly-minted UUID, so a retry can't
        // double-write. Avoids the "tool said it failed but it actually
        // persisted" ambiguity seen in live testing.
        let mut last_err = None;
        for attempt in 0..2 {
            match sqlx::query("INSERT INTO reminders (id, message, fire_at) VALUES ($1, $2, $3)")
                .bind(&id)
                .bind(message)
                .bind(fire_at)
                .execute(&pool)
                .await
            {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }
        if let Some(e) = last_err {
            return Err(ToolError::Database(format!("Failed to store reminder: {e}")));
        }

        let local = fire_at.with_timezone(&tz);
        // Unambiguous success — leads with a clear marker + echoes the parsed
        // time so the model can confidently confirm to the user.
        Ok(format!(
            "✅ Reminder created (id={}). I'll remind you \"{}\" on {} ({}).",
            id,
            message,
            local.format("%A %B %-d at %-I:%M %p"),
            tz.name(),
        ))
    }
}

// ---------------------------------------------------------------------------
// reminder_list
// ---------------------------------------------------------------------------

pub struct ReminderList;

#[async_trait]
impl RustTool for ReminderList {
    fn name(&self) -> &str { "reminder_list" }

    fn description(&self) -> &str {
        "List pending reminders — upcoming scheduled alerts. \
         Use when the user asks about their reminders or what's scheduled."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = get_pool().await?;

        let rows: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT id, message, fire_at FROM reminders \
             WHERE fired = false AND cancelled = false \
             ORDER BY fire_at ASC",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to list reminders: {e}")))?;

        if rows.is_empty() {
            return Ok("No pending reminders.".into());
        }

        let tz = resolve_timezone(None).unwrap_or(chrono_tz::America::Los_Angeles);
        let mut out = format!("{} pending reminder(s):\n\n", rows.len());
        for (id, message, fire_at) in &rows {
            let local = fire_at.with_timezone(&tz);
            out.push_str(&format!(
                "[id={id}] {} — {message}\n",
                local.format("%A %B %-d at %-I:%M %p")
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// reminder_cancel
// ---------------------------------------------------------------------------

pub struct ReminderCancel;

#[async_trait]
impl RustTool for ReminderCancel {
    fn name(&self) -> &str { "reminder_cancel" }

    fn description(&self) -> &str {
        "Cancel a reminder — remove a scheduled alert."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reminder_id": { "type": "string", "description": "ID of the reminder to cancel" }
            },
            "required": ["reminder_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let reminder_id = args["reminder_id"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'reminder_id' must be a string".into()))?;

        let pool = get_pool().await?;

        let result = sqlx::query(
            "UPDATE reminders SET cancelled = true \
             WHERE id = $1 AND fired = false AND cancelled = false",
        )
        .bind(reminder_id)
        .execute(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to cancel reminder: {e}")))?;

        if result.rows_affected() == 0 {
            Err(ToolError::NotFound(format!(
                "Reminder id={reminder_id} not found, already fired, or already cancelled"
            )))
        } else {
            Ok(format!("Reminder id={reminder_id} cancelled"))
        }
    }
}

// ---------------------------------------------------------------------------
// reminder_poll (internal — scheduler use)
// ---------------------------------------------------------------------------

pub struct ReminderPoll;

#[async_trait]
impl RustTool for ReminderPoll {
    fn name(&self) -> &str { "reminder_poll" }

    fn description(&self) -> &str {
        "Internal: atomically claim due reminders and mark them fired. Returns JSON [{id, message}]. \
         Used by the lumina-core scheduler — not for direct user use."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = get_pool().await?;

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| ToolError::Database(format!("Failed to begin transaction: {e}")))?;

        // Claim due rows with row-level locks so concurrent pollers don't double-fire.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, message FROM reminders \
             WHERE fire_at <= now() AND fired = false AND cancelled = false \
             ORDER BY fire_at ASC \
             FOR UPDATE SKIP LOCKED",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to select due reminders: {e}")))?;

        if !rows.is_empty() {
            let ids: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
            sqlx::query("UPDATE reminders SET fired = true WHERE id = ANY($1)")
                .bind(&ids)
                .execute(&mut *tx)
                .await
                .map_err(|e| ToolError::Database(format!("Failed to mark reminders fired: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| ToolError::Database(format!("Failed to commit poll transaction: {e}")))?;

        let payload: Vec<Value> = rows
            .into_iter()
            .map(|(id, message)| json!({ "id": id, "message": message }))
            .collect();

        serde_json::to_string(&payload)
            .map_err(|e| ToolError::Database(format!("Failed to serialize reminders: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ReminderSet));
    registry.register_or_replace(Box::new(ReminderList));
    registry.register_or_replace(Box::new(ReminderCancel));
    registry.register_or_replace(Box::new(ReminderPoll));
}

// ---------------------------------------------------------------------------
// Unit tests (metadata + DB-absence behavior; time parsing tested in parse.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reminder_set_metadata() {
        let t = ReminderSet;
        assert_eq!(t.name(), "reminder_set");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        let req = p["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "message"));
        assert!(req.iter().any(|v| v == "time"));
        // Discovery keywords present in the description.
        let d = t.description().to_lowercase();
        assert!(d.contains("remind me"));
        assert!(d.contains("set a timer"));
    }

    #[test]
    fn test_reminder_list_metadata() {
        let t = ReminderList;
        assert_eq!(t.name(), "reminder_list");
        assert_eq!(t.parameters()["type"], "object");
        assert!(t.description().to_lowercase().contains("pending"));
    }

    #[test]
    fn test_reminder_cancel_metadata() {
        let t = ReminderCancel;
        assert_eq!(t.name(), "reminder_cancel");
        let p = t.parameters();
        let req = p["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "reminder_id"));
    }

    #[test]
    fn test_reminder_poll_metadata() {
        let t = ReminderPoll;
        assert_eq!(t.name(), "reminder_poll");
        assert_eq!(t.parameters()["type"], "object");
    }

    #[test]
    fn test_resolve_timezone_default() {
        std::env::remove_var("LUMINA_TIMEZONE");
        let tz = resolve_timezone(None).unwrap();
        assert_eq!(tz.name(), "America/Los_Angeles");
    }

    #[test]
    fn test_resolve_timezone_explicit() {
        let tz = resolve_timezone(Some("America/New_York")).unwrap();
        assert_eq!(tz.name(), "America/New_York");
    }

    #[test]
    fn test_resolve_timezone_invalid() {
        assert!(resolve_timezone(Some("Mars/Olympus_Mons")).is_err());
    }

    #[tokio::test]
    async fn test_reminder_set_missing_db_url() {
        std::env::remove_var("DATABASE_URL");
        let t = ReminderSet;
        let r = t.execute(json!({"message": "x", "time": "in 5 minutes"})).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn test_reminder_set_bad_time_is_invalid_arg() {
        // Bad time should fail before DB connection is attempted.
        std::env::remove_var("DATABASE_URL");
        let t = ReminderSet;
        let r = t.execute(json!({"message": "x", "time": "flibbertigibbet"})).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    fn test_registration() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("reminder_set"));
        assert!(reg.contains("reminder_list"));
        assert!(reg.contains("reminder_cancel"));
        assert!(reg.contains("reminder_poll"));
    }
}
