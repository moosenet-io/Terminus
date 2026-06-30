//! Seer tools — research backend integration via typed HTTP (reqwest).
//!
//! All 3 tools communicate with the Seer research service identified by
//! `SEER_API_URL`. Query strings are validated (max 500 chars, control chars
//! stripped) before being sent. Shell commands are **never** used.
//!
//! Tools: seer_query, seer_status, seer_recent

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Query sanitization
// ---------------------------------------------------------------------------

/// Sanitize a research query:
/// - Strip ASCII control characters (0x00-0x1F, 0x7F)
/// - Truncate to 500 characters
/// - Return an error if the result is empty
pub fn sanitize_query(raw: &str) -> Result<String, ToolError> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_ascii_control())
        .collect();

    let truncated = if cleaned.chars().count() > 500 {
        cleaned.chars().take(500).collect()
    } else {
        cleaned
    };

    if truncated.trim().is_empty() {
        return Err(ToolError::InvalidArgument(
            "query must not be empty after sanitization".into(),
        ));
    }

    Ok(truncated)
}

// ---------------------------------------------------------------------------
// Helper — get SEER_API_URL
// ---------------------------------------------------------------------------

fn seer_api_url() -> Result<String, ToolError> {
    std::env::var("SEER_API_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .map_err(|_| ToolError::NotConfigured("SEER_API_URL not set".into()))
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ResearchResponse {
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    sources: Vec<SourceEntry>,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SourceEntry {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResearchRequest {
    question: String,
    max_sources: u32,
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RecentEntry {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool: seer_query
// ---------------------------------------------------------------------------

pub struct SeerQuery;

#[async_trait]
impl RustTool for SeerQuery {
    fn name(&self) -> &str { "seer_query" }

    fn description(&self) -> &str {
        "Submit a research question to the Seer research backend. \
         Returns a synthesized answer with cited sources."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question":    {"type": "string", "description": "Research question (max 500 chars)"},
                "max_sources": {"type": "integer", "description": "Maximum number of sources to include (1-20, default 5)", "default": 5}
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw_question = args["question"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'question'".into()))?;

        let question = sanitize_query(raw_question)?;
        let max_sources = args["max_sources"].as_u64().unwrap_or(5).clamp(1, 20) as u32;

        let base = seer_api_url()?;
        let url = format!("{base}/api/research");

        let client = reqwest::Client::new();
        let body = ResearchRequest { question, max_sources };

        let resp = client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Seer request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(ToolError::Http(format!(
                "Seer returned HTTP {status}"
            )));
        }

        let result: ResearchResponse = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to parse Seer response: {e}")))?;

        let mut output = String::new();

        if let Some(q) = &result.query {
            output.push_str(&format!("Question: {q}\n\n"));
        }

        if let Some(answer) = &result.answer {
            output.push_str("Answer:\n");
            output.push_str(answer);
            output.push('\n');
        } else {
            output.push_str("(No answer returned)\n");
        }

        if !result.sources.is_empty() {
            output.push_str("\nSources:\n");
            for (i, src) in result.sources.iter().enumerate() {
                let title = src.title.as_deref().unwrap_or("Untitled");
                let url_str = src.url.as_deref().unwrap_or("(no URL)");
                output.push_str(&format!("  {}. {} — {}\n", i + 1, title, url_str));
            }
        }

        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Tool: seer_status
// ---------------------------------------------------------------------------

pub struct SeerStatus;

#[async_trait]
impl RustTool for SeerStatus {
    fn name(&self) -> &str { "seer_status" }

    fn description(&self) -> &str {
        "Check whether the Seer research service is online and healthy."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let base = seer_api_url()?;
        let url = format!("{base}/api/health");

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Seer health check failed: {e}")))?;

        let http_status = resp.status();
        let parsed: Result<HealthResponse, _> = resp.json().await;

        let service_status = parsed
            .ok()
            .and_then(|r| r.status)
            .unwrap_or_else(|| if http_status.is_success() {
                "ok".into()
            } else {
                "unknown".into()
            });

        if http_status.is_success() {
            Ok(format!("Seer research service: {service_status}"))
        } else {
            Err(ToolError::Http(format!(
                "Seer returned HTTP {http_status} (status={service_status})"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: seer_recent
// ---------------------------------------------------------------------------

pub struct SeerRecent;

#[async_trait]
impl RustTool for SeerRecent {
    fn name(&self) -> &str { "seer_recent" }

    fn description(&self) -> &str {
        "Return the most recent research queries processed by Seer."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "description": "Maximum number of entries to return (1-50, default 10)", "default": 10}
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50);

        let base = seer_api_url()?;
        let url = format!("{base}/api/recent?limit={limit}");

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Seer request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(ToolError::Http(format!("Seer returned HTTP {status}")));
        }

        let entries: Vec<RecentEntry> = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to parse Seer response: {e}")))?;

        if entries.is_empty() {
            return Ok("No recent research queries found".into());
        }

        let lines: Vec<String> = entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let q = e.query.as_deref().unwrap_or("(unknown)");
                let ts = e.created_at.as_deref().unwrap_or("(no timestamp)");
                format!("  {}. [{ts}] {q}", i + 1)
            })
            .collect();

        Ok(format!("Recent research queries:\n{}", lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all 3 Seer tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(SeerQuery),
        Box::new(SeerStatus),
        Box::new(SeerRecent),
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

    // --- Query sanitization -------------------------------------------------

    #[test]
    fn test_sanitize_query_normal_string() {
        let result = sanitize_query("What is the capital of France?").unwrap();
        assert_eq!(result, "What is the capital of France?");
    }

    #[test]
    fn test_sanitize_query_strips_control_chars() {
        let raw = "hello\x00\x01\x1Fworld\x7F";
        let result = sanitize_query(raw).unwrap();
        assert_eq!(result, "helloworld");
    }

    #[test]
    fn test_sanitize_query_truncates_at_500_chars() {
        let long = "a".repeat(600);
        let result = sanitize_query(&long).unwrap();
        assert_eq!(result.len(), 500);
    }

    #[test]
    fn test_sanitize_query_exactly_500_chars_passes() {
        let exact = "a".repeat(500);
        let result = sanitize_query(&exact).unwrap();
        assert_eq!(result.len(), 500);
    }

    #[test]
    fn test_sanitize_query_empty_after_strip_returns_error() {
        let raw = "\x00\x01\x1F\x7F";
        let err = sanitize_query(raw).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_sanitize_query_empty_string_returns_error() {
        let err = sanitize_query("").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_sanitize_query_whitespace_only_returns_error() {
        let err = sanitize_query("   ").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_sanitize_query_preserves_unicode() {
        let q = "Qu\u{00E9}bec recherche";
        let result = sanitize_query(q).unwrap();
        assert_eq!(result, q);
    }

    // --- NotConfigured when SEER_API_URL not set ----------------------------

    #[tokio::test]
    #[serial]
    async fn test_seer_query_not_configured_without_env() {
        if std::env::var("SEER_API_URL").is_ok() {
            return; // real service available, skip NotConfigured test
        }
        let tool = SeerQuery;
        let result = tool
            .execute(json!({"question": "What is Rust?"}))
            .await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_seer_status_not_configured_without_env() {
        if std::env::var("SEER_API_URL").is_ok() {
            return;
        }
        let tool = SeerStatus;
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_seer_recent_not_configured_without_env() {
        if std::env::var("SEER_API_URL").is_ok() {
            return;
        }
        let tool = SeerRecent;
        let result = tool.execute(json!({"limit": 5})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    // --- Input validation ---------------------------------------------------

    #[tokio::test]
    async fn test_seer_query_rejects_empty_question() {
        let tool = SeerQuery;
        let result = tool.execute(json!({"question": ""})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn test_seer_query_rejects_control_char_only_question() {
        let tool = SeerQuery;
        let result = tool.execute(json!({"question": "\x00\x01"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    // --- Registration -------------------------------------------------------

    #[test]
    fn test_seer_registers_3_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 3);
    }

    #[test]
    fn test_seer_tool_names_present() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("seer_query"));
        assert!(registry.contains("seer_status"));
        assert!(registry.contains("seer_recent"));
    }

    // --- Parameter schema ---------------------------------------------------

    #[test]
    fn test_seer_query_parameters_include_max_sources() {
        let tool = SeerQuery;
        let params = tool.parameters();
        assert!(params["properties"]["max_sources"].is_object());
    }

    #[test]
    fn test_seer_recent_parameters_include_limit() {
        let tool = SeerRecent;
        let params = tool.parameters();
        assert!(params["properties"]["limit"].is_object());
    }
}
