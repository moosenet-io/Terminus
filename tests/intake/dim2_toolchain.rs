//! Integration tests for S84 ASMT-03 (Dimension 2 — tool chaining,
//! conversational / implicit-intent).
//!
//! These exercise the dim-2 runner + deterministic scorer through its PUBLIC
//! surface only (mocked tool dispatch), with no DB and no network — proving:
//!   - a conversational scenario yields a correct chain-accuracy score,
//!   - wrong-order and spurious-call are recorded DISTINCTLY from a clean pass,
//!   - wrong-arg-handoff is detected,
//!   - the embedded conversational corpus is schema-valid and is a 3–5 step chain,
//!   - the S83 explicit scenarios are referenced BY ID and NOT duplicated into the
//!     conversational corpus (the lint-style anti-duplication guard).
//!
//! The live path (`LiveToolModel`) routes through the unified tool-dispatch proxy
//! (`context::chat_with_tools` → `infer`/`infer_with_metrics`); here a
//! deterministic mock is substituted so the test is hermetic.

use async_trait::async_trait;
use serde_json::{json, Value};

use terminus_rs::intake::assistant::dim2_toolchain::{
    self as dim2, ChainOutcome, ConversationToolModel, ConversationalCorpus, DispatchedCall,
    DIMENSION, JUDGE_DETERMINISTIC, METRIC_CHAIN_ACCURACY, METRIC_CONVERSATIONAL_PASS_RATE,
    METRIC_MEAN_CHAIN_ACCURACY, METRIC_SCENARIO_PASS,
};
use terminus_rs::intake::assistant::{BackendTag, ModelId};

// ── Mock tool model: returns a scripted dispatch keyed by first-turn text ──

struct MockToolModel {
    plan: std::collections::BTreeMap<String, Vec<DispatchedCall>>,
}

#[async_trait]
impl ConversationToolModel for MockToolModel {
    async fn dispatch(&self, turns: &[String], _catalog: &Value) -> Vec<DispatchedCall> {
        let key = turns.first().cloned().unwrap_or_default();
        self.plan.get(&key).cloned().unwrap_or_default()
    }
}

fn call(tool: &str, args: Value) -> DispatchedCall {
    DispatchedCall::new(tool, args)
}

// ===========================================================================

#[test]
fn embedded_conversational_corpus_is_schema_valid() {
    let corpus = dim2::load_corpus().expect("corpus parses");
    let problems = dim2::validate_corpus(&corpus);
    assert!(problems.is_empty(), "corpus problems: {problems:?}");
    // 3–5 conversational scenarios, each a 3–5 step chain.
    assert!((3..=5).contains(&corpus.scenarios.len()));
    for s in &corpus.scenarios {
        assert!((3..=5).contains(&s.expected_chain.len()), "{}", s.id);
        assert!(!s.turns.is_empty(), "{}", s.id);
    }
    // References S83 explicit scenarios by id (not by body).
    assert!(!corpus.s83_reference_ids.is_empty());
}

/// CRITICAL anti-duplication guard: the S83 explicit agent scenarios must be
/// referenced BY ID, never copied into the conversational corpus. This test
/// FAILS if any S83 `multi_step` scenario body (its distinctive prompt text) is
/// found embedded in `toolchain_conversational.json`.
#[test]
fn no_s83_scenario_bodies_duplicated() {
    // The raw conversational corpus text.
    let corpus_text = include_str!("../../src/intake/assistant/corpora/toolchain_conversational.json");
    let corpus_lc = corpus_text.to_lowercase();

    // Distinctive verbatim fragments from S83 multi_step scenario *bodies*
    // (agent-scenarios.json). If ANY appears in our corpus, a body was copied in.
    // These are the exact prompts of the S83 multi_step scenarios we reference.
    let s83_bodies = [
        "check the weather in tampa and suggest what i should pack",
        "check my calendar and set a reminder for my next meeting",
        "check my commute and tell me if i'll make my first meeting on time",
        "look at what's in my pantry and add anything i'm low on to the shopping list",
        "what's the top tech headline today, and can you find more detail on it",
        "check my email and remind me to reply to anything urgent this afternoon",
        "i have a trip on my calendar this week",
    ];
    for body in s83_bodies {
        assert!(
            !corpus_lc.contains(body),
            "S83 scenario body duplicated into conversational corpus: {body:?} — \
             reference S83 scenarios by id, do not copy their prompts"
        );
    }

    // And positively: the corpus DOES carry the S83 reference ids.
    let corpus = dim2::load_corpus().unwrap();
    assert!(corpus.s83_reference_ids.iter().all(|id| id.starts_with("ms-")));
    assert!(!corpus.s83_reference_ids.is_empty());
}

#[tokio::test]
async fn conversational_scenario_yields_correct_chain_accuracy() {
    let corpus = dim2::load_corpus().unwrap();
    let trip = corpus
        .scenarios
        .iter()
        .find(|s| s.id == "cv-trip-pack-01")
        .unwrap();

    // Clean pass: weather -> calendar -> reminder with the destination carried.
    let mut plan = std::collections::BTreeMap::new();
    plan.insert(
        trip.turns[0].clone(),
        vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("google_calendar_week", json!({"query": "this weekend"})),
            call("reminder_set", json!({"query": "pack for the weekend in Zubrovka"})),
        ],
    );
    let model = MockToolModel { plan };

    let outcome = dim2::run_dim2(&model, &corpus, vec![]).await;
    let trip_score = outcome
        .per_scenario
        .iter()
        .find(|s| s.scenario_id == "cv-trip-pack-01")
        .unwrap();
    assert_eq!(trip_score.outcome, ChainOutcome::CleanPass);
    assert!(trip_score.passed());
    assert!((trip_score.chain_accuracy - 1.0).abs() < 1e-9);

    // Storage rows: every row deterministic; aggregates present.
    let rows = outcome.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Gpu);
    assert!(rows.iter().all(|r| r.judge == JUDGE_DETERMINISTIC));
    assert!(rows.iter().all(|r| r.dimension == DIMENSION));
    assert!(rows.iter().all(|r| r.std_dev.is_none())); // deterministic — no SD
    assert!(rows.iter().any(|r| r.metric == METRIC_CONVERSATIONAL_PASS_RATE));
    assert!(rows.iter().any(|r| r.metric == METRIC_MEAN_CHAIN_ACCURACY));
    let pass_row = rows
        .iter()
        .find(|r| r.metric == format!("{}:{}", METRIC_SCENARIO_PASS, "cv-trip-pack-01"))
        .unwrap();
    assert_eq!(pass_row.value, 1.0);
    let acc_row = rows
        .iter()
        .find(|r| r.metric == format!("{}:{}", METRIC_CHAIN_ACCURACY, "cv-trip-pack-01"))
        .unwrap();
    assert!((acc_row.value - 1.0).abs() < 1e-9);
}

#[tokio::test]
async fn wrong_order_and_spurious_call_recorded_distinctly() {
    let corpus = dim2::load_corpus().unwrap();
    let trip = corpus
        .scenarios
        .iter()
        .find(|s| s.id == "cv-trip-pack-01")
        .unwrap();

    // Wrong order: reminder before calendar (right tools, no spurious).
    let mut plan = std::collections::BTreeMap::new();
    plan.insert(
        trip.turns[0].clone(),
        vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("reminder_set", json!({"query": "pack for the weekend"})),
            call("google_calendar_week", json!({"query": "weekend"})),
        ],
    );
    let model = MockToolModel { plan };
    let outcome = dim2::run_dim2(&model, &corpus, vec![]).await;
    let s = outcome
        .per_scenario
        .iter()
        .find(|s| s.scenario_id == "cv-trip-pack-01")
        .unwrap();
    assert_eq!(s.outcome, ChainOutcome::WrongOrder);
    assert!(!s.passed(), "wrong-order must NOT read as a clean pass");

    // Spurious call: correct chain plus a hallucinated extra tool.
    let mut plan2 = std::collections::BTreeMap::new();
    plan2.insert(
        trip.turns[0].clone(),
        vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("google_calendar_week", json!({"query": "weekend"})),
            call("reminder_set", json!({"query": "pack for the weekend in Zubrovka"})),
            call("book_flight", json!({"query": "Zubrovka"})),
        ],
    );
    let model2 = MockToolModel { plan: plan2 };
    let outcome2 = dim2::run_dim2(&model2, &corpus, vec![]).await;
    let s2 = outcome2
        .per_scenario
        .iter()
        .find(|s| s.scenario_id == "cv-trip-pack-01")
        .unwrap();
    assert_eq!(s2.outcome, ChainOutcome::SpuriousCall);
    assert!(!s2.passed(), "spurious-call must NOT read as a clean pass");
    assert_eq!(s2.spurious_tools, vec!["book_flight".to_string()]);

    // The two failure modes are distinct from each other and from a clean pass.
    assert_ne!(s.outcome, s2.outcome);
    assert_ne!(s.outcome, ChainOutcome::CleanPass);
    assert_ne!(s2.outcome, ChainOutcome::CleanPass);
}

#[tokio::test]
async fn missing_arg_handoff_is_detected() {
    let corpus = dim2::load_corpus().unwrap();
    let trip = corpus
        .scenarios
        .iter()
        .find(|s| s.id == "cv-trip-pack-01")
        .unwrap();
    // Correct tools/order/stopped, but the carried value never reaches reminder args.
    let mut plan = std::collections::BTreeMap::new();
    plan.insert(
        trip.turns[0].clone(),
        vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("google_calendar_week", json!({"query": "this weekend"})),
            call("reminder_set", json!({"query": "do the thing"})),
        ],
    );
    let model = MockToolModel { plan };
    let outcome = dim2::run_dim2(&model, &corpus, vec![]).await;
    let s = outcome
        .per_scenario
        .iter()
        .find(|s| s.scenario_id == "cv-trip-pack-01")
        .unwrap();
    assert_eq!(s.outcome, ChainOutcome::HandoffMissing);
    assert!(!s.correct_handoff);
    assert!(s.completed && s.correct_tools && s.correct_order && s.stopped);
}

#[tokio::test]
async fn missing_s83_reference_is_recorded_not_a_crash() {
    // A referenced S83 scenario absent from the corpus → recorded in s83_missing,
    // conversational chains still run (spec EDGE CASE).
    let corpus: ConversationalCorpus = serde_json::from_str(
        r#"{
          "s83_reference_ids": ["ms-does-not-exist-99"],
          "scenarios": [{
            "id":"cv-x","turns":["a","b"],
            "expected_chain":["weather","google_calendar_today","reminder_set"],
            "stop_after":3,
            "handoffs":[{"from_step":0,"to_step":2,"to_tool":"reminder_set","expect_arg_substrings":["x"]}]
          }]
        }"#,
    )
    .unwrap();
    let model = MockToolModel { plan: std::collections::BTreeMap::new() };
    let outcome = dim2::run_dim2(&model, &corpus, vec!["ms-does-not-exist-99".to_string()]).await;
    assert_eq!(outcome.per_scenario.len(), 1); // conversational chain still ran
    assert_eq!(outcome.s83_missing_ids, vec!["ms-does-not-exist-99".to_string()]);
    // The missing id surfaces in the aggregate audit, not as a crash.
    let rows = outcome.into_dimension_scores(&ModelId::from("m"), BackendTag::Cpu);
    let agg = rows
        .iter()
        .find(|r| r.metric == METRIC_CONVERSATIONAL_PASS_RATE)
        .unwrap();
    assert!(agg.raw_json.as_ref().unwrap().contains("ms-does-not-exist-99"));
}
