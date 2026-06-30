//! Integration tests for S84 ASMT-04 (Dimension 3 — memory integration,
//! multi-session).
//!
//! These exercise the dim-3 runner through its PUBLIC surface only (mocked
//! generation), driving the REAL S78 3-tier conversation pipeline
//! (`lumina_core::conversation::buffer::ConversationBuffer`: verbatim buffer +
//! progressive summarization) with NO DB and NO network. They prove:
//!   - a full plant→summarize→recall runs end-to-end against the real buffer and
//!     produces a `fact_survival_rate`,
//!   - survival is split by summarized-vs-buffer tier (the real summarization
//!     boundary, verified against the buffer, decides each fact's tier),
//!   - a model that drops a summarized fact lowers `summarized_survival_rate`
//!     independently of `buffer_survival_rate` (isolating recall-from-summary),
//!   - conflation (recall returns a sibling fact's value) is flagged + scored as a miss,
//!   - the fixed summarizer id is recorded in run metadata,
//!   - results are keyed on the S83-consistent model_id + backend_tag,
//!   - the embedded corpus is schema-valid.
//!
//! The live path (`LiveMemoryModel`) routes both candidate recall AND the fixed
//! summarizer through the unified inference proxy (`context::generate` →
//! `infer`/`infer_with_metrics`); here a deterministic mock is substituted so the
//! test is hermetic.

use async_trait::async_trait;

use terminus_rs::intake::assistant::dim3_memory::{
    self as dim3, FactTier, MemoryCorpus, MemoryModel, DIMENSION, JUDGE_DETERMINISTIC,
    METRIC_BUFFER_SURVIVAL_RATE, METRIC_FACT_SURVIVAL_RATE, METRIC_SUMMARIZED_SURVIVAL_RATE,
};
use terminus_rs::intake::assistant::{BackendTag, ModelId};

// ── Mock memory model ──────────────────────────────────────────────────────
//
// `answer` recalls a fact iff its (lowercased) expected value is reachable from
// the assembled `context` (the real buffer's summary + verbatim turns) AND the
// candidate is configured to "remember" that tier. This lets a test simulate a
// candidate that recalls buffer facts but loses summarized ones (poor
// recall-from-summary) without ever touching the network.
//
// `summarize` is the FIXED summarizer: it can either PRESERVE facts (echo the
// turns' content, so values survive into the summary) or DROP them (return a lossy
// summary), exercising the "summarization drops a fact = pipeline finding" edge.

struct MockMemoryModel {
    /// If true, the summarizer preserves verbatim turn content into the summary.
    summarizer_preserves: bool,
    /// If true, the candidate can recall facts that ended up in a summary block.
    candidate_recalls_summary: bool,
    /// Optional forced conflation: when answering, inject this literal regardless.
    forced_answer: Option<String>,
}

#[async_trait]
impl MemoryModel for MockMemoryModel {
    async fn answer(&self, context: &str, question: &str) -> String {
        if let Some(forced) = &self.forced_answer {
            return forced.clone();
        }
        // The mock "recalls" by surfacing the relevant span of context. A summary
        // block is prefixed "[Earlier conversation summary]"; verbatim turns are not.
        // If the candidate can't recall from summary, strip summary lines first.
        let usable: String = context
            .lines()
            .filter(|l| {
                self.candidate_recalls_summary
                    || !l.contains("[Earlier conversation summary]")
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Echo back the usable context — the deterministic scorer's normalized
        // substring match then decides survival exactly as it would for a real model.
        format!("Based on our chat ({question}): {usable}")
    }

    async fn summarize(&self, turns: &[(String, String)]) -> String {
        if self.summarizer_preserves {
            // Lossless: carry every fact value forward verbatim.
            let mut s = String::from("Earlier we discussed: ");
            for (u, a) in turns {
                s.push_str(u);
                s.push(' ');
                s.push_str(a);
                s.push(' ');
            }
            s
        } else {
            // Lossy: a generic summary that preserves no specific fact value.
            "Earlier the user discussed some plans and preferences.".to_string()
        }
    }
}

// ===========================================================================

#[test]
fn embedded_corpus_is_schema_valid() {
    let corpus = dim3::load_corpus().expect("corpus parses");
    let problems = dim3::validate_corpus(&corpus);
    assert!(problems.is_empty(), "corpus problems: {problems:?}");
    assert!(!corpus.scripts.is_empty());
    // Every script forces at least one summarization cycle and plants both tiers.
    for s in &corpus.scripts {
        assert!(
            s.session1.len() >= corpus.summarization_threshold,
            "{} must force a summarization cycle",
            s.id
        );
        assert!(s.facts.iter().any(|f| f.tier == FactTier::Summarized), "{}", s.id);
        assert!(s.facts.iter().any(|f| f.tier == FactTier::Buffer), "{}", s.id);
    }
}

#[tokio::test]
async fn lossless_pipeline_high_survival_across_real_buffer() {
    let corpus = dim3::load_corpus().unwrap();
    // Fixed summarizer preserves facts; candidate recalls from both tiers.
    let model = MockMemoryModel {
        summarizer_preserves: true,
        candidate_recalls_summary: true,
        forced_answer: None,
    };

    let outcome = dim3::run_dim3(&model, &corpus, "qwen3:8b").await;

    // Drove the real pipeline for every fact in every script.
    let total_facts: usize = corpus.scripts.iter().map(|s| s.facts.len()).sum();
    assert_eq!(outcome.per_fact.len(), total_facts, "scored every planted fact");

    // With a lossless summarizer + full-recall candidate, everything survives.
    assert_eq!(outcome.fact_survival_rate(), Some(1.0));
    assert_eq!(outcome.summarized_survival_rate(), Some(1.0));
    assert_eq!(outcome.buffer_survival_rate(), Some(1.0));
    assert_eq!(outcome.summarizer_model, "qwen3:8b");

    // At least one fact actually landed in a summary block (real summarization fired).
    assert!(
        outcome.per_fact.iter().any(|f| f.tier == FactTier::Summarized),
        "real buffer summarization must have compressed at least one planted fact"
    );
    assert!(outcome.per_fact.iter().any(|f| f.tier == FactTier::Buffer));
}

#[tokio::test]
async fn lossy_summarizer_isolates_summarized_from_buffer_survival() {
    let corpus = dim3::load_corpus().unwrap();
    // Fixed summarizer DROPS facts (lossy). Candidate would recall from summary if
    // the value were there — but it isn't, so summarized facts are lost while
    // buffer facts (still verbatim) survive. This isolates recall-from-summary.
    let model = MockMemoryModel {
        summarizer_preserves: false,
        candidate_recalls_summary: true,
        forced_answer: None,
    };

    let outcome = dim3::run_dim3(&model, &corpus, "qwen3:8b").await;

    let summ = outcome.summarized_survival_rate().expect("summarized facts present");
    let buf = outcome.buffer_survival_rate().expect("buffer facts present");
    assert_eq!(summ, 0.0, "lossy summary drops summarized facts (pipeline finding)");
    assert_eq!(buf, 1.0, "verbatim buffer facts still recalled");
    // Overall rate is between the two splits — proves they are reported separately.
    let overall = outcome.fact_survival_rate().unwrap();
    assert!(overall > 0.0 && overall < 1.0, "overall {overall} sits between splits");
}

#[tokio::test]
async fn zero_survival_is_data_not_error() {
    let corpus = dim3::load_corpus().unwrap();
    // Lossy summarizer AND candidate cannot recall from summary; buffer facts still
    // survive, but if a model also forgot buffer facts we'd want 0 recorded cleanly.
    let model = MockMemoryModel {
        summarizer_preserves: false,
        candidate_recalls_summary: false,
        // Force an answer with no fact value at all → every probe is a miss.
        forced_answer: Some("I'm sorry, I don't have that information.".to_string()),
    };

    let outcome = dim3::run_dim3(&model, &corpus, "qwen3:8b").await;
    assert_eq!(outcome.fact_survival_rate(), Some(0.0), "0 survival recorded as data");
    // Still produces rows (the run completed; it's a finding, not a crash).
    let rows = outcome.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Cpu);
    assert!(rows.iter().any(|r| r.metric == METRIC_FACT_SURVIVAL_RATE && r.value == 0.0));
}

#[tokio::test]
async fn conflation_is_flagged_and_scored_as_miss() {
    // Single-script corpus: the candidate always answers with a SIBLING's value.
    let corpus: MemoryCorpus = serde_json::from_str(
        r#"{
          "summarization_threshold": 4,
          "scripts": [{
            "id":"conflate-01",
            "facts":[
              {"key":"destination","value":"Zubrovka","plant_turn":1,"tier":"summarized"},
              {"key":"companion","value":"Agatha","plant_turn":5,"tier":"buffer"}
            ],
            "session1":[
              {"user":"My destination is Zubrovka.","assistant":"Noted Zubrovka."},
              {"user":"Filler one.","assistant":"ok"},
              {"user":"Filler two.","assistant":"ok"},
              {"user":"Filler three.","assistant":"ok"},
              {"user":"My companion is Agatha.","assistant":"Noted Agatha."}
            ],
            "session2":[],
            "probes":[
              {"key":"destination","question":"Where am I going?","expect_substrings":["zubrovka"]},
              {"key":"companion","question":"Who with?","expect_substrings":["agatha"]}
            ]
          }]
        }"#,
    )
    .unwrap();
    assert!(dim3::validate_corpus(&corpus).is_empty(), "control corpus valid");

    // Candidate always answers "Agatha" → for the destination probe that's a sibling
    // value (conflation); for the companion probe it's a correct recall.
    let model = MockMemoryModel {
        summarizer_preserves: true,
        candidate_recalls_summary: true,
        forced_answer: Some("It's Agatha.".to_string()),
    };

    let outcome = dim3::run_dim3(&model, &corpus, "qwen3:8b").await;
    let dest = outcome.per_fact.iter().find(|f| f.fact_key == "destination").unwrap();
    assert!(!dest.survived, "destination not recalled");
    assert!(dest.conflation, "destination conflated with a sibling");
    assert_eq!(dest.conflated_with.as_deref(), Some("companion"));

    let comp = outcome.per_fact.iter().find(|f| f.fact_key == "companion").unwrap();
    assert!(comp.survived, "companion correctly recalled");
    assert!(!comp.conflation);
}

#[tokio::test]
async fn rows_keyed_on_model_and_backend_with_fixed_summarizer_metadata() {
    let corpus = dim3::load_corpus().unwrap();
    let model = MockMemoryModel {
        summarizer_preserves: true,
        candidate_recalls_summary: true,
        forced_answer: None,
    };
    let outcome = dim3::run_dim3(&model, &corpus, dim3::fixed_summarizer_model()).await;

    // S83-consistent identity: pass-through model id, gpu/cpu backend tag.
    let mid = ModelId::from("Qwen3:8B"); // case preserved (no normalization)
    let rows = outcome.into_dimension_scores(&mid, BackendTag::Gpu);
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| r.model_id == mid), "byte-identical model id");
    assert!(rows.iter().all(|r| r.backend_tag == BackendTag::Gpu));
    assert!(rows.iter().all(|r| r.dimension == DIMENSION));
    assert!(rows.iter().all(|r| r.judge == JUDGE_DETERMINISTIC));

    // Aggregate rows present, with the fixed summarizer in run metadata.
    let agg = rows.iter().find(|r| r.metric == METRIC_FACT_SURVIVAL_RATE).unwrap();
    let audit = agg.raw_json.as_ref().unwrap();
    assert!(audit.contains("summarizer_model"));
    assert!(audit.contains(&dim3::fixed_summarizer_model()));
    assert!(rows.iter().any(|r| r.metric == METRIC_SUMMARIZED_SURVIVAL_RATE));
    assert!(rows.iter().any(|r| r.metric == METRIC_BUFFER_SURVIVAL_RATE));
}
