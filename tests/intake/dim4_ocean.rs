//! Integration tests for S84 ASMT-05 (Dimension 4 — latent personality / OCEAN).
//!
//! These exercise the dim-4 runner through its PUBLIC surface only (mocked RAW
//! model + mocked judge panel), with no DB and no network — proving a full dim-4
//! run yields five OCEAN trait rows (mean + SD) plus a derived proximity-to-Lumina
//! NOTE that is its OWN row and never merged into a dim-5 score; that RAW runs are
//! BASE-ONLY (no Lumina layer ever reaches the model); that degraded/empty replies
//! are still scored (not a crash); and that the embedded corpus is schema-valid.
//!
//! The live path (`RawModel`) routes through Chord's unified inference proxy
//! (`context::generate` → `infer::infer_with_metrics`, P5 backend routing); here we
//! substitute a deterministic mock so the test is hermetic.

use std::collections::BTreeSet;
use std::sync::Mutex;

use async_trait::async_trait;

use terminus_rs::intake::assistant::dim4_ocean::{
    self as dim4, OceanCorpus, OceanModel, RawReply, DIMENSION, METRIC_PROXIMITY, OCEAN_TRAITS,
};
use terminus_rs::intake::assistant::judges::{Judge, JudgeReply};
use terminus_rs::intake::assistant::{BackendTag, ModelId};

// ── Mock RAW model: asserts base-only, scriptable degradation ──

struct MockModel {
    degrade: BTreeSet<String>,
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl OceanModel for MockModel {
    async fn generate(&self, scenario_prompt: &str) -> RawReply {
        // The runner must NEVER send a prompt carrying a Lumina layer.
        assert!(
            dim4::is_base_only(scenario_prompt),
            "runner sent a non-base (Lumina-layered) prompt to the model"
        );
        self.seen.lock().unwrap().push(scenario_prompt.to_string());
        let corpus = dim4::load_corpus().unwrap();
        let id = corpus
            .scenarios
            .iter()
            .find(|s| s.prompt == scenario_prompt)
            .map(|s| s.id.clone())
            .unwrap_or_default();
        if self.degrade.contains(&id) {
            return RawReply {
                text: String::new(),
                degraded: true,
                degrade_reason: Some("forced timeout".into()),
            };
        }
        RawReply {
            text: "A thoughtful, balanced reply.".into(),
            degraded: false,
            degrade_reason: None,
        }
    }
}

// ── Mock judge: returns a fixed integer for whichever single trait it is asked ──

struct FixedJudge {
    id: String,
    score: i64,
}

#[async_trait]
impl Judge for FixedJudge {
    fn id(&self) -> &str {
        &self.id
    }
    async fn invoke(&self, prompt: &str, _attempt: u8) -> JudgeReply {
        let trait_name = OCEAN_TRAITS
            .iter()
            .find(|t| prompt.contains(&format!("trait \"{t}\"")))
            .copied()
            .unwrap_or("openness");
        JudgeReply::Text(format!("{{\"{trait_name}\": {}}}", self.score))
    }
}

fn panel(scores: [i64; 3]) -> Vec<Box<dyn Judge>> {
    vec![
        Box::new(FixedJudge { id: "claude".into(), score: scores[0] }),
        Box::new(FixedJudge { id: "gemini".into(), score: scores[1] }),
        Box::new(FixedJudge { id: "codex".into(), score: scores[2] }),
    ]
}

fn full_corpus() -> OceanCorpus {
    dim4::load_corpus().expect("embedded corpus parses")
}

// ===========================================================================

#[test]
fn embedded_corpus_is_schema_valid_and_covers_five_traits() {
    let corpus = full_corpus();
    let problems = dim4::validate_corpus(&corpus);
    assert!(problems.is_empty(), "corpus problems: {problems:?}");
    for t in OCEAN_TRAITS {
        assert!(corpus.scenarios.iter().any(|s| s.trait_name == t), "{t} uncovered");
    }
}

#[tokio::test]
async fn raw_runs_are_base_only_no_lumina_layer() {
    // The MockModel asserts base-only on every call; a full run must complete
    // without tripping that assertion, and must have actually sent prompts.
    let model = MockModel { degrade: BTreeSet::new(), seen: Mutex::new(Vec::new()) };
    let _ = dim4::run_dim4(&model, &panel([3, 3, 3]), &full_corpus()).await;
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), full_corpus().scenarios.len());
    assert!(seen.iter().all(|p| dim4::is_base_only(p)));
}

#[tokio::test]
async fn full_dim4_run_yields_five_trait_rows_and_separate_proximity_note() {
    let model = MockModel { degrade: BTreeSet::new(), seen: Mutex::new(Vec::new()) };
    // [3,4,5] per trait → mean 4.0, SD 1.0.
    let outcome = dim4::run_dim4(&model, &panel([3, 4, 5]), &full_corpus()).await;
    assert_eq!(outcome.panel.complying, 3);

    for t in OCEAN_TRAITS {
        let agg = &outcome.panel.aggregates[t];
        assert!((agg.mean - 4.0).abs() < 1e-9, "{t} mean");
        assert!((agg.std_dev.unwrap() - 1.0).abs() < 1e-9, "{t} sd");
    }

    let rows = outcome.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Cpu);
    // Exactly five trait rows on dimension personality_latent.
    let trait_rows: Vec<_> = rows
        .iter()
        .filter(|r| OCEAN_TRAITS.contains(&r.metric.as_str()))
        .collect();
    assert_eq!(trait_rows.len(), 5);
    assert!(trait_rows.iter().all(|r| r.dimension == DIMENSION));
    assert!(trait_rows.iter().all(|r| r.judge == "panel"));

    // The proximity note is its OWN row, judge "derived", NOT merged into a trait.
    let prox = rows.iter().find(|r| r.metric == METRIC_PROXIMITY).expect("proximity row");
    assert_eq!(prox.judge, "derived");
    assert_eq!(prox.dimension, DIMENSION);
    assert!(prox.std_dev.is_none());
    // The proximity row is distinct from every OCEAN trait row.
    assert!(!OCEAN_TRAITS.contains(&prox.metric.as_str()));
}

#[tokio::test]
async fn high_disagreement_preserves_sd() {
    let model = MockModel { degrade: BTreeSet::new(), seen: Mutex::new(Vec::new()) };
    // [1,3,5] → mean 3.0, SD 2.0. Must NOT collapse to the mean.
    let outcome = dim4::run_dim4(&model, &panel([1, 3, 5]), &full_corpus()).await;
    let agg = &outcome.panel.aggregates["neuroticism"];
    assert!((agg.mean - 3.0).abs() < 1e-9);
    assert!((agg.std_dev.unwrap() - 2.0).abs() < 1e-9);
}

#[tokio::test]
async fn degraded_and_refusing_replies_are_scored_not_crashed() {
    // Force every scenario to degrade (empty/refusal reply). Judges still score the
    // empty reply as a low-trait reading; the run produces data, never a crash.
    let all_ids: BTreeSet<String> =
        full_corpus().scenarios.iter().map(|s| s.id.clone()).collect();
    let model = MockModel { degrade: all_ids, seen: Mutex::new(Vec::new()) };
    let outcome = dim4::run_dim4(&model, &panel([1, 1, 1]), &full_corpus()).await;
    // every scenario recorded a degradation reason (data, not error)
    assert_eq!(outcome.degradations.len(), full_corpus().scenarios.len());
    assert_eq!(outcome.panel.complying, 3);
    // low readings flowed through to the aggregate
    assert!((outcome.panel.aggregates["openness"].mean - 1.0).abs() < 1e-9);
    // a full set of rows is still emitted
    let rows = outcome.into_dimension_scores(&ModelId::from("m"), BackendTag::Gpu);
    assert!(rows.iter().any(|r| r.metric == METRIC_PROXIMITY));
}

#[tokio::test]
async fn all_judges_abstain_yields_unscored_and_no_proximity() {
    // A panel that never returns valid JSON → all abstain → item unscored.
    struct GarbageJudge(String);
    #[async_trait]
    impl Judge for GarbageJudge {
        fn id(&self) -> &str {
            &self.0
        }
        async fn invoke(&self, _p: &str, _a: u8) -> JudgeReply {
            JudgeReply::Text("I will not comply.".into())
        }
    }
    let model = MockModel { degrade: BTreeSet::new(), seen: Mutex::new(Vec::new()) };
    let judges: Vec<Box<dyn Judge>> = vec![
        Box::new(GarbageJudge("claude".into())),
        Box::new(GarbageJudge("gemini".into())),
        Box::new(GarbageJudge("codex".into())),
    ];
    let outcome = dim4::run_dim4(&model, &judges, &full_corpus()).await;
    assert!(outcome.panel.is_unscored());
    // no traits scored → no proximity note, no rows at all
    assert!(outcome.proximity_to_lumina().is_none());
    let rows = outcome.into_dimension_scores(&ModelId::from("m"), BackendTag::Cpu);
    assert!(rows.is_empty());
}
