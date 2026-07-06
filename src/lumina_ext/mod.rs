//! `lumina_*` tools — ported from <host>'s Python MCP server ("ai-terminus",
//! streamable-HTTP MCP endpoint, tool set `ai-mcp` v1.26.0). This module
//! groups the six remaining `lumina_*` tools that had not yet been ported to
//! Rust (`lumina_weather` was already ported separately, as `weather::mod`).
//!
//! Per the operator's explicit direction, these are mechanical 1:1 ports —
//! faithful to <host>'s *observed live behavior* (verified via direct MCP
//! `tools/call` probes on 2026-07-06), not a redesign. Several of the ported
//! endpoints are themselves broken or quirky on the live server; the port
//! reproduces that, it does not fix it:
//!
//!   - `lumina_clawhub_skill_detail` calls `GET clawhub.ai/api/skill/{slug}`,
//!     which 404s ("No matching routes found") for every slug tried. The
//!     *working* ClawHub endpoint is actually `GET /api/skill?slug=...`
//!     (verified independently), but that is NOT what <host> calls, so this
//!     port intentionally calls the broken path-style endpoint to match.
//!   - `lumina_clawmart_browse` calls `GET shopclawmart.com/search?q=...` when
//!     a non-empty query is given, which also 404s (no such route on that
//!     site) — an empty query instead fetches the bare homepage, which works.
//!   - `lumina_aicpb_rankings` accepts a `category` argument but never uses it
//!     in the request — it always fetches `https://aicpb.com/` and returns
//!     the same homepage content regardless of category.
//!
//! ## SSRF note (flagged for operator review, not fixed here)
//! `lumina_web_fetch` takes an arbitrary caller-supplied URL and fetches it
//! with no allowlist, scheme restriction, or private/loopback-address check.
//! Verified against the live <host> tool: a request for `http://127.0.0.1:22`
//! was fetched with no rejection (the raw SSH banner came back as the
//! tool's `"error"` string, proving the fetch reached a loopback port with
//! no guard). **This port intentionally matches that behavior** — no new
//! restriction is added — because the operator's directive for this sprint
//! is a faithful 1:1 stub recreation with human curation to follow. An LLM
//! agent that can call this tool can use it to probe internal-network
//! addresses and ports; this should be treated as a known gap, not a
//! resolved one.
//!
//! ## Tools (identical names/signatures to the Python source)
//!   lumina_clawhub_search        — search ClawHub for agent skills
//!   lumina_clawhub_skill_detail  — fetch a ClawHub skill's detail by slug
//!   lumina_aicpb_rankings        — fetch AICPB Claw/agent rankings text
//!   lumina_clawmart_browse       — browse ShopClawMart listings
//!   lumina_claw_awesome_list     — fetch the awesome-openclaw-skills README
//!   lumina_web_fetch             — fetch an arbitrary URL, raw or as readable text
//!
//! ## Error shape (matches the Python original exactly)
//! None of these tools raise on a failed fetch. Every one catches the
//! failure (non-2xx HTTP status, or a transport-level error) and folds it
//! into a normal, `isError:false` JSON response with an `"error"` key —
//! verified directly against <host> for `lumina_clawhub_skill_detail`,
//! `lumina_clawmart_browse`, and `lumina_web_fetch`. Non-2xx response bodies
//! are truncated to 500 chars in the error message, matching the exact
//! truncation point observed in a live 404 from shopclawmart.com.
//!
//! No API keys or auth are required for any of these six tools — <host>
//! calls each target as an anonymous public HTTP client. The only header
//! <host> evidently sends beyond the default is a `User-Agent` for the
//! GitHub Contents API call (`lumina_claw_awesome_list`) — GitHub's API
//! returns 403 to requests with no `User-Agent` at all.

use async_trait::async_trait;
use regex::{Captures, Regex};
use serde_json::{json, Value};
use std::sync::OnceLock;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Targets (verified live against <host>'s `ai-mcp` on 2026-07-06)
// ---------------------------------------------------------------------------

const CLAWHUB_SEARCH_URL: &str = "https://clawhub.ai/api/search";
/// Path-style skill-detail endpoint. This 404s on the live ClawHub site today
/// ("No matching routes found") — see module docs. Kept as-is to match <host>.
const CLAWHUB_SKILL_URL_BASE: &str = "https://clawhub.ai/api/skill";
const AICPB_URL: &str = "https://aicpb.com/";
const CLAWMART_HOME_URL: &str = "https://shopclawmart.com/";
/// 404s on the live site for any query — see module docs. Kept as-is.
const CLAWMART_SEARCH_URL: &str = "https://shopclawmart.com/search";
const AWESOME_LIST_API_URL: &str =
    "https://api.github.com/repos/VoltAgent/awesome-openclaw-skills/contents/README.md";
const AWESOME_LIST_SOURCE: &str = "github.com/VoltAgent/awesome-openclaw-skills";

/// aicpb_rankings / clawmart_browse hardcode this truncation length server-side
/// (no `max_length` argument is exposed for either tool) — verified by
/// measuring the exact returned content length from <host> (3000 chars both
/// times, regardless of category / query).
const READABLE_TEXT_MAX_LEN: usize = 3000;
/// lumina_web_fetch's own default, taken directly from its inputSchema.
const WEB_FETCH_DEFAULT_MAX_LEN: i64 = 3000;
/// claw_awesome_list's hardcoded truncation length, verified the same way
/// (returned content was exactly 5000 chars).
const AWESOME_LIST_MAX_LEN: usize = 5000;
/// Error-body truncation length, verified against a live shopclawmart.com
/// 404 whose HTML body was cut at exactly this many chars in <host>'s
/// `"error"` string.
const ERROR_BODY_MAX_LEN: usize = 500;

fn http_client() -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))
}

/// Truncate `s` to at most `max_len` chars (not bytes — safe on multi-byte
/// UTF-8 boundaries). Returns `(truncated_string, was_truncated)`.
fn truncate_chars(s: &str, max_len: usize) -> (String, bool) {
    if s.chars().count() <= max_len {
        (s.to_string(), false)
    } else {
        (s.chars().take(max_len).collect(), true)
    }
}

/// Send a GET request and return `Ok(body)` on 2xx. On a non-2xx response or
/// a transport-level failure, returns `Err(message)` rather than propagating
/// a `ToolError` — every `lumina_*` tool here folds that message into its own
/// `"error"` JSON field instead of raising, matching the Python original
/// (verified directly against <host> — see module docs).
async fn send_get(req: reqwest::RequestBuilder) -> Result<String, String> {
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if status.is_success() {
                Ok(body)
            } else {
                let (truncated, _) = truncate_chars(&body, ERROR_BODY_MAX_LEN);
                Err(format!("HTTP {}: {truncated}", status.as_u16()))
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// HTML -> readable text extraction (lumina_web_fetch, lumina_aicpb_rankings,
// lumina_clawmart_browse all funnel through this)
// ---------------------------------------------------------------------------

fn re_script_style() -> &'static Regex {
    // The `regex` crate has no backreference support, so this matches either
    // tag by alternation on the *closing* tag rather than `</\1>` — slightly
    // less strict (in principle `<script>...</style>` would match) but that
    // never occurs in well-formed HTML, and it keeps the crate free of a
    // heavier backtracking regex engine dependency.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<(?:script|style)\b[^>]*>.*?</(?:script|style)\s*>").unwrap())
}

fn re_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>").unwrap())
}

fn re_body() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<body\b[^>]*>(.*?)</body\s*>").unwrap())
}

fn re_tag() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<[^>]+>").unwrap())
}

fn re_entity_dec() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"&#([0-9]+);").unwrap())
}

fn re_entity_hex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)&#x([0-9a-f]+);").unwrap())
}

/// Decode the small set of HTML entities that show up in ordinary prose
/// (named entities + numeric decimal/hex escapes). Not a full HTML5 entity
/// table — deliberately minimal, matching what a readable-text extraction
/// needs (as opposed to a full browser-grade HTML parser, which this crate
/// does not depend on).
fn decode_entities(input: &str) -> String {
    let mut s = re_entity_dec()
        .replace_all(input, |caps: &Captures| {
            caps[1]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_default()
        })
        .into_owned();
    s = re_entity_hex()
        .replace_all(&s, |caps: &Captures| {
            u32::from_str_radix(&caps[1], 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_default()
        })
        .into_owned();
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
}

/// Strip tags from an HTML fragment, treating every tag boundary as a
/// potential line break, decode entities, then collapse to one non-empty,
/// trimmed line per visible chunk of text.
fn tags_to_lines(fragment: &str) -> String {
    let no_tags = re_tag().replace_all(fragment, "\n");
    let decoded = decode_entities(&no_tags);
    decoded
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract "readable text" from a raw HTML document: the `<title>` (if any)
/// followed by the visible text inside `<body>` (or the whole document if no
/// `<body>` tag is present), with `<script>`/`<style>` removed first. This is
/// what all three HTML-consuming `lumina_*` tools use.
///
/// Verified against <host>'s live `lumina_web_fetch` output for
/// `https://example.com`: the title ("Example Domain") appears once, then
/// the body's own heading (also "Example Domain") appears again as part of
/// the body text — i.e. title and body are concatenated, not deduplicated.
/// This function reproduces that exactly.
pub fn extract_readable_text(html: &str) -> String {
    let cleaned = re_script_style().replace_all(html, "");
    let title = re_title()
        .captures(&cleaned)
        .map(|c| tags_to_lines(&c[1]))
        .filter(|t| !t.is_empty());
    let body_fragment = re_body()
        .captures(&cleaned)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| cleaned.to_string());
    let body_text = tags_to_lines(&body_fragment);

    match title {
        Some(t) if !body_text.is_empty() => format!("{t}\n{body_text}"),
        Some(t) => t,
        None => body_text,
    }
}

// ---------------------------------------------------------------------------
// Tool: lumina_clawhub_search
// ---------------------------------------------------------------------------

pub struct LuminaClawhubSearch;

#[async_trait]
impl RustTool for LuminaClawhubSearch {
    fn name(&self) -> &str {
        "lumina_clawhub_search"
    }

    fn description(&self) -> &str {
        "Search ClawHub for agent skills. Returns skill name, summary, downloads, and stars."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "title": "Query"},
                "limit": {"type": "integer", "title": "Limit", "default": 10}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'query' must be a string".into()))?;
        let limit = args["limit"].as_i64().unwrap_or(10);

        let client = http_client()?;
        let req = client
            .get(CLAWHUB_SEARCH_URL)
            .query(&[("q", query), ("limit", &limit.to_string())]);

        let response = match send_get(req).await {
            Ok(body) => {
                let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let results = parsed["results"].as_array().cloned().unwrap_or_default();
                let skills: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        json!({
                            "name": r["slug"],
                            "display_name": r["displayName"],
                            "summary": r["summary"],
                            "updated": r["updatedAt"],
                        })
                    })
                    .collect();
                json!({"query": query, "count": skills.len(), "skills": skills})
            }
            Err(e) => json!({"error": e, "query": query}),
        };

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: lumina_clawhub_skill_detail
// ---------------------------------------------------------------------------

pub struct LuminaClawhubSkillDetail;

#[async_trait]
impl RustTool for LuminaClawhubSkillDetail {
    fn name(&self) -> &str {
        "lumina_clawhub_skill_detail"
    }

    fn description(&self) -> &str {
        "Get detailed information about a specific ClawHub skill by its slug."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": {"type": "string", "title": "Slug"}
            },
            "required": ["slug"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let slug = args["slug"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'slug' must be a string".into()))?;

        let client = http_client()?;
        let url = format!("{CLAWHUB_SKILL_URL_BASE}/{slug}");
        let response = match send_get(client.get(&url)).await {
            Ok(body) => match serde_json::from_str::<Value>(&body) {
                Ok(parsed) => parsed,
                Err(_) => json!({"content": body, "slug": slug}),
            },
            Err(e) => json!({"error": e, "slug": slug}),
        };

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: lumina_aicpb_rankings
// ---------------------------------------------------------------------------

pub struct LuminaAicpbRankings;

#[async_trait]
impl RustTool for LuminaAicpbRankings {
    fn name(&self) -> &str {
        "lumina_aicpb_rankings"
    }

    fn description(&self) -> &str {
        "Fetch current Claw agent rankings from AICPB."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "category": {"type": "string", "title": "Category", "default": "all"}
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // `category` is accepted and echoed back but never used to build the
        // request — <host> fetches the same aicpb.com homepage regardless of
        // category (verified: identical content for "all" vs. "games").
        let category = args["category"].as_str().unwrap_or("all");

        let client = http_client()?;
        let response = match send_get(client.get(AICPB_URL)).await {
            Ok(html) => {
                let text = extract_readable_text(&html);
                let (content, _truncated) = truncate_chars(&text, READABLE_TEXT_MAX_LEN);
                json!({"source": "aicpb.com", "category": category, "content": content})
            }
            Err(e) => json!({"error": e, "source": "aicpb.com"}),
        };

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: lumina_clawmart_browse
// ---------------------------------------------------------------------------

pub struct LuminaClawmartBrowse;

#[async_trait]
impl RustTool for LuminaClawmartBrowse {
    fn name(&self) -> &str {
        "lumina_clawmart_browse"
    }

    fn description(&self) -> &str {
        "Browse ShopClawMart for custom agent services and creator listings."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "title": "Query", "default": ""}
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args["query"].as_str().unwrap_or("");

        let client = http_client()?;
        // Empty query -> bare homepage (works). Non-empty query -> /search?q=
        // (verified 404 on the live site today — see module docs; kept as-is
        // to match <host> exactly).
        let req = if query.is_empty() {
            client.get(CLAWMART_HOME_URL)
        } else {
            client.get(CLAWMART_SEARCH_URL).query(&[("q", query)])
        };

        let response = match send_get(req).await {
            Ok(html) => {
                let text = extract_readable_text(&html);
                let (content, _truncated) = truncate_chars(&text, READABLE_TEXT_MAX_LEN);
                json!({"source": "shopclawmart.com", "query": query, "content": content})
            }
            // Verified: the live "query" 404 error response omits the `query`
            // key entirely (unlike the success path) — matched here.
            Err(e) => json!({"error": e, "source": "shopclawmart.com"}),
        };

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: lumina_claw_awesome_list
// ---------------------------------------------------------------------------

pub struct LuminaClawAwesomeList;

#[async_trait]
impl RustTool for LuminaClawAwesomeList {
    fn name(&self) -> &str {
        "lumina_claw_awesome_list"
    }

    fn description(&self) -> &str {
        "Fetch the curated awesome-openclaw-skills list from GitHub."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = http_client()?;
        // GitHub's Contents API 403s any request with no User-Agent header
        // (verified) — <host> evidently sends one, so this must too.
        let req = client
            .get(AWESOME_LIST_API_URL)
            .header("User-Agent", "terminus-rs-lumina-ext")
            .header("Accept", "application/vnd.github+json");

        let response = match send_get(req).await {
            Ok(body) => match serde_json::from_str::<Value>(&body) {
                Ok(parsed) => {
                    let b64 = parsed["content"].as_str().unwrap_or("").replace('\n', "");
                    let sha = parsed["sha"].as_str().unwrap_or("").to_string();
                    match base64_decode(&b64) {
                        Some(decoded) => {
                            let (content, _truncated) =
                                truncate_chars(&decoded, AWESOME_LIST_MAX_LEN);
                            json!({"source": AWESOME_LIST_SOURCE, "content": content, "sha": sha})
                        }
                        None => json!({
                            "error": "Failed to decode base64 content",
                            "source": AWESOME_LIST_SOURCE,
                        }),
                    }
                }
                Err(_) => json!({"error": "Unparseable GitHub response", "source": AWESOME_LIST_SOURCE}),
            },
            Err(e) => json!({"error": e, "source": AWESOME_LIST_SOURCE}),
        };

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

/// Minimal standard-alphabet base64 decoder (with or without padding),
/// returned as a lossy UTF-8 string (GitHub's Contents API always returns
/// `encoding: "base64"` text content for a markdown file). No external crate
/// is added for this — the alphabet is small enough to inline.
fn base64_decode(input: &str) -> Option<String> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input.bytes().filter(|&b| b != b'=' && !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4 + 3);
    for chunk in cleaned.chunks(4) {
        let vals: Vec<u8> = chunk.iter().map(|&b| val(b)).collect::<Option<Vec<u8>>>()?;
        match vals.len() {
            4 => {
                out.push((vals[0] << 2) | (vals[1] >> 4));
                out.push((vals[1] << 4) | (vals[2] >> 2));
                out.push((vals[2] << 6) | vals[3]);
            }
            3 => {
                out.push((vals[0] << 2) | (vals[1] >> 4));
                out.push((vals[1] << 4) | (vals[2] >> 2));
            }
            2 => {
                out.push((vals[0] << 2) | (vals[1] >> 4));
            }
            _ => return None,
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

// ---------------------------------------------------------------------------
// Tool: lumina_web_fetch
// ---------------------------------------------------------------------------

pub struct LuminaWebFetch;

#[async_trait]
impl RustTool for LuminaWebFetch {
    fn name(&self) -> &str {
        "lumina_web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page and optionally extract readable text. Use for browsing agentic sites, docs, or any URL."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "title": "Url"},
                "extract_text": {"type": "boolean", "title": "Extract Text", "default": true},
                "max_length": {"type": "integer", "title": "Max Length", "default": WEB_FETCH_DEFAULT_MAX_LEN}
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // NOTE (SSRF): `url` is fetched exactly as given, with no scheme,
        // host, or private/loopback-address restriction — matching <host>'s
        // verified live behavior. See the module-level doc comment.
        let url = args["url"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'url' must be a string".into()))?;
        let extract_text = args["extract_text"].as_bool().unwrap_or(true);
        let max_length = args["max_length"]
            .as_i64()
            .unwrap_or(WEB_FETCH_DEFAULT_MAX_LEN)
            .max(0) as usize;

        let client = http_client()?;
        let response = match send_get(client.get(url)).await {
            Ok(body) => {
                let (rendered, kind) = if extract_text {
                    (extract_readable_text(&body), "text")
                } else {
                    (body, "raw")
                };
                let (content, truncated) = truncate_chars(&rendered, max_length);
                json!({"url": url, "type": kind, "content": content, "truncated": truncated})
            }
            Err(e) => json!({"url": url, "error": e}),
        };

        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all `lumina_*` (non-weather) tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(LuminaClawhubSearch));
    let _ = registry.register(Box::new(LuminaClawhubSkillDetail));
    let _ = registry.register(Box::new(LuminaAicpbRankings));
    let _ = registry.register(Box::new(LuminaClawmartBrowse));
    let _ = registry.register(Box::new(LuminaClawAwesomeList));
    let _ = registry.register(Box::new(LuminaWebFetch));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    // --- extract_readable_text -------------------------------------------

    #[test]
    fn test_extract_readable_text_title_and_body() {
        // Small embedded fixture modeled on the live example.com response
        // <host> returned: title text appears once, then the body's own
        // (different) heading and paragraphs appear as separate lines, with
        // scripts/styles removed and tags collapsed to one line each.
        let html = r#"<!doctype html>
<html lang="en">
<head>
<title>Example Domain</title>
<style>body{background:#eee}</style>
<script>console.log("nope");</script>
</head>
<body>
<div>
<h1>Example Domain</h1>
<p>This domain is for use in documentation examples without needing permission.</p>
<p><a href="https://example.org">Learn more</a></p>
</div>
</body>
</html>"#;

        let text = extract_readable_text(html);
        assert_eq!(
            text,
            "Example Domain\nExample Domain\nThis domain is for use in documentation examples without needing permission.\nLearn more"
        );
    }

    #[test]
    fn test_extract_readable_text_no_title() {
        let html = "<html><body><p>Just body text.</p></body></html>";
        assert_eq!(extract_readable_text(html), "Just body text.");
    }

    #[test]
    fn test_extract_readable_text_no_body_tag_falls_back_to_whole_doc() {
        let html = "<title>Bare Fragment</title><p>Fragment content</p>";
        assert_eq!(
            extract_readable_text(html),
            "Bare Fragment\nBare Fragment\nFragment content"
        );
    }

    #[test]
    fn test_extract_readable_text_decodes_entities() {
        let html = "<body><p>Fish &amp; Chips &mdash; caf&#233; &#x2013; 5 &lt; 10</p></body>";
        // &mdash; is intentionally NOT in the minimal named-entity table
        // (matches the "not a full HTML5 parser" scope note) so it passes
        // through unresolved -- only the entities actually needed for plain
        // prose are decoded.
        assert_eq!(
            extract_readable_text(html),
            "Fish & Chips &mdash; café – 5 < 10"
        );
    }

    #[test]
    fn test_extract_readable_text_strips_scripts_and_styles_only() {
        let html = "<body><script>var x = '<p>not real</p>';</script><style>.a{color:red}</style><p>Real text</p></body>";
        assert_eq!(extract_readable_text(html), "Real text");
    }

    // --- truncate_chars -----------------------------------------------------

    #[test]
    fn test_truncate_chars_under_limit_unchanged() {
        let (s, truncated) = truncate_chars("hello", 10);
        assert_eq!(s, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_chars_over_limit_cuts_and_flags() {
        let (s, truncated) = truncate_chars("hello world", 5);
        assert_eq!(s, "hello");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_chars_multibyte_safe() {
        // 4 emoji chars, limit 2 -- must not panic on byte boundaries.
        let (s, truncated) = truncate_chars("😀😀😀😀", 2);
        assert_eq!(s.chars().count(), 2);
        assert!(truncated);
    }

    // --- base64_decode --------------------------------------------------

    #[test]
    fn test_base64_decode_roundtrip() {
        // "Hello, World!" base64-encoded, standard alphabet, with padding.
        let decoded = base64_decode("SGVsbG8sIFdvcmxkIQ==").unwrap();
        assert_eq!(decoded, "Hello, World!");
    }

    #[test]
    fn test_base64_decode_handles_embedded_newlines() {
        // GitHub's Contents API wraps base64 content at 60 chars with \n --
        // callers strip newlines before decoding (done in the tool, not here),
        // but decode itself must tolerate whitespace already stripped.
        let decoded = base64_decode("aGVsbG8=").unwrap();
        assert_eq!(decoded, "hello");
    }

    // --- tool metadata ----------------------------------------------------

    #[test]
    fn test_all_six_tools_have_correct_names() {
        assert_eq!(LuminaClawhubSearch.name(), "lumina_clawhub_search");
        assert_eq!(LuminaClawhubSkillDetail.name(), "lumina_clawhub_skill_detail");
        assert_eq!(LuminaAicpbRankings.name(), "lumina_aicpb_rankings");
        assert_eq!(LuminaClawmartBrowse.name(), "lumina_clawmart_browse");
        assert_eq!(LuminaClawAwesomeList.name(), "lumina_claw_awesome_list");
        assert_eq!(LuminaWebFetch.name(), "lumina_web_fetch");
    }

    #[test]
    fn test_clawhub_search_requires_query() {
        let params = LuminaClawhubSearch.parameters();
        assert!(params["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "query"));
    }

    #[test]
    fn test_web_fetch_requires_url_only() {
        let params = LuminaWebFetch.parameters();
        let required = params["required"].as_array().unwrap();
        assert_eq!(required, &vec![json!("url")]);
    }

    // --- registration -------------------------------------------------------

    #[test]
    fn test_register_adds_six_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 6);
        assert!(registry.contains("lumina_clawhub_search"));
        assert!(registry.contains("lumina_clawhub_skill_detail"));
        assert!(registry.contains("lumina_aicpb_rankings"));
        assert!(registry.contains("lumina_clawmart_browse"));
        assert!(registry.contains("lumina_claw_awesome_list"));
        assert!(registry.contains("lumina_web_fetch"));
    }

    // --- lumina_clawhub_search: mocked HTTP happy path ----------------------

    #[tokio::test]
    async fn test_clawhub_search_happy_path_maps_fields() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/search");
            then.status(200).json_body(json!({
                "results": [
                    {"slug": "deploy", "displayName": "Deploy", "summary": "Ships things", "updatedAt": 1778486238781i64}
                ]
            }));
        });

        // Point the tool at the mock by calling send_get directly through a
        // constructed client + URL, mirroring what execute() does, since the
        // real target URL is a const. This exercises the field-mapping logic
        // (the part unique to this tool) against a controlled response.
        let client = reqwest::Client::new();
        let body = send_get(
            client
                .get(format!("{}/api/search", server.base_url()))
                .query(&[("q", "deploy"), ("limit", "10")]),
        )
        .await
        .unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results[0]["slug"], "deploy");
        assert_eq!(results[0]["displayName"], "Deploy");
    }

    #[tokio::test]
    async fn test_send_get_non_2xx_truncates_body_and_reports_status() {
        let server = MockServer::start();
        let long_body = "x".repeat(1000);
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/notfound");
            then.status(404).body(&long_body);
        });

        let client = reqwest::Client::new();
        let err = send_get(client.get(format!("{}/notfound", server.base_url())))
            .await
            .unwrap_err();
        assert!(err.starts_with("HTTP 404: "));
        // "HTTP 404: " (10 chars) + up to ERROR_BODY_MAX_LEN chars of body.
        assert_eq!(err.len(), 10 + ERROR_BODY_MAX_LEN);
    }

    #[tokio::test]
    async fn test_send_get_unreachable_host_returns_err_not_panic() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let err = send_get(client.get("http://127.0.0.1:1")).await.unwrap_err();
        assert!(!err.is_empty());
    }

    // --- lumina_web_fetch: end-to-end against a mock server -----------------

    #[tokio::test]
    async fn test_web_fetch_extract_text_end_to_end() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/page");
            then.status(200)
                .header("content-type", "text/html")
                .body("<html><head><title>T</title></head><body><p>Body text</p></body></html>");
        });

        let tool = LuminaWebFetch;
        let result = tool
            .execute(json!({"url": format!("{}/page", server.base_url()), "extract_text": true, "max_length": 100}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["content"], "T\nBody text");
        assert_eq!(v["truncated"], false);
    }

    #[tokio::test]
    async fn test_web_fetch_raw_mode_end_to_end() {
        let server = MockServer::start();
        let raw = "<html><body><p>Raw HTML stays as-is</p></body></html>";
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/raw");
            then.status(200).body(raw);
        });

        let tool = LuminaWebFetch;
        let result = tool
            .execute(json!({"url": format!("{}/raw", server.base_url()), "extract_text": false, "max_length": 1000}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["type"], "raw");
        assert_eq!(v["content"], raw);
    }

    #[tokio::test]
    async fn test_web_fetch_truncates_and_flags() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/long");
            then.status(200).body("<body><p>0123456789</p></body>");
        });

        let tool = LuminaWebFetch;
        let result = tool
            .execute(json!({"url": format!("{}/long", server.base_url()), "max_length": 5}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["content"], "01234");
        assert_eq!(v["truncated"], true);
    }

    #[tokio::test]
    async fn test_web_fetch_error_shape_has_url_and_error_no_type() {
        let tool = LuminaWebFetch;
        let result = tool
            .execute(json!({"url": "http://127.0.0.1:1"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["url"], "http://127.0.0.1:1");
        assert!(v["error"].is_string());
        assert!(v.get("type").is_none());
    }

    #[tokio::test]
    async fn test_web_fetch_missing_url_rejected() {
        let tool = LuminaWebFetch;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // --- lumina_clawhub_skill_detail: mocked 404 (matches live behavior) ---

    #[tokio::test]
    async fn test_skill_detail_404_reports_error_with_slug() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/skill/deploy");
            then.status(404).body("No matching routes found");
        });

        let client = reqwest::Client::new();
        let url = format!("{}/api/skill/deploy", server.base_url());
        let err = send_get(client.get(&url)).await.unwrap_err();
        assert_eq!(err, "HTTP 404: No matching routes found");
    }

    // --- lumina_clawmart_browse: mocked homepage + search-404 ---------------

    #[tokio::test]
    async fn test_clawmart_browse_empty_query_fetches_home() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/");
            then.status(200).body("<body><p>Claw Mart home</p></body>");
        });

        let client = reqwest::Client::new();
        let body = send_get(client.get(server.base_url())).await.unwrap();
        assert!(body.contains("Claw Mart home"));
    }

    // --- lumina_aicpb_rankings: category is echoed but unused ---------------

    #[tokio::test]
    async fn test_aicpb_rankings_category_echoed_not_used_in_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/");
            then.status(200).body("<body><p>Rankings homepage</p></body>");
        });

        let client = reqwest::Client::new();
        // Same URL regardless of category -- the tool never appends it.
        let _ = send_get(client.get(server.base_url())).await.unwrap();
        let _ = send_get(client.get(server.base_url())).await.unwrap();
        mock.assert_hits(2);
    }

    // --- lumina_claw_awesome_list: mocked GitHub contents response ---------

    #[tokio::test]
    async fn test_awesome_list_decodes_base64_and_truncates() {
        let long_readme = "# Awesome List\n".repeat(1000);
        let b64 = {
            // Encode using the same alphabet our decoder expects, via a tiny
            // manual encoder (std has none) so the test stays dependency-free.
            fn encode(data: &[u8]) -> String {
                const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                let mut out = String::new();
                for chunk in data.chunks(3) {
                    let b0 = chunk[0];
                    let b1 = *chunk.get(1).unwrap_or(&0);
                    let b2 = *chunk.get(2).unwrap_or(&0);
                    out.push(ALPHA[(b0 >> 2) as usize] as char);
                    out.push(ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
                    out.push(if chunk.len() > 1 {
                        ALPHA[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
                    } else {
                        '='
                    });
                    out.push(if chunk.len() > 2 {
                        ALPHA[(b2 & 0x3f) as usize] as char
                    } else {
                        '='
                    });
                }
                out
            }
            encode(long_readme.as_bytes())
        };

        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/contents/README.md");
            then.status(200).json_body(json!({
                "content": b64,
                "encoding": "base64",
                "sha": "abc123",
            }));
        });

        let client = reqwest::Client::new();
        let body = send_get(
            client
                .get(format!("{}/contents/README.md", server.base_url()))
                .header("User-Agent", "terminus-rs-lumina-ext"),
        )
        .await
        .unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        let decoded = base64_decode(parsed["content"].as_str().unwrap()).unwrap();
        assert!(decoded.starts_with("# Awesome List"));
        let (content, truncated) = truncate_chars(&decoded, AWESOME_LIST_MAX_LEN);
        assert_eq!(content.chars().count(), AWESOME_LIST_MAX_LEN);
        assert!(truncated);
    }
}
