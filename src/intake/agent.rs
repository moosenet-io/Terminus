//! Agent and tool workflow profiling suite (S83 MINT-03).
//!
//! Reads `agent-scenarios.json` from `INTAKE_CORPUS_DIR` and runs each scenario
//! against the target model. The model is exercised through Ollama's `/api/chat`
//! tool-calling path (`context::chat_with_tools`) — the same inference engine
//! the rest of the fleet uses — with a synthetic tool catalog. For
//! `tool_selection` we vary the advertised tool count per `tool_count_band`
//! (10/50/100/200) by padding the catalog with realistic decoy tools, so we can
//! measure accuracy degradation as the catalog grows.
//!
//! Scoring per category:
//!   - tool_selection        → correct_tool_selected (dispatched expected_tool,
//!                             or NO tool for adversarial scenarios)
//!   - multi_step            → multi_step_completed (all expected tools called,
//!                             in order)
//!   - instruction_following → instruction_followed (check: bullet_count /
//!                             word_count_max / language / no_tool_name_leak /
//!                             starts_with / all_uppercase / valid_json)
//!   - hallucination         → hallucination_detected = it fabricated
//!                             (pass = it did NOT)
//!   - personality           → LLM-judged quality 1-5
//!
//! One `agent_profile_runs` row per scenario; aggregates (tool accuracy overall
//! and at 200 tools, multistep rate) are patched into the operational profile.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::intake::code::corpus_dir;
use crate::intake::context;
use crate::intake::storage::{self, AgentRunRow};

/// Top-level scenarios file.
#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioFile {
    pub scenarios: Vec<Scenario>,
}

/// One agent scenario (union over all categories — optional fields per kind).
#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub id: String,
    pub category: String,
    pub prompt: String,
    #[serde(default)]
    pub expected_tool: Option<String>,
    #[serde(default)]
    pub expected_tools: Vec<String>,
    #[serde(default)]
    pub adversarial: bool,
    #[serde(default)]
    pub tool_count_band: Vec<i64>,
    #[serde(default)]
    pub check: Option<Value>,
    #[serde(default)]
    pub must_not_fabricate: Option<bool>,
}

/// Read + parse `agent-scenarios.json`.
pub fn read_scenarios(dir: &Path) -> Result<Vec<Scenario>, ToolError> {
    let path = dir.join("agent-scenarios.json");
    let body = std::fs::read_to_string(&path).map_err(|e| {
        ToolError::NotConfigured(format!("agent scenarios not found at {}: {e}", path.display()))
    })?;
    let f: ScenarioFile = serde_json::from_str(&body)
        .map_err(|e| ToolError::Execution(format!("scenarios parse error: {e}")))?;
    Ok(f.scenarios)
}

// ---------------------------------------------------------------------------
// Tool catalog construction (pure)
// ---------------------------------------------------------------------------

/// The real tool families referenced by the scenarios. Kept here so the catalog
/// always advertises the expected tools alongside decoys.
pub const REAL_TOOLS: &[(&str, &str)] = &[
    ("weather", "Get current weather conditions for a location."),
    ("google_email_inbox", "List recent emails in the user's inbox."),
    ("google_calendar_today", "List today's calendar events."),
    ("google_calendar_week", "List this week's calendar events."),
    ("news_headlines", "Get the top news headlines."),
    ("commute_estimate", "Estimate the user's commute duration right now."),
    ("route_traffic", "Report traffic incidents and accidents on a route."),
    ("dura_constellation_health", "Report health status of the server fleet."),
    ("reminder_set", "Create a reminder at a given time."),
    ("reminder_list", "List the user's pending reminders."),
    ("hearth_pantry_list", "List items currently in the pantry."),
    ("hearth_shopping_list", "List items on the shopping list."),
    ("searxng_search", "Run a general web search and return results."),
    ("plane_create_issue", "Create a work item / issue in Plane."),
    ("plane_list_issues", "List work items / issues in Plane."),
    ("gitea_create_pull_request", "Open a pull request in Gitea."),
    ("jellyseerr_requests", "List media requests in <media-service>."), // pii-test-fixture
];

/// Build a single Ollama tool spec from (name, description). One string param
/// `query` keeps the schema valid without constraining tool selection. Pure.
fn tool_spec(name: &str, description: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Free-form argument for the tool." }
                },
                "required": []
            }
        }
    })
}

/// Generate a deterministic decoy tool name + description for padding. Pure.
fn decoy(i: usize) -> (String, String) {
    const VERBS: &[&str] = &["fetch", "list", "create", "update", "delete", "query", "sync", "scan"];
    const NOUNS: &[&str] = &[
        "ledger", "widget", "manifest", "beacon", "lattice", "cipher", "harbor", "quartz",
        "vector", "ember", "spindle", "torrent", "almanac", "conduit",
    ];
    let v = VERBS[i % VERBS.len()];
    let n = NOUNS[(i / VERBS.len()) % NOUNS.len()];
    let name = format!("{v}_{n}_{i}");
    let desc = format!("Internal utility that {v}s the {n} subsystem (decoy {i}).");
    (name, desc)
}

/// Build a tool catalog of exactly `count` tools that ALWAYS includes the given
/// `required` tool names (so the correct answer is reachable), padded with
/// distinct decoys. Real non-required tools are added before decoys. Pure.
pub fn build_catalog(count: usize, required: &[String]) -> Value {
    let mut tools: Vec<Value> = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 1. Required tools first.
    for r in required {
        if used.insert(r.clone()) {
            let desc = REAL_TOOLS
                .iter()
                .find(|(n, _)| n == r)
                .map(|(_, d)| *d)
                .unwrap_or("Tool required by this scenario.");
            tools.push(tool_spec(r, desc));
        }
    }
    // 2. Other real tools.
    for (n, d) in REAL_TOOLS {
        if tools.len() >= count {
            break;
        }
        if used.insert((*n).to_string()) {
            tools.push(tool_spec(n, d));
        }
    }
    // 3. Decoys to reach the requested size.
    let mut i = 0usize;
    while tools.len() < count {
        let (name, desc) = decoy(i);
        if used.insert(name.clone()) {
            tools.push(tool_spec(&name, &desc));
        }
        i += 1;
        if i > count * 4 {
            break; // safety
        }
    }
    // Truncate if `required` alone exceeded `count`.
    tools.truncate(count.max(required.len()));
    Value::Array(tools)
}

// ---------------------------------------------------------------------------
// Scoring (pure)
// ---------------------------------------------------------------------------

/// Score a tool-selection scenario. `dispatched` is the list of tool names the
/// model called. Pass = called exactly the expected tool (non-adversarial) or
/// called NO tool (adversarial). Pure.
pub fn score_tool_selection(
    expected_tool: Option<&str>,
    adversarial: bool,
    dispatched: &[String],
) -> bool {
    if adversarial {
        return dispatched.is_empty();
    }
    match expected_tool {
        Some(t) => dispatched.iter().any(|d| d == t),
        None => dispatched.is_empty(),
    }
}

/// Score multi-step completion: every expected tool was called, in the expected
/// relative order (subsequence match). Pure.
pub fn score_multi_step(expected_tools: &[String], dispatched: &[String]) -> bool {
    if expected_tools.is_empty() {
        return false;
    }
    let mut it = dispatched.iter();
    for want in expected_tools {
        if !it.any(|d| d == want) {
            return false;
        }
    }
    true
}

/// Evaluate an instruction-following `check` against the model's text. Pure.
pub fn score_instruction(check: &Value, response: &str) -> bool {
    let kind = check.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "bullet_count" => {
            let want = check.get("expected").and_then(|v| v.as_i64()).unwrap_or(-1);
            count_bullets(response) as i64 == want
        }
        "word_count_max" => {
            let max = check.get("expected").and_then(|v| v.as_i64()).unwrap_or(i64::MAX);
            (word_count(response) as i64) <= max
        }
        "language" => {
            let lang = check.get("expected").and_then(|v| v.as_str()).unwrap_or("");
            looks_like_language(response, lang)
        }
        "no_tool_name_leak" => !leaks_tool_name(response),
        "starts_with" => {
            let pfx = check.get("expected").and_then(|v| v.as_str()).unwrap_or("");
            response.trim_start().starts_with(pfx)
        }
        "all_uppercase" => {
            let letters: String = response.chars().filter(|c| c.is_alphabetic()).collect();
            !letters.is_empty() && letters.chars().all(|c| c.is_uppercase())
        }
        "valid_json" => {
            let t = strip_code_fence(response);
            serde_json::from_str::<Value>(t.trim()).is_ok()
        }
        _ => false,
    }
}

/// Count Markdown-style bullet / numbered list items. Pure.
pub fn count_bullets(s: &str) -> usize {
    s.lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("- ")
                || t.starts_with("* ")
                || t.starts_with("• ")
                || numbered_item(t)
        })
        .count()
}

/// True for `1.` / `2)` style numbered list lines. Pure.
fn numbered_item(t: &str) -> bool {
    let mut chars = t.chars();
    let mut saw_digit = false;
    for c in chars.by_ref() {
        if c.is_ascii_digit() {
            saw_digit = true;
        } else if (c == '.' || c == ')') && saw_digit {
            return true;
        } else {
            break;
        }
    }
    false
}

/// Word count (whitespace split). Pure.
pub fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

/// Strip a leading/trailing ``` fence if the whole response is one block. Pure.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // drop the optional language tag line
        let rest = rest.splitn(2, '\n').nth(1).unwrap_or(rest);
        return rest.trim_end_matches("```").trim();
    }
    t
}

/// Cheap language heuristic for the two languages the corpus checks (es/fr):
/// look for high-frequency function words / accents. Pure.
pub fn looks_like_language(s: &str, lang: &str) -> bool {
    let lc = s.to_lowercase();
    match lang {
        "es" => {
            let hits = ["el ", "la ", "los ", "las ", "que ", "para ", "con ", "una ", "está", "ñ", "¿", "¡"]
                .iter()
                .filter(|w| lc.contains(*w))
                .count();
            hits >= 2
        }
        "fr" => {
            let hits = ["le ", "la ", "les ", "des ", "une ", "vous ", "est ", "pour ", "à ", "ça", "é"]
                .iter()
                .filter(|w| lc.contains(*w))
                .count();
            hits >= 2
        }
        _ => false,
    }
}

/// Detect leakage of an internal tool name in user-facing text. Pure.
pub fn leaks_tool_name(s: &str) -> bool {
    let lc = s.to_lowercase();
    REAL_TOOLS.iter().any(|(n, _)| lc.contains(&n.to_lowercase()))
}

/// Heuristic hallucination detector for the hallucination category. Pass (no
/// hallucination) = the model expresses a limitation / lack of knowledge.
/// Returns `true` when it APPEARS TO FABRICATE (no refusal markers). Pure.
pub fn detected_hallucination(response: &str) -> bool {
    let lc = response.to_lowercase();
    let refusal_markers = [
        "i don't", "i do not", "i can't", "i cannot", "i'm not able", "i am not able",
        "no information", "don't have", "do not have", "can't book", "cannot book",
        "not able to", "i don't know", "no access", "unable to", "i have no",
        "no record", "i'm unable", "i am unable", "not stored", "no data",
        "can't access", "cannot access", "no way to know", "don't have access",
    ];
    !refusal_markers.iter().any(|m| lc.contains(m))
}

// ---------------------------------------------------------------------------
// Aggregation (pure)
// ---------------------------------------------------------------------------

/// Aggregate scores folded into the operational profile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentAggregate {
    pub tool_accuracy_overall: Option<f64>,
    pub tool_accuracy_at_200: Option<f64>,
    pub multistep_rate: Option<f64>,
    pub instruction_adherence: Option<f64>,
    pub hallucination_rate: Option<f64>,
    pub personality_quality: Option<f64>,
    pub recommended_role: String,
}

/// Inputs to aggregation: per-scenario booleans tagged by category, plus the
/// tool-count for tool_selection rows and personality quality scores.
#[derive(Debug, Clone, Default)]
pub struct AggregateInputs {
    /// (tool_count, passed) for every tool_selection row.
    pub tool_selection: Vec<(i64, bool)>,
    pub multi_step: Vec<bool>,
    pub instruction: Vec<bool>,
    /// hallucination_detected per hallucination row (true = fabricated).
    pub hallucination: Vec<bool>,
    pub personality_quality: Vec<f64>,
}

fn frac(passed: usize, total: usize) -> Option<f64> {
    if total == 0 { None } else { Some(passed as f64 / total as f64) }
}

/// Compute aggregates + a recommended role. Pure.
pub fn aggregate(inp: &AggregateInputs) -> AgentAggregate {
    let mut a = AgentAggregate::default();

    if !inp.tool_selection.is_empty() {
        let pass = inp.tool_selection.iter().filter(|(_, p)| *p).count();
        a.tool_accuracy_overall = frac(pass, inp.tool_selection.len());
        let at200: Vec<&(i64, bool)> = inp.tool_selection.iter().filter(|(c, _)| *c == 200).collect();
        if !at200.is_empty() {
            let p = at200.iter().filter(|(_, p)| *p).count();
            a.tool_accuracy_at_200 = frac(p, at200.len());
        }
    }
    a.multistep_rate = frac(inp.multi_step.iter().filter(|p| **p).count(), inp.multi_step.len());
    a.instruction_adherence =
        frac(inp.instruction.iter().filter(|p| **p).count(), inp.instruction.len());
    // hallucination_rate = fraction that fabricated.
    a.hallucination_rate =
        frac(inp.hallucination.iter().filter(|p| **p).count(), inp.hallucination.len());
    if !inp.personality_quality.is_empty() {
        let sum: f64 = inp.personality_quality.iter().sum();
        a.personality_quality = Some(sum / inp.personality_quality.len() as f64);
    }

    a.recommended_role = recommend_role(&a);
    a
}

/// Pick a recommended role from the aggregate signals. Pure.
pub fn recommend_role(a: &AgentAggregate) -> String {
    let tool = a.tool_accuracy_overall.unwrap_or(0.0);
    let multistep = a.multistep_rate.unwrap_or(0.0);
    let hall = a.hallucination_rate.unwrap_or(1.0);
    let pers = a.personality_quality.unwrap_or(0.0);

    if pers >= 4.0 && hall <= 0.2 {
        "personality".to_string()
    } else if tool >= 0.8 && multistep >= 0.7 {
        "router".to_string()
    } else if tool >= 0.8 {
        "summarizer".to_string()
    } else if hall <= 0.15 {
        "reviewer".to_string()
    } else {
        "review-only".to_string()
    }
}

// ---------------------------------------------------------------------------
// Suite driver (live)
// ---------------------------------------------------------------------------

/// Per-scenario inference timeout (overridable via `INTAKE_AGENT_TIMEOUT_SEC`).
/// Delegates to the canonical resolver (Phase 2 item 3) — same default
/// (180s), same env var, same behavior.
fn agent_timeout() -> Duration {
    super::timeouts::env_timeout("INTAKE_AGENT_TIMEOUT_SEC", 180)
}

/// Tool-count bands used for tool_selection when a scenario omits its own band.
const DEFAULT_BANDS: &[i64] = &[10, 50, 100, 200];

/// Outcome of the agent suite for the tool summary.
pub struct AgentSuiteOutcome {
    pub scenarios_run: usize,
    pub rows_written: usize,
    pub aggregate: AgentAggregate,
}

/// Run the agent suite end-to-end against `model_name`. `limit` optionally caps
/// the number of scenarios (smoke runs). Stores one `agent_profile_runs` row per
/// scenario execution and patches agent aggregates into the operational profile.
pub async fn run_agent_suite(
    model_name: &str,
    profile_id: uuid::Uuid,
    limit: Option<usize>,
) -> Result<AgentSuiteOutcome, ToolError> {
    let dir: PathBuf = corpus_dir()?;
    let mut scenarios = read_scenarios(&dir)?;
    if let Some(n) = limit {
        scenarios.truncate(n);
    }
    if scenarios.is_empty() {
        return Err(ToolError::NotConfigured("no agent scenarios found".into()));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| ToolError::Http(format!("client build failed: {e}")))?;
    let pool = storage::get_pool().await?;

    let mut inputs = AggregateInputs::default();
    let mut rows_written = 0usize;
    let timeout = agent_timeout();

    for sc in &scenarios {
        match sc.category.as_str() {
            "tool_selection" => {
                let bands = if sc.tool_count_band.is_empty() {
                    DEFAULT_BANDS.to_vec()
                } else {
                    sc.tool_count_band.clone()
                };
                let required: Vec<String> =
                    sc.expected_tool.iter().cloned().collect();
                for band in bands {
                    let catalog = build_catalog(band as usize, &required);
                    let out = context::chat_with_tools(&client, model_name, &sc.prompt, &catalog, timeout).await;
                    let dispatched: Vec<String> = out.tool_calls.iter().map(|(n, _)| n.clone()).collect();
                    let params_valid = out.tool_calls.iter().all(|(_, a)| a.is_object() || a.is_null());
                    let passed = score_tool_selection(sc.expected_tool.as_deref(), sc.adversarial, &dispatched);
                    inputs.tool_selection.push((band, passed));
                    let row = AgentRunRow {
                        test_name: format!("{}@{}", sc.id, band),
                        tool_count_available: Some(band as i32),
                        correct_tool_selected: Some(passed),
                        tool_params_valid: Some(params_valid),
                        total_time_ms: out.total_time_ms,
                        error: out.error.clone(),
                        ..Default::default()
                    };
                    let agent_run_id = storage::insert_agent_run(&pool, profile_id, &row).await?;
                    rows_written += 1;

                    // multi-point-score-tracking: preserve per-band tool
                    // accuracy (pass=1.0/fail=0.0) vs. advertised tool count,
                    // alongside the fixed `tool_accuracy_at_200` column the
                    // operational profile keeps. Best-effort: the durable agent
                    // row is already persisted, so a score-point failure is
                    // logged and swallowed rather than aborting the suite.
                    let points = vec![storage::ScorePoint {
                        axis: "tool_count".to_string(),
                        x_value: band as f64,
                        x_label: None,
                        metric: "tool_accuracy".to_string(),
                        value: Some(if passed { 1.0 } else { 0.0 }),
                    }];
                    if let Err(e) = storage::insert_score_points(
                        &pool,
                        storage::ScorePointParent::Agent(agent_run_id),
                        profile_id,
                        &points,
                    )
                    .await
                    {
                        tracing::warn!("intake: failed to persist agent score points: {e}");
                    }
                }
            }
            "multi_step" => {
                let catalog = build_catalog(50, &sc.expected_tools);
                let out = context::chat_with_tools(&client, model_name, &sc.prompt, &catalog, timeout).await;
                let dispatched: Vec<String> = out.tool_calls.iter().map(|(n, _)| n.clone()).collect();
                let passed = score_multi_step(&sc.expected_tools, &dispatched);
                inputs.multi_step.push(passed);
                let row = AgentRunRow {
                    test_name: sc.id.clone(),
                    tool_count_available: Some(50),
                    multi_step_completed: Some(passed),
                    total_time_ms: out.total_time_ms,
                    error: out.error.clone(),
                    ..Default::default()
                };
                storage::insert_agent_run(&pool, profile_id, &row).await?;
                rows_written += 1;
            }
            "instruction_following" => {
                // No tools — pure text generation.
                let out = context::generate(&client, model_name, &sc.prompt, timeout).await;
                let passed = sc
                    .check
                    .as_ref()
                    .map(|c| score_instruction(c, &out.response))
                    .unwrap_or(false);
                inputs.instruction.push(passed);
                let row = AgentRunRow {
                    test_name: sc.id.clone(),
                    instruction_followed: Some(passed),
                    total_time_ms: out.total_time_ms,
                    error: out.error.clone(),
                    ..Default::default()
                };
                storage::insert_agent_run(&pool, profile_id, &row).await?;
                rows_written += 1;
            }
            "hallucination" => {
                let out = context::generate(&client, model_name, &sc.prompt, timeout).await;
                let fabricated = detected_hallucination(&out.response);
                inputs.hallucination.push(fabricated);
                let row = AgentRunRow {
                    test_name: sc.id.clone(),
                    hallucination_detected: Some(fabricated),
                    total_time_ms: out.total_time_ms,
                    error: out.error.clone(),
                    ..Default::default()
                };
                storage::insert_agent_run(&pool, profile_id, &row).await?;
                rows_written += 1;
            }
            "personality" => {
                let out = context::generate(&client, model_name, &sc.prompt, timeout).await;
                let quality = judge_personality(&client, &sc.prompt, &out.response).await;
                if let Some(q) = quality {
                    inputs.personality_quality.push(q);
                }
                let row = AgentRunRow {
                    test_name: sc.id.clone(),
                    response_quality_score: quality,
                    total_time_ms: out.total_time_ms,
                    error: out.error.clone(),
                    ..Default::default()
                };
                storage::insert_agent_run(&pool, profile_id, &row).await?;
                rows_written += 1;
            }
            _ => { /* unknown category — skip */ }
        }
    }

    let agg = aggregate(&inputs);
    storage::update_op_agent(&pool, profile_id, agg.tool_accuracy_overall, agg.multistep_rate).await?;

    Ok(AgentSuiteOutcome {
        scenarios_run: scenarios.len(),
        rows_written,
        aggregate: agg,
    })
}

/// LLM-judge personality response warmth/quality 1-5 via qwen3:8b. Best-effort.
async fn judge_personality(client: &reqwest::Client, prompt: &str, response: &str) -> Option<f64> {
    let judge_model =
        std::env::var("INTAKE_JUDGE_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    let p = format!(
        "Rate the assistant's reply for warmth, engagement, and natural personality on a \
         scale of 1 to 5 (5 = warm, genuine, engaged; 1 = flat, robotic, or inappropriate). \
         Reply with ONLY the integer.\n\nUser said: {}\nAssistant replied: {}\n\nRating (1-5):",
        prompt.trim(),
        response.trim()
    );
    let out = context::generate(client, &judge_model, &p, Duration::from_secs(120)).await;
    if out.error.is_some() {
        return None;
    }
    crate::intake::code::parse_rating(&out.response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_catalog_sizes_and_includes_required() {
        let cat = build_catalog(10, &["weather".to_string()]);
        let arr = cat.as_array().unwrap();
        assert_eq!(arr.len(), 10);
        assert!(arr.iter().any(|t| t["function"]["name"] == "weather"));
        // 200 tools — required still present, all names unique.
        let big = build_catalog(200, &["plane_create_issue".to_string()]);
        let barr = big.as_array().unwrap();
        assert_eq!(barr.len(), 200);
        let names: std::collections::HashSet<_> =
            barr.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 200);
        assert!(names.contains("plane_create_issue"));
    }

    #[test]
    fn score_tool_selection_cases() {
        assert!(score_tool_selection(Some("weather"), false, &["weather".into()]));
        assert!(!score_tool_selection(Some("weather"), false, &["news_headlines".into()]));
        // adversarial: pass only when no tool called.
        assert!(score_tool_selection(None, true, &[]));
        assert!(!score_tool_selection(None, true, &["weather".into()]));
    }

    #[test]
    fn score_multi_step_subsequence_order() {
        let want = vec!["a".to_string(), "b".to_string()];
        assert!(score_multi_step(&want, &["a".into(), "x".into(), "b".into()]));
        assert!(!score_multi_step(&want, &["b".into(), "a".into()])); // wrong order
        assert!(!score_multi_step(&want, &["a".into()])); // missing b
        assert!(!score_multi_step(&[], &["a".into()])); // empty expected
    }

    #[test]
    fn count_bullets_markers() {
        assert_eq!(count_bullets("- one\n- two\n- three"), 3);
        assert_eq!(count_bullets("1. a\n2. b"), 2);
        assert_eq!(count_bullets("* x\nplain\n• y"), 2);
    }

    #[test]
    fn score_instruction_variants() {
        assert!(score_instruction(&json!({"type":"bullet_count","expected":3}), "- a\n- b\n- c"));
        assert!(!score_instruction(&json!({"type":"bullet_count","expected":3}), "- a\n- b"));
        assert!(score_instruction(&json!({"type":"word_count_max","expected":5}), "one two three"));
        assert!(!score_instruction(&json!({"type":"word_count_max","expected":2}), "one two three"));
        assert!(score_instruction(&json!({"type":"starts_with","expected":"Recommendation:"}), "Recommendation: buy"));
        assert!(score_instruction(&json!({"type":"all_uppercase","expected":true}), "HELLO WORLD"));
        assert!(!score_instruction(&json!({"type":"all_uppercase","expected":true}), "Hello"));
        assert!(score_instruction(&json!({"type":"valid_json","expected":true}), "{\"a\":1}"));
        assert!(score_instruction(&json!({"type":"valid_json","expected":true}), "```json\n{\"a\":1}\n```"));
        assert!(!score_instruction(&json!({"type":"valid_json","expected":true}), "not json"));
    }

    #[test]
    fn no_tool_leak_check() {
        assert!(score_instruction(&json!({"type":"no_tool_name_leak","expected":[]}), "I checked your inbox."));
        assert!(!score_instruction(&json!({"type":"no_tool_name_leak","expected":[]}), "Calling google_email_inbox now."));
    }

    #[test]
    fn language_heuristics() {
        assert!(looks_like_language("Hoy hace sol y la temperatura está alta para una salida.", "es"));
        assert!(looks_like_language("Le temps est ensoleillé et la température est élevée pour vous.", "fr"));
        assert!(!looks_like_language("The weather is sunny today.", "es"));
    }

    #[test]
    fn hallucination_detection_refusal_vs_fabrication() {
        assert!(!detected_hallucination("I don't have any flight information for you."));
        assert!(!detected_hallucination("I cannot book hotels."));
        assert!(detected_hallucination("Your flight number is UA482 departing at 9am."));
    }

    #[test]
    fn aggregate_and_role() {
        let inp = AggregateInputs {
            tool_selection: vec![(10, true), (200, true), (200, false)],
            multi_step: vec![true, true, false],
            instruction: vec![true],
            hallucination: vec![false, false, true], // 1/3 fabricated
            personality_quality: vec![5.0, 4.0],
        };
        let a = aggregate(&inp);
        assert!((a.tool_accuracy_overall.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!((a.tool_accuracy_at_200.unwrap() - 0.5).abs() < 1e-9);
        assert!((a.multistep_rate.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!((a.hallucination_rate.unwrap() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(a.personality_quality, Some(4.5));
        // pers 4.5 >= 4 and hall 0.33 > 0.2 → not "personality"; tool 0.66 < 0.8 → review-only.
        assert_eq!(a.recommended_role, "review-only");
    }

    #[test]
    fn recommend_role_personality() {
        let a = AgentAggregate {
            personality_quality: Some(4.5),
            hallucination_rate: Some(0.1),
            ..Default::default()
        };
        assert_eq!(recommend_role(&a), "personality");
    }

    #[test]
    fn recommend_role_router() {
        let a = AgentAggregate {
            tool_accuracy_overall: Some(0.9),
            multistep_rate: Some(0.8),
            hallucination_rate: Some(0.5),
            ..Default::default()
        };
        assert_eq!(recommend_role(&a), "router");
    }
}
