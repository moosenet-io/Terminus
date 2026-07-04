//! Obsidian Circle (council) tools — ported from the Python `council_tools.py`
//! on <host> (OC.8).
//!
//! The Python source is a thin MCP wrapper around a separate, much larger
//! Python subsystem: `obsidian_circle.engine` (~490 lines) which runs a real
//! multi-model ReAct deliberation loop — calling multiple LLM providers
//! through LiteLLM, broadcasting tool results between "members", synthesizing
//! a recommendation, validating it against a JSON schema, and evaluating a
//! confidence threshold — plus `obsidian_circle.output` (formatting) and
//! `obsidian_circle.personas` (persona definitions).
//!
//! ## What this port does and does not do
//! - `council_presets` is ported **faithfully and fully**: the 7 built-in
//!   presets are static data (mirrors `presets.py::_BUILTIN_PRESETS`), plus
//!   optional custom presets read from a `constellation.yaml` (mirrors
//!   `presets.py::_load_yaml_presets`).
//! - `council_status` / `council_history` are ported **faithfully and
//!   fully**: they only ever read the in-memory session store populated by
//!   `council_convene` — no engine dependency.
//! - `council_convene` is **NOT** backed by a working deliberation engine in
//!   this port. Porting `engine.py` would mean re-implementing an entire
//!   separate multi-provider LLM orchestration system (cost tables, ReAct
//!   loop, budget enforcement, schema validation) as a side effect of a
//!   4-module tool port — well beyond this task's scope, and exactly the
//!   kind of dead-behind-the-tool-name output the task asked to flag rather
//!   than fake. This mirrors the Python source's *own* designed fallback:
//!   when `obsidian_circle` fails to import, the Python tool already returns
//!   `'Obsidian Circle not available — obsidian_circle module not found at
//!   FLEET_DIR'` instead of deliberating. This Rust port takes that same
//!   honest path unconditionally (see `docs/module.md` / final report for
//!   the recommended follow-up: either bridge to the existing Python engine
//!   over HTTP, or a dedicated `obsidian-circle-rs` port).
//!
//! ## Tools (identical names to the Python source)
//!   council_convene  — convene the circle (NotConfigured: no engine wired up)
//!   council_presets  — list available presets
//!   council_status   — check a specific session or list recent
//!   council_history  — full history table this runtime
//!
//! ## Configuration (environment only — no hardcoded paths)
//!   COUNCIL_CONSTELLATION_YAML_PATH — optional path to `constellation.yaml`
//!     for custom preset definitions (`council.circles`). Unset -> only the
//!     7 built-in presets are listed (matches Python behavior when the file
//!     is absent).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Built-in presets (mirrors presets.py::_BUILTIN_PRESETS — metadata only;
// the `members`/`synthesis_model`/etc. fields that drive real deliberation
// are omitted here since nothing in this crate executes them yet)
// ---------------------------------------------------------------------------

struct BuiltinPreset {
    name: &'static str,
    display_name: &'static str,
    description: &'static str,
    member_count: usize,
}

const BUILTIN_PRESETS: &[BuiltinPreset] = &[
    BuiltinPreset { name: "quick", display_name: "Quick", description: "Mr. Wizard solo — fast single-model answer", member_count: 1 },
    BuiltinPreset { name: "architecture", display_name: "Architecture", description: "3 Prism personas — Architect, Skeptic, Pragmatist", member_count: 3 },
    BuiltinPreset { name: "security", display_name: "Security", description: "Adversarial review — Security Auditor + Skeptic", member_count: 2 },
    BuiltinPreset { name: "cost", display_name: "Cost", description: "Cost + efficiency review — Cost Optimizer + Pragmatist", member_count: 2 },
    BuiltinPreset { name: "research", display_name: "Research", description: "Multi-model research synthesis — 4 distinct architectures", member_count: 4 },
    BuiltinPreset { name: "full", display_name: "Full Council", description: "All 7 personas — maximum deliberation, highest cost", member_count: 7 },
    BuiltinPreset { name: "custom", display_name: "Custom", description: "User-defined preset from constellation.yaml", member_count: 1 },
];

#[derive(Debug, Clone, Deserialize, Default)]
struct YamlPreset {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    members: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CouncilSection {
    #[serde(default)]
    circles: HashMap<String, YamlPreset>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ConstellationYaml {
    #[serde(default)]
    council: CouncilSection,
}

fn load_yaml_presets() -> HashMap<String, YamlPreset> {
    let path = match std::env::var("COUNCIL_CONSTELLATION_YAML_PATH") {
        Ok(p) if !p.is_empty() => p,
        _ => return HashMap::new(),
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_yaml::from_str::<ConstellationYaml>(&s).ok())
        .map(|y| y.council.circles)
        .unwrap_or_default()
}

/// List all available presets (built-in + custom). Mirrors `presets.py::list_presets`.
fn list_presets() -> Vec<Value> {
    let mut out: Vec<Value> = BUILTIN_PRESETS
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "display_name": p.display_name,
                "description": p.description,
                "member_count": p.member_count,
                "source": "builtin",
            })
        })
        .collect();

    let builtin_names: std::collections::HashSet<&str> =
        BUILTIN_PRESETS.iter().map(|p| p.name).collect();

    for (name, p) in load_yaml_presets() {
        if builtin_names.contains(name.as_str()) {
            continue;
        }
        out.push(json!({
            "name": name,
            "display_name": p.display_name.clone().unwrap_or_else(|| name.clone()),
            "description": p.description.clone().unwrap_or_default(),
            "member_count": p.members.len(),
            "source": "custom",
        }));
    }

    out
}

// ---------------------------------------------------------------------------
// Session store (mirrors the Python module-level `_SESSIONS: dict`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Session {
    id: String,
    timestamp: DateTime<Utc>,
    question: String,
    circle: String,
    confidence: f64,
    action: String,
    cost_usd: f64,
    elapsed_s: f64,
    member_count: u64,
}

#[derive(Default)]
struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    fn new() -> Self {
        Self { sessions: Mutex::new(HashMap::new()) }
    }

    #[cfg(test)]
    fn insert(&self, session: Session) {
        self.sessions.lock().unwrap().insert(session.id.clone(), session);
    }
}

// ---------------------------------------------------------------------------
// Tool: council_convene
// ---------------------------------------------------------------------------

pub struct CouncilConvene;

#[async_trait]
impl RustTool for CouncilConvene {
    fn name(&self) -> &str {
        "council_convene"
    }

    fn description(&self) -> &str {
        "Convene the Obsidian Circle for multi-model deliberation. The Circle is a \
         multi-model reasoning council where different AI models and personas \
         deliberate independently on a question, then synthesize a recommendation. \
         NOTE: the deliberation engine (obsidian_circle) has not been ported to \
         Rust yet — this tool currently returns NotConfigured, matching the \
         Python source's own fallback for when the engine module is unavailable."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question, decision, or problem to deliberate on." },
                "circle": {
                    "type": "string",
                    "description": "Preset — quick, architecture, security, cost, research, full, custom",
                    "default": "quick"
                },
                "budget": { "type": "number", "description": "Max USD to spend", "default": 0.10 },
                "mode": { "type": "string", "description": "'multi' or 'prism'", "default": "multi" },
                "output_format": { "type": "string", "description": "'text' or 'json'", "default": "text" }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let _question = args["question"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'question' must be a string".into()))?;

        Err(ToolError::NotConfigured(
            "Obsidian Circle not available — the deliberation engine (obsidian_circle) \
             has not been ported to Rust. Use the Python MCP tool or wire up \
             COUNCIL_ENGINE_URL to bridge to a running engine."
                .into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: council_presets
// ---------------------------------------------------------------------------

pub struct CouncilPresets;

#[async_trait]
impl RustTool for CouncilPresets {
    fn name(&self) -> &str {
        "council_presets"
    }

    fn description(&self) -> &str {
        "List all available Obsidian Circle presets with descriptions and member \
         counts. Returns the 7 built-in presets plus any custom presets from \
         constellation.yaml. Use this to pick the right circle for a deliberation."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let presets = list_presets();
        let mut lines = vec!["Obsidian Circle presets:".to_string(), String::new()];
        for p in &presets {
            let tag = if p["source"] == "custom" { " [custom]" } else { "" };
            let desc = p["description"].as_str().unwrap_or("");
            let desc_trunc: String = desc.chars().take(65).collect();
            lines.push(format!(
                "  {:<14} {} member(s)  {}{}",
                p["name"].as_str().unwrap_or(""),
                p["member_count"],
                desc_trunc,
                tag
            ));
        }
        lines.push(String::new());
        lines.push("Use: council_convene(question, circle=\"<name>\")".to_string());
        Ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Tool: council_status
// ---------------------------------------------------------------------------

pub struct CouncilStatus {
    store: std::sync::Arc<SessionStore>,
}

#[async_trait]
impl RustTool for CouncilStatus {
    fn name(&self) -> &str {
        "council_status"
    }

    fn description(&self) -> &str {
        "Check status of a council session or list recent sessions. Args: \
         session_id (specific session ID from council_convene; leave empty for \
         recent list). Returns session details including question, circle, \
         confidence, action, and cost."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Specific session ID (optional)", "default": "" }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let session_id = args["session_id"].as_str().unwrap_or("");
        let sessions = self.store.sessions.lock().unwrap();

        if !session_id.is_empty() {
            return match sessions.get(session_id) {
                None => Ok(format!("Session '{session_id}' not found in this runtime")),
                Some(s) => Ok(format!(
                    "Session:    {}\n\
                     Question:   {}\n\
                     Circle:     {}\n\
                     Members:    {}\n\
                     Confidence: {:.0}%\n\
                     Action:     {}\n\
                     Cost:       ${:.4}\n\
                     Elapsed:    {}s\n\
                     Timestamp:  {}\n",
                    s.id,
                    &s.question.chars().take(100).collect::<String>(),
                    s.circle,
                    s.member_count,
                    s.confidence * 100.0,
                    s.action,
                    s.cost_usd,
                    s.elapsed_s,
                    s.timestamp.to_rfc3339(),
                )),
            };
        }

        if sessions.is_empty() {
            return Ok("No council sessions this runtime".to_string());
        }

        let mut recent: Vec<&Session> = sessions.values().collect();
        recent.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        recent.truncate(8);

        let mut lines = vec![format!(
            "Recent council sessions ({} total this runtime):",
            sessions.len()
        ), String::new()];
        for s in recent {
            let q_brief: String = if s.question.chars().count() > 55 {
                let mut t: String = s.question.chars().take(55).collect();
                t.push_str("...");
                t
            } else {
                s.question.clone()
            };
            let id_tail: String = s.id.chars().rev().take(12).collect::<String>().chars().rev().collect();
            lines.push(format!(
                "  {:<12}  [{:<12}]  conf={:.0}%  {:<22}  {}",
                id_tail, s.circle, s.confidence * 100.0, s.action, q_brief
            ));
        }
        Ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Tool: council_history
// ---------------------------------------------------------------------------

pub struct CouncilHistory {
    store: std::sync::Arc<SessionStore>,
}

#[async_trait]
impl RustTool for CouncilHistory {
    fn name(&self) -> &str {
        "council_history"
    }

    fn description(&self) -> &str {
        "Return the history of council deliberations this runtime session. Shows a \
         summary table of recent decisions with questions, presets, confidence, \
         actions taken, and costs."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max number of sessions (default 10, max 50)", "default": 10 }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let limit = (args["limit"].as_u64().unwrap_or(10) as usize).min(50);
        let sessions = self.store.sessions.lock().unwrap();

        if sessions.is_empty() {
            return Ok("No council history this session".to_string());
        }

        let mut all: Vec<&Session> = sessions.values().collect();
        all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        let shown = &all[..all.len().min(limit)];

        let mut lines = vec![
            format!(
                "Council history ({} total this runtime, showing {}):",
                sessions.len(),
                shown.len()
            ),
            String::new(),
            format!(
                "  {:<14} {:<13} {:<6} {:<22} {:>8}  Question",
                "Session", "Circle", "Conf", "Action", "Cost"
            ),
            format!("  {}", "-".repeat(90)),
        ];

        for s in shown {
            let id_tail: String = s.id.chars().rev().take(12).collect::<String>().chars().rev().collect();
            let q: String = if s.question.chars().count() > 42 {
                let mut t: String = s.question.chars().take(42).collect();
                t.push_str("...");
                t
            } else {
                s.question.clone()
            };
            lines.push(format!(
                "  {:<14} {:<13} {:<6} {:<22} ${:>6.4}  {}",
                id_tail,
                s.circle,
                format!("{:.0}%", s.confidence * 100.0),
                s.action,
                s.cost_usd,
                q
            ));
        }

        let total_cost: f64 = sessions.values().map(|s| s.cost_usd).sum();
        lines.push(String::new());
        lines.push(format!("  Total council spend this runtime: ${total_cost:.4}"));

        Ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Obsidian Circle (council) tools into the ToolRegistry.
pub fn register(registry: &mut ToolRegistry) {
    let store = std::sync::Arc::new(SessionStore::new());

    let _ = registry.register(Box::new(CouncilConvene));
    let _ = registry.register(Box::new(CouncilPresets));
    let _ = registry.register(Box::new(CouncilStatus { store: std::sync::Arc::clone(&store) }));
    let _ = registry.register(Box::new(CouncilHistory { store }));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn sample_session(id: &str, question: &str, ts_offset_secs: i64) -> Session {
        Session {
            id: id.to_string(),
            timestamp: Utc::now() + chrono::Duration::seconds(ts_offset_secs),
            question: question.to_string(),
            circle: "quick".to_string(),
            confidence: 0.85,
            action: "auto_act".to_string(),
            cost_usd: 0.012,
            elapsed_s: 4.2,
            member_count: 1,
        }
    }

    // --- list_presets ---------------------------------------------------

    #[test]
    fn test_list_presets_includes_all_seven_builtins() {
        let presets = list_presets();
        assert_eq!(presets.iter().filter(|p| p["source"] == "builtin").count(), 7);
        let names: Vec<&str> = presets.iter().map(|p| p["name"].as_str().unwrap()).collect();
        for n in ["quick", "architecture", "security", "cost", "research", "full", "custom"] {
            assert!(names.contains(&n), "missing builtin preset: {n}");
        }
    }

    #[test]
    #[serial]
    fn test_list_presets_no_custom_yaml_configured() {
        std::env::remove_var("COUNCIL_CONSTELLATION_YAML_PATH");
        let presets = list_presets();
        assert!(presets.iter().all(|p| p["source"] == "builtin"));
    }

    #[test]
    #[serial]
    fn test_list_presets_missing_yaml_file_degrades_gracefully() {
        std::env::set_var("COUNCIL_CONSTELLATION_YAML_PATH", "/nonexistent/constellation.yaml");
        let presets = list_presets();
        assert_eq!(presets.len(), 7, "should fall back to builtins only");
        std::env::remove_var("COUNCIL_CONSTELLATION_YAML_PATH");
    }

    // --- tool metadata ------------------------------------------------

    #[test]
    fn test_council_convene_metadata() {
        let tool = CouncilConvene;
        assert_eq!(tool.name(), "council_convene");
        let params = tool.parameters();
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "question"));
    }

    #[test]
    fn test_council_presets_metadata() {
        assert_eq!(CouncilPresets.name(), "council_presets");
    }

    // --- council_convene: honest NotConfigured, not fake deliberation ---

    #[tokio::test]
    async fn test_convene_returns_not_configured() {
        let tool = CouncilConvene;
        let err = tool
            .execute(json!({"question": "should we ship this?"}))
            .await
            .unwrap_err();
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("obsidian_circle")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_convene_missing_question_rejected() {
        let tool = CouncilConvene;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // --- council_presets: happy path -------------------------------------

    #[tokio::test]
    async fn test_council_presets_execute_lists_quick() {
        let tool = CouncilPresets;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("quick"));
        assert!(result.contains("council_convene"));
    }

    // --- council_status ---------------------------------------------------

    #[tokio::test]
    async fn test_council_status_empty_store() {
        let store = std::sync::Arc::new(SessionStore::new());
        let tool = CouncilStatus { store };
        let result = tool.execute(json!({})).await.unwrap();
        assert_eq!(result, "No council sessions this runtime");
    }

    #[tokio::test]
    async fn test_council_status_unknown_session_id() {
        let store = std::sync::Arc::new(SessionStore::new());
        let tool = CouncilStatus { store };
        let result = tool.execute(json!({"session_id": "oc_999"})).await.unwrap();
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn test_council_status_known_session_id() {
        let store = std::sync::Arc::new(SessionStore::new());
        store.insert(sample_session("oc_1", "Should we adopt Rust for tool X?", 0));
        let tool = CouncilStatus { store };
        let result = tool.execute(json!({"session_id": "oc_1"})).await.unwrap();
        assert!(result.contains("oc_1"));
        assert!(result.contains("auto_act"));
        assert!(result.contains("85%"));
    }

    #[tokio::test]
    async fn test_council_status_lists_recent_when_no_id() {
        let store = std::sync::Arc::new(SessionStore::new());
        store.insert(sample_session("oc_1", "question one", 0));
        store.insert(sample_session("oc_2", "question two", 10));
        let tool = CouncilStatus { store };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("2 total this runtime"));
    }

    // --- council_history ---------------------------------------------------

    #[tokio::test]
    async fn test_council_history_empty_store() {
        let store = std::sync::Arc::new(SessionStore::new());
        let tool = CouncilHistory { store };
        let result = tool.execute(json!({})).await.unwrap();
        assert_eq!(result, "No council history this session");
    }

    #[tokio::test]
    async fn test_council_history_shows_total_spend() {
        let store = std::sync::Arc::new(SessionStore::new());
        store.insert(sample_session("oc_1", "q1", 0));
        store.insert(sample_session("oc_2", "q2", 5));
        let tool = CouncilHistory { store };
        let result = tool.execute(json!({"limit": 10})).await.unwrap();
        assert!(result.contains("Total council spend this runtime: $0.0240"));
    }

    #[tokio::test]
    async fn test_council_history_respects_limit() {
        let store = std::sync::Arc::new(SessionStore::new());
        for i in 0..5 {
            store.insert(sample_session(&format!("oc_{i}"), "q", i as i64));
        }
        let tool = CouncilHistory { store };
        let result = tool.execute(json!({"limit": 2})).await.unwrap();
        assert!(result.contains("showing 2"));
    }

    // --- registration -----------------------------------------------------

    #[test]
    fn test_register_adds_four_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 4);
        assert!(registry.contains("council_convene"));
        assert!(registry.contains("council_presets"));
        assert!(registry.contains("council_status"));
        assert!(registry.contains("council_history"));
    }
}
