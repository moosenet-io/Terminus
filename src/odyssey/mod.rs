//! Odyssey tools — trip planning (bucket list, loyalty cards, trip log,
//! deals, research, optimize), ported from the source host's `odyssey_tools.py`.
//!
//! This module carries two families of tools:
//! - **CRUD** (`odyssey_bucket_add`, `odyssey_bucket_list`,
//!   `odyssey_update_points`, `odyssey_list_cards`, `odyssey_log_trip`,
//!   `odyssey_deals`) — instant round trips against an Engram-backed REST
//!   facade.
//! - **Async/long-running** (`odyssey_research`, `odyssey_optimize`) — tools
//!   that delegate to other backend services (Seer, the Wizard council) and
//!   take longer than an instant round trip.
//!
//! ## Backend assumption (CRUD tools) — FLAG FOR HUMAN AUDIT
//! The live source-host implementation does **not** talk to Engram over HTTP. It
//! SSHes from the source host to a fleet host (`root@YOUR_FLEET_SERVER_IP`) and runs
//! `<path>/odyssey/odyssey.py <subcommand>`, which in turn imports
//! `<path>/engram/engram.py` and/or opens
//! `<path>/engram/engram.db` (SQLite) directly. There is no HTTP
//! wire protocol to observe or replicate — confirmed by reading the live
//! `odyssey_tools.py` off the source host and the canonical `odyssey.py` / `engram.py`
//! sources in this monorepo (`src/fleet/odyssey/odyssey.py`,
//! `src/engram/engram.py`). Additionally, at port time the fleet server is
//! unreachable ("no route to host") and its successor (per current topology)
//! no longer has `odyssey.py` on disk — the original backend is gone, not
//! just offline.
//!
//! Per this crate's `RustTool` contract (`src/tool.rs`), tools must use typed
//! HTTP clients or parameterized SQL — never shell/subprocess. There is also
//! no existing Engram HTTP client anywhere in this repo (checked: `grep -ri
//! engram src/` turns up only unrelated corpora/doc references). Rather than
//! inventing an unverifiable ad hoc wire format, this module follows the
//! established sibling pattern already merged for other Engram-backed
//! modules in this crate family (see `src/vitals/mod.rs`: "N tools use
//! reqwest... required env var `VITALS_API_URL`") and assumes a REST facade
//! will exist at `ODYSSEY_API_URL` with the endpoints below. **These
//! endpoint paths and payload shapes are this porting agent's design, not
//! verified against any live service — human audit should confirm/replace
//! them before this is wired to production.**
//!
//! Assumed endpoints:
//!   POST {base}/travel/bucket        — add a bucket-list destination
//!   GET  {base}/travel/bucket        — list bucket-list destinations (?status=)
//!   POST {base}/travel/points        — upsert a loyalty/card balance
//!   GET  {base}/travel/cards         — list card portfolio
//!   POST {base}/travel/trips         — log a completed trip
//!   GET  {base}/travel/deals         — list stored deals (?destination_filter=)
//!
//! ## Note on the original's SQL handling
//! The legacy `odyssey_deals` implementation built a SQL `LIKE` clause by
//! directly string-formatting `destination_filter` into a query
//! (`AND key LIKE '%{filter}%'`) with no quote-escaping — a SQL-injection
//! bug in the original. This port never constructs SQL client-side; the
//! filter is passed as an HTTP query parameter and parameterization is the
//! (assumed) backend's responsibility.
//!
//! ## Observed source-host behavior (async tools, verified 2026-07-06 via a live // pii-test-fixture
//! `tools/call` against the source host's MCP endpoint)
//!
//! `odyssey_research` and `odyssey_optimize` are **synchronous** on the source host: a
//! single `tools/call` request blocks for the full duration of the
//! underlying work and returns one result — there is no separate
//! "submit"/"poll"/"cancel" tool triple for Odyssey (contrast with
//! `axon_submit` / `axon_status` / `axon_cancel`, which the source host *does* expose
//! for genuinely async work). A live call to each tool returned promptly
//! with a JSON body shaped `{"status": "failed"|"success", "output": ...,
//! "error": ...}`, which is the source host's own SSH-to-fleet-host wrapper shape (it
//! shells out to the agent fleet host) — plumbing this Rust port does not
//! replicate, since `RustTool::execute` must never use shell/subprocess
//! calls (see `crate::tool`) and this repo already has a native HTTP-based
//! Seer client (`crate::seer`) and Wizard client (`crate::wizard`) that
//! reach the same underlying capability. This port reuses those clients'
//! patterns (their env var names, sanitization approach, and `reqwest` +
//! timeout shape) rather than inventing a new async job/handle abstraction
//! that neither the source host nor this repo's existing seer/wizard modules actually
//! use.
//!
//! Both async tools are therefore implemented the same way: bounded
//! synchronous HTTP calls (`tokio::time::timeout` wrapping a `reqwest` call
//! with its own client-side timeout) so a hung or slow backend cannot hang
//! the calling MCP server indefinitely.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ──────────────────────────────────────────────
// Shared client config (CRUD tools)
// ──────────────────────────────────────────────

#[derive(Clone)]
struct OdysseyConfig {
    base_url: String,
}

impl OdysseyConfig {
    fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("ODYSSEY_API_URL").map_err(|_| {
            ToolError::NotConfigured("ODYSSEY_API_URL not set".into())
        })?;
        Ok(Self { base_url })
    }

    fn client(&self) -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }
}

// ──────────────────────────────────────────────
// Input validation helpers (CRUD tools)
// ──────────────────────────────────────────────

const MAX_SHORT: usize = 200;
const MAX_LONG: usize = 500;

fn sanitize_len(s: &str, field: &str, max_len: usize) -> Result<String, ToolError> {
    let trimmed = s.trim();
    if trimmed.len() > max_len {
        return Err(ToolError::InvalidArgument(format!(
            "{field} exceeds {max_len} character limit"
        )));
    }
    Ok(trimmed.to_string())
}

fn required_str(args: &Value, field: &str, max_len: usize) -> Result<String, ToolError> {
    let raw = args[field]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArgument(format!("{field} is required")))?;
    let s = sanitize_len(raw, field, max_len)?;
    if s.is_empty() {
        return Err(ToolError::InvalidArgument(format!("{field} must not be empty")));
    }
    Ok(s)
}

fn optional_str(args: &Value, field: &str, max_len: usize) -> Result<String, ToolError> {
    match args[field].as_str() {
        Some(raw) => sanitize_len(raw, field, max_len),
        None => Ok(String::new()),
    }
}

fn parse_non_negative_f64(v: &Value, field: &str) -> Result<f64, ToolError> {
    if v.is_null() {
        return Ok(0.0);
    }
    let n = v
        .as_f64()
        .ok_or_else(|| ToolError::InvalidArgument(format!("{field} must be a number")))?;
    if !n.is_finite() || n < 0.0 {
        return Err(ToolError::InvalidArgument(format!(
            "{field} must be a non-negative finite number"
        )));
    }
    Ok(n)
}

const VALID_PRIORITIES: [&str; 4] = ["urgent", "high", "medium", "low"];
const VALID_BUCKET_STATUSES: [&str; 5] =
    ["dream", "researched", "planned", "booked", "completed"];
const VALID_CARD_TYPES: [&str; 4] = ["credit", "airline", "hotel", "misc"];
const MAX_BENEFITS: usize = 20;

/// Split a comma-separated benefits string into a validated, capped list.
fn parse_benefits(raw: &str) -> Result<Vec<String>, ToolError> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > MAX_SHORT {
            return Err(ToolError::InvalidArgument(format!(
                "benefits entries must not exceed {MAX_SHORT} characters"
            )));
        }
        out.push(trimmed.to_string());
        if out.len() > MAX_BENEFITS {
            return Err(ToolError::InvalidArgument(format!(
                "benefits must not exceed {MAX_BENEFITS} entries"
            )));
        }
    }
    Ok(out)
}

fn http_err(url: &str, e: reqwest::Error) -> ToolError {
    warn!("odyssey: request to {url} failed: {e}");
    ToolError::Http("The odyssey/travel service is unreachable.".into())
}

fn status_err(status: reqwest::StatusCode) -> ToolError {
    ToolError::Http(format!("Odyssey service returned {status}"))
}

// ---------------------------------------------------------------------------
// Sanitization (async tools; mirrors crate::seer::sanitize_query /
// crate::wizard's sanitize_question — each module keeps its own copy by repo
// convention rather than sharing a helper across modules)
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
// Env helpers (async tools; each module keeps its own accessor by repo
// convention; see crate::seer::seer_api_url and crate::wizard's
// chord_proxy_url)
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
// Request / response types (async tools)
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
// Timeouts (async tools)
// ---------------------------------------------------------------------------

/// The source host's docstring says "Takes 2-5 minutes"; bound generously but finitely
/// so a wedged Seer backend cannot hang the calling MCP server forever.
const RESEARCH_TIMEOUT_SECS: u64 = 300;

/// The source host's docstring says "Takes up to 60 seconds"; give some headroom over
/// that for a slower-than-usual Wizard council round, still bounded.
const OPTIMIZE_TIMEOUT_SECS: u64 = 90;

// ──────────────────────────────────────────────
// Tool: odyssey_bucket_add
// ──────────────────────────────────────────────

pub struct OdysseyBucketAdd;

#[async_trait]
impl RustTool for OdysseyBucketAdd {
    fn name(&self) -> &str {
        "odyssey_bucket_add"
    }

    fn description(&self) -> &str {
        "Add a destination to the operator's travel bucket list. Stores in Engram, \
         regenerates HTML at /travel/bucket-list/."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "destination": { "type": "string", "description": "Full destination name, e.g. 'Kyoto, Japan'" },
                "priority":    { "type": "string", "description": "'urgent', 'high', 'medium', or 'low'", "default": "medium", "enum": VALID_PRIORITIES },
                "season":      { "type": "string", "description": "Best travel window, e.g. 'spring', 'Oct-Nov'", "default": "" },
                "budget":      { "type": "number", "description": "Estimated trip budget in USD (0 = unknown)", "default": 0 },
                "notes":       { "type": "string", "description": "Any notes, e.g. 'cherry blossom season'", "default": "" }
            },
            "required": ["destination"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let destination = required_str(&args, "destination", MAX_SHORT)?;
        let priority = if args["priority"].is_string() {
            let p = optional_str(&args, "priority", 20)?;
            if p.is_empty() {
                "medium".to_string()
            } else {
                p
            }
        } else {
            "medium".to_string()
        };
        if !VALID_PRIORITIES.contains(&priority.as_str()) {
            return Err(ToolError::InvalidArgument(format!(
                "priority must be one of: {}",
                VALID_PRIORITIES.join(", ")
            )));
        }
        let season = optional_str(&args, "season", MAX_SHORT)?;
        let budget = parse_non_negative_f64(&args["budget"], "budget")?;
        let notes = optional_str(&args, "notes", MAX_LONG)?;

        let cfg = OdysseyConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/travel/bucket", cfg.base_url);
        let payload = json!({
            "destination": destination,
            "priority": priority,
            "season": season,
            "budget": budget,
            "notes": notes,
        });
        let resp = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| http_err(&url, e))?;
        if !resp.status().is_success() {
            return Err(status_err(resp.status()));
        }
        Ok(format!(
            "Added to bucket list: {destination} (priority: {priority})"
        ))
    }
}

// ──────────────────────────────────────────────
// Tool: odyssey_bucket_list
// ──────────────────────────────────────────────

pub struct OdysseyBucketList;

#[async_trait]
impl RustTool for OdysseyBucketList {
    fn name(&self) -> &str {
        "odyssey_bucket_list"
    }

    fn description(&self) -> &str {
        "Get the operator's travel bucket list from Engram. Returns all destinations \
         sorted by priority."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status_filter": {
                    "type": "string",
                    "description": "Optionally filter by status — 'dream', 'researched', 'planned', 'booked', 'completed'. Leave empty for all.",
                    "default": "",
                    "enum": VALID_BUCKET_STATUSES
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let status_filter = optional_str(&args, "status_filter", 20)?;
        if !status_filter.is_empty() && !VALID_BUCKET_STATUSES.contains(&status_filter.as_str()) {
            return Err(ToolError::InvalidArgument(format!(
                "status_filter must be one of: {}",
                VALID_BUCKET_STATUSES.join(", ")
            )));
        }

        let cfg = OdysseyConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/travel/bucket", cfg.base_url);
        let mut req = client.get(&url);
        if !status_filter.is_empty() {
            req = req.query(&[("status", status_filter.as_str())]);
        }
        let resp = req.send().await.map_err(|e| http_err(&url, e))?;
        if !resp.status().is_success() {
            return Err(status_err(resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        let count = body.as_array().map(|a| a.len()).unwrap_or(0);
        let wrapped = json!({ "destinations": body, "count": count });
        Ok(serde_json::to_string_pretty(&wrapped)
            .unwrap_or_else(|_| "No destinations yet.".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: odyssey_update_points
// ──────────────────────────────────────────────

pub struct OdysseyUpdatePoints;

#[async_trait]
impl RustTool for OdysseyUpdatePoints {
    fn name(&self) -> &str {
        "odyssey_update_points"
    }

    fn description(&self) -> &str {
        "Store/update a loyalty program or credit card balance in Engram."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "program":  { "type": "string",  "description": "e.g. 'Chase Sapphire Reserve', 'Delta SkyMiles', 'Marriott Bonvoy'" },
                "balance":  { "type": "integer", "description": "Current point/mile balance (non-negative)" },
                "card_type":{ "type": "string",  "description": "'credit', 'airline', 'hotel', or 'misc'", "default": "credit", "enum": VALID_CARD_TYPES },
                "tier":     { "type": "string",  "description": "Optional elite tier, e.g. 'sapphire-reserve', 'gold', 'platinum'", "default": "" },
                "benefits": { "type": "string",  "description": "Comma-separated benefits, e.g. '3x travel,lounge access,trip delay'", "default": "" }
            },
            "required": ["program", "balance"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let program = required_str(&args, "program", MAX_SHORT)?;
        let balance = args["balance"]
            .as_i64()
            .ok_or_else(|| ToolError::InvalidArgument("balance must be an integer".into()))?;
        if balance < 0 {
            return Err(ToolError::InvalidArgument(
                "balance must be non-negative".into(),
            ));
        }
        let card_type = if args["card_type"].is_string() {
            let t = optional_str(&args, "card_type", 20)?;
            if t.is_empty() {
                "credit".to_string()
            } else {
                t
            }
        } else {
            "credit".to_string()
        };
        if !VALID_CARD_TYPES.contains(&card_type.as_str()) {
            return Err(ToolError::InvalidArgument(format!(
                "card_type must be one of: {}",
                VALID_CARD_TYPES.join(", ")
            )));
        }
        let tier = optional_str(&args, "tier", MAX_SHORT)?;
        let benefits_raw = optional_str(&args, "benefits", MAX_LONG)?;
        let benefits = parse_benefits(&benefits_raw)?;

        let cfg = OdysseyConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/travel/points", cfg.base_url);
        let payload = json!({
            "program": program,
            "balance": balance,
            "card_type": card_type,
            "tier": tier,
            "benefits": benefits,
        });
        let resp = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| http_err(&url, e))?;
        if !resp.status().is_success() {
            return Err(status_err(resp.status()));
        }
        Ok(format!("Updated {program}: {balance} points/miles"))
    }
}

// ──────────────────────────────────────────────
// Tool: odyssey_list_cards
// ──────────────────────────────────────────────

pub struct OdysseyListCards;

#[async_trait]
impl RustTool for OdysseyListCards {
    fn name(&self) -> &str {
        "odyssey_list_cards"
    }

    fn description(&self) -> &str {
        "List the operator's full card and loyalty program portfolio from Engram. \
         Returns all cards sorted by balance (highest first) with type, tier, and benefits."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let cfg = OdysseyConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/travel/cards", cfg.base_url);
        let resp = client.get(&url).send().await.map_err(|e| http_err(&url, e))?;
        if !resp.status().is_success() {
            return Err(status_err(resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        let count = body.as_array().map(|a| a.len()).unwrap_or(0);
        let wrapped = json!({ "cards": body, "count": count });
        Ok(serde_json::to_string_pretty(&wrapped)
            .unwrap_or_else(|_| "No cards stored yet.".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: odyssey_log_trip
// ──────────────────────────────────────────────

pub struct OdysseyLogTrip;

#[async_trait]
impl RustTool for OdysseyLogTrip {
    fn name(&self) -> &str {
        "odyssey_log_trip"
    }

    fn description(&self) -> &str {
        "Log a completed trip to the adventure log in Engram. Updates bucket list \
         status to 'completed'. Generates updated HTML."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "destination": { "type": "string",  "description": "e.g. 'Tokyo, Japan'" },
                "dates":       { "type": "string",  "description": "Trip dates, e.g. 'March 15-25, 2027'" },
                "highlights":  { "type": "string",  "description": "1-2 sentence summary of the trip" },
                "rating":      { "type": "integer", "description": "1-5 stars", "default": 5, "minimum": 1, "maximum": 5 },
                "cost":        { "type": "number",  "description": "Actual total trip cost in USD", "default": 0 }
            },
            "required": ["destination", "dates", "highlights"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let destination = required_str(&args, "destination", MAX_SHORT)?;
        let dates = required_str(&args, "dates", MAX_SHORT)?;
        let highlights = required_str(&args, "highlights", MAX_LONG)?;
        let rating = if args["rating"].is_number() {
            let r = args["rating"]
                .as_i64()
                .ok_or_else(|| ToolError::InvalidArgument("rating must be an integer 1-5".into()))?;
            if !(1..=5).contains(&r) {
                return Err(ToolError::InvalidArgument(
                    "rating must be between 1 and 5".into(),
                ));
            }
            r
        } else {
            5
        };
        let cost = parse_non_negative_f64(&args["cost"], "cost")?;

        let cfg = OdysseyConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/travel/trips", cfg.base_url);
        let payload = json!({
            "destination": destination,
            "dates": dates,
            "highlights": highlights,
            "rating": rating,
            "cost": cost,
        });
        let resp = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| http_err(&url, e))?;
        if !resp.status().is_success() {
            return Err(status_err(resp.status()));
        }
        Ok(format!("Trip logged: {destination} ({dates}) — {rating}/5 stars"))
    }
}

// ──────────────────────────────────────────────
// Tool: odyssey_deals
// ──────────────────────────────────────────────

pub struct OdysseyDeals;

#[async_trait]
impl RustTool for OdysseyDeals {
    fn name(&self) -> &str {
        "odyssey_deals"
    }

    fn description(&self) -> &str {
        "Search Engram for stored travel deals. Returns deal entries tagged 'deal' \
         in the Engram knowledge base."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "destination_filter": {
                    "type": "string",
                    "description": "Optional partial destination name to filter results. Leave empty for all stored deals.",
                    "default": ""
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let destination_filter = optional_str(&args, "destination_filter", MAX_SHORT)?;

        let cfg = OdysseyConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/travel/deals", cfg.base_url);
        let mut req = client.get(&url);
        if !destination_filter.is_empty() {
            req = req.query(&[("destination_filter", destination_filter.as_str())]);
        }
        let resp = req.send().await.map_err(|e| http_err(&url, e))?;
        if !resp.status().is_success() {
            return Err(status_err(resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        let count = body.as_array().map(|a| a.len()).unwrap_or(0);
        let filter_label = if destination_filter.is_empty() {
            "all".to_string()
        } else {
            destination_filter.clone()
        };
        let wrapped = json!({ "deals": body, "count": count, "filter": filter_label });
        Ok(serde_json::to_string_pretty(&wrapped)
            .unwrap_or_else(|_| "No deals stored yet.".to_string()))
    }
}

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

// ──────────────────────────────────────────────
// Register all Odyssey tools (CRUD + async/research)
// ──────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(OdysseyBucketAdd));
    registry.register_or_replace(Box::new(OdysseyBucketList));
    registry.register_or_replace(Box::new(OdysseyUpdatePoints));
    registry.register_or_replace(Box::new(OdysseyListCards));
    registry.register_or_replace(Box::new(OdysseyLogTrip));
    registry.register_or_replace(Box::new(OdysseyDeals));
    registry.register_or_replace(Box::new(OdysseyResearch));
    registry.register_or_replace(Box::new(OdysseyOptimize));
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;
    use std::sync::Mutex;

    /// Serialise all tests that touch ODYSSEY_API_URL env var.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env(url: &str) {
        std::env::set_var("ODYSSEY_API_URL", url);
    }

    // ── validation helpers (CRUD) ──

    #[test]
    fn test_required_str_rejects_empty() {
        let v = json!({"destination": "   "});
        assert!(required_str(&v, "destination", MAX_SHORT).is_err());
    }

    #[test]
    fn test_required_str_rejects_missing() {
        let v = json!({});
        assert!(required_str(&v, "destination", MAX_SHORT).is_err());
    }

    #[test]
    fn test_required_str_rejects_too_long() {
        let long = "a".repeat(MAX_SHORT + 1);
        let v = json!({"destination": long});
        assert!(required_str(&v, "destination", MAX_SHORT).is_err());
    }

    #[test]
    fn test_parse_non_negative_f64_rejects_negative() {
        assert!(parse_non_negative_f64(&json!(-1.0), "budget").is_err());
    }

    #[test]
    fn test_parse_non_negative_f64_accepts_zero_and_null() {
        assert!(parse_non_negative_f64(&json!(0.0), "budget").is_ok());
        assert!(parse_non_negative_f64(&Value::Null, "budget").is_ok());
    }

    #[test]
    fn test_parse_benefits_splits_and_trims() {
        let out = parse_benefits("3x travel, lounge access ,trip delay").unwrap();
        assert_eq!(out, vec!["3x travel", "lounge access", "trip delay"]);
    }

    #[test]
    fn test_parse_benefits_rejects_too_many() {
        let raw = (0..30).map(|i| format!("b{i}")).collect::<Vec<_>>().join(",");
        assert!(parse_benefits(&raw).is_err());
    }

    // ── odyssey_bucket_add ──

    #[tokio::test]
    async fn test_bucket_add_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(POST).path("/travel/bucket");
            then.status(200).json_body(json!({"added": "Kyoto, Japan"}));
        });

        let tool = OdysseyBucketAdd;
        let result = tool
            .execute(json!({"destination": "Kyoto, Japan", "priority": "high"}))
            .await
            .unwrap();
        assert!(result.contains("Kyoto"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_bucket_add_rejects_invalid_priority() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyBucketAdd;
        let err = tool
            .execute(json!({"destination": "Kyoto, Japan", "priority": "whenever"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_bucket_add_rejects_missing_destination() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyBucketAdd;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_bucket_add_rejects_negative_budget() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyBucketAdd;
        let err = tool
            .execute(json!({"destination": "Kyoto, Japan", "budget": -100}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_bucket_add_defaults_priority_to_medium() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/travel/bucket")
                .json_body_partial(r#"{"priority":"medium"}"#);
            then.status(200).json_body(json!({}));
        });
        let tool = OdysseyBucketAdd;
        let result = tool.execute(json!({"destination": "Lisbon"})).await.unwrap();
        assert!(result.contains("medium"));
        mock.assert();
    }

    // ── odyssey_bucket_list ──

    #[tokio::test]
    async fn test_bucket_list_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/travel/bucket")
                .query_param("status", "dream");
            then.status(200)
                .json_body(json!([{"destination": "Kyoto, Japan", "status": "dream"}]));
        });

        let tool = OdysseyBucketList;
        let result = tool
            .execute(json!({"status_filter": "dream"}))
            .await
            .unwrap();
        assert!(result.contains("Kyoto"));
        assert!(result.contains("\"count\": 1"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_bucket_list_no_filter_omits_query_param() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET).path("/travel/bucket");
            then.status(200).json_body(json!([]));
        });

        let tool = OdysseyBucketList;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("\"count\": 0"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_bucket_list_rejects_invalid_status_filter() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyBucketList;
        let err = tool
            .execute(json!({"status_filter": "not-a-status"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── odyssey_update_points ──

    #[tokio::test]
    async fn test_update_points_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(POST).path("/travel/points");
            then.status(200).json_body(json!({"updated": "Chase Sapphire Reserve"}));
        });

        let tool = OdysseyUpdatePoints;
        let result = tool
            .execute(json!({
                "program": "Chase Sapphire Reserve",
                "balance": 125000,
                "benefits": "3x travel,lounge access"
            }))
            .await
            .unwrap();
        assert!(result.contains("125000"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_update_points_rejects_negative_balance() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyUpdatePoints;
        let err = tool
            .execute(json!({"program": "Delta SkyMiles", "balance": -5}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_update_points_rejects_invalid_card_type() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyUpdatePoints;
        let err = tool
            .execute(json!({"program": "Delta SkyMiles", "balance": 100, "card_type": "cash"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_update_points_rejects_non_integer_balance() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyUpdatePoints;
        let err = tool
            .execute(json!({"program": "Delta SkyMiles", "balance": "lots"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── odyssey_list_cards ──

    #[tokio::test]
    async fn test_list_cards_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET).path("/travel/cards");
            then.status(200).json_body(json!([
                {"program": "Chase Sapphire Reserve", "balance": 125000}
            ]));
        });

        let tool = OdysseyListCards;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Chase Sapphire Reserve"));
        assert!(result.contains("\"count\": 1"));
        mock.assert();
    }

    // ── odyssey_log_trip ──

    #[tokio::test]
    async fn test_log_trip_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(POST).path("/travel/trips");
            then.status(200).json_body(json!({"logged": "Tokyo, Japan"}));
        });

        let tool = OdysseyLogTrip;
        let result = tool
            .execute(json!({
                "destination": "Tokyo, Japan",
                "dates": "March 15-25, 2027",
                "highlights": "Great trip",
                "rating": 4
            }))
            .await
            .unwrap();
        assert!(result.contains("Tokyo"));
        assert!(result.contains("4/5"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_log_trip_rejects_rating_out_of_range() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyLogTrip;
        let err = tool
            .execute(json!({
                "destination": "Tokyo, Japan",
                "dates": "March 2027",
                "highlights": "Great trip",
                "rating": 6
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_log_trip_rejects_missing_required_fields() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyLogTrip;
        let err = tool
            .execute(json!({"destination": "Tokyo, Japan"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_log_trip_rejects_negative_cost() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = OdysseyLogTrip;
        let err = tool
            .execute(json!({
                "destination": "Tokyo, Japan",
                "dates": "March 2027",
                "highlights": "Great trip",
                "cost": -1
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── odyssey_deals ──

    #[tokio::test]
    async fn test_deals_sends_correct_request_with_filter() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/travel/deals")
                .query_param("destination_filter", "tokyo");
            then.status(200)
                .json_body(json!([{"key": "travel/deals/tokyo-flight", "content": "..."}]));
        });

        let tool = OdysseyDeals;
        let result = tool
            .execute(json!({"destination_filter": "tokyo"}))
            .await
            .unwrap();
        assert!(result.contains("tokyo"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_deals_no_filter_returns_all() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET).path("/travel/deals");
            then.status(200).json_body(json!([]));
        });

        let tool = OdysseyDeals;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("\"filter\": \"all\""));
        mock.assert();
    }

    // ── config / not-configured (CRUD) ──

    #[tokio::test]
    async fn test_missing_api_url_returns_not_configured() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ODYSSEY_API_URL");
        let tool = OdysseyListCards;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    // ── odyssey_research / odyssey_optimize: sanitize_text ──

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

    // ── odyssey_research / odyssey_optimize: NotConfigured without env ──

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

    // ── odyssey_research / odyssey_optimize: input validation ──

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

    // ── odyssey_research / odyssey_optimize: parameter schema ──

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

    // ── odyssey_research / odyssey_optimize: timeout bounding (mocked via
    // httpmock, no real network / no real waits) ──

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

    // ── register: all 8 Odyssey tools (CRUD + async) ──

    #[test]
    fn test_odyssey_tools_registered() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 8);
        assert!(reg.contains("odyssey_bucket_add"));
        assert!(reg.contains("odyssey_bucket_list"));
        assert!(reg.contains("odyssey_update_points"));
        assert!(reg.contains("odyssey_list_cards"));
        assert!(reg.contains("odyssey_log_trip"));
        assert!(reg.contains("odyssey_deals"));
        assert!(reg.contains("odyssey_research"));
        assert!(reg.contains("odyssey_optimize"));
    }
}
