//! Category: `tool_routing` — the first-class tool-routing / function-calling
//! profiler (S125 SUITE-TOOL, TERM-511).
//!
//! ## What this generalizes
//! The S83 `agent` suite ([`crate::intake::agent`]) already exercises tool
//! selection, but it is welded to Ollama's `/api/chat` tool path
//! (`context::chat_with_tools`) and folds tool accuracy into the operational
//! profile as a couple of scalar columns. This module lifts the same corpus
//! (`agent-scenarios.json`) into a first-class `newcats`-style profiling suite
//! that:
//!   - routes through **Chord's OpenAI-compatible `/v1/chat/completions`** with
//!     a `tools` array (via [`crate::intake::infer::tool_infer_with_metrics`]'s
//!     `openai` arm), so any backend Chord serves can be profiled — not only
//!     Ollama; and
//!   - writes discrete per-scenario `assistant_dimension_score` rows tagged
//!     `task_category = "tool_routing"`, so tool-routing coverage sits in the
//!     fleet catalog on its own axis (mirroring `diffusion`/`image_parsing`).
//!
//! It **reuses** [`crate::intake::agent`]'s pure primitives verbatim — the
//! scenario schema ([`crate::intake::agent::Scenario`]), the tool-catalog
//! builder ([`crate::intake::agent::build_catalog`], real tools + graduated
//! decoys), and the multi-step subsequence scorer
//! ([`crate::intake::agent::score_multi_step`]) — so there is ONE catalog/scoring
//! source of truth and the existing `agent` suite is left completely untouched.
//!
//! ## Metrics (all in `[0.0, 1.0]`, one row per scenario per metric)
//! `dimension = "tool_routing"` for every row; the `metric` field distinguishes:
//!   - `correct_tool_at_1`   — the FIRST tool the model called is the expected
//!     tool (correct-tool@1). Scored on non-adversarial `tool_selection`.
//!   - `parameter_validity`  — every emitted tool call carried a well-formed
//!     JSON-object arguments payload. Scored on `tool_selection` + `multi_step`.
//!   - `decoy_rejection`     — for an adversarial / decoy-only prompt (no correct
//!     tool exists), the model correctly called NO tool. Scored on adversarial
//!     `tool_selection`.
//!   - `multi_step_success`  — every expected tool was called, in order
//!     (subsequence). Scored on `multi_step`.
//!
//! ## Backend / testability
//! Mirrors the other `newcats` modules: a small [`ToolRoutingModel`] trait is the
//! seam a runner calls against the live Chord route; unit tests inject a mock
//! [`RoutingOutcome`] and exercise [`build_scores`] with no network.

use sqlx::PgPool;

use crate::error::ToolError;

use super::super::agent::{score_multi_step, Scenario};
use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};

/// `task_category` value this module writes (the catalog cell axis).
pub const TASK_CATEGORY: &str = "tool_routing";
/// `dimension` value written for every tool-routing metric row.
pub const DIMENSION: &str = "tool_routing";

/// `metric`: correct-tool@1 — the first tool called is the expected one.
pub const METRIC_CORRECT_TOOL: &str = "correct_tool_at_1";
/// `metric`: parameter validity — all tool calls carried object-shaped args.
pub const METRIC_PARAM_VALIDITY: &str = "parameter_validity";
/// `metric`: decoy rejection — an adversarial/decoy-only prompt called no tool.
pub const METRIC_DECOY_REJECT: &str = "decoy_rejection";
/// `metric`: multi-step success — all expected tools called, in order.
pub const METRIC_MULTI_STEP: &str = "multi_step_success";

/// Outcome of one (real or mock) tool-routing turn: the tools the model chose
/// (function name + parsed argument JSON, in order) and any transport error.
#[derive(Debug, Clone, Default)]
pub struct RoutingOutcome {
    /// `(function_name, parsed_arguments)` for each tool call, in order. For an
    /// OpenAI-compatible backend the arguments string is parsed to a `Value`
    /// (see [`crate::intake::infer::tool_infer_with_metrics`]); for Ollama it is
    /// already an object.
    pub tool_calls: Vec<(String, serde_json::Value)>,
    /// Set when the underlying inference failed (transport/HTTP/parse). A runner
    /// SKIPS scoring an errored turn rather than recording a fabricated `0.0`.
    pub error: Option<String>,
}

/// Seam for calling the tool-routing backend; a runner implements this against
/// [`crate::intake::infer::tool_infer_with_metrics`] (Chord `/v1/chat/completions`
/// with a `tools` array), tests inject a mock outcome.
pub trait ToolRoutingModel {
    fn route(&self, prompt: &str, tools: &serde_json::Value) -> Result<RoutingOutcome, String>;
}

// ---------------------------------------------------------------------------
// Scoring (pure)
// ---------------------------------------------------------------------------

/// correct-tool@1: the FIRST tool the model called is the expected tool. When a
/// scenario expects no tool (`expected == None`), the pass condition is calling
/// no tool at all. Pure.
pub fn correct_tool_at_1(expected: Option<&str>, dispatched: &[String]) -> bool {
    match expected {
        Some(t) => dispatched.first().map(|d| d == t).unwrap_or(false),
        None => dispatched.is_empty(),
    }
}

/// Parameter validity: at least one tool was called AND every call carried a
/// well-formed JSON-object arguments payload (a valid function-call schema). An
/// empty/absent or non-object arguments value fails. Pure.
pub fn parameters_valid(calls: &[(String, serde_json::Value)]) -> bool {
    !calls.is_empty() && calls.iter().all(|(_, a)| a.is_object())
}

/// Decoy rejection: for an adversarial / decoy-only prompt (no correct tool
/// exists in the catalog), the model correctly declined to call any tool. Pure.
pub fn decoy_rejected(dispatched: &[String]) -> bool {
    dispatched.is_empty()
}

/// Build one `DimensionScore` row for a boolean metric outcome. Pure.
fn metric_row(
    model_id: ModelId,
    backend_tag: BackendTag,
    scenario_id: &str,
    metric: &str,
    passed: bool,
    dispatched: &[String],
) -> DimensionScore {
    DimensionScore {
        model_id,
        backend_tag,
        dimension: DIMENSION.to_string(),
        metric: metric.to_string(),
        value: if passed { 1.0 } else { 0.0 },
        std_dev: None,
        judge: "derived".to_string(),
        low_confidence: false,
        raw_json: Some(
            serde_json::json!({
                "scenario": scenario_id,
                "dispatched": dispatched,
            })
            .to_string(),
        ),
    }
}

/// Build the `assistant_dimension_score` rows for one tool-routing scenario
/// attempt. Which metrics are emitted depends on the scenario shape:
///   - non-adversarial `tool_selection` → `correct_tool_at_1` + `parameter_validity`
///   - adversarial `tool_selection`     → `decoy_rejection`
///   - `multi_step`                     → `multi_step_success` + `parameter_validity`
///
/// Any other category yields no rows (the routing suite scores tool-calling
/// behaviour only — instruction/hallucination/personality stay with the `agent`
/// suite). Pure.
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    scenario: &Scenario,
    outcome: &RoutingOutcome,
) -> Vec<DimensionScore> {
    // Finding 5: an errored inference turn (transport/HTTP/parse) must be SKIPPED,
    // not scored. Otherwise an adversarial turn that errored with no tool calls
    // would score decoy_rejection = 1.0 (a false perfect), and a non-adversarial
    // one correct_tool_at_1 = 0.0 (a false miss) — both fabricated. This matches
    // the skip-on-error contract the other suites use.
    if outcome.error.is_some() {
        return Vec::new();
    }
    let dispatched: Vec<String> = outcome.tool_calls.iter().map(|(n, _)| n.clone()).collect();
    let mut rows = Vec::new();
    match scenario.category.as_str() {
        "tool_selection" => {
            if scenario.adversarial {
                rows.push(metric_row(
                    model_id,
                    backend_tag,
                    &scenario.id,
                    METRIC_DECOY_REJECT,
                    decoy_rejected(&dispatched),
                    &dispatched,
                ));
            } else {
                rows.push(metric_row(
                    model_id.clone(),
                    backend_tag,
                    &scenario.id,
                    METRIC_CORRECT_TOOL,
                    correct_tool_at_1(scenario.expected_tool.as_deref(), &dispatched),
                    &dispatched,
                ));
                rows.push(metric_row(
                    model_id,
                    backend_tag,
                    &scenario.id,
                    METRIC_PARAM_VALIDITY,
                    parameters_valid(&outcome.tool_calls),
                    &dispatched,
                ));
            }
        }
        "multi_step" => {
            rows.push(metric_row(
                model_id.clone(),
                backend_tag,
                &scenario.id,
                METRIC_MULTI_STEP,
                score_multi_step(&scenario.expected_tools, &dispatched),
                &dispatched,
            ));
            rows.push(metric_row(
                model_id,
                backend_tag,
                &scenario.id,
                METRIC_PARAM_VALIDITY,
                parameters_valid(&outcome.tool_calls),
                &dispatched,
            ));
        }
        _ => {}
    }
    rows
}

/// Score one (mock or live) tool-routing attempt and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "tool_routing")`.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    scenario: &Scenario,
    outcome: &RoutingOutcome,
) -> Result<usize, ToolError> {
    let rows = build_scores(model_id, backend_tag, scenario, outcome);
    let n = rows.len();
    for score in &rows {
        insert_dimension_score_with_category(pool, run_id, score, TASK_CATEGORY).await?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sc(id: &str, category: &str, adversarial: bool, expected_tool: Option<&str>, expected_tools: &[&str]) -> Scenario {
        Scenario {
            id: id.to_string(),
            category: category.to_string(),
            prompt: "p".to_string(),
            expected_tool: expected_tool.map(String::from),
            expected_tools: expected_tools.iter().map(|s| s.to_string()).collect(),
            adversarial,
            tool_count_band: vec![],
            check: None,
            must_not_fabricate: None,
        }
    }

    fn calls(pairs: &[(&str, serde_json::Value)]) -> Vec<(String, serde_json::Value)> {
        pairs.iter().map(|(n, a)| (n.to_string(), a.clone())).collect()
    }

    #[test]
    fn correct_tool_at_1_is_first_pick() {
        assert!(correct_tool_at_1(Some("weather"), &["weather".into()]));
        assert!(correct_tool_at_1(Some("weather"), &["weather".into(), "news".into()]));
        // a wrong FIRST pick fails even if the right tool appears later.
        assert!(!correct_tool_at_1(Some("weather"), &["news".into(), "weather".into()]));
        assert!(!correct_tool_at_1(Some("weather"), &[]));
        // expected none → must call nothing.
        assert!(correct_tool_at_1(None, &[]));
        assert!(!correct_tool_at_1(None, &["weather".into()]));
    }

    #[test]
    fn parameters_valid_requires_object_args() {
        assert!(parameters_valid(&calls(&[("weather", json!({"query": "Tampa"}))])));
        // empty args object is still a valid object.
        assert!(parameters_valid(&calls(&[("weather", json!({}))])));
        // no tool called → not valid (nothing to validate).
        assert!(!parameters_valid(&[]));
        // non-object args (a bare string that failed to parse) → invalid.
        assert!(!parameters_valid(&calls(&[("weather", json!("not-an-object"))])));
        assert!(!parameters_valid(&calls(&[("weather", serde_json::Value::Null)])));
    }

    #[test]
    fn decoy_rejected_only_when_no_tool_called() {
        assert!(decoy_rejected(&[]));
        assert!(!decoy_rejected(&["weather".into()]));
    }

    #[test]
    fn build_scores_tool_selection_emits_correct_and_param() {
        let s = sc("ts-1", "tool_selection", false, Some("weather"), &[]);
        let out = RoutingOutcome {
            tool_calls: calls(&[("weather", json!({"query": "Tampa"}))]),
            error: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &s, &out);
        assert_eq!(rows.len(), 2);
        let correct = rows.iter().find(|r| r.metric == METRIC_CORRECT_TOOL).unwrap();
        assert_eq!(correct.dimension, DIMENSION);
        assert_eq!(correct.value, 1.0);
        let param = rows.iter().find(|r| r.metric == METRIC_PARAM_VALIDITY).unwrap();
        assert_eq!(param.value, 1.0);
    }

    #[test]
    fn build_scores_wrong_tool_scores_zero() {
        let s = sc("ts-2", "tool_selection", false, Some("weather"), &[]);
        let out = RoutingOutcome {
            tool_calls: calls(&[("news_headlines", json!({}))]),
            error: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &s, &out);
        let correct = rows.iter().find(|r| r.metric == METRIC_CORRECT_TOOL).unwrap();
        assert_eq!(correct.value, 0.0);
    }

    #[test]
    fn build_scores_adversarial_emits_decoy_rejection_only() {
        let s = sc("ts-adv", "tool_selection", true, None, &[]);
        // model correctly declined to call any decoy tool.
        let out = RoutingOutcome { tool_calls: vec![], error: None };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &s, &out);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metric, METRIC_DECOY_REJECT);
        assert_eq!(rows[0].value, 1.0);
        // ...and it fails when it took the bait.
        let bait = RoutingOutcome { tool_calls: calls(&[("fetch_widget_3", json!({}))]), error: None };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &s, &bait);
        assert_eq!(rows[0].value, 0.0);
    }

    #[test]
    fn build_scores_multi_step_emits_success_and_param() {
        let s = sc("ms-1", "multi_step", false, None, &["google_calendar_today", "reminder_set"]);
        let out = RoutingOutcome {
            tool_calls: calls(&[
                ("google_calendar_today", json!({})),
                ("reminder_set", json!({"query": "meeting"})),
            ]),
            error: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &s, &out);
        assert_eq!(rows.len(), 2);
        let ms = rows.iter().find(|r| r.metric == METRIC_MULTI_STEP).unwrap();
        assert_eq!(ms.value, 1.0);
        // wrong order fails the multi-step subsequence scorer.
        let bad = RoutingOutcome {
            tool_calls: calls(&[("reminder_set", json!({})), ("google_calendar_today", json!({}))]),
            error: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &s, &bad);
        assert_eq!(rows.iter().find(|r| r.metric == METRIC_MULTI_STEP).unwrap().value, 0.0);
    }

    // Finding 5: an errored adversarial turn (no tool calls) must NOT be scored
    // decoy_rejection = 1.0 — it must produce NO rows (skip-on-error).
    #[test]
    fn build_scores_skips_errored_turn() {
        let s = sc("ts-adv", "tool_selection", true, None, &[]);
        let errored = RoutingOutcome {
            tool_calls: vec![],
            error: Some("transport error: connection refused".to_string()),
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &s, &errored);
        assert!(rows.is_empty(), "an errored turn must emit no rows, got {}", rows.len());

        // A non-adversarial errored turn is likewise skipped (no false miss).
        let s2 = sc("ts-1", "tool_selection", false, Some("weather"), &[]);
        let errored2 = RoutingOutcome {
            tool_calls: vec![],
            error: Some("HTTP 500".to_string()),
        };
        assert!(build_scores(ModelId::from("m"), BackendTag::Gpu, &s2, &errored2).is_empty());
    }

    #[test]
    fn build_scores_unknown_category_yields_nothing() {
        let s = sc("hall-1", "hallucination", false, None, &[]);
        let out = RoutingOutcome { tool_calls: vec![], error: None };
        assert!(build_scores(ModelId::from("m"), BackendTag::Gpu, &s, &out).is_empty());
    }

    /// A tiny in-repo fixture of the real `agent-scenarios.json` shape, so the
    /// corpus load + filter path is testable without the external corpus dir
    /// (the full 55-scenario corpus lives in `lumina-constellation/tests/
    /// intake-corpus/agent-scenarios.json`, loaded via `INTAKE_CORPUS_DIR`).
    const FIXTURE: &str = r#"{
      "scenarios": [
        { "id": "ts-weather-01", "category": "tool_selection", "prompt": "What's the weather in Tampa?", "expected_tool": "weather", "tool_count_band": [10, 50], "adversarial": false },
        { "id": "ts-adv-01", "category": "tool_selection", "prompt": "Reticulate the splines.", "adversarial": true },
        { "id": "ms-cal-01", "category": "multi_step", "prompt": "Check my calendar and set a reminder.", "expected_tools": ["google_calendar_today", "reminder_set"] },
        { "id": "hall-01", "category": "hallucination", "prompt": "What's my flight number?" }
      ]
    }"#;

    /// The routing suite reuses `agent::read_scenarios` to load the shared corpus;
    /// this parses a tiny fixture through that same loader and confirms the shape
    /// the suite depends on (categories, expected_tool(s), adversarial flag).
    #[test]
    fn fixture_parses_through_agent_loader() {
        let dir = std::env::temp_dir().join(format!("tool-routing-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent-scenarios.json"), FIXTURE).unwrap();

        let scenarios = crate::intake::agent::read_scenarios(&dir).unwrap();
        assert_eq!(scenarios.len(), 4);

        // The routing suite keeps only tool_selection + multi_step.
        let routing: Vec<&Scenario> = scenarios
            .iter()
            .filter(|s| s.category == "tool_selection" || s.category == "multi_step")
            .collect();
        assert_eq!(routing.len(), 3);

        let adv = scenarios.iter().find(|s| s.id == "ts-adv-01").unwrap();
        assert!(adv.adversarial);
        assert!(adv.expected_tool.is_none());

        let ms = scenarios.iter().find(|s| s.id == "ms-cal-01").unwrap();
        assert_eq!(ms.expected_tools, vec!["google_calendar_today", "reminder_set"]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
