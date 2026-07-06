//! Odyssey tools — trip planning CRUD (bucket list, loyalty cards, trip log,
//! deals) backed by Engram, ported from <host>'s `odyssey_tools.py`.
//!
//! ## Backend assumption — FLAG FOR HUMAN AUDIT
//! The live <host> implementation does **not** talk to Engram over HTTP. It
//! SSHes from <host> to a fleet host (`root@YOUR_FLEET_SERVER_IP`) and runs
//! `<path>/odyssey/odyssey.py <subcommand>`, which in turn imports
//! `<path>/engram/engram.py` and/or opens
//! `<path>/engram/engram.db` (SQLite) directly. There is no HTTP
//! wire protocol to observe or replicate — confirmed by reading the live
//! `odyssey_tools.py` off <host> and the canonical `odyssey.py` / `engram.py`
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

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ──────────────────────────────────────────────
// Shared client config
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
// Input validation helpers
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

// ──────────────────────────────────────────────
// Register all Odyssey CRUD tools
// ──────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(OdysseyBucketAdd));
    registry.register_or_replace(Box::new(OdysseyBucketList));
    registry.register_or_replace(Box::new(OdysseyUpdatePoints));
    registry.register_or_replace(Box::new(OdysseyListCards));
    registry.register_or_replace(Box::new(OdysseyLogTrip));
    registry.register_or_replace(Box::new(OdysseyDeals));
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::sync::Mutex;

    /// Serialise all tests that touch ODYSSEY_API_URL env var.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env(url: &str) {
        std::env::set_var("ODYSSEY_API_URL", url);
    }

    // ── validation helpers ──

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

    // ── config / not-configured ──

    #[tokio::test]
    async fn test_missing_api_url_returns_not_configured() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ODYSSEY_API_URL");
        let tool = OdysseyListCards;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    // ── register ──

    #[test]
    fn test_odyssey_tools_registered() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 6);
        assert!(reg.contains("odyssey_bucket_add"));
        assert!(reg.contains("odyssey_bucket_list"));
        assert!(reg.contains("odyssey_update_points"));
        assert!(reg.contains("odyssey_list_cards"));
        assert!(reg.contains("odyssey_log_trip"));
        assert!(reg.contains("odyssey_deals"));
    }
}
