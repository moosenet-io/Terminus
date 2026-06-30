//! Dimension 4 — latent personality / OCEAN on the RAW model (S84 ASMT-05).
//!
//! Scores a candidate model's **latent disposition** on the Big Five (OCEAN) with
//! **NO Lumina prompt loaded** — measuring what training baked in, so we can
//! shortlist base models that won't fight Lumina's intended voice. Panel-scored,
//! mean + sample SD per trait (ASMT-01 aggregation).
//!
//! ## RAW runs (CRITICAL — base-only prompt, asserted)
//! Each elicitation scenario is sent to the model through Chord's unified path with
//! ONLY the scenario text as the prompt. We do NOT prepend any Lumina layer
//! (persona, instructions, memory, tools) — the prompt the model sees is exactly
//! [`RawModel::assemble_base_prompt`], which is the scenario text verbatim. The
//! [`is_base_only`] guard rejects any prompt carrying a Lumina marker, and a unit
//! test asserts assembly is base-only. This isolates the *latent* disposition from
//! anything Lumina would impose in dim-5 (prompted adherence).
//!
//! ## Inference path (CRITICAL — matches the coder harness)
//! Every model generation runs through Chord's **unified inference path**, NOT a
//! direct Ollama call: [`RawModel::generate`] calls
//! [`crate::intake::context::generate`], which delegates to
//! [`crate::intake::infer::infer_with_metrics`] (P5 backend routing — resolves the
//! model's tagged backend, GPU vs CPU, and serves on its correct hardware). The
//! harness is a client of the unified proxy, exactly like S83's `context`/`agent`
//! suites. The runner depends only on the [`OceanModel`] trait, so unit/integration
//! tests inject a mock while the live path stays the one shared proxy.
//!
//! ## Open-ended elicitation (not multiple-choice)
//! Scenarios are open-ended prompts (see `corpora/ocean_inventory.json`) so a model
//! cannot game a fixed-choice questionnaire. The judge panel reads the free-form
//! response against `rubrics/ocean_rubric.md` and returns trait → 1–5.
//!
//! ## Proximity-to-Lumina note (derived, NOT merged)
//! After scoring, we derive a `proximity_to_lumina` NOTE — how close the latent
//! profile sits to Lumina's target disposition (ASMT-11 shortlist input). It is a
//! SEPARATE row (`metric = "proximity_to_lumina"`, `judge = "derived"`) and is
//! **never merged** into any dim-5 prompted-adherence score.
//!
//! ## Keying
//! Results store per (`model_id`, `backend_tag`) with
//! `dimension = "personality_latent"`, one row per OCEAN trait (judge values + mean
//! + SD), `model_id` byte-identical to S83 via [`super::ModelId`] (pass-through).

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use super::judges::{self, Judge, JSON_CONTRACT_SUFFIX};
use super::{BackendTag, DimensionScore, JudgeOutcome, ModelId, PanelResult};

// ===========================================================================
// Corpus types (scenarios live in JSON, never in code)
// ===========================================================================

/// The whole `ocean_inventory.json` corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct OceanCorpus {
    #[serde(default)]
    pub schema_version: String,
    /// The five OCEAN traits this corpus covers (declared for schema validation).
    pub traits: Vec<String>,
    pub scenarios: Vec<Scenario>,
}

/// One open-ended elicitation scenario targeting a single trait.
#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub id: String,
    /// The OCEAN trait this scenario is designed to surface.
    #[serde(rename = "trait")]
    pub trait_name: String,
    /// The open-ended prompt (free-form, NOT multiple-choice).
    pub prompt: String,
}

/// The embedded corpus, checked into the repo (PII-free by construction; the
/// harness needs no external file at runtime).
const OCEAN_INVENTORY_JSON: &str = include_str!("corpora/ocean_inventory.json");

/// Load + parse the embedded corpus.
pub fn load_corpus() -> Result<OceanCorpus, String> {
    serde_json::from_str(OCEAN_INVENTORY_JSON)
        .map_err(|e| format!("ocean_inventory.json parse error: {e}"))
}

/// The five canonical OCEAN traits, in storage order. The dimension's contract:
/// exactly these five trait rows per (model, backend).
pub const OCEAN_TRAITS: [&str; 5] = [
    "openness",
    "conscientiousness",
    "extraversion",
    "agreeableness",
    "neuroticism",
];

/// Validate corpus integrity: declared `traits` are exactly the five OCEAN traits,
/// every scenario targets one of them, every trait is covered by at least one
/// scenario, ids are unique, and no prompt is empty. Returns the list of problems
/// (empty ⇒ valid).
pub fn validate_corpus(corpus: &OceanCorpus) -> Vec<String> {
    let mut problems = Vec::new();

    // Declared trait set must be exactly the five OCEAN traits.
    let declared: std::collections::BTreeSet<&str> =
        corpus.traits.iter().map(|s| s.as_str()).collect();
    let expected: std::collections::BTreeSet<&str> = OCEAN_TRAITS.iter().copied().collect();
    if declared != expected {
        problems.push(format!(
            "declared traits {:?} != the five OCEAN traits {:?}",
            corpus.traits, OCEAN_TRAITS
        ));
    }

    let mut seen_ids = std::collections::BTreeSet::new();
    let mut covered: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for s in &corpus.scenarios {
        if !seen_ids.insert(s.id.as_str()) {
            problems.push(format!("duplicate scenario id '{}'", s.id));
        }
        if !expected.contains(s.trait_name.as_str()) {
            problems.push(format!(
                "scenario '{}' targets unknown trait '{}'",
                s.id, s.trait_name
            ));
        } else {
            covered.insert(
                OCEAN_TRAITS
                    .iter()
                    .find(|t| **t == s.trait_name)
                    .copied()
                    .unwrap(),
            );
        }
        if s.prompt.trim().is_empty() {
            problems.push(format!("scenario '{}' has an empty prompt", s.id));
        }
    }

    for t in OCEAN_TRAITS {
        if !covered.contains(t) {
            problems.push(format!("trait '{t}' has no elicitation scenario"));
        }
    }

    problems
}

// ===========================================================================
// Base-only prompt assembly (RAW model — NO Lumina layers)
// ===========================================================================

/// Substrings that, if present in an elicitation prompt, would mean a Lumina layer
/// leaked into the RAW run. The base-only guard rejects any of these. This is the
/// machine-checkable assertion behind "raw runs load NO Lumina prompt layers".
const LUMINA_MARKERS: &[&str] = &[
    "lumina",
    "you are lumina",
    "constellation",
    "engram",
    "nexus",
    "obsidian circle",
    "system prompt:",
    "persona:",
];

/// True iff `prompt` is base-only: it carries NO Lumina persona/instruction/memory
/// marker. Case-insensitive. Used to ASSERT a RAW run never smuggles a Lumina layer
/// into the model's context.
pub fn is_base_only(prompt: &str) -> bool {
    let lc = prompt.to_lowercase();
    !LUMINA_MARKERS.iter().any(|m| lc.contains(m))
}

// ===========================================================================
// Inference abstraction (unified path live; mockable for tests)
// ===========================================================================

/// One RAW generation: given a single open-ended scenario prompt (base-only, no
/// Lumina layer), produce the model's free-form response. Never panics — a
/// timeout / transport error / empty output maps to an empty/degraded reply that
/// the judges still score (a refusal/empty is a valid low-trait reading).
#[derive(Debug, Clone, Default)]
pub struct RawReply {
    pub text: String,
    /// True ⇒ the generation degraded (timeout, transport error, OOM, empty). The
    /// response text is still passed to the judges (empty ⇒ low-trait reading).
    pub degraded: bool,
    /// Human-readable degradation reason (audit only), when `degraded`.
    pub degrade_reason: Option<String>,
}

/// The inference surface the runner depends on. The live impl ([`RawModel`]) routes
/// through Chord's unified path; tests inject a mock.
#[async_trait::async_trait]
pub trait OceanModel: Send + Sync {
    /// Produce the RAW response to a single base-only `scenario_prompt`. Must never
    /// panic — failures map to `RawReply { degraded: true, .. }`.
    async fn generate(&self, scenario_prompt: &str) -> RawReply;
}

/// Live RAW model: runs each scenario through Chord's unified inference path via
/// [`crate::intake::context::generate`] → [`crate::intake::infer::infer_with_metrics`]
/// (P5 backend routing). Base-only by construction: the prompt sent is exactly the
/// scenario text (see [`RawModel::assemble_base_prompt`]); NO Lumina layer is added.
pub struct RawModel {
    client: reqwest::Client,
    model_name: String,
    timeout: Duration,
}

impl RawModel {
    pub fn new(client: reqwest::Client, model_name: impl Into<String>, timeout: Duration) -> Self {
        RawModel {
            client,
            model_name: model_name.into(),
            timeout,
        }
    }

    /// Assemble the prompt for a RAW run. This is the WHOLE prompt the model sees:
    /// the scenario text, verbatim — NO Lumina persona, instructions, memory, or
    /// tool layers. Returns `Err` (instead of silently running) if the scenario
    /// text itself contains a Lumina marker, so the base-only invariant is
    /// enforced at the only place a prompt is built.
    pub fn assemble_base_prompt(scenario_prompt: &str) -> Result<String, String> {
        let prompt = scenario_prompt.to_string();
        if !is_base_only(&prompt) {
            return Err(format!(
                "refusing RAW run: scenario prompt carries a Lumina layer marker \
                 (base-only invariant): {:.80}",
                prompt
            ));
        }
        Ok(prompt)
    }
}

#[async_trait::async_trait]
impl OceanModel for RawModel {
    async fn generate(&self, scenario_prompt: &str) -> RawReply {
        let prompt = match Self::assemble_base_prompt(scenario_prompt) {
            Ok(p) => p,
            Err(reason) => {
                // A leaked Lumina layer is a degradation, never a crash; the
                // scenario is skipped with an auditable reason.
                return RawReply {
                    text: String::new(),
                    degraded: true,
                    degrade_reason: Some(reason),
                };
            }
        };

        // UNIFIED PATH: context::generate → infer::infer_with_metrics (P5).
        let out =
            crate::intake::context::generate(&self.client, &self.model_name, &prompt, self.timeout)
                .await;

        if let Some(err) = out.error {
            return RawReply {
                text: String::new(),
                degraded: true,
                degrade_reason: Some(if out.oom {
                    format!("oom/overload: {err}")
                } else {
                    format!("inference error: {err}")
                }),
            };
        }
        if out.response.trim().is_empty() {
            // Empty = a refusal-shaped reading; judges still score it (low trait).
            return RawReply {
                text: String::new(),
                degraded: true,
                degrade_reason: Some("empty response (truncation/refusal)".into()),
            };
        }
        RawReply {
            text: out.response,
            degraded: false,
            degrade_reason: None,
        }
    }
}

// ===========================================================================
// Judge panel prompt (OCEAN rubric)
// ===========================================================================

/// Build a judge prompt scoring one RAW response against the OCEAN rubric for the
/// scenario's single target trait. Ends with the shared [`JSON_CONTRACT_SUFFIX`] so
/// the judge harness's strict extractor applies. The rubric anchors are inlined
/// (mirroring `rubrics/ocean_rubric.md`) so the judge sees the same 5-point scale.
pub fn ocean_judge_prompt(trait_name: &str, scenario_prompt: &str, response: &str) -> String {
    format!(
        "You are scoring a language model's RAW, un-prompted response for the Big \
Five (OCEAN) trait \"{trait}\". The model was given ONLY the scenario below, with \
no persona or system instructions, so its reply reflects its latent disposition. \
Score the single trait \"{trait}\" on this 1-5 scale (5 = strongly expresses the \
high pole of the trait, 3 = balanced/neutral, 1 = strongly expresses the low pole; \
for neuroticism, 5 = highly reactive/anxious and 1 = very stable/calm). A refusal \
or empty reply is a valid LOW reading, not an error.\n\n\
Scenario shown to the model:\n{scenario}\n\n\
Model's reply:\n{resp}\n\n\
Score the single trait \"{trait}\". {suffix}",
        trait = trait_name,
        scenario = scenario_prompt.trim(),
        resp = if response.trim().is_empty() {
            "(empty / refused)"
        } else {
            response.trim()
        },
        suffix = JSON_CONTRACT_SUFFIX,
    )
}

// ===========================================================================
// Runner
// ===========================================================================

/// Dimension label written to `assistant_dimension_score.dimension`.
pub const DIMENSION: &str = "personality_latent";

/// Metric name for the derived proximity NOTE (a SEPARATE row, never merged into
/// any dim-5 score).
pub const METRIC_PROXIMITY: &str = "proximity_to_lumina";

/// Lumina's target latent disposition on the 1-5 OCEAN scale (high pole = 5; for
/// neuroticism the scale is reactivity, so a LOW target = calm/stable). This is the
/// reference point for the derived proximity note — judgment about voice fit, kept
/// OUT of dim-5's prompted-adherence measurement. Not infra; a design constant.
pub const LUMINA_TARGET_OCEAN: [(&str, f64); 5] = [
    ("openness", 4.0),
    ("conscientiousness", 5.0),
    ("extraversion", 3.0),
    ("agreeableness", 4.0),
    ("neuroticism", 2.0),
];

/// The full dim-4 outcome for one (model, backend): the five-trait panel result
/// plus the derived proximity note, ready to flatten into storage rows.
#[derive(Debug, Clone)]
pub struct Dim4Outcome {
    /// Panel result over all five OCEAN traits (mean + SD per trait). Empty/unscored
    /// when no judge ever complied.
    pub panel: PanelResult,
    /// Per-trait degradation reasons collected during the RAW runs (audit only).
    pub degradations: Vec<(String, String)>,
}

impl Dim4Outcome {
    /// Derived proximity-to-Lumina note: mean absolute distance between the model's
    /// per-trait latent means and [`LUMINA_TARGET_OCEAN`], mapped to a 1-5
    /// "closeness" value (5 = identical disposition, 1 = maximally far). Returns
    /// `None` when no trait was scored. PURE — no judge, no DB.
    ///
    /// This is a NOTE for the ASMT-11 shortlist read; it is emitted as its OWN row
    /// (`metric = "proximity_to_lumina"`, `judge = "derived"`) and is NEVER merged
    /// into any dim-5 prompted-adherence score.
    pub fn proximity_to_lumina(&self) -> Option<f64> {
        let target: BTreeMap<&str, f64> = LUMINA_TARGET_OCEAN.iter().copied().collect();
        let mut dists = Vec::new();
        for (trait_name, tgt) in &target {
            if let Some(agg) = self.panel.aggregates.get(*trait_name) {
                dists.push((agg.mean - tgt).abs());
            }
        }
        if dists.is_empty() {
            return None;
        }
        let mean_abs = dists.iter().sum::<f64>() / dists.len() as f64;
        // Max possible per-trait distance on a 1-5 scale is 4.0; map distance 0→5,
        // distance 4→1 linearly so the note shares the panel's 1-5 orientation.
        let closeness = 5.0 - (mean_abs / 4.0) * 4.0;
        Some(closeness.clamp(1.0, 5.0))
    }

    /// Flatten into `DimensionScore` rows for one (model, backend):
    ///   - one row per scored OCEAN trait (judge = "panel" / single judge id),
    ///   - one derived `proximity_to_lumina` row (judge = "derived"), NOT merged.
    pub fn into_dimension_scores(
        &self,
        model_id: &ModelId,
        backend_tag: BackendTag,
    ) -> Vec<DimensionScore> {
        // Per-trait panel rows (mean + SD), via the shared ASMT-01 flattener.
        let mut rows = self.panel.into_dimension_scores(model_id, backend_tag);

        // Derived proximity NOTE — a SEPARATE row, never merged with dim-5.
        if let Some(closeness) = self.proximity_to_lumina() {
            let audit = serde_json::json!({
                "kind": "derived_note",
                "note": "proximity of latent OCEAN profile to Lumina's target disposition; \
                         feeds ASMT-11 shortlist; NOT a dim-5 prompted-adherence score",
                "lumina_target": LUMINA_TARGET_OCEAN
                    .iter()
                    .map(|(k, v)| (k.to_string(), *v))
                    .collect::<BTreeMap<_, _>>(),
                "model_means": self
                    .panel
                    .aggregates
                    .iter()
                    .map(|(k, a)| (k.clone(), a.mean))
                    .collect::<BTreeMap<_, _>>(),
            })
            .to_string();
            rows.push(DimensionScore {
                model_id: model_id.clone(),
                backend_tag,
                dimension: DIMENSION.to_string(),
                metric: METRIC_PROXIMITY.to_string(),
                value: closeness,
                std_dev: None,
                judge: "derived".to_string(),
                low_confidence: false,
                raw_json: Some(audit),
            });
        }

        rows
    }
}

/// Run the full dim-4 OCEAN assessment for one (model, backend):
///   1. For each scenario, RAW-generate the model's response through the unified
///      path (base-only prompt, asserted) — never panics.
///   2. Score each response with the judge panel against the OCEAN rubric on the
///      scenario's single target trait.
///   3. Pool per-judge scores by trait so each OCEAN trait gets one mean + sample
///      SD across complying judges (ASMT-01 aggregation).
///
/// `judges` is the panel ([`super::judges::CliJudge::panel`] live, or mocks in
/// tests). Pure orchestration over the injected [`OceanModel`] + judges — no DB, no
/// direct network. Never panics.
pub async fn run_dim4(
    model: &dyn OceanModel,
    judges: &[Box<dyn Judge>],
    corpus: &OceanCorpus,
) -> Dim4Outcome {
    // judge_id -> trait -> Vec<score>: pool every (judge × scenario) observation by
    // the scenario's target trait, so a judge contributes one mean per trait.
    let mut pooled: BTreeMap<String, BTreeMap<String, Vec<i64>>> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut degradations: Vec<(String, String)> = Vec::new();
    let mut any_complied = false;

    for scenario in &corpus.scenarios {
        let reply = model.generate(&scenario.prompt).await;
        if reply.degraded {
            if let Some(reason) = &reply.degrade_reason {
                degradations.push((scenario.id.clone(), reason.clone()));
            }
            // NOTE: we still judge the (empty) reply below — empty = low-trait read.
        }

        if judges.is_empty() {
            continue;
        }

        let prompt = ocean_judge_prompt(&scenario.trait_name, &scenario.prompt, &reply.text);
        let pr = judges::run_panel(judges, DIMENSION, &prompt, &[scenario.trait_name.as_str()]).await;
        warnings.extend(pr.warnings.iter().cloned());

        for outcome in &pr.outcomes {
            if let JudgeOutcome::Scored { judge, traits } = outcome {
                if let Some(v) = traits.get(&scenario.trait_name) {
                    pooled
                        .entry(judge.clone())
                        .or_default()
                        .entry(scenario.trait_name.clone())
                        .or_default()
                        .push(*v);
                    any_complied = true;
                }
            }
        }
    }

    let panel = if !any_complied {
        PanelResult::aggregate(
            DIMENSION,
            vec![JudgeOutcome::Abstained {
                judge: "panel".to_string(),
                reason: "no judge scored any OCEAN scenario".to_string(),
                raw: None,
            }],
            dedup(warnings),
        )
    } else {
        // Reduce each judge's pooled scores to one integer per trait (rounded mean
        // in [1,5]) so the shared integer-based ASMT-01 aggregation (mean + sample
        // SD across judges) applies unchanged, one trait row per OCEAN dimension.
        let outcomes: Vec<JudgeOutcome> = pooled
            .into_iter()
            .map(|(judge, by_trait)| {
                let traits: BTreeMap<String, i64> = by_trait
                    .into_iter()
                    .map(|(trait_name, scores)| {
                        let mean = scores.iter().sum::<i64>() as f64 / scores.len() as f64;
                        (trait_name, mean.round().clamp(1.0, 5.0) as i64)
                    })
                    .collect();
                JudgeOutcome::Scored { judge, traits }
            })
            .collect();
        PanelResult::aggregate(DIMENSION, outcomes, dedup(warnings))
    };

    Dim4Outcome {
        panel,
        degradations,
    }
}

/// Order-preserving dedup of operator warnings.
fn dedup(mut v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|w| seen.insert(w.clone()));
    v
}

// ===========================================================================
// Tests (pure unit coverage; integration lives in tests/intake/)
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_corpus_parses_and_validates() {
        let corpus = load_corpus().expect("corpus parses");
        let problems = validate_corpus(&corpus);
        assert!(problems.is_empty(), "corpus problems: {problems:?}");
        // All five OCEAN traits are covered.
        for t in OCEAN_TRAITS {
            assert!(
                corpus.scenarios.iter().any(|s| s.trait_name == t),
                "trait {t} missing"
            );
        }
    }

    #[test]
    fn validate_catches_missing_trait_and_unknown_trait() {
        let corpus: OceanCorpus = serde_json::from_str(
            r#"{
              "traits": ["openness","conscientiousness","extraversion","agreeableness","neuroticism"],
              "scenarios": [
                {"id":"a","trait":"openness","prompt":"p"},
                {"id":"b","trait":"telepathy","prompt":"p"}
              ]
            }"#,
        )
        .unwrap();
        let problems = validate_corpus(&corpus);
        assert!(problems.iter().any(|p| p.contains("unknown trait 'telepathy'")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("'conscientiousness' has no")), "{problems:?}");
    }

    #[test]
    fn validate_catches_duplicate_id_and_empty_prompt() {
        let corpus: OceanCorpus = serde_json::from_str(
            r#"{
              "traits": ["openness","conscientiousness","extraversion","agreeableness","neuroticism"],
              "scenarios": [
                {"id":"x","trait":"openness","prompt":"p"},
                {"id":"x","trait":"conscientiousness","prompt":"  "},
                {"id":"y","trait":"extraversion","prompt":"p"},
                {"id":"z","trait":"agreeableness","prompt":"p"},
                {"id":"w","trait":"neuroticism","prompt":"p"}
              ]
            }"#,
        )
        .unwrap();
        let problems = validate_corpus(&corpus);
        assert!(problems.iter().any(|p| p.contains("duplicate scenario id 'x'")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("empty prompt")), "{problems:?}");
    }

    // ── base-only prompt invariant ──

    #[test]
    fn is_base_only_rejects_lumina_markers() {
        assert!(is_base_only("Describe your ideal afternoon."));
        assert!(!is_base_only("You are Lumina. Describe your ideal afternoon."));
        assert!(!is_base_only("Use the Engram memory then answer."));
        assert!(!is_base_only("CONSTELLATION persona: warm"));
    }

    #[test]
    fn assemble_base_prompt_is_scenario_verbatim_and_base_only() {
        let scenario = "You are handed a blank notebook and an afternoon. What do you do?";
        let p = RawModel::assemble_base_prompt(scenario).expect("base-only");
        // The whole prompt is exactly the scenario — no Lumina layer prepended.
        assert_eq!(p, scenario);
        assert!(is_base_only(&p));
        // Every real corpus scenario assembles base-only.
        let corpus = load_corpus().unwrap();
        for s in &corpus.scenarios {
            let built = RawModel::assemble_base_prompt(&s.prompt).expect("corpus scenario base-only");
            assert!(is_base_only(&built), "scenario {} not base-only", s.id);
        }
    }

    #[test]
    fn assemble_base_prompt_refuses_leaked_lumina_layer() {
        let leaked = "You are Lumina, the orchestrator. Describe your ideal afternoon.";
        assert!(RawModel::assemble_base_prompt(leaked).is_err());
    }

    #[test]
    fn ocean_judge_prompt_ends_with_contract_and_names_trait() {
        let p = ocean_judge_prompt("openness", "scenario text", "a curious reply");
        assert!(p.trim_end().ends_with(JSON_CONTRACT_SUFFIX));
        assert!(p.contains("openness"));
        // empty response handled without panic
        let p2 = ocean_judge_prompt("neuroticism", "scenario", "  ");
        assert!(p2.contains("(empty / refused)"));
    }

    // ── proximity-to-Lumina derivation (note, not merged) ──

    #[test]
    fn proximity_is_five_when_profile_matches_target() {
        // Build a panel whose per-trait means equal LUMINA_TARGET_OCEAN exactly.
        let outcomes = vec![exact_target_judge("claude"), exact_target_judge("gemini")];
        let panel = PanelResult::aggregate(DIMENSION, outcomes, vec![]);
        let outcome = Dim4Outcome { panel, degradations: vec![] };
        let prox = outcome.proximity_to_lumina().unwrap();
        assert!((prox - 5.0).abs() < 1e-9, "expected 5.0, got {prox}");
    }

    #[test]
    fn proximity_is_one_when_profile_maximally_far() {
        // Means all distance 4 from target (e.g. target 4→reading would be off by 4
        // only at the extremes; we force a 4.0 mean-abs by flipping each trait to
        // its far pole where possible). Simpler: synthesize aggregates directly.
        let mut panel = PanelResult::aggregate(
            DIMENSION,
            vec![JudgeOutcome::Scored {
                judge: "j".into(),
                traits: BTreeMap::new(),
            }],
            vec![],
        );
        // openness target 4 → reading 1? max dist would be |1-4|=3, not 4. To force
        // mean_abs = 4.0 we set readings 4 away from each target where the scale
        // allows it (consc target 5→reading 1 = 4; neuro target 2→reading... max 3).
        // We instead inject an artificial single-trait aggregate at distance 4.
        panel.aggregates.clear();
        panel.aggregates.insert(
            "conscientiousness".to_string(),
            super::super::TraitAggregate { mean: 1.0, std_dev: None, n: 1, low_confidence: true },
        );
        let outcome = Dim4Outcome { panel, degradations: vec![] };
        // conscientiousness target 5, reading 1 → dist 4 → closeness 1.0.
        let prox = outcome.proximity_to_lumina().unwrap();
        assert!((prox - 1.0).abs() < 1e-9, "expected 1.0, got {prox}");
    }

    #[test]
    fn proximity_none_when_no_traits_scored() {
        let panel = PanelResult::aggregate(
            DIMENSION,
            vec![JudgeOutcome::Abstained {
                judge: "j".into(),
                reason: "x".into(),
                raw: None,
            }],
            vec![],
        );
        let outcome = Dim4Outcome { panel, degradations: vec![] };
        assert!(outcome.proximity_to_lumina().is_none());
    }

    /// A judge whose scores equal LUMINA_TARGET_OCEAN exactly (rounded integers).
    fn exact_target_judge(id: &str) -> JudgeOutcome {
        let traits = LUMINA_TARGET_OCEAN
            .iter()
            .map(|(k, v)| (k.to_string(), *v as i64))
            .collect();
        JudgeOutcome::Scored {
            judge: id.to_string(),
            traits,
        }
    }

    // ── full runner with mock model + scripted judges ──

    struct MockModel {
        /// reply text per scenario id (default: a generic reply).
        replies: BTreeMap<String, String>,
        /// scenario ids to force-degrade (empty reply).
        degrade: std::collections::BTreeSet<String>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl OceanModel for MockModel {
        async fn generate(&self, scenario_prompt: &str) -> RawReply {
            // base-only invariant must hold for every prompt the runner sends.
            assert!(is_base_only(scenario_prompt), "runner sent a non-base prompt");
            self.seen.lock().unwrap().push(scenario_prompt.to_string());
            // map by prompt → find scenario id from the embedded corpus
            let corpus = load_corpus().unwrap();
            let id = corpus
                .scenarios
                .iter()
                .find(|s| s.prompt == scenario_prompt)
                .map(|s| s.id.clone())
                .unwrap_or_default();
            if self.degrade.contains(&id) {
                return RawReply { text: String::new(), degraded: true, degrade_reason: Some("forced".into()) };
            }
            let text = self
                .replies
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "A measured, balanced reply.".to_string());
            RawReply { text, degraded: false, degrade_reason: None }
        }
    }

    /// A judge that returns a fixed integer for whatever single trait it is asked.
    struct FixedTraitJudge {
        id: String,
        score: i64,
    }

    #[async_trait::async_trait]
    impl Judge for FixedTraitJudge {
        fn id(&self) -> &str {
            &self.id
        }
        async fn invoke(&self, prompt: &str, _attempt: u8) -> judges::JudgeReply {
            // Recover which trait was requested from the prompt to answer the
            // contract for exactly that key.
            let trait_name = OCEAN_TRAITS
                .iter()
                .find(|t| prompt.contains(&format!("trait \"{t}\"")))
                .copied()
                .unwrap_or("openness");
            judges::JudgeReply::Text(format!("{{\"{trait_name}\": {}}}", self.score))
        }
    }

    fn small_corpus() -> OceanCorpus {
        // one scenario per trait, drawn from the real corpus prompts
        let full = load_corpus().unwrap();
        let mut by_trait: BTreeMap<String, Scenario> = BTreeMap::new();
        for s in full.scenarios {
            by_trait.entry(s.trait_name.clone()).or_insert(s);
        }
        OceanCorpus {
            schema_version: "test".into(),
            traits: OCEAN_TRAITS.iter().map(|s| s.to_string()).collect(),
            scenarios: by_trait.into_values().collect(),
        }
    }

    #[tokio::test]
    async fn run_dim4_produces_five_trait_rows_with_mean_sd() {
        let model = MockModel {
            replies: BTreeMap::new(),
            degrade: Default::default(),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        // Three judges score every trait 3,4,5 → mean 4.0, SD 1.0 per trait.
        let panel: Vec<Box<dyn Judge>> = vec![
            Box::new(FixedTraitJudge { id: "claude".into(), score: 3 }),
            Box::new(FixedTraitJudge { id: "gemini".into(), score: 4 }),
            Box::new(FixedTraitJudge { id: "codex".into(), score: 5 }),
        ];
        let outcome = run_dim4(&model, &panel, &small_corpus()).await;
        assert_eq!(outcome.panel.complying, 3);
        // five trait rows, each mean 4.0 SD 1.0
        for t in OCEAN_TRAITS {
            let agg = &outcome.panel.aggregates[t];
            assert!((agg.mean - 4.0).abs() < 1e-9, "{t} mean");
            assert!((agg.std_dev.unwrap() - 1.0).abs() < 1e-9, "{t} sd");
        }
        let rows = outcome.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Gpu);
        // 5 trait rows + 1 derived proximity row
        let trait_rows: Vec<_> = rows.iter().filter(|r| OCEAN_TRAITS.contains(&r.metric.as_str())).collect();
        assert_eq!(trait_rows.len(), 5);
        let prox = rows.iter().find(|r| r.metric == METRIC_PROXIMITY).expect("proximity row");
        assert_eq!(prox.judge, "derived");
        assert_eq!(prox.dimension, DIMENSION);
        // every trait row keyed on dimension personality_latent
        assert!(trait_rows.iter().all(|r| r.dimension == DIMENSION));
    }

    #[tokio::test]
    async fn run_dim4_high_disagreement_preserves_sd() {
        let model = MockModel {
            replies: BTreeMap::new(),
            degrade: Default::default(),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        // [1,3,5] → mean 3.0, SD 2.0; must NOT collapse.
        let panel: Vec<Box<dyn Judge>> = vec![
            Box::new(FixedTraitJudge { id: "claude".into(), score: 1 }),
            Box::new(FixedTraitJudge { id: "gemini".into(), score: 3 }),
            Box::new(FixedTraitJudge { id: "codex".into(), score: 5 }),
        ];
        let outcome = run_dim4(&model, &panel, &small_corpus()).await;
        let agg = &outcome.panel.aggregates["openness"];
        assert!((agg.mean - 3.0).abs() < 1e-9);
        assert!((agg.std_dev.unwrap() - 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn run_dim4_degraded_scenarios_still_scored_not_crashed() {
        // Force every scenario to degrade (empty reply). Judges still score the
        // empty reply (a low-trait reading) — the run produces data, not a crash.
        let all_ids: std::collections::BTreeSet<String> =
            small_corpus().scenarios.iter().map(|s| s.id.clone()).collect();
        let model = MockModel {
            replies: BTreeMap::new(),
            degrade: all_ids,
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let panel: Vec<Box<dyn Judge>> = vec![
            Box::new(FixedTraitJudge { id: "claude".into(), score: 1 }),
            Box::new(FixedTraitJudge { id: "gemini".into(), score: 1 }),
        ];
        let outcome = run_dim4(&model, &panel, &small_corpus()).await;
        assert_eq!(outcome.degradations.len(), 5); // one per scenario
        assert_eq!(outcome.panel.complying, 2);
        assert!((outcome.panel.aggregates["openness"].mean - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn run_dim4_no_judges_yields_unscored_no_panic() {
        let model = MockModel {
            replies: BTreeMap::new(),
            degrade: Default::default(),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let outcome = run_dim4(&model, &[], &small_corpus()).await;
        assert!(outcome.panel.is_unscored());
        // proximity is None (no traits scored), so no proximity row emitted.
        let rows = outcome.into_dimension_scores(&ModelId::from("m"), BackendTag::Cpu);
        assert!(rows.is_empty());
    }
}
