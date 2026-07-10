//! Google tools — Calendar (CalDAV) + Email (IMAP read / SMTP send).
//!
//! Eight tools, ported from the Python google_tools.py on the legacy Terminus host:
//!   google_calendar_today / _week    — read events across all calendars (caldav.rs)
//!   google_calendar_add              — create an event (caldav.rs)
//!   google_calendar_conflicts        — check a slot for conflicts (caldav.rs)
//!   google_email_inbox / _read       — list / read mail (imap.rs)
//!   google_email_summary             — LLM-summarized inbox digest (imap.rs)
//!   google_email_send                — send mail (smtp.rs)
//!
//! Auth: Gmail App Password (no OAuth) over standard CalDAV / IMAPS / SMTP+STARTTLS.
//! Works with any compatible provider.
//!
//! Required env:
//!   GOOGLE_LUMINA_EMAIL   — the account address (also IMAP/SMTP/CalDAV username)
//!   GOOGLE_APP_PASSWORD   — Gmail App Password
//! Optional env:
//!   GOOGLE_SECONDARY_EMAIL      — operator's personal calendar (default <email>) // pii-test-fixture
//!   GOOGLE_LUMINA_CALENDAR_ID   — extra group calendar id to include
//!   GOOGLE_EXTRA_CALENDARS      — comma-separated extra calendar ids

pub mod caldav;
pub mod imap;
pub mod smtp;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// Shared Google account configuration, read once from the environment.
#[derive(Clone)]
pub struct GoogleConfig {
    pub email: String,
    pub app_password: String,
    pub secondary_email: String,
    pub lumina_calendar_id: Option<String>,
    pub extra_calendars: Vec<String>,
}

impl GoogleConfig {
    pub fn from_env() -> Result<Self, ToolError> {
        let email = std::env::var("GOOGLE_LUMINA_EMAIL")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("GOOGLE_LUMINA_EMAIL not set".into()))?;
        let app_password = std::env::var("GOOGLE_APP_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("GOOGLE_APP_PASSWORD not set".into()))?;
        let secondary_email = std::env::var("GOOGLE_SECONDARY_EMAIL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "<email>".into()); // pii-test-fixture
        let lumina_calendar_id =
            std::env::var("GOOGLE_LUMINA_CALENDAR_ID").ok().filter(|s| !s.is_empty());
        let extra_calendars = std::env::var("GOOGLE_EXTRA_CALENDARS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self { email, app_password, secondary_email, lumina_calendar_id, extra_calendars })
    }

    /// Every calendar id to query, in priority order, de-duplicated:
    /// the account's own calendar, the operator's personal calendar, any extras,
    /// and the Lumina group calendar.
    pub fn all_calendar_ids(&self) -> Vec<String> {
        let mut ids = vec![self.email.clone(), self.secondary_email.clone()];
        ids.extend(self.extra_calendars.iter().cloned());
        if let Some(ref c) = self.lumina_calendar_id {
            ids.push(c.clone());
        }
        let mut seen = std::collections::HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));
        ids
    }
}

/// All eight tool names, used for stub registration when unconfigured.
pub const GOOGLE_TOOL_NAMES: &[&str] = &[
    "google_calendar_today",
    "google_calendar_week",
    "google_calendar_add",
    "google_calendar_conflicts",
    "google_email_inbox",
    "google_email_read",
    "google_email_summary",
    "google_email_send",
];

pub fn register(registry: &mut ToolRegistry) {
    match GoogleConfig::from_env() {
        Ok(cfg) => {
            caldav::register(registry, &cfg);
            imap::register(registry, &cfg);
            smtp::register(registry, &cfg);
        }
        Err(e) => {
            tracing::warn!("Google tools not configured: {e}. Registering stubs.");
            for name in GOOGLE_TOOL_NAMES {
                registry.register_or_replace(Box::new(NotConfiguredStub(name)));
            }
        }
    }
}

/// Stub returned when GOOGLE_* credentials are absent: the tool stays visible in
/// the catalog and returns a clear error instead of disappearing.
pub struct NotConfiguredStub(pub &'static str);

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "Google tool (GOOGLE_LUMINA_EMAIL / GOOGLE_APP_PASSWORD not configured)"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured(
            "Google tools need GOOGLE_LUMINA_EMAIL and GOOGLE_APP_PASSWORD".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GoogleConfig {
        GoogleConfig {
            email: "<email>".into(), // pii-test-fixture
            app_password: "x".into(),
            secondary_email: "<email>".into(), // pii-test-fixture
            lumina_calendar_id: Some("<email>".into()), // pii-test-fixture
            extra_calendars: vec![],
        }
    }

    #[test]
    fn all_calendar_ids_dedups_and_orders() {
        let ids = cfg().all_calendar_ids();
        assert_eq!(ids[0], "<email>"); // pii-test-fixture
        assert_eq!(ids[1], "<email>"); // pii-test-fixture
        assert!(ids.contains(&"<email>".to_string())); // pii-test-fixture
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }

    #[test]
    fn stub_name_is_stable() {
        assert_eq!(NotConfiguredStub("google_email_inbox").name(), "google_email_inbox");
    }
}
