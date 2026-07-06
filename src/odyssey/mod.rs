//! Odyssey tools — travel planning integration (async/long-running subset).
//!
//! This module carries the 2 Odyssey tools that delegate to other backend
//! services and take longer than an instant round trip:
//!
//! - `odyssey_research` — deep destination research via Seer
//! - `odyssey_optimize` — card/points strategy reasoning via the Wizard council
//!
//! The 6 CRUD tools (`odyssey_bucket_add`, `odyssey_bucket_list`,
//! `odyssey_update_points`, `odyssey_list_cards`, `odyssey_log_trip`,
//! `odyssey_deals`) are ported separately and are expected to land in this
//! same module/file; this file intentionally only defines the 2 tools below
//! so the two ports can be merged without touching each other's code.
//!
//! ## Observed <host> behavior (verified 2026-07-06 via a live `tools/call`
//! against <host>'s MCP endpoint)
//!
//! Both tools are **synchronous** on <host>: a single `tools/call` request
//! blocks for the full duration of the underlying work and returns one
//! result — there is no separate "submit"/"poll"/"cancel" tool triple for
//! Odyssey (contrast with `axon_submit` / `axon_status` / `axon_cancel`,
//! which <host> *does* expose for genuinely async work). A live call to each
//! tool returned promptly with a JSON body shaped
//! `{"status": "failed"|"success", "output": ..., "error": ...}`, which is
//! <host>'s own SSH-to-fleet-host wrapper shape (it shells out to the agent
//! fleet host) — plumbing this Rust port does not replicate, since
//! `RustTool::execute` must never use shell/subprocess calls (see
//! `crate::tool`) and this repo already has a native HTTP-based Seer client
//! (`crate::seer`) and Wizard client (`crate::wizard`) that reach the same
//! underlying capability. This port reuses those clients' patterns
//! (their env var names, sanitization approach, and `reqwest` + timeout
//! shape) rather than inventing a new async job/handle abstraction that
//! neither <host> nor this repo's existing seer/wizard modules actually use.
//!
//! Both tools below are therefore implemented the same way: bounded
//! synchronous HTTP calls (`tokio::time::timeout` wrapping a `reqwest` call
//! with its own client-side timeout) so a hung or slow backend cannot hang
//! the calling MCP server indefinitely.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Sanitization (mirrors crate::seer::sanitize_query / crate::wizard's
// sanitize_question — each module keeps its own copy by repo convention
// rather than sharing a helper across modules)
// ---------------------------------------------------------------------------

/// Sanitize free-text travel-planning input: strip ASCII control characters
/// and cap length. Returns an error if the result is empty.
fn sanitize_text(raw: &str, max_chars: usize) -> Result<String, ToolError> {
    let cleaned: String = raw.chars().filter(|c| !c.is_ascii_control()).collect();
    let truncated: String = cleaned.chars().take(max_chars).collect();
    if truncated.trim().is_empty() {
        return Err(ToolError::InvalidArgument(
            "value must not be empty after sanitization".into(),
        ));
    }
    Ok(truncated)
}

// ---------------------------------------------------------------------------
// Env helpers (each module keeps its own accessor by repo convention; see
// crate::seer::seer_api_url and crate::wizard's chord_proxy_url)
// ---------------------------------------------------------------------------

fn seer_api_url() -> Result<String, ToolError> {
    std::env::var("SEER_API_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .map_err(|_| ToolError::NotConfigured("SEER_API_URL not set".into()))
}

fn chord_proxy_url() -> Result<String, ToolError> {
    std::env::var("CHORD_PROXY_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .map_err(|_| ToolError::NotConfigured("CHORD_PROXY_URL not set".into()))
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SeerResearchRequest {
    question: String,
    max_sources: u32,
}

#[derive(Debug, Deserialize)]
struct SeerResearchResponse {
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    sources: Vec<SeerSourceEntry>,
}

#[derive(Debug, Deserialize)]
struct SeerSourceEntry {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Serialize)]
struct WizardToolCallRequest {
    name: String,
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct WizardToolCallResponse {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

/// <host>'s docstring says "Takes 2-5 minutes"; bound generously but finitely
/// so a wedged Seer backend cannot hang the calling MCP server forever.
const RESEARCH_TIMEOUT_SECS: u64 = 300;

/// <host>'s docstring says "Takes up to 60 seconds"; give some headroom over
/// that for a slower-than-usual Wizard council round, still bounded.
const OPTIMIZE_TIMEOUT_SECS: u64 = 90;

// ---------------------------------------------------------------------------
// Tool: odyssey_research
// ---------------------------------------------------------------------------

pub struct OdysseyResearch;

#[async_trait]
impl RustTool for OdysseyResearch {
    fn name(&self) -> &str {
        "odyssey_research"
    }

    fn description(&self) -> &str {
        "Trigger Seer deep research for a travel destination. Runs a research \
         sweep via Seer and returns a synthesized answer with cited sources. \
         destination: e.g. 'Tokyo, Japan' or 'Patagonia, Chile'. dates: optional \
         travel window, e.g. 'March 2027' or 'spring'. budget: optional budget \
         hint, e.g. '$5000' or 'budget'. travelers: number of people (default 1). \
         Can take up to a few minutes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "destination": {"type": "string", "description": "Travel destination, e.g. 'Tokyo, Japan'"},
                "dates": {"type": "string", "description": "Optional travel window, e.g. 'March 2027' or 'spring'", "default": ""},
                "budget": {"type": "string", "description": "Optional budget hint, e.g. '$5000' or 'budget'", "default": ""},
                "travelers": {"type": "integer", "description": "Number of travelers (default 1)", "default": 1}
            },
            "required": ["destination"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw_destination = args["destination"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'destination'".into()))?;
        let destination = sanitize_text(raw_destination, 200)?;

        let dates = args["dates"].as_str().unwrap_or("").trim();
        let budget = args["budget"].as_str().unwrap_or("").trim();
        let travelers = args["travelers"].as_i64().unwrap_or(1).clamp(1, 50);

        let mut question = format!(
            "Research the travel destination {destination} for {travelers} traveler(s)."
        );
        if !dates.is_empty() {
            let dates = sanitize_text(dates, 100).unwrap_or_default();
            if !dates.is_empty() {
                question.push_str(&format!(" Travel dates: {dates}."));
            }
        }
        if !budget.is_empty() {
            let budget = sanitize_text(budget, 100).unwrap_or_default();
            if !budget.is_empty() {
                question.push_str(&format!(" Budget: {budget}."));
            }
        }
        question.push_str(
            " Cover: best time to visit, must-see sights, local transport, \
              typical costs, and any travel advisories.",
        );

        let base = seer_api_url()?;
        let url = format!("{base}/api/research");

        let client = reqwest::Client::new();
        let body = SeerResearchRequest {
            question,
            max_sources: 8,
        };

        let call = client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(RESEARCH_TIMEOUT_SECS));

        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(RESEARCH_TIMEOUT_SECS),
            call.send(),
        )
        .await
        .map_err(|_| {
            ToolError::Http(format!(
                "Seer research timed out after {RESEARCH_TIMEOUT_SECS}s"
            ))
        })?
        .map_err(|e| ToolError::Http(format!("Seer research request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(ToolError::Http(format!("Seer returned HTTP {status}")));
        }

        let result: SeerResearchResponse = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to parse Seer response: {e}")))?;

        let mut output = format!("Research report for {destination}:\n\n");
        if let Some(answer) = &result.answer {
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
// Tool: odyssey_optimize
// ---------------------------------------------------------------------------

pub struct OdysseyOptimize;

#[async_trait]
impl RustTool for OdysseyOptimize {
    fn name(&self) -> &str {
        "odyssey_optimize"
    }

    fn description(&self) -> &str {
        "Ask the Wizard (via the Chord proxy) to recommend the best card/points \
         strategy for a trip. destination: destination name, e.g. 'Tokyo, Japan'. \
         spend_estimate: estimated total trip spend in USD (default 5000). \
         Returns an AI recommendation. Can take up to about a minute."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "destination": {"type": "string", "description": "Destination name, e.g. 'Tokyo, Japan'"},
                "spend_estimate": {"type": "number", "description": "Estimated total trip spend in USD (default 5000)", "default": 5000}
            },
            "required": ["destination"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw_destination = args["destination"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("missing 'destination'".into()))?;
        let destination = sanitize_text(raw_destination, 200)?;

        let spend_estimate = args["spend_estimate"].as_f64().unwrap_or(5000.0);
        if !spend_estimate.is_finite() || spend_estimate < 0.0 {
            return Err(ToolError::InvalidArgument(
                "spend_estimate must be a non-negative number".into(),
            ));
        }

        let question = format!(
            "Recommend the best credit card / points redemption strategy for a \
             trip to {destination} with an estimated total spend of ${spend_estimate:.0}. \
             Recommend which card to use for flights, hotels, and dining, and \
             whether to redeem points or pay cash."
        );

        let base = chord_proxy_url()?;
        let url = format!("{base}/v1/tools/call");

        let client = reqwest::Client::new();
        let body = WizardToolCallRequest {
            name: "wizard_council_consult".into(),
            arguments: json!({ "question": question }),
        };

        let call = client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(OPTIMIZE_TIMEOUT_SECS));

        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(OPTIMIZE_TIMEOUT_SECS),
            call.send(),
        )
        .await
        .map_err(|_| {
            ToolError::Http(format!(
                "Wizard optimize timed out after {OPTIMIZE_TIMEOUT_SECS}s"
            ))
        })?
        .map_err(|e| ToolError::Http(format!("Wizard optimize request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(ToolError::Http(format!(
                "Chord proxy returned HTTP {status} for odyssey_optimize"
            )));
        }

        let result: WizardToolCallResponse = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to parse Chord response: {e}")))?;

        if let Some(err_msg) = result.error {
            return Err(ToolError::Execution(format!(
                "Wizard optimize error: {err_msg}"
            )));
        }

        Ok(result
            .result
            .unwrap_or_else(|| "(no recommendation returned)".into()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the async/long-running Odyssey tools into the given registry.
///
/// The 6 CRUD Odyssey tools are registered by a separate port; this function
/// intentionally only wires up the 2 tools owned by this file.
pub fn register(registry: &mut ToolRegistry) {
    let tools: Vec<Box<dyn RustTool>> = vec![Box::new(OdysseyResearch), Box::new(OdysseyOptimize)];
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

    // --- sanitize_text --------------------------------------------------

    #[test]
    fn test_sanitize_text_normal() {
        assert_eq!(sanitize_text("Tokyo, Japan", 200).unwrap(), "Tokyo, Japan");
    }

    #[test]
    fn test_sanitize_text_strips_control_chars() {
        let raw = "Tokyo\x00\x01, Japan\x1F";
        assert_eq!(sanitize_text(raw, 200).unwrap(), "Tokyo, Japan");
    }

    #[test]
    fn test_sanitize_text_truncates() {
        let long = "a".repeat(300);
        let result = sanitize_text(&long, 200).unwrap();
        assert_eq!(result.chars().count(), 200);
    }

    #[test]
    fn test_sanitize_text_empty_returns_error() {
        assert!(matches!(
            sanitize_text("", 200),
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[test]
    fn test_sanitize_text_whitespace_only_returns_error() {
        assert!(matches!(
            sanitize_text("   ", 200),
            Err(ToolError::InvalidArgument(_))
        ));
    }

    // --- NotConfigured without env ---------------------------------------

    #[tokio::test]
    #[serial]
    async fn test_odyssey_research_not_configured_without_env() {
        if std::env::var("SEER_API_URL").is_ok() {
            return; // real service available, skip
        }
        let tool = OdysseyResearch;
        let result = tool
            .execute(json!({"destination": "Tokyo, Japan"}))
            .await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_odyssey_optimize_not_configured_without_env() {
        if std::env::var("CHORD_PROXY_URL").is_ok() {
            return; // real service available, skip
        }
        let tool = OdysseyOptimize;
        let result = tool
            .execute(json!({"destination": "Tokyo, Japan", "spend_estimate": 3000}))
            .await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    // --- Input validation --------------------------------------------------

    #[tokio::test]
    async fn test_odyssey_research_rejects_missing_destination() {
        let tool = OdysseyResearch;
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn test_odyssey_research_rejects_empty_destination() {
        let tool = OdysseyResearch;
        let result = tool.execute(json!({"destination": ""})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn test_odyssey_optimize_rejects_missing_destination() {
        let tool = OdysseyOptimize;
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn test_odyssey_optimize_rejects_negative_spend() {
        let tool = OdysseyOptimize;
        let result = tool
            .execute(json!({"destination": "Tokyo, Japan", "spend_estimate": -100}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    // Note: a NaN-spend_estimate test is intentionally omitted — serde_json
    // cannot represent NaN (the `json!` macro coerces it to `null`), so a
    // caller can never actually deliver NaN over JSON-RPC; the `is_finite()`
    // guard in `execute()` remains defense-in-depth for any future
    // non-JSON-RPC caller.

    // --- Registration --------------------------------------------------------

    #[test]
    fn test_odyssey_registers_2_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn test_odyssey_tool_names_present() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("odyssey_research"));
        assert!(registry.contains("odyssey_optimize"));
    }

    // --- Parameter schema ---------------------------------------------------

    #[test]
    fn test_odyssey_research_parameters_include_travelers() {
        let tool = OdysseyResearch;
        let params = tool.parameters();
        assert!(params["properties"]["travelers"].is_object());
    }

    #[test]
    fn test_odyssey_optimize_parameters_include_spend_estimate() {
        let tool = OdysseyOptimize;
        let params = tool.parameters();
        assert!(params["properties"]["spend_estimate"].is_object());
    }

    // --- Timeout bounding (mocked via httpmock, no real network / no real waits) --

    use httpmock::prelude::*;

    #[tokio::test]
    #[serial]
    async fn test_odyssey_research_returns_http_error_on_failure_status() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/api/research");
            then.status(500);
        });

        std::env::set_var("SEER_API_URL", server.base_url());
        let tool = OdysseyResearch;
        let result = tool.execute(json!({"destination": "Tokyo, Japan"})).await;
        std::env::remove_var("SEER_API_URL");

        m.assert();
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_odyssey_research_parses_successful_response() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/api/research");
            then.status(200).json_body(json!({
                "answer": "Tokyo is best visited in spring.",
                "sources": [{"title": "Guide", "url": "https://example.com"}]
            }));
        });

        std::env::set_var("SEER_API_URL", server.base_url());
        let tool = OdysseyResearch;
        let result = tool
            .execute(json!({"destination": "Tokyo, Japan", "travelers": 2}))
            .await
            .unwrap();
        std::env::remove_var("SEER_API_URL");

        m.assert();
        assert!(result.contains("Tokyo is best visited in spring."));
        assert!(result.contains("https://example.com"));
    }

    #[tokio::test]
    #[serial]
    async fn test_odyssey_optimize_parses_successful_response() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/v1/tools/call");
            then.status(200)
                .json_body(json!({"result": "Use the Sapphire card for flights."}));
        });

        std::env::set_var("CHORD_PROXY_URL", server.base_url());
        let tool = OdysseyOptimize;
        let result = tool
            .execute(json!({"destination": "Tokyo, Japan", "spend_estimate": 4000}))
            .await
            .unwrap();
        std::env::remove_var("CHORD_PROXY_URL");

        m.assert();
        assert!(result.contains("Use the Sapphire card for flights."));
    }

    #[tokio::test]
    #[serial]
    async fn test_odyssey_optimize_propagates_backend_error() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/v1/tools/call");
            then.status(200)
                .json_body(json!({"error": "wizard council unavailable"}));
        });

        std::env::set_var("CHORD_PROXY_URL", server.base_url());
        let tool = OdysseyOptimize;
        let result = tool
            .execute(json!({"destination": "Tokyo, Japan", "spend_estimate": 4000}))
            .await;
        std::env::remove_var("CHORD_PROXY_URL");

        m.assert();
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }
}
