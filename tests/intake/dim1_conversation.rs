//! Integration tests for S84 ASMT-02 (Dimension 1 — conversation depth).
//!
//! These exercise the dim-1 runner through its PUBLIC surface only (mocked model
//! + mocked judge panel), with no DB and no network — proving a full dim-1 run
//! yields a recall curve + a coherence mean/SD row, that timeout/truncation is
//! recorded as degradation depth (not a crash), and that the embedded corpus is
//! schema-valid. The live path (`LiveModel`) routes through the unified inference
//! proxy (`context::generate` → `infer::infer_with_metrics`); here we substitute
//! a deterministic mock so the test is hermetic.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use terminus_rs::intake::assistant::dim1_conversation::{
    self as dim1, ConversationCorpus, ConversationModel, TurnReply, COHERENCE_TRAIT, DIMENSION,
    METRIC_RECALL_CEILING,
};
use terminus_rs::intake::assistant::judges::{Judge, JudgeReply};
use terminus_rs::intake::assistant::{BackendTag, ModelId};

// ── Mock model: scripted recall answers + optional forced degradation ──

struct MockModel {
    answers: BTreeMap<String, String>,
    degrade_at: Option<usize>,
    turn: Mutex<usize>,
}

impl MockModel {
    fn new(answers: BTreeMap<String, String>, degrade_at: Option<usize>) -> Self {
        MockModel { answers, degrade_at, turn: Mutex::new(0) }
    }
}

#[async_trait]
impl ConversationModel for MockModel {
    async fn respond(&self, _history: &[(String, String)], user_text: &str) -> TurnReply {
        let n = {
            let mut g = self.turn.lock().unwrap();
            *g += 1;
            *g
        };
        if Some(n) == self.degrade_at {
            return TurnReply {
                text: String::new(),
                degraded: true,
                degrade_reason: Some("forced timeout".into()),
            };
        }
        for (k, v) in &self.answers {
            if user_text.to_lowercase().contains(&k.to_lowercase()) {
                return TurnReply { text: v.clone(), degraded: false, degrade_reason: None };
            }
        }
        TurnReply { text: "Here is a friendly suggestion.".into(), degraded: false, degrade_reason: None }
    }
}

// ── Mock judge: returns a fixed coherence integer ──

struct FixedJudge {
    id: String,
    score: i64,
}

#[async_trait]
impl Judge for FixedJudge {
    fn id(&self) -> &str {
        &self.id
    }
    async fn invoke(&self, _prompt: &str, _attempt: u8) -> JudgeReply {
        JudgeReply::Text(format!("{{\"coherence\": {}}}", self.score))
    }
}

fn panel(scores: [i64; 3]) -> Vec<Box<dyn Judge>> {
    vec![
        Box::new(FixedJudge { id: "claude".into(), score: scores[0] }),
        Box::new(FixedJudge { id: "gemini".into(), score: scores[1] }),
        Box::new(FixedJudge { id: "codex".into(), score: scores[2] }),
    ]
}

// ===========================================================================

#[test]
fn embedded_corpus_is_schema_valid() {
    let corpus = dim1::load_corpus().expect("corpus parses");
    let problems = dim1::validate_corpus(&corpus);
    assert!(problems.is_empty(), "corpus problems: {problems:?}");
    assert_eq!(corpus.conversations.len(), 5);
}

#[tokio::test]
async fn full_dim1_run_produces_recall_curve_and_coherence_mean_sd() {
    // A model that recalls the planted city when asked.
    let mut answers = BTreeMap::new();
    answers.insert("which city".to_string(), "You said the city is Zubrovka.".to_string());
    let model = MockModel::new(answers, None);

    let corpus: ConversationCorpus = serde_json::from_str(
        r#"{
          "recall_threshold": 0.6,
          "conversations": [{
            "id":"c","length_turns":4,"coherence_sample_turns":[2],
            "facts":[{"key":"city","plant_turn":1,"value":"Zubrovka","statement":"s"}],
            "turns":[
              {"index":1,"role":"user","text":"My city is Zubrovka","plants":["city"]},
              {"index":2,"role":"user","text":"any tips for a weekend?"},
              {"index":3,"role":"user","text":"more tips?"},
              {"index":4,"role":"user","text":"which city did I name?","probes":[{"key":"city","kind":"verbatim","expect_substrings":["zubrovka"]}]}
            ]
          }]
        }"#,
    )
    .unwrap();

    let outcome = dim1::run_dim1(&model, &panel([4, 5, 3]), &corpus).await;

    // Recall axis: hit at depth 4 → ceiling 4, no degradation.
    assert_eq!(outcome.recall_ceiling_turns, 4);
    assert!(outcome.per_conversation[0].degraded_at.is_none());
    assert!(!outcome.per_conversation[0].recall.curve.is_empty());

    // Coherence axis: [4,5,3] → mean 4.0, SD 1.0.
    let coh = outcome.coherence.as_ref().expect("coherence present");
    assert_eq!(coh.complying, 3);
    let agg = &coh.aggregates[COHERENCE_TRAIT];
    assert!((agg.mean - 4.0).abs() < 1e-9);
    assert!((agg.std_dev.unwrap() - 1.0).abs() < 1e-9);

    // Storage rows: one deterministic recall_ceiling row + one panel coherence row.
    let rows = outcome.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Cpu);
    let recall_row = rows
        .iter()
        .find(|r| r.metric == METRIC_RECALL_CEILING)
        .expect("recall row");
    assert_eq!(recall_row.value, 4.0);
    assert_eq!(recall_row.judge, "deterministic");
    assert_eq!(recall_row.dimension, DIMENSION);
    let coh_row = rows.iter().find(|r| r.metric == COHERENCE_TRAIT).expect("coherence row");
    assert_eq!(coh_row.judge, "panel");
    assert!((coh_row.value - 4.0).abs() < 1e-9);
    assert!((coh_row.std_dev.unwrap() - 1.0).abs() < 1e-9);
}

#[tokio::test]
async fn timeout_at_depth_records_degradation_not_crash() {
    // Force degradation at turn 3; the probe at turn 4 is never evaluated.
    let model = MockModel::new(BTreeMap::new(), Some(3));
    let corpus: ConversationCorpus = serde_json::from_str(
        r#"{
          "recall_threshold": 0.6,
          "conversations": [{
            "id":"c","length_turns":4,"coherence_sample_turns":[2],
            "facts":[{"key":"city","plant_turn":1,"value":"Zubrovka","statement":"s"}],
            "turns":[
              {"index":1,"role":"user","text":"My city is Zubrovka","plants":["city"]},
              {"index":2,"role":"user","text":"any tips?"},
              {"index":3,"role":"user","text":"more tips?"},
              {"index":4,"role":"user","text":"which city did I name?","probes":[{"key":"city","kind":"verbatim","expect_substrings":["zubrovka"]}]}
            ]
          }]
        }"#,
    )
    .unwrap();

    // Must complete (no panic) and record degradation at depth 3.
    let outcome = dim1::run_dim1(&model, &panel([3, 3, 3]), &corpus).await;
    let run = &outcome.per_conversation[0];
    assert_eq!(run.degraded_at, Some(3));
    // Recall ceiling collapses (the deep probe was never reached).
    assert_eq!(outcome.recall_ceiling_turns, 0);
    // A recall row is still emitted — the run produced data, not a crash.
    let rows = outcome.into_dimension_scores(&ModelId::from("m"), BackendTag::Gpu);
    assert!(rows.iter().any(|r| r.metric == METRIC_RECALL_CEILING));
}

#[tokio::test]
async fn refusal_and_empty_responses_count_as_recall_miss() {
    // Model never recalls (default reply lacks the planted value).
    let model = MockModel::new(BTreeMap::new(), None);
    let corpus = dim1::load_corpus().unwrap();
    let outcome = dim1::run_dim1(&model, &panel([5, 5, 5]), &corpus).await;
    // No planted fact is ever surfaced → ceiling 0 across the whole corpus.
    assert_eq!(outcome.recall_ceiling_turns, 0);
    // Coherence still scores (responses are coherent even if non-recalling).
    assert!(outcome.coherence.is_some());
}

#[tokio::test]
async fn high_panel_disagreement_preserves_sd() {
    let mut answers = BTreeMap::new();
    answers.insert("which city".to_string(), "Zubrovka.".to_string());
    let model = MockModel::new(answers, None);
    let corpus: ConversationCorpus = serde_json::from_str(
        r#"{
          "recall_threshold": 0.6,
          "conversations": [{
            "id":"c","length_turns":4,"coherence_sample_turns":[2],
            "facts":[{"key":"city","plant_turn":1,"value":"Zubrovka","statement":"s"}],
            "turns":[
              {"index":1,"role":"user","text":"My city is Zubrovka","plants":["city"]},
              {"index":2,"role":"user","text":"tips?"},
              {"index":3,"role":"user","text":"more?"},
              {"index":4,"role":"user","text":"which city did I name?","probes":[{"key":"city","kind":"verbatim","expect_substrings":["zubrovka"]}]}
            ]
          }]
        }"#,
    )
    .unwrap();
    // Judges wildly disagree: [1,3,5] → mean 3.0, SD 2.0. SD must NOT collapse.
    let outcome = dim1::run_dim1(&model, &panel([1, 3, 5]), &corpus).await;
    let coh = outcome.coherence.as_ref().unwrap();
    let agg = &coh.aggregates[COHERENCE_TRAIT];
    assert!((agg.mean - 3.0).abs() < 1e-9);
    assert!((agg.std_dev.unwrap() - 2.0).abs() < 1e-9);
}
