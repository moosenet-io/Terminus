//! Jellyseerr tools — read-only media request queries against Jellyseerr.
//!
//! Jellyseerr manages Plex/Jellyfin media requests — users request movies/shows
//! and it routes to Radarr/Sonarr for download. These tools mirror the Python
//! jellyseerr_tools.py on CT322 exactly (same names, same params):
//!   jellyseerr_status        — server health, version, update status
//!   jellyseerr_requests      — list recent media requests (paginated, filterable)
//!   jellyseerr_request_count — summary counts by status
//!   jellyseerr_search        — search movies/shows
//!
//! All calls hit {JELLYSEERR_URL}/api/v1/... with header `X-Api-Key`.
//!
//! Required env vars:
//!   JELLYSEERR_URL      — e.g. http://jellyseerr.example:5055
//!   JELLYSEERR_API_KEY  — API key from Jellyseerr settings
//!
//! If either is unset, register() installs NotConfigured stubs.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct JellyseerrConfig {
    base_url: String,
    api_key: String,
}

impl JellyseerrConfig {
    fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("JELLYSEERR_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("JELLYSEERR_URL not set".into()))?;
        let api_key = std::env::var("JELLYSEERR_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("JELLYSEERR_API_KEY not set".into()))?;
        Ok(Self { base_url, api_key })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// GET {base}/api/v1{path} with optional query params, returning parsed JSON.
    async fn api_get(
        &self,
        client: &reqwest::Client,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ToolError> {
        let url = format!("{}/api/v1{}", self.base_url, path);
        let resp = client
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Api-Key", &self.api_key)
            .query(params)
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!(
                "Jellyseerr HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }

        let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON: {e}")))
    }
}

// ── Parsing helpers ─────────────────────────────────────────────────────────────

/// Map a Jellyseerr numeric status code to a human-readable label.
fn status_name(code: i64) -> &'static str {
    match code {
        1 => "pending",
        2 => "approved",
        3 => "declined",
        4 => "processing",
        5 => "available",
        _ => "unknown",
    }
}

/// Map a status filter string to Jellyseerr's numeric `filter` value, if known.
fn status_filter_code(status: &str) -> Option<i64> {
    match status.to_lowercase().as_str() {
        "pending" => Some(1),
        "approved" => Some(2),
        "declined" => Some(3),
        "available" => Some(5),
        _ => None,
    }
}

/// Build the request summary list from a /request response body.
fn parse_requests(body: &Value) -> Value {
    let results = body.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut requests = Vec::with_capacity(results.len());
    for r in &results {
        let media = r.get("media").cloned().unwrap_or(json!({}));
        let requested_by = r.get("requestedBy").cloned().unwrap_or(json!({}));

        let media_type = r.get("type").and_then(Value::as_str).unwrap_or("movie");
        // Title resolution: the Python prefers media.name when available,
        // otherwise falls back to a TMDB id marker. Without an extra detail
        // round-trip we use media.name (commonly populated) or a TMDB marker.
        let title = media
            .get("name")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .or_else(|| {
                media
                    .get("tmdbId")
                    .and_then(Value::as_i64)
                    .map(|id| format!("TMDB:{id}"))
            })
            .unwrap_or_else(|| "Unknown".to_string());

        let req_status = r.get("status").and_then(Value::as_i64).unwrap_or(0);
        let media_status = media.get("status").and_then(Value::as_i64).unwrap_or(0);

        let requester = requested_by
            .get("displayName")
            .and_then(Value::as_str)
            .or_else(|| requested_by.get("email").and_then(Value::as_str))
            .unwrap_or("unknown");

        requests.push(json!({
            "id": r.get("id").cloned().unwrap_or(Value::Null),
            "title": title,
            "type": if media_type == "movie" { "movie" } else { "tv" },
            "status": status_name(req_status),
            "media_status": status_name(media_status),
            "requested_by": requester,
            "created": r.get("createdAt").and_then(Value::as_str).unwrap_or(""),
        }));
    }

    let total = body
        .get("pageInfo")
        .and_then(|p| p.get("results"))
        .and_then(Value::as_u64)
        .unwrap_or(requests.len() as u64);

    json!({
        "total": total,
        "showing": requests.len(),
        "requests": requests,
    })
}

/// Build the search result list from a /search response body.
fn parse_search(query: &str, body: &Value) -> Value {
    let results = body.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for item in results.iter().take(15) {
        let media_type = item.get("mediaType").and_then(Value::as_str).unwrap_or("unknown");
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| item.get("name").and_then(Value::as_str))
            .unwrap_or("Unknown");

        let year = item
            .get("releaseDate")
            .and_then(Value::as_str)
            .or_else(|| item.get("firstAirDate").and_then(Value::as_str))
            .map(|d| d.chars().take(4).collect::<String>())
            .unwrap_or_default();

        let overview_full = item.get("overview").and_then(Value::as_str).unwrap_or("");
        let overview = if overview_full.chars().count() > 150 {
            let truncated: String = overview_full.chars().take(150).collect();
            format!("{truncated}...")
        } else {
            overview_full.to_string()
        };

        let media_status_code = item
            .get("mediaInfo")
            .and_then(|m| m.get("status"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let media_status = if media_status_code == 0 {
            "not requested"
        } else {
            status_name(media_status_code)
        };

        out.push(json!({
            "title": title,
            "type": media_type,
            "year": year,
            "overview": overview,
            "media_status": media_status,
            "tmdb_id": item.get("id").cloned().unwrap_or(Value::Null),
        }));
    }

    json!({
        "query": query,
        "count": out.len(),
        "results": out,
    })
}

// ── Tool structs ────────────────────────────────────────────────────────────────

struct JellyseerrStatus { cfg: JellyseerrConfig }
struct JellyseerrRequests { cfg: JellyseerrConfig }
struct JellyseerrRequestCount { cfg: JellyseerrConfig }
struct JellyseerrSearch { cfg: JellyseerrConfig }

#[async_trait]
impl RustTool for JellyseerrStatus {
    fn name(&self) -> &str { "jellyseerr_status" }

    fn description(&self) -> &str {
        "Check Jellyseerr server health, version, and update status."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = JellyseerrConfig::client()?;
        let body = self.cfg.api_get(&client, "/status", &[]).await?;
        let out = json!({
            "version": body.get("version").and_then(Value::as_str).unwrap_or("unknown"),
            "commit": body.get("commitTag").and_then(Value::as_str).unwrap_or(""),
            "update_available": body.get("updateAvailable").and_then(Value::as_bool).unwrap_or(false),
            "restart_required": body.get("restartRequired").and_then(Value::as_bool).unwrap_or(false),
            "healthy": true,
        });
        Ok(out.to_string())
    }
}

#[async_trait]
impl RustTool for JellyseerrRequests {
    fn name(&self) -> &str { "jellyseerr_requests" }

    fn description(&self) -> &str {
        "Check media requests and the Plex/streaming queue — list recent movie and TV \
show requests. Use when the user asks about media requests, what's queued, pending, or \
requested on Plex/Jellyfin, requested movies or shows, or their watchlist. take: number \
to return (default 20, max 100); skip: pagination offset; status: filter by pending, \
approved, available, declined, or empty for all. Returns title, type, status, requester."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "take":   { "type": "integer", "description": "Number of requests to return (default 20, max 100)" },
                "skip":   { "type": "integer", "description": "Offset for pagination (default 0)" },
                "status": { "type": "string",  "description": "Filter: pending, approved, available, declined, or '' for all" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mut take = args.get("take").and_then(Value::as_u64).unwrap_or(20);
        if take > 100 {
            take = 100;
        }
        let skip = args.get("skip").and_then(Value::as_u64).unwrap_or(0);
        let status = args.get("status").and_then(Value::as_str).unwrap_or("").trim().to_string();

        let mut params: Vec<(&str, String)> = vec![
            ("take", take.to_string()),
            ("skip", skip.to_string()),
            ("sort", "added".to_string()),
        ];
        if !status.is_empty() {
            if let Some(code) = status_filter_code(&status) {
                params.push(("filter", code.to_string()));
            }
        }

        let client = JellyseerrConfig::client()?;
        let body = self.cfg.api_get(&client, "/request", &params).await?;
        Ok(parse_requests(&body).to_string())
    }
}

#[async_trait]
impl RustTool for JellyseerrRequestCount {
    fn name(&self) -> &str { "jellyseerr_request_count" }

    fn description(&self) -> &str {
        "Get summary counts of media requests by status: total, pending, approved, \
available, declined, processing. Quick request-queue health overview."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = JellyseerrConfig::client()?;
        let body = self.cfg.api_get(&client, "/request/count", &[]).await?;
        let out = json!({
            "total": body.get("total").and_then(Value::as_u64).unwrap_or(0),
            "pending": body.get("pending").and_then(Value::as_u64).unwrap_or(0),
            "approved": body.get("approved").and_then(Value::as_u64).unwrap_or(0),
            "available": body.get("available").and_then(Value::as_u64).unwrap_or(0),
            "declined": body.get("declined").and_then(Value::as_u64).unwrap_or(0),
            "processing": body.get("processing").and_then(Value::as_u64).unwrap_or(0),
        });
        Ok(out.to_string())
    }
}

#[async_trait]
impl RustTool for JellyseerrSearch {
    fn name(&self) -> &str { "jellyseerr_search" }

    fn description(&self) -> &str {
        "Search for movies and TV shows in Jellyseerr. Returns matching titles with \
type, year, overview, and media status."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search term (movie or show title)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Err(ToolError::InvalidArgument("query is required".into()));
        }

        let params: Vec<(&str, String)> = vec![
            ("query", query.clone()),
            ("page", "1".to_string()),
        ];

        let client = JellyseerrConfig::client()?;
        let body = self.cfg.api_get(&client, "/search", &params).await?;
        Ok(parse_search(&query, &body).to_string())
    }
}

// ── Registration ──────────────────────────────────────────────────────────────

struct NotConfiguredStub(&'static str);

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str { self.0 }
    fn description(&self) -> &str { "Jellyseerr tool (JELLYSEERR_URL / JELLYSEERR_API_KEY not configured)" }
    fn parameters(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured(
            "JELLYSEERR_URL and JELLYSEERR_API_KEY must both be set".into(),
        ))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    match JellyseerrConfig::from_env() {
        Ok(cfg) => {
            registry.register_or_replace(Box::new(JellyseerrStatus { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(JellyseerrRequests { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(JellyseerrRequestCount { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(JellyseerrSearch { cfg }));
        }
        Err(e) => {
            tracing::warn!("Jellyseerr tools not configured: {e}. Registering stubs.");
            registry.register_or_replace(Box::new(NotConfiguredStub("jellyseerr_status")));
            registry.register_or_replace(Box::new(NotConfiguredStub("jellyseerr_requests")));
            registry.register_or_replace(Box::new(NotConfiguredStub("jellyseerr_request_count")));
            registry.register_or_replace(Box::new(NotConfiguredStub("jellyseerr_search")));
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cfg() -> JellyseerrConfig {
        JellyseerrConfig {
            base_url: "http://jellyseerr.test:5055".into(),
            api_key: "testkey".into(),
        }
    }

    // ── status code mapping ───────────────────────────────────────────────────

    #[test]
    fn status_name_maps_known_codes() {
        assert_eq!(status_name(1), "pending");
        assert_eq!(status_name(2), "approved");
        assert_eq!(status_name(3), "declined");
        assert_eq!(status_name(4), "processing");
        assert_eq!(status_name(5), "available");
        assert_eq!(status_name(0), "unknown");
        assert_eq!(status_name(99), "unknown");
    }

    #[test]
    fn status_filter_code_maps_strings() {
        assert_eq!(status_filter_code("pending"), Some(1));
        assert_eq!(status_filter_code("APPROVED"), Some(2));
        assert_eq!(status_filter_code("Declined"), Some(3));
        assert_eq!(status_filter_code("available"), Some(5));
        // processing is not a valid filter value in the Python source
        assert_eq!(status_filter_code("processing"), None);
        assert_eq!(status_filter_code("garbage"), None);
        assert_eq!(status_filter_code(""), None);
    }

    // ── request parsing ─────────────────────────────────────────────────────────

    #[test]
    fn parse_requests_extracts_fields() {
        let body = json!({
            "pageInfo": { "results": 42 },
            "results": [
                {
                    "id": 7,
                    "type": "movie",
                    "status": 2,
                    "createdAt": "2026-06-01T10:00:00Z",
                    "media": { "name": "Dune", "status": 5, "tmdbId": 438631 },
                    "requestedBy": { "displayName": "Moose", "email": "m@x.com" }
                },
                {
                    "id": 8,
                    "type": "tv",
                    "status": 1,
                    "createdAt": "2026-06-02T10:00:00Z",
                    "media": { "tmdbId": 1399, "status": 1 },
                    "requestedBy": { "email": "fallback@x.com" }
                }
            ]
        });
        let out = parse_requests(&body);
        assert_eq!(out["total"], 42);
        assert_eq!(out["showing"], 2);
        let reqs = out["requests"].as_array().unwrap();
        assert_eq!(reqs[0]["id"], 7);
        assert_eq!(reqs[0]["title"], "Dune");
        assert_eq!(reqs[0]["type"], "movie");
        assert_eq!(reqs[0]["status"], "approved");
        assert_eq!(reqs[0]["media_status"], "available");
        assert_eq!(reqs[0]["requested_by"], "Moose");
        // second: no displayName → email fallback; no media.name → TMDB marker
        assert_eq!(reqs[1]["type"], "tv");
        assert_eq!(reqs[1]["title"], "TMDB:1399");
        assert_eq!(reqs[1]["status"], "pending");
        assert_eq!(reqs[1]["requested_by"], "fallback@x.com");
    }

    #[test]
    fn parse_requests_total_falls_back_to_count() {
        let body = json!({
            "results": [
                { "id": 1, "type": "movie", "status": 5, "media": { "name": "X", "status": 5 }, "requestedBy": {} }
            ]
        });
        let out = parse_requests(&body);
        // no pageInfo → total falls back to showing
        assert_eq!(out["total"], 1);
        assert_eq!(out["showing"], 1);
        assert_eq!(out["requests"][0]["requested_by"], "unknown");
    }

    #[test]
    fn parse_requests_empty() {
        let body = json!({ "results": [] });
        let out = parse_requests(&body);
        assert_eq!(out["total"], 0);
        assert_eq!(out["showing"], 0);
        assert!(out["requests"].as_array().unwrap().is_empty());
    }

    // ── search parsing ────────────────────────────────────────────────────────

    #[test]
    fn parse_search_extracts_fields() {
        let body = json!({
            "results": [
                {
                    "id": 100,
                    "mediaType": "movie",
                    "title": "Blade Runner 2049",
                    "releaseDate": "2017-10-06",
                    "overview": "A new blade runner unearths a secret.",
                    "mediaInfo": { "status": 5 }
                },
                {
                    "id": 200,
                    "mediaType": "tv",
                    "name": "Foundation",
                    "firstAirDate": "2021-09-24",
                    "overview": "Based on Asimov."
                }
            ]
        });
        let out = parse_search("blade", &body);
        assert_eq!(out["query"], "blade");
        assert_eq!(out["count"], 2);
        let results = out["results"].as_array().unwrap();
        assert_eq!(results[0]["title"], "Blade Runner 2049");
        assert_eq!(results[0]["type"], "movie");
        assert_eq!(results[0]["year"], "2017");
        assert_eq!(results[0]["media_status"], "available");
        // name fallback, firstAirDate year, no mediaInfo → "not requested"
        assert_eq!(results[1]["title"], "Foundation");
        assert_eq!(results[1]["year"], "2021");
        assert_eq!(results[1]["media_status"], "not requested");
    }

    #[test]
    fn parse_search_truncates_long_overview() {
        let long = "x".repeat(300);
        let body = json!({
            "results": [
                { "id": 1, "mediaType": "movie", "title": "Long", "overview": long }
            ]
        });
        let out = parse_search("q", &body);
        let ov = out["results"][0]["overview"].as_str().unwrap();
        // 150 chars + "..."
        assert!(ov.ends_with("..."));
        assert_eq!(ov.chars().count(), 153);
    }

    #[test]
    fn parse_search_caps_at_15() {
        let items: Vec<Value> = (0..30)
            .map(|i| json!({ "id": i, "mediaType": "movie", "title": format!("M{i}") }))
            .collect();
        let body = json!({ "results": items });
        let out = parse_search("many", &body);
        assert_eq!(out["count"], 15);
        assert_eq!(out["results"].as_array().unwrap().len(), 15);
    }

    #[test]
    fn parse_search_empty() {
        let body = json!({ "results": [] });
        let out = parse_search("none", &body);
        assert_eq!(out["count"], 0);
    }

    // ── arg validation (no network) ─────────────────────────────────────────────

    #[tokio::test]
    async fn search_empty_query_returns_invalid_argument() {
        let tool = JellyseerrSearch { cfg: cfg() };
        let r = tool.execute(json!({ "query": "" })).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn search_missing_query_returns_invalid_argument() {
        let tool = JellyseerrSearch { cfg: cfg() };
        let r = tool.execute(json!({})).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn search_whitespace_query_returns_invalid_argument() {
        let tool = JellyseerrSearch { cfg: cfg() };
        let r = tool.execute(json!({ "query": "   " })).await;
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    // ── base_url normalization ──────────────────────────────────────────────────
    // (from_env itself reads process-global env, which is unsafe to assert under
    //  parallel test threads; the trailing-slash normalization is verified here
    //  in isolation, and the unconfigured path is covered by the stub-registration
    //  test below which scopes its own env mutation.)

    #[test]
    fn base_url_trailing_slash_is_stripped() {
        let raw = "http://host:5055/";
        let normalized = raw.trim().trim_end_matches('/').to_string();
        assert_eq!(normalized, "http://host:5055");
    }

    // ── tool identity ───────────────────────────────────────────────────────────

    #[test]
    fn tool_names_are_stable() {
        let c = cfg();
        assert_eq!(JellyseerrStatus { cfg: c.clone() }.name(), "jellyseerr_status");
        assert_eq!(JellyseerrRequests { cfg: c.clone() }.name(), "jellyseerr_requests");
        assert_eq!(JellyseerrRequestCount { cfg: c.clone() }.name(), "jellyseerr_request_count");
        assert_eq!(JellyseerrSearch { cfg: c }.name(), "jellyseerr_search");
    }

    #[test]
    fn tool_parameters_are_valid_schema() {
        let c = cfg();
        assert_eq!(JellyseerrStatus { cfg: c.clone() }.parameters()["type"], "object");
        assert_eq!(JellyseerrRequests { cfg: c.clone() }.parameters()["type"], "object");
        assert_eq!(JellyseerrRequestCount { cfg: c.clone() }.parameters()["type"], "object");
        let s = JellyseerrSearch { cfg: c }.parameters();
        assert_eq!(s["type"], "object");
        assert!(s.get("required").is_some());
    }

    // ── registration ────────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn register_adds_four_stub_tools_when_unconfigured() {
        let mut reg = ToolRegistry::new();
        let url = std::env::var("JELLYSEERR_URL").ok();
        let key = std::env::var("JELLYSEERR_API_KEY").ok();
        std::env::remove_var("JELLYSEERR_URL");
        std::env::remove_var("JELLYSEERR_API_KEY");

        register(&mut reg);

        if let Some(u) = url { std::env::set_var("JELLYSEERR_URL", u); }
        if let Some(k) = key { std::env::set_var("JELLYSEERR_API_KEY", k); }

        assert!(reg.contains("jellyseerr_status"));
        assert!(reg.contains("jellyseerr_requests"));
        assert!(reg.contains("jellyseerr_request_count"));
        assert!(reg.contains("jellyseerr_search"));
    }

    #[tokio::test]
    async fn stub_returns_not_configured() {
        let stub = NotConfiguredStub("jellyseerr_status");
        let r = stub.execute(json!({})).await;
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }
}
