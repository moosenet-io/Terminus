//! Soma tools — ported from the Python `soma_tools.py` on <host>.
//!
//! Soma is the Lumina Constellation admin panel/API running on the fleet host
//! (<host>). The Python original (a `soma_tools.py` on <host>'s Python MCP
//! server) is a thin `urllib.request` wrapper: every tool except
//! `soma_status` sends an `X-Soma-Key` header sourced from `SOMA_SECRET_KEY`
//! and hits a fixed path under `SOMA_URL` (server-side default; see
//! `DEFAULT_SOMA_URL` below — deliberately a placeholder, not the real
//! internal address, to keep this source PII-clean; set `SOMA_URL` in the
//! deployment environment to the real fleet-host address).
//!
//! Confirmed live against <host> (`tools/call`, 2026-07-06):
//! - `soma_status` needs no auth. With Soma unreachable it returned exactly
//!   `{"status": "unreachable", "error": "<urlopen error ...>", "url":
//!   "<SOMA_URL>"}` — matching the Python `except` branch verbatim.
//! - Every other tool (`soma_modules`, `soma_skills_list`, `soma_backup_status`,
//!   `soma_cost_summary`, `soma_inference_status`, `soma_constellation_config`,
//!   `soma_run_validation`, `soma_rename_agent`, `soma_skill_approve`) returned
//!   `Error executing tool <name>: SOMA_SECRET_KEY not set in environment`
//!   because <host>'s own environment has no `SOMA_SECRET_KEY` configured right
//!   now. This is a live behavioral fact, not a bug we're introducing: the
//!   current source has no fallback dev key, so a missing key is a hard
//!   `NotConfigured`-style failure for every authenticated endpoint. This port
//!   reproduces that exact message so a NotConfigured error looks the same
//!   from either implementation.
//! - `soma_run_validation`'s docstring says "check soma_validation_status() for
//!   results", but **no such tool exists** anywhere in <host>'s live 126-tool
//!   catalog (confirmed via `tools/list`). This is a stale/dead docstring
//!   reference in the Python source, not something this port invents a
//!   companion tool for — `soma_run_validation` is ported faithfully as a
//!   fire-and-forget POST that returns whatever Soma's `/api/validate/smoke-test`
//!   responds with (expected to be a bare pid/ack).
//!
//! ## Known overlap (flagged for human curation, not resolved here)
//! `soma_skills_list` / `soma_skill_approve` overlap conceptually with a
//! separate `skills_list` / `skills_read` / `skills_create` tool set being
//! ported in parallel by a different agent (not yet merged as of this
//! branch's base commit) from <host>'s standalone `skills_*` module. The two
//! families read from the same `active/`/`proposed/` skill directories but
//! through different transports: `skills_*` goes over SSH to the fleet
//! host's filesystem directly, while `soma_skills_list` / `soma_skill_approve`
//! go through the Soma HTTP admin API. Per instruction, they are kept as
//! distinct `soma_`-prefixed tools here; do not merge.
//!
//! ## Tools (identical names to the Python source)
//!   soma_status               — Soma admin API health (no auth)
//!   soma_rename_agent         — PUT a new display_name for an agent (config write)
//!   soma_constellation_config — GET the full constellation.yaml
//!   soma_inference_status     — GET LiteLLM inference layer status
//!   soma_cost_summary         — GET Myelin cost/token usage summary
//!   soma_backup_status        — GET Dura backup status
//!   soma_run_validation       — POST to kick off a Dura smoke test (async, returns pid)
//!   soma_skills_list          — GET active/proposed skills
//!   soma_skill_approve        — POST to approve a proposed skill
//!   soma_modules              — GET status of all Lumina modules
//!
//! ## Configuration (environment only — no hardcoded hosts/keys)
//!   SOMA_URL         — Soma admin API base URL; set this to the real fleet-host
//!                      address in deployment (falls back to a non-routable
//!                      placeholder, `DEFAULT_SOMA_URL`, if unset).
//!   SOMA_SECRET_KEY  — shared secret sent as the `X-Soma-Key` header. Required
//!                      for every tool except `soma_status`; if unset, those
//!                      tools return `NotConfigured("SOMA_SECRET_KEY not set in
//!                      environment")` — no dev-key fallback (matches live <host>).
//!
//! ## Security model
//! - `agent_id` (soma_rename_agent) and `skill_name` (soma_skill_approve) are
//!   both interpolated into the request path. The Python original placed them
//!   into an f-string with no validation at all. This port adds a strict
//!   path-segment allowlist (ASCII alphanumerics, `-`, `_` only) before either
//!   value reaches a URL, closing a path-traversal / header-injection gap that
//!   existed in the source Python (e.g. an `agent_id` of `../../something`
//!   could not have been used to escape the intended path segment).
//! - `soma_rename_agent` additionally rejects an empty or excessively long
//!   `display_name`, and (per adversarial review) bidi-control / invisible
//!   Unicode format characters (e.g. right-to-left override, zero-width
//!   joiners) that could visually spoof another agent's name in any UI that
//!   renders `display_name` raw — before sending the config-write PUT. Full
//!   mixed-script/homoglyph detection is a known follow-up, not attempted
//!   here.

use std::env;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// Deliberately a placeholder, not the real internal fleet-host address —
// keeps this source PII-clean. Real deployments must set SOMA_URL.
const DEFAULT_SOMA_URL: &str = "http://YOUR_FLEET_SERVER_IP:8082";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SomaConfig {
    base_url: String,
}

impl SomaConfig {
    fn from_env() -> Self {
        let base_url = env::var("SOMA_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SOMA_URL.to_string());
        Self { base_url }
    }

    /// The shared secret sent as `X-Soma-Key`. Required for every tool except
    /// `soma_status`. No dev-key fallback — matches the live <host> behavior
    /// observed via `tools/call` (a missing key is a hard failure there too).
    fn key() -> Result<String, ToolError> {
        env::var("SOMA_SECRET_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("SOMA_SECRET_KEY not set in environment".into()))
    }

    fn client(timeout_secs: u64) -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a value destined for a single URL path segment. Accepts only
/// ASCII alphanumerics, `-`, and `_` — no `/`, `.`, or other characters that
/// could traverse paths or inject additional path segments / headers.
fn validate_path_segment(value: &str, field: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        return Err(ToolError::InvalidArgument(format!("{field} must not be empty")));
    }
    if value.len() > 128 {
        return Err(ToolError::InvalidArgument(format!("{field} is too long")));
    }
    let ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(ToolError::InvalidArgument(format!(
            "{field} '{value}' contains disallowed characters (only alphanumerics, '-', '_' permitted)"
        )));
    }
    Ok(())
}

/// Validate a display_name payload value (not a path segment — a JSON body
/// field — so we only bound its length and reject empty/control characters).
/// Unicode bidi-control / invisible-format characters that can be used to
/// visually spoof a display name (e.g. a right-to-left override making text
/// render reversed/misleadingly, or zero-width joiners hiding characters)
/// without tripping `char::is_control()`, which only covers the Cc category.
/// This list covers the bidi-control block plus common zero-width/format
/// characters; it is not a full Unicode "Cf" category table, but it closes
/// the concrete spoofing vectors these characters enable (adversarial review
/// finding — a full mixed-script/confusable-homoglyph defense is a known
/// follow-up, not attempted here).
const DISALLOWED_FORMAT_CHARS: &[char] = &[
    '\u{200B}', // zero width space
    '\u{200C}', // zero width non-joiner
    '\u{200D}', // zero width joiner
    '\u{200E}', // left-to-right mark
    '\u{200F}', // right-to-left mark
    '\u{202A}', // left-to-right embedding
    '\u{202B}', // right-to-left embedding
    '\u{202C}', // pop directional formatting
    '\u{202D}', // left-to-right override
    '\u{202E}', // right-to-left override
    '\u{2060}', // word joiner
    '\u{2061}', '\u{2062}', '\u{2063}', '\u{2064}', // invisible math operators
    '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', // directional isolates
    '\u{FEFF}', // BOM / zero width no-break space
    '\u{061C}', // Arabic letter mark
];

fn validate_display_name(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::InvalidArgument("display_name must not be empty".into()));
    }
    if value.chars().count() > 200 {
        return Err(ToolError::InvalidArgument("display_name is too long".into()));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(ToolError::InvalidArgument(
            "display_name must not contain control characters".into(),
        ));
    }
    if value.chars().any(|c| DISALLOWED_FORMAT_CHARS.contains(&c)) {
        return Err(ToolError::InvalidArgument(
            "display_name must not contain bidi-control or invisible formatting characters".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Truncate an error string to `n` chars, matching the Python `str(e)[:150]`
/// / `[:200]` truncation used across the source's exception handlers.
fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Unauthenticated GET against `{base}/health` — used only by `soma_status`.
/// Never returns an Err: any failure is folded into the Python-shaped
/// `{"status": "unreachable", "error": ..., "url": ...}` payload.
async fn soma_health(base: &str) -> Value {
    let client = match SomaConfig::client(5) {
        Ok(c) => c,
        Err(e) => return json!({"status": "unreachable", "error": truncate(&e.to_string(), 150), "url": base}),
    };

    let url = format!("{base}/health");
    match client.get(&url).send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(mut data) => {
                if let Value::Object(ref mut map) = data {
                    map.insert("url".to_string(), json!(base));
                    data
                } else {
                    json!({"status": "unreachable", "error": "unexpected response shape", "url": base})
                }
            }
            Err(e) => {
                warn!("soma: /health response was not valid JSON: {e}");
                json!({"status": "unreachable", "error": truncate(&e.to_string(), 150), "url": base})
            }
        },
        Err(e) => {
            warn!("soma: /health request failed: {e}");
            json!({"status": "unreachable", "error": truncate(&e.to_string(), 150), "url": base})
        }
    }
}

/// Authenticated GET against `{base}{path}`, sending `X-Soma-Key`.
async fn soma_get(base: &str, path: &str) -> Result<Value, ToolError> {
    let key = SomaConfig::key()?;
    let client = SomaConfig::client(10)?;
    let url = format!("{base}{path}");

    let resp = client
        .get(&url)
        .header("X-Soma-Key", key)
        .send()
        .await
        .map_err(|e| {
            warn!("soma: GET {path} failed: {e}");
            ToolError::Http("The Soma admin API is unreachable.".into())
        })?;

    let status = resp.status();
    let raw = resp.text().await.map_err(|e| {
        warn!("soma: reading GET {path} response failed: {e}");
        ToolError::Http("The Soma admin API is unreachable.".into())
    })?;

    if !status.is_success() {
        return Err(ToolError::Http(format!(
            "HTTP {status}: {}",
            truncate(&raw, 200)
        )));
    }
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw).map_err(|e| ToolError::Http(format!("Bad JSON: {e}")))
}

/// Authenticated POST against `{base}{path}` with a JSON body, sending
/// `X-Soma-Key`.
async fn soma_post(base: &str, path: &str, payload: &Value) -> Result<Value, ToolError> {
    let key = SomaConfig::key()?;
    let client = SomaConfig::client(15)?;
    let url = format!("{base}{path}");

    let resp = client
        .post(&url)
        .header("X-Soma-Key", key)
        .json(payload)
        .send()
        .await
        .map_err(|e| {
            warn!("soma: POST {path} failed: {e}");
            ToolError::Http("The Soma admin API is unreachable.".into())
        })?;

    let status = resp.status();
    let raw = resp.text().await.map_err(|e| {
        warn!("soma: reading POST {path} response failed: {e}");
        ToolError::Http("The Soma admin API is unreachable.".into())
    })?;

    if !status.is_success() {
        return Err(ToolError::Http(format!(
            "HTTP {status}: {}",
            truncate(&raw, 200)
        )));
    }
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw).map_err(|e| ToolError::Http(format!("Bad JSON: {e}")))
}

/// Authenticated PUT against `{base}{path}` with a JSON body, sending
/// `X-Soma-Key`.
async fn soma_put(base: &str, path: &str, payload: &Value) -> Result<Value, ToolError> {
    let key = SomaConfig::key()?;
    let client = SomaConfig::client(10)?;
    let url = format!("{base}{path}");

    let resp = client
        .put(&url)
        .header("X-Soma-Key", key)
        .json(payload)
        .send()
        .await
        .map_err(|e| {
            warn!("soma: PUT {path} failed: {e}");
            ToolError::Http("The Soma admin API is unreachable.".into())
        })?;

    let status = resp.status();
    let raw = resp.text().await.map_err(|e| {
        warn!("soma: reading PUT {path} response failed: {e}");
        ToolError::Http("The Soma admin API is unreachable.".into())
    })?;

    if !status.is_success() {
        return Err(ToolError::Http(format!(
            "HTTP {status}: {}",
            truncate(&raw, 200)
        )));
    }
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw).map_err(|e| ToolError::Http(format!("Bad JSON: {e}")))
}

// ---------------------------------------------------------------------------
// Tool: soma_status
// ---------------------------------------------------------------------------

struct SomaStatus {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaStatus {
    fn name(&self) -> &str {
        "soma_status"
    }

    fn description(&self) -> &str {
        "Check if Soma admin API is up. Returns version and status."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let data = soma_health(&self.cfg.base_url).await;
        Ok(data.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_rename_agent
// ---------------------------------------------------------------------------

struct SomaRenameAgent {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaRenameAgent {
    fn name(&self) -> &str {
        "soma_rename_agent"
    }

    fn description(&self) -> &str {
        "\nRename an agent's display name in constellation.yaml via Soma.\nagent_id: internal agent key (e.g. 'vigil', 'axon', 'lumina')\ndisplay_name: new human-readable name\n"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": { "type": "string", "description": "Internal agent key (e.g. 'vigil', 'axon', 'lumina')" },
                "display_name": { "type": "string", "description": "New human-readable name" }
            },
            "required": ["agent_id", "display_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let agent_id = args
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("'agent_id' must be a string".into()))?;
        let display_name = args
            .get("display_name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("'display_name' must be a string".into()))?;

        validate_path_segment(agent_id, "agent_id")?;
        validate_display_name(display_name)?;

        let path = format!("/api/constellation/agent/{agent_id}/display_name");
        let body = soma_put(&self.cfg.base_url, &path, &json!({ "name": display_name })).await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_constellation_config
// ---------------------------------------------------------------------------

struct SomaConstellationConfig {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaConstellationConfig {
    fn name(&self) -> &str {
        "soma_constellation_config"
    }

    fn description(&self) -> &str {
        "\nGet the current constellation.yaml config via Soma.\nReturns all agents, modules, and system metadata.\n"
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let body = soma_get(&self.cfg.base_url, "/api/constellation").await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_inference_status
// ---------------------------------------------------------------------------

struct SomaInferenceStatus {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaInferenceStatus {
    fn name(&self) -> &str {
        "soma_inference_status"
    }

    fn description(&self) -> &str {
        "\nCheck LiteLLM inference layer status via Soma.\nReturns list of available models and online/error status.\n"
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let body = soma_get(&self.cfg.base_url, "/api/inference/status").await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_cost_summary
// ---------------------------------------------------------------------------

struct SomaCostSummary {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaCostSummary {
    fn name(&self) -> &str {
        "soma_cost_summary"
    }

    fn description(&self) -> &str {
        "\nGet Myelin cost/token usage summary via Soma.\nReturns daily/weekly spend data if Myelin is collecting.\n"
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let body = soma_get(&self.cfg.base_url, "/api/cost").await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_backup_status
// ---------------------------------------------------------------------------

struct SomaBackupStatus {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaBackupStatus {
    fn name(&self) -> &str {
        "soma_backup_status"
    }

    fn description(&self) -> &str {
        "\nGet Dura backup status via Soma.\nReturns last backup run time, success/failure, and file counts.\n"
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let body = soma_get(&self.cfg.base_url, "/api/backup/status").await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_run_validation
// ---------------------------------------------------------------------------

struct SomaRunValidation {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaRunValidation {
    fn name(&self) -> &str {
        "soma_run_validation"
    }

    fn description(&self) -> &str {
        // NOTE: the referenced `soma_validation_status()` does not exist in
        // <host>'s live tool catalog (confirmed via tools/list). This is a
        // stale docstring in the Python source, ported verbatim rather than
        // silently "fixed" with an invented tool. See module doc comment.
        "\nTrigger a Dura smoke test run via Soma.\nRuns asynchronously — check soma_validation_status() for results.\nReturns pid of the background test process.\n"
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let body = soma_post(&self.cfg.base_url, "/api/validate/smoke-test", &json!({})).await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_skills_list
// ---------------------------------------------------------------------------

struct SomaSkillsList {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaSkillsList {
    fn name(&self) -> &str {
        "soma_skills_list"
    }

    fn description(&self) -> &str {
        "List all active and proposed agent skills from the skills directory.\nReturns: active skills (ready to use), proposed skills (awaiting approval)."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let body = soma_get(&self.cfg.base_url, "/api/skills").await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_skill_approve
// ---------------------------------------------------------------------------

struct SomaSkillApprove {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaSkillApprove {
    fn name(&self) -> &str {
        "soma_skill_approve"
    }

    fn description(&self) -> &str {
        "Approve a proposed skill, moving it from proposed/ to active/.\nskill_name: the skill directory name (e.g. 'morning-briefing-v2')\nThe operator must approve skills before they can be used by agents."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": { "type": "string", "description": "The skill directory name (e.g. 'morning-briefing-v2')" }
            },
            "required": ["skill_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let skill_name = args
            .get("skill_name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("'skill_name' must be a string".into()))?;
        validate_path_segment(skill_name, "skill_name")?;

        let path = format!("/api/skills/{skill_name}/approve");
        let body = soma_post(&self.cfg.base_url, &path, &json!({})).await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool: soma_modules
// ---------------------------------------------------------------------------

struct SomaModules {
    cfg: SomaConfig,
}

#[async_trait]
impl RustTool for SomaModules {
    fn name(&self) -> &str {
        "soma_modules"
    }

    fn description(&self) -> &str {
        "Get status of all Lumina modules (enabled/disabled, running/stopped).\nReturns list of modules with name, status, and health."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let body = soma_get(&self.cfg.base_url, "/api/modules").await?;
        Ok(body.to_string())
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let cfg = SomaConfig::from_env();

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(SomaStatus { cfg: cfg.clone() }),
        Box::new(SomaRenameAgent { cfg: cfg.clone() }),
        Box::new(SomaConstellationConfig { cfg: cfg.clone() }),
        Box::new(SomaInferenceStatus { cfg: cfg.clone() }),
        Box::new(SomaCostSummary { cfg: cfg.clone() }),
        Box::new(SomaBackupStatus { cfg: cfg.clone() }),
        Box::new(SomaRunValidation { cfg: cfg.clone() }),
        Box::new(SomaSkillsList { cfg: cfg.clone() }),
        Box::new(SomaSkillApprove { cfg: cfg.clone() }),
        Box::new(SomaModules { cfg }),
    ];

    for tool in tools {
        registry.register_or_replace(tool);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    fn cfg(server: &MockServer) -> SomaConfig {
        SomaConfig { base_url: server.base_url() }
    }

    fn clear_key() -> Option<String> {
        let existing = env::var("SOMA_SECRET_KEY").ok();
        env::remove_var("SOMA_SECRET_KEY");
        existing
    }

    fn restore_key(existing: Option<String>) {
        if let Some(v) = existing {
            env::set_var("SOMA_SECRET_KEY", v);
        } else {
            env::remove_var("SOMA_SECRET_KEY");
        }
    }

    fn set_key(key: &str) -> Option<String> {
        let existing = env::var("SOMA_SECRET_KEY").ok();
        env::set_var("SOMA_SECRET_KEY", key);
        existing
    }

    // ── validation ──────────────────────────────────────────────────────────

    #[test]
    fn validate_path_segment_accepts_agent_ids() {
        assert!(validate_path_segment("vigil", "agent_id").is_ok());
        assert!(validate_path_segment("axon", "agent_id").is_ok());
        assert!(validate_path_segment("morning-briefing-v2", "skill_name").is_ok());
        assert!(validate_path_segment("under_score1", "agent_id").is_ok());
    }

    #[test]
    fn validate_path_segment_rejects_empty() {
        assert!(validate_path_segment("", "agent_id").is_err());
    }

    #[test]
    fn validate_path_segment_rejects_traversal() {
        let err = validate_path_segment("../../etc/passwd", "agent_id").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn validate_path_segment_rejects_slash() {
        assert!(validate_path_segment("vigil/../axon", "agent_id").is_err());
        assert!(validate_path_segment("vigil/axon", "agent_id").is_err());
    }

    #[test]
    fn validate_path_segment_rejects_header_injection_chars() {
        assert!(validate_path_segment("vigil\r\nX-Evil: 1", "agent_id").is_err());
        assert!(validate_path_segment("vigil?query=1", "agent_id").is_err());
        assert!(validate_path_segment("vigil#frag", "agent_id").is_err());
    }

    #[test]
    fn validate_path_segment_rejects_too_long() {
        let long = "a".repeat(129);
        assert!(validate_path_segment(&long, "agent_id").is_err());
    }

    #[test]
    fn validate_display_name_accepts_normal_names() {
        assert!(validate_display_name("Vigil").is_ok());
        assert!(validate_display_name("The Obsidian Circle").is_ok());
    }

    #[test]
    fn validate_display_name_rejects_empty() {
        assert!(validate_display_name("").is_err());
        assert!(validate_display_name("   ").is_err());
    }

    #[test]
    fn validate_display_name_rejects_too_long() {
        let long = "a".repeat(201);
        assert!(validate_display_name(&long).is_err());
    }

    #[test]
    fn validate_display_name_rejects_control_chars() {
        assert!(validate_display_name("Vigil\n\rEvil").is_err());
        assert!(validate_display_name("Vigil\0").is_err());
    }

    #[test]
    fn validate_display_name_rejects_bidi_override() {
        // Adversarial review finding: right-to-left override can make a
        // display name render reversed/misleadingly in any UI that shows it
        // raw. This must be rejected even though it isn't a Cc control char.
        assert!(validate_display_name("Vigil\u{202E}reversed").is_err());
        assert!(validate_display_name("\u{202D}Axon\u{202C}").is_err());
    }

    #[test]
    fn validate_display_name_rejects_invisible_format_chars() {
        assert!(validate_display_name("Vigil\u{200B}").is_err()); // zero width space
        assert!(validate_display_name("Vigil\u{FEFF}").is_err()); // BOM
        assert!(validate_display_name("Vi\u{200D}gil").is_err()); // zero width joiner
    }

    #[test]
    fn validate_display_name_still_allows_normal_unicode_letters() {
        // This fix targets bidi-control/invisible-format characters, not
        // legitimate non-ASCII letters (accents, other scripts written
        // left-to-right/right-to-left normally without override controls).
        assert!(validate_display_name("Séance").is_ok());
        assert!(validate_display_name("日本語").is_ok());
    }

    #[test]
    fn truncate_limits_length() {
        assert_eq!(truncate("hello world", 5), "hello");
        assert_eq!(truncate("hi", 5), "hi");
    }

    // ── soma_status (no auth, never errors) ──────────────────────────────────

    #[tokio::test]
    async fn soma_status_success_injects_url() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/health");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"status": "ok", "version": "1.2.3"}));
        });

        let tool = SomaStatus { cfg: cfg(&server) };
        let result = tool.execute(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["version"], "1.2.3");
        assert_eq!(v["url"], server.base_url());
        mock.assert();
    }

    #[tokio::test]
    async fn soma_status_unreachable_never_errors() {
        // Nothing listening on this port — connection refused.
        let cfg = SomaConfig { base_url: "http://127.0.0.1:1".to_string() };
        let tool = SomaStatus { cfg };
        let result = tool.execute(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["status"], "unreachable");
        assert!(v["error"].as_str().is_some());
        assert_eq!(v["url"], "http://127.0.0.1:1");
    }

    #[tokio::test]
    #[serial]
    async fn soma_status_does_not_require_key() {
        let existing = clear_key();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/health");
            then.status(200).json_body(json!({"status": "ok"}));
        });
        let tool = SomaStatus { cfg: cfg(&server) };
        let result = tool.execute(json!({})).await;
        restore_key(existing);
        assert!(result.is_ok());
    }

    // ── auth requirement (SOMA_SECRET_KEY) ───────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn soma_modules_missing_key_returns_not_configured_with_live_message() {
        let existing = clear_key();
        let server = MockServer::start();
        let tool = SomaModules { cfg: cfg(&server) };
        let err = tool.execute(json!({})).await.unwrap_err();
        restore_key(existing);
        match err {
            ToolError::NotConfigured(msg) => assert_eq!(msg, "SOMA_SECRET_KEY not set in environment"),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn soma_skills_list_missing_key_returns_not_configured() {
        let existing = clear_key();
        let server = MockServer::start();
        let tool = SomaSkillsList { cfg: cfg(&server) };
        let err = tool.execute(json!({})).await.unwrap_err();
        restore_key(existing);
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    #[tokio::test]
    #[serial]
    async fn soma_get_sends_key_header_when_configured() {
        let existing = set_key("test-secret");
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/modules")
                .header("X-Soma-Key", "test-secret");
            then.status(200).json_body(json!({"modules": []}));
        });

        let tool = SomaModules { cfg: cfg(&server) };
        let result = tool.execute(json!({})).await;
        restore_key(existing);

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    #[serial]
    async fn soma_get_maps_http_error_status() {
        let existing = set_key("test-secret");
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/cost");
            then.status(500).body("internal error");
        });

        let tool = SomaCostSummary { cfg: cfg(&server) };
        let err = tool.execute(json!({})).await.unwrap_err();
        restore_key(existing);
        match err {
            ToolError::Http(msg) => assert!(msg.contains("500")),
            other => panic!("expected Http error, got {other:?}"),
        }
    }

    // ── soma_constellation_config / soma_inference_status / soma_backup_status ──

    #[tokio::test]
    #[serial]
    async fn soma_constellation_config_passthrough() {
        let existing = set_key("k");
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/constellation");
            then.status(200).json_body(json!({"agents": ["vigil", "axon"]}));
        });
        let tool = SomaConstellationConfig { cfg: cfg(&server) };
        let result = tool.execute(json!({})).await.unwrap();
        restore_key(existing);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["agents"][0], "vigil");
    }

    #[tokio::test]
    #[serial]
    async fn soma_inference_status_passthrough() {
        let existing = set_key("k");
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/inference/status");
            then.status(200).json_body(json!({"models": [], "online": true}));
        });
        let tool = SomaInferenceStatus { cfg: cfg(&server) };
        let result = tool.execute(json!({})).await.unwrap();
        restore_key(existing);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["online"], true);
    }

    #[tokio::test]
    #[serial]
    async fn soma_backup_status_passthrough() {
        let existing = set_key("k");
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/backup/status");
            then.status(200).json_body(json!({"last_run": "2026-07-01"}));
        });
        let tool = SomaBackupStatus { cfg: cfg(&server) };
        let result = tool.execute(json!({})).await.unwrap();
        restore_key(existing);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["last_run"], "2026-07-01");
    }

    // ── soma_run_validation ───────────────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn soma_run_validation_posts_and_returns_pid() {
        let existing = set_key("k");
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/validate/smoke-test");
            then.status(200).json_body(json!({"pid": 12345}));
        });
        let tool = SomaRunValidation { cfg: cfg(&server) };
        let result = tool.execute(json!({})).await.unwrap();
        restore_key(existing);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["pid"], 12345);
        mock.assert();
    }

    #[test]
    fn soma_run_validation_description_documents_dead_reference() {
        // Regression guard: this docstring intentionally still references the
        // nonexistent soma_validation_status — do not "fix" it by inventing
        // a companion tool. See module docs for the confirmation.
        let cfg = SomaConfig { base_url: DEFAULT_SOMA_URL.to_string() };
        let tool = SomaRunValidation { cfg };
        assert!(tool.description().contains("soma_validation_status"));
    }

    // ── soma_rename_agent (config write — validation focus) ──────────────────

    #[tokio::test]
    async fn soma_rename_agent_rejects_missing_fields() {
        let server = MockServer::start();
        let tool = SomaRenameAgent { cfg: cfg(&server) };
        let err = tool.execute(json!({"agent_id": "vigil"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn soma_rename_agent_rejects_path_traversal_agent_id() {
        let server = MockServer::start();
        let tool = SomaRenameAgent { cfg: cfg(&server) };
        let err = tool
            .execute(json!({"agent_id": "../../etc/passwd", "display_name": "Vigil"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn soma_rename_agent_rejects_empty_display_name() {
        let server = MockServer::start();
        let tool = SomaRenameAgent { cfg: cfg(&server) };
        let err = tool
            .execute(json!({"agent_id": "vigil", "display_name": ""}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn soma_rename_agent_rejects_control_chars_in_display_name() {
        let server = MockServer::start();
        let tool = SomaRenameAgent { cfg: cfg(&server) };
        let err = tool
            .execute(json!({"agent_id": "vigil", "display_name": "Vigil\r\nX-Evil: 1"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    #[serial]
    async fn soma_rename_agent_puts_expected_path_and_body() {
        let existing = set_key("k");
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/api/constellation/agent/vigil/display_name")
                .header("X-Soma-Key", "k")
                .json_body(json!({"name": "Vigil Prime"}));
            then.status(200).json_body(json!({"ok": true}));
        });

        let tool = SomaRenameAgent { cfg: cfg(&server) };
        let result = tool
            .execute(json!({"agent_id": "vigil", "display_name": "Vigil Prime"}))
            .await;
        restore_key(existing);

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    #[serial]
    async fn soma_rename_agent_missing_key_returns_not_configured() {
        let existing = clear_key();
        let server = MockServer::start();
        let tool = SomaRenameAgent { cfg: cfg(&server) };
        let err = tool
            .execute(json!({"agent_id": "vigil", "display_name": "Vigil"}))
            .await
            .unwrap_err();
        restore_key(existing);
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    // ── soma_skill_approve ────────────────────────────────────────────────────

    #[tokio::test]
    async fn soma_skill_approve_rejects_missing_skill_name() {
        let server = MockServer::start();
        let tool = SomaSkillApprove { cfg: cfg(&server) };
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn soma_skill_approve_rejects_path_traversal() {
        let server = MockServer::start();
        let tool = SomaSkillApprove { cfg: cfg(&server) };
        let err = tool
            .execute(json!({"skill_name": "../../active/evil"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    #[serial]
    async fn soma_skill_approve_posts_expected_path() {
        let existing = set_key("k");
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/skills/morning-briefing-v2/approve")
                .header("X-Soma-Key", "k");
            then.status(200).json_body(json!({"approved": true}));
        });

        let tool = SomaSkillApprove { cfg: cfg(&server) };
        let result = tool
            .execute(json!({"skill_name": "morning-briefing-v2"}))
            .await;
        restore_key(existing);

        assert!(result.is_ok());
        mock.assert();
    }

    // ── soma_modules ──────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn soma_modules_passthrough() {
        let existing = set_key("k");
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/modules");
            then.status(200).json_body(json!({"modules": [{"name": "vigil", "status": "running"}]}));
        });
        let tool = SomaModules { cfg: cfg(&server) };
        let result = tool.execute(json!({})).await.unwrap();
        restore_key(existing);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["modules"][0]["name"], "vigil");
    }

    // ── metadata / registration ───────────────────────────────────────────────

    #[test]
    fn tool_names_are_stable() {
        let cfg = SomaConfig::from_env();
        assert_eq!(SomaStatus { cfg: cfg.clone() }.name(), "soma_status");
        assert_eq!(SomaRenameAgent { cfg: cfg.clone() }.name(), "soma_rename_agent");
        assert_eq!(SomaConstellationConfig { cfg: cfg.clone() }.name(), "soma_constellation_config");
        assert_eq!(SomaInferenceStatus { cfg: cfg.clone() }.name(), "soma_inference_status");
        assert_eq!(SomaCostSummary { cfg: cfg.clone() }.name(), "soma_cost_summary");
        assert_eq!(SomaBackupStatus { cfg: cfg.clone() }.name(), "soma_backup_status");
        assert_eq!(SomaRunValidation { cfg: cfg.clone() }.name(), "soma_run_validation");
        assert_eq!(SomaSkillsList { cfg: cfg.clone() }.name(), "soma_skills_list");
        assert_eq!(SomaSkillApprove { cfg: cfg.clone() }.name(), "soma_skill_approve");
        assert_eq!(SomaModules { cfg }.name(), "soma_modules");
    }

    #[test]
    fn tool_parameters_are_objects() {
        let cfg = SomaConfig::from_env();
        assert_eq!(SomaStatus { cfg: cfg.clone() }.parameters()["type"], "object");
        let rename_params = SomaRenameAgent { cfg: cfg.clone() }.parameters();
        assert_eq!(rename_params["type"], "object");
        assert!(rename_params["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "agent_id"));
        assert!(rename_params["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "display_name"));
        let approve_params = SomaSkillApprove { cfg }.parameters();
        assert!(approve_params["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "skill_name"));
    }

    #[test]
    fn register_adds_ten_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 10);
        for name in [
            "soma_status",
            "soma_rename_agent",
            "soma_constellation_config",
            "soma_inference_status",
            "soma_cost_summary",
            "soma_backup_status",
            "soma_run_validation",
            "soma_skills_list",
            "soma_skill_approve",
            "soma_modules",
        ] {
            assert!(reg.contains(name), "missing tool {name}");
        }
    }
}
