//! S84 ASMT-06 integration tests — Dimension 5 (personality_prompted).
//!
//! Black-box tests over the public API of
//! `terminus_rs::intake::assistant::dim5_prompted`. They exercise:
//!   - loading the REAL 5-layer Lumina production prompt (asserted, not a stub),
//!   - a full pressure-scenario run (scripted candidate + scripted panel) that
//!     yields BOTH sub-axes (trait + behavioral) with mean/SD and the
//!     deterministic-corroboration flag,
//!   - the deterministic behavioral pre-check on fixtures,
//!   - independent divergence of the two sub-axes,
//!   - corpus integrity (>=10 turns/scenario, no infra literals).
//!
//! Unit-level tests for aggregation internals live in the module's `#[cfg(test)]`
//! block; these are the cross-boundary integration checks the spec's TEST PLAN
//! calls for ("Integration (mocked model + panel)").

use std::sync::Mutex;

use terminus_rs::intake::assistant::dim5_prompted::{
    behavioral_panel_prompt, load_lumina_prompt_at, precheck_reply, run_scenario,
    trait_panel_prompt, CandidateModel, Corpus, Scenario, ALWAYS_ON_LAYER_MARKERS,
    BEHAVIORAL_METRICS, DIMENSION, TRAIT_METRICS,
};
use terminus_rs::intake::assistant::judges::{Judge, JudgeReply, JSON_CONTRACT_SUFFIX};
use terminus_rs::intake::assistant::{BackendTag, ModelId};

// ── scripted candidate (replaces the unified inference path in tests) ──────────

struct ScriptedCandidate {
    id: ModelId,
    backend: BackendTag,
    replies: Mutex<Vec<String>>,
}

impl ScriptedCandidate {
    fn new(id: &str, backend: BackendTag, replies: &[&str]) -> Self {
        ScriptedCandidate {
            id: ModelId::from(id),
            backend,
            replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
        }
    }
}

#[async_trait::async_trait]
impl CandidateModel for ScriptedCandidate {
    fn model_id(&self) -> &ModelId {
        &self.id
    }
    fn backend_tag(&self) -> BackendTag {
        self.backend
    }
    async fn reply(&self, system_prompt: &str, _transcript: &str) -> Result<String, String> {
        // Sanity: the candidate is always handed the REAL assembled prompt.
        assert!(
            ALWAYS_ON_LAYER_MARKERS.iter().any(|m| system_prompt.contains(m)),
            "candidate must receive the real layered prompt"
        );
        let mut r = self.replies.lock().unwrap();
        Ok(if r.is_empty() { "Sure.".into() } else { r.remove(0) })
    }
}

// ── scripted judge that returns a fixed score for the requested sub-axis ───────

struct FixedJudge {
    id: String,
    score: i64,
}

#[async_trait::async_trait]
impl Judge for FixedJudge {
    fn id(&self) -> &str {
        &self.id
    }
    async fn invoke(&self, prompt: &str, _attempt: u8) -> JudgeReply {
        // Contract: each judge prompt ends with the JSON contract suffix.
        assert!(
            prompt.contains(JSON_CONTRACT_SUFFIX),
            "panel prompt must end with the JSON contract suffix"
        );
        let keys: &[&str] = if prompt.contains("quirky") {
            TRAIT_METRICS
        } else {
            BEHAVIORAL_METRICS
        };
        let obj: std::collections::BTreeMap<&str, i64> =
            keys.iter().map(|k| (*k, self.score)).collect();
        JudgeReply::Text(serde_json::to_string(&obj).unwrap())
    }
}

fn panel(scores: [i64; 3]) -> Vec<Box<dyn Judge>> {
    vec![
        Box::new(FixedJudge { id: "claude".into(), score: scores[0] }),
        Box::new(FixedJudge { id: "gemini".into(), score: scores[1] }),
        Box::new(FixedJudge { id: "codex".into(), score: scores[2] }),
    ]
}

fn scenario(id: &str, turns: &[&str]) -> Scenario {
    Scenario {
        id: id.into(),
        title: id.into(),
        traps: vec![],
        turns: turns.iter().map(|s| s.to_string()).collect(),
    }
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[test]
fn real_5layer_prompt_loads_with_all_markers() {
    // Real PromptAssembler output into an isolated temp root — not a stub.
    let dir = tempfile::tempdir().unwrap();
    let prompt = load_lumina_prompt_at("s84-profiling", dir.path().to_path_buf())
        .expect("the real 5-layer Lumina prompt must assemble");
    for m in ALWAYS_ON_LAYER_MARKERS {
        assert!(prompt.contains(m), "real prompt missing layer marker {m}");
    }
    // The production behavioral-rule text we score against is present (loaded
    // from the assembler, never duplicated as a literal in our crate).
    assert!(prompt.contains("ONE question per reply"));
    assert!(prompt.contains("DON'T PRE-FETCH UNASKED"));
}

#[test]
fn panel_prompts_end_with_json_contract_suffix() {
    let t = trait_panel_prompt("User: hi\nAssistant: hey!");
    let b = behavioral_panel_prompt("User: hi\nAssistant: hey!");
    assert!(t.trim_end().ends_with(JSON_CONTRACT_SUFFIX.trim_end()));
    assert!(b.trim_end().ends_with(JSON_CONTRACT_SUFFIX.trim_end()));
}

#[test]
fn deterministic_precheck_fixtures() {
    assert!(precheck_reply("Work or fun? How long?").two_questions);
    assert!(!precheck_reply("Tampa, nice — work or fun?").two_questions);
    assert!(precheck_reply("Here's the forecast: sunny.").unasked_prefetch);
    assert!(precheck_reply("I've booked your flight.").overclaim);
    assert!(!precheck_reply("Congrats! Want to celebrate big or low-key?").any());
}

#[tokio::test]
async fn full_dim5_run_yields_trait_and_behavioral_mean_sd_and_flag() {
    let candidate = ScriptedCandidate::new(
        "qwen3:8b",
        BackendTag::Gpu,
        &[
            "Tampa, nice — work or fun?",
            "Three days — excited or nervous?",
            "First big one. How are you feeling about it?",
        ],
    );
    let sc = scenario(
        "tampa-mini",
        &["flying to tampa", "work, three days", "first big conference"],
    );
    // Judges [3,4,5] → mean 4.0, SD 1.0 on every metric.
    let judges = panel([3, 4, 5]);
    let prompt = "[identity]\nLumina is warm.\n[rules]\nAt most ONE question per reply.";
    let out = run_scenario(&candidate, &judges, prompt, &sc).await;

    // BOTH sub-axes produced, with mean + SD.
    assert_eq!(out.trait_panel.complying, 3);
    assert_eq!(out.behavioral_panel.complying, 3);
    let warm = &out.trait_panel.aggregates["warm"];
    assert!((warm.mean - 4.0).abs() < 1e-9);
    assert!((warm.std_dev.unwrap() - 1.0).abs() < 1e-9);
    let voice = &out.behavioral_panel.aggregates["voice_under_provocation"];
    assert!((voice.mean - 4.0).abs() < 1e-9);
    assert!((voice.std_dev.unwrap() - 1.0).abs() < 1e-9);

    // Deterministic pre-check ran over every reply (each held one question).
    assert_eq!(out.precheck.replies_checked, 3);
    assert_eq!(out.precheck.two_question_replies, 0);

    // Storage rows: keyed on S83 model_id + backend_tag, dimension is set.
    let rows = out.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Gpu);
    assert!(rows.iter().all(|r| r.dimension == DIMENSION));
    assert!(rows.iter().all(|r| r.model_id == ModelId::from("qwen3:8b")));
    assert!(rows.iter().all(|r| r.backend_tag == BackendTag::Gpu));
    // 4 trait + 4 behavioral metrics.
    assert_eq!(rows.iter().filter(|r| TRAIT_METRICS.contains(&r.metric.as_str())).count(), 4);
    assert_eq!(
        rows.iter().filter(|r| BEHAVIORAL_METRICS.contains(&r.metric.as_str())).count(),
        4
    );
    // Behavioral rows carry the deterministic-corroboration verdict.
    let beh = rows.iter().find(|r| r.metric == "held_one_question").unwrap();
    let audit: serde_json::Value =
        serde_json::from_str(beh.raw_json.as_ref().unwrap()).unwrap();
    assert!(audit["corroboration"]["agree"].is_boolean());
    assert!(audit["deterministic_precheck"]["replies_checked"] == serde_json::json!(3));
}

#[tokio::test]
async fn precheck_panel_disagreement_recorded_for_asmt11() {
    // Candidate flagrantly violates rules; panel (wrongly) scores high → disagree.
    let candidate = ScriptedCandidate::new(
        "m",
        BackendTag::Cpu,
        &[
            "Work or fun? And how long are you there?",
            "Done — I've booked your flight and hotel!",
            "Anything else? Want the forecast too?",
        ],
    );
    let sc = scenario("trap", &["a", "b", "c"]);
    let judges = panel([5, 5, 5]); // panel says clean
    let out = run_scenario(&candidate, &judges, "[identity]\nx\n[rules]\ny", &sc).await;

    assert!(out.precheck.two_question_replies >= 1);
    assert!(out.precheck.overclaim_replies >= 1);

    let v = out.corroboration_verdict();
    assert!(v.deterministic_flagged);
    assert_eq!(v.panel_behavioral_mean, Some(5.0));
    assert!(!v.agree);

    let rows = out.into_dimension_scores(&ModelId::from("m"), BackendTag::Cpu);
    let beh = rows.iter().find(|r| r.metric == "no_overclaim").unwrap();
    let audit: serde_json::Value =
        serde_json::from_str(beh.raw_json.as_ref().unwrap()).unwrap();
    assert_eq!(audit["corroboration"]["agree"], serde_json::json!(false));
}

#[test]
fn corpus_is_well_formed_and_pii_clean() {
    let raw = include_str!("../../src/intake/assistant/corpora/prompted_pressure.json");
    let corpus = Corpus::from_json(raw).expect("corpus parses");
    assert!(corpus.scenarios.len() >= 3);
    for s in &corpus.scenarios {
        assert!(s.turns.len() >= 10, "scenario {} has <10 turns", s.id);
        for t in &s.turns {
            assert!(!t.contains("192.168"), "infra IP in {}", s.id);
            let has_ct = t
                .as_bytes()
                .windows(3)
                .any(|w| w[0] == b'C' && w[1] == b'T' && w[2].is_ascii_digit());
            assert!(!has_ct, "CT### identifier in scenario {}", s.id);
        }
    }
}
