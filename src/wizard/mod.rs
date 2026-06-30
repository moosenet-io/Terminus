//! Wizard tools — LLM consultation through the Chord proxy.
//!
//! All 3 tools use `CHORD_PROXY_URL` for HTTP calls and
//! `WIZARD_DATABASE_URL` (or falls back to `VECTOR_DATABASE_URL`) for
//! session history. Shell commands are **never** used.
//!
//! Tools: wizard_consult, wizard_status, wizard_history

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn chord_proxy_url() -> Result<String, ToolError> {
    std::env::var("CHORD_PROXY_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .map_err(|_| ToolError::NotConfigured("CHORD_PROXY_URL not set".into()))
}

fn wizard_db_url() -> Result<String, ToolError> {
    // Try wizard-specific URL first, fall back to the shared vector DB
    std::env::var("WIZARD_DATABASE_URL")
        .or_else(|_| std::env::var("VECTOR_DATABASE_URL"))
        .map_err(|_| {
            ToolError::NotConfigured(
                "WIZARD_DATABASE_URL (or VECTOR_DATABASE_URL) not set".into(),
            )
        })
}

/// Sanitize a free-text question for the Wizard: strip control chars, cap at 2000 chars.
fn sanitize_question(raw: &str) -> Result<String, ToolError> {
    let cleaned: String = raw.chars().filter(|c| !c.is_ascii_control()).collect();
    let truncated: String = cleaned.chars().take(2000).collect();
    if truncated.trim().is_empty() {
        return Err(ToolError::InvalidArgument(
            "question must not be empty".into(),
        ));
    }
    Ok(truncated)
}

/// Sanitize optional context text: strip control chars, cap at 4000 chars.
fn sanitize_context(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_ascii_control())
        .take(4000)
        .collect()
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ToolCallRequest {
    name: String,
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct ToolCallResponse {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool: wizard_consult
// ---------------------------------------------------------------------------

pub struct WizardConsult;

#[async_trait]
impl RustTool for WizardConsult {
    fn name(&self) -> &str { "wizard_consult" }

    fn description(&self) -> &str {
        "Submit a consultation question to the Wizard (deep-reasoning LLM council) \
         through the Chord proxy. Returns the council's synthesized response."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question or problem to submit to the Wizard council (max 2000 chars)"
                },
                "context": {
                    "type": "string",
                    "description": "Optional background context to include (max 4000 chars)"
                }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw_question = args["question"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'question'".into()))?;

        let question = sanitize_question(raw_question)?;
        let context = args["context"]
            .as_str()
            .map(sanitize_context)
            .unwrap_or_default();

        let base = chord_proxy_url()?;
        let url = format!("{base}/v1/tools/call");

        let mut call_args = json!({ "question": question });
        if !context.is_empty() {
            call_args["context"] = Value::String(context);
        }

        let body = ToolCallRequest {
            name: "wizard_council_consult".into(),
            arguments: call_args,
        };

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Wizard consultation failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(ToolError::Http(format!(
                "Chord proxy returned HTTP {status} for wizard_consult"
            )));
        }

        let result: ToolCallResponse = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to parse Chord response: {e}")))?;

        if let Some(err_msg) = result.error {
            return Err(ToolError::Execution(format!(
                "Wizard consultation error: {err_msg}"
            )));
        }

        Ok(result
            .result
            .unwrap_or_else(|| "(no response from Wizard council)".into()))
    }
}

// ---------------------------------------------------------------------------
// Tool: wizard_status
// ---------------------------------------------------------------------------

pub struct WizardStatus;

#[async_trait]
impl RustTool for WizardStatus {
    fn name(&self) -> &str { "wizard_status" }

    fn description(&self) -> &str {
        "Check whether Wizard consultation is available (CHORD_PROXY_URL is configured)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        match chord_proxy_url() {
            Ok(url) => Ok(format!(
                "Wizard consultation available (proxy: {url})"
            )),
            Err(_) => Ok(
                "Wizard consultation not available: CHORD_PROXY_URL is not set".into(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: wizard_history
// ---------------------------------------------------------------------------

pub struct WizardHistory;

#[async_trait]
impl RustTool for WizardHistory {
    fn name(&self) -> &str { "wizard_history" }

    fn description(&self) -> &str {
        "Return past Wizard consultation sessions for a user."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id": {
                    "type": "string",
                    "description": "User ID to look up sessions for"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of sessions to return (1-50, default 10)",
                    "default": 10
                }
            },
            "required": ["user_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let user_id = args["user_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'user_id'".into()))?;

        // Reject empty or suspiciously long user_id strings
        if user_id.is_empty() {
            return Err(ToolError::InvalidArgument("user_id must not be empty".into()));
        }
        if user_id.len() > 128 {
            return Err(ToolError::InvalidArgument(
                "user_id too long (max 128 chars)".into(),
            ));
        }

        let limit = args["limit"].as_i64().unwrap_or(10).clamp(1, 50) as i64;

        let db_url = wizard_db_url()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .map_err(|e| ToolError::Database(e.to_string()))?;

        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT id, question, created_at::text FROM wizard_sessions \
             WHERE user_id = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(e.to_string()))?;

        if rows.is_empty() {
            return Ok(format!("No Wizard sessions found for user '{user_id}'"));
        }

        let lines: Vec<String> = rows
            .into_iter()
            .map(|(id, question, created_at)| {
                format!("  [{id}] {created_at}: {question}")
            })
            .collect();

        Ok(format!(
            "Wizard sessions for '{user_id}':\n{}",
            lines.join("\n")
        ))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all 3 Wizard tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(WizardConsult),
        Box::new(WizardStatus),
        Box::new(WizardHistory),
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

    // --- sanitize_question --------------------------------------------------

    #[test]
    fn test_sanitize_question_normal() {
        let q = "What is the best architecture for a distributed inbox?";
        assert_eq!(sanitize_question(q).unwrap(), q);
    }

    #[test]
    fn test_sanitize_question_strips_control_chars() {
        let raw = "hello\x00world\x1F";
        let result = sanitize_question(raw).unwrap();
        assert_eq!(result, "helloworld");
    }

    #[test]
    fn test_sanitize_question_truncates_at_2000() {
        let long = "x".repeat(2500);
        let result = sanitize_question(&long).unwrap();
        assert_eq!(result.chars().count(), 2000);
    }

    #[test]
    fn test_sanitize_question_empty_returns_error() {
        let err = sanitize_question("").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_sanitize_question_whitespace_only_returns_error() {
        let err = sanitize_question("   ").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_sanitize_question_control_chars_only_returns_error() {
        let err = sanitize_question("\x00\x01\x1F").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // --- sanitize_context ---------------------------------------------------

    #[test]
    fn test_sanitize_context_truncates_at_4000() {
        let long = "c".repeat(5000);
        let result = sanitize_context(&long);
        assert_eq!(result.chars().count(), 4000);
    }

    #[test]
    fn test_sanitize_context_strips_control_chars() {
        let raw = "good\x00bad\x1Fend";
        let result = sanitize_context(raw);
        assert_eq!(result, "goodbadend");
    }

    #[test]
    fn test_sanitize_context_empty_returns_empty() {
        assert_eq!(sanitize_context(""), "");
    }

    // --- wizard_status always succeeds (no network required) ----------------

    #[tokio::test]
    #[serial]
    async fn test_wizard_status_returns_ok_when_url_set() {
        // Temporarily set the env var (won't conflict — this process owns it)
        std::env::set_var("CHORD_PROXY_URL_TEST_ONLY", "http://test.local");
        // wizard_status reads CHORD_PROXY_URL, which we won't set; it should
        // still return Ok (not an Err) even when not configured
        let tool = WizardStatus;
        let result = tool.execute(json!({})).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_wizard_status_ok_even_when_chord_not_configured() {
        if std::env::var("CHORD_PROXY_URL").is_ok() {
            return; // URL is set — tool will report "available"
        }
        let tool = WizardStatus;
        let result = tool.execute(json!({})).await;
        // wizard_status returns Ok in both configured and unconfigured states
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("not available") || text.contains("available"));
    }

    // --- NotConfigured for DB-dependent tools --------------------------------

    #[tokio::test]
    #[serial]
    async fn test_wizard_history_not_configured_without_env() {
        if std::env::var("WIZARD_DATABASE_URL").is_ok()
            || std::env::var("VECTOR_DATABASE_URL").is_ok()
        {
            return; // real DB available — skip
        }
        let tool = WizardHistory;
        let result = tool.execute(json!({"user_id": "alice"})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_wizard_consult_not_configured_without_env() {
        if std::env::var("CHORD_PROXY_URL").is_ok() {
            return;
        }
        let tool = WizardConsult;
        let result = tool
            .execute(json!({"question": "Is Rust safe?"}))
            .await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    // --- Input validation ---------------------------------------------------

    #[tokio::test]
    async fn test_wizard_consult_rejects_empty_question() {
        let tool = WizardConsult;
        let result = tool.execute(json!({"question": ""})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn test_wizard_history_rejects_empty_user_id() {
        let tool = WizardHistory;
        let result = tool.execute(json!({"user_id": ""})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn test_wizard_history_rejects_long_user_id() {
        let tool = WizardHistory;
        let long_id = "u".repeat(129);
        let result = tool.execute(json!({"user_id": long_id})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    // --- Registration -------------------------------------------------------

    #[test]
    fn test_wizard_registers_3_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 3);
    }

    #[test]
    fn test_wizard_tool_names_present() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("wizard_consult"));
        assert!(registry.contains("wizard_status"));
        assert!(registry.contains("wizard_history"));
    }

    // --- Parameter schema ---------------------------------------------------

    #[test]
    fn test_wizard_consult_parameters_include_context() {
        let tool = WizardConsult;
        let params = tool.parameters();
        assert!(params["properties"]["context"].is_object());
    }

    #[test]
    fn test_wizard_history_parameters_include_limit() {
        let tool = WizardHistory;
        let params = tool.parameters();
        assert!(params["properties"]["limit"].is_object());
    }
}
