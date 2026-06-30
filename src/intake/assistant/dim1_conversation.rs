//! Dimension 1 — conversation depth / turn-to-degradation (S84 ASMT-02).
//!
//! Measures how many turns a model sustains before it degrades, on two axes:
//!   - **Deterministic recall** — replay a scripted conversation and, at each
//!     recall probe, check whether the model still holds the planted fact. Yields
//!     a recall curve by turn depth and `recall_ceiling_turns` (the deepest turn
//!     index where recall is still ≥ the corpus threshold).
//!   - **Judged coherence / on-voice** — sample model responses at several depths
//!     and score them with the 3-judge panel ([`crate::intake::assistant::judges`])
//!     on a 1–5 coherence rubric → mean + sample SD.
//!
//! ## Inference path (CRITICAL — matches the coder harness)
//! Every model turn runs through Chord's **unified inference path**, NOT a direct
//! Ollama HTTP call: [`LiveModel::respond`] calls
//! [`crate::intake::context::generate`], which delegates to
//! [`crate::intake::infer::infer_with_metrics`] (P5 backend routing — resolves the
//! model's tagged backend, GPU vs CPU, and serves on its correct hardware). The
//! harness is a client of the unified proxy, exactly like S83's suites
//! (`context`/`agent`). The runner depends only on the [`ConversationModel`]
//! trait, so unit/integration tests inject a mock and the live path stays the one
//! shared proxy.
//!
//! ## Degradation, never a crash
//! A timeout, truncation (small context window), transport error, empty string,
//! or refusal at depth D is recorded as **degradation at depth D** — the
//! conversation stops there and the depth becomes the recorded ceiling. Nothing
//! here panics or aborts the run (acceptance: "Timeout/truncation recorded as
//! degradation depth, never a crashed run").
//!
//! ## Keying
//! Results are stored per (`model_id`, `backend_tag`) with
//! `dimension = "conversation_depth"`, the `model_id` byte-identical to S83 via
//! [`super::ModelId`] (pass-through), so the S84 assistant profile joins the S83
//! builder profile on one record.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use super::judges::{self, Judge, JSON_CONTRACT_SUFFIX};
use super::{BackendTag, DimensionScore, ModelId, PanelResult};

// ===========================================================================
// Corpus types (facts + probes live in JSON, never in code)
// ===========================================================================

/// The whole `conversation_depth.json` corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct ConversationCorpus {
    #[serde(default)]
    pub schema_version: String,
    /// Fraction of planted facts that must be recalled at a probe depth for that
    /// depth to count toward `recall_ceiling_turns`.
    pub recall_threshold: f64,
    pub conversations: Vec<Conversation>,
}

/// One scripted conversation of a fixed length.
#[derive(Debug, Clone, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub length_turns: usize,
    /// User-turn indices whose model responses are sampled for coherence.
    #[serde(default)]
    pub coherence_sample_turns: Vec<usize>,
    /// Planted facts (declared once; `turns[*].plants` reference them by key).
    pub facts: Vec<Fact>,
    pub turns: Vec<Turn>,
}

/// A planted recall target.
#[derive(Debug, Clone, Deserialize)]
pub struct Fact {
    pub key: String,
    pub plant_turn: usize,
    /// Canonical value (audit/debug only; scoring uses probe `expect_substrings`).
    #[serde(default)]
    pub value: String,
    /// The natural-language statement the user makes when planting (audit only;
    /// the turn text carries the actual planted sentence).
    #[serde(default)]
    pub statement: String,
}

/// One scripted user turn.
#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub index: usize,
    /// Always `"user"` in this corpus (the model produces the assistant turns).
    pub role: String,
    pub text: String,
    /// Fact keys planted by this turn.
    #[serde(default)]
    pub plants: Vec<String>,
    /// Recall probes evaluated against the model's response to this turn.
    #[serde(default)]
    pub probes: Vec<Probe>,
}

/// A recall probe: at this turn, the model's response should still surface the
/// planted fact (matched by case-insensitive substring).
#[derive(Debug, Clone, Deserialize)]
pub struct Probe {
    /// The planted fact this probe checks.
    pub key: String,
    /// `"verbatim"` or `"paraphrased"` (recorded for analysis; scoring is identical).
    #[serde(default)]
    pub kind: String,
    /// Any of these (lowercased) appearing in the response counts as recalled.
    pub expect_substrings: Vec<String>,
}

/// The embedded corpus, checked into the repo (so the harness needs no external
/// file at runtime; PII-free by construction).
const CONVERSATION_DEPTH_JSON: &str = include_str!("corpora/conversation_depth.json");

/// Load + parse the embedded corpus.
pub fn load_corpus() -> Result<ConversationCorpus, String> {
    serde_json::from_str(CONVERSATION_DEPTH_JSON)
        .map_err(|e| format!("conversation_depth.json parse error: {e}"))
}

/// Validate corpus integrity: every planted fact has at least one matching probe
/// at a strictly later turn, every probe references a declared+planted fact, and
/// turn indices are 1..=length, strictly increasing. Returns the list of problems
/// (empty ⇒ valid).
pub fn validate_corpus(corpus: &ConversationCorpus) -> Vec<String> {
    let mut problems = Vec::new();
    if !(0.0..=1.0).contains(&corpus.recall_threshold) {
        problems.push(format!(
            "recall_threshold {} not in [0,1]",
            corpus.recall_threshold
        ));
    }
    for c in &corpus.conversations {
        let declared: BTreeMap<&str, &Fact> =
            c.facts.iter().map(|f| (f.key.as_str(), f)).collect();

        // turn indices monotonic + within range
        let mut prev = 0usize;
        for t in &c.turns {
            if t.index <= prev {
                problems.push(format!("{}: turn index {} not increasing", c.id, t.index));
            }
            prev = t.index;
            if t.index < 1 || t.index > c.length_turns {
                problems.push(format!(
                    "{}: turn index {} outside 1..={}",
                    c.id, t.index, c.length_turns
                ));
            }
        }

        // every plant references a declared fact and matches its plant_turn
        for t in &c.turns {
            for p in &t.plants {
                match declared.get(p.as_str()) {
                    None => problems
                        .push(format!("{}: turn {} plants undeclared fact '{}'", c.id, t.index, p)),
                    Some(f) if f.plant_turn != t.index => problems.push(format!(
                        "{}: fact '{}' declared plant_turn {} but planted at turn {}",
                        c.id, p, f.plant_turn, t.index
                    )),
                    _ => {}
                }
            }
        }

        // every probe references a declared fact and probes strictly after plant
        for t in &c.turns {
            for pr in &t.probes {
                match declared.get(pr.key.as_str()) {
                    None => problems.push(format!(
                        "{}: turn {} probes undeclared fact '{}'",
                        c.id, t.index, pr.key
                    )),
                    Some(f) if f.plant_turn >= t.index => problems.push(format!(
                        "{}: fact '{}' probed at turn {} not after plant_turn {}",
                        c.id, pr.key, t.index, f.plant_turn
                    )),
                    _ => {}
                }
                if pr.expect_substrings.is_empty() {
                    problems.push(format!(
                        "{}: probe for '{}' at turn {} has no expect_substrings",
                        c.id, pr.key, t.index
                    ));
                }
            }
        }

        // every declared fact is probed at least once
        for f in &c.facts {
            let probed = c
                .turns
                .iter()
                .any(|t| t.probes.iter().any(|p| p.key == f.key));
            if !probed {
                problems.push(format!("{}: fact '{}' never probed", c.id, f.key));
            }
            let planted = c
                .turns
                .iter()
                .any(|t| t.plants.iter().any(|k| k == &f.key));
            if !planted {
                problems.push(format!("{}: fact '{}' never planted in a turn", c.id, f.key));
            }
        }
    }
    problems
}

// ===========================================================================
// Recall scoring (pure, deterministic)
// ===========================================================================

/// One probe outcome at a depth: whether the planted fact was recalled.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeOutcome {
    /// Turn index at which the probe was evaluated (the depth).
    pub depth: usize,
    pub fact_key: String,
    pub kind: String,
    pub recalled: bool,
}

/// Does `response` recall the probe's fact? Case-insensitive substring match on
/// any expected variant. An empty response or refusal trivially fails (no
/// substring present), which is exactly the "refusal/empty ⇒ miss" rule.
pub fn probe_recalled(probe: &Probe, response: &str) -> bool {
    let lc = response.to_lowercase();
    probe
        .expect_substrings
        .iter()
        .any(|s| lc.contains(&s.to_lowercase()))
}

/// A point on the recall curve: at this probe depth, `recalled`/`total` planted
/// facts were recalled.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallPoint {
    pub depth: usize,
    pub recalled: usize,
    pub total: usize,
}

impl RecallPoint {
    pub fn rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.recalled as f64 / self.total as f64
        }
    }
}

/// The full deterministic recall result for one conversation replay.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallResult {
    /// Per-probe-depth recall points, ascending by depth.
    pub curve: Vec<RecallPoint>,
    /// Deepest probe depth whose recall rate is ≥ threshold. `0` ⇒ never met
    /// threshold (immediate degradation / no recall).
    pub recall_ceiling_turns: usize,
    /// Per-probe detail (for audit).
    pub probes: Vec<ProbeOutcome>,
    /// Depth at which the conversation degraded early (timeout/truncation/error),
    /// if any — probes beyond this depth were never evaluated.
    pub degraded_at: Option<usize>,
}

/// Compute the recall curve + ceiling from per-probe outcomes (grouped by depth)
/// against `threshold`. `degraded_at` is carried through for the runner. Pure.
pub fn compute_recall(
    probes: Vec<ProbeOutcome>,
    threshold: f64,
    degraded_at: Option<usize>,
) -> RecallResult {
    // Group probe outcomes by depth, preserving ascending depth order.
    let mut by_depth: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for p in &probes {
        let e = by_depth.entry(p.depth).or_insert((0, 0));
        e.1 += 1;
        if p.recalled {
            e.0 += 1;
        }
    }
    let curve: Vec<RecallPoint> = by_depth
        .into_iter()
        .map(|(depth, (recalled, total))| RecallPoint {
            depth,
            recalled,
            total,
        })
        .collect();

    // recall_ceiling_turns: deepest depth still meeting threshold.
    let recall_ceiling_turns = curve
        .iter()
        .filter(|p| p.rate() >= threshold)
        .map(|p| p.depth)
        .max()
        .unwrap_or(0);

    RecallResult {
        curve,
        recall_ceiling_turns,
        probes,
        degraded_at,
    }
}

// ===========================================================================
// Inference abstraction (unified path live; mockable for tests)
// ===========================================================================

/// One model turn: given the running conversation history (alternating
/// user/assistant text) and the next user message, produce the assistant reply.
///
/// The reply carries a `degraded` signal so a timeout / truncation / transport
/// error / empty output becomes **degradation at the current depth**, never a
/// panic. The runner stops the conversation when `degraded` is set.
#[derive(Debug, Clone, Default)]
pub struct TurnReply {
    pub text: String,
    /// True ⇒ the turn degraded (timeout, truncation, transport error, OOM, or an
    /// empty response). The runner records degradation at this depth and stops.
    pub degraded: bool,
    /// Human-readable degradation reason (for audit), when `degraded`.
    pub degrade_reason: Option<String>,
}

/// The inference surface the runner depends on. The live impl ([`LiveModel`])
/// routes through Chord's unified path; tests inject a mock.
#[async_trait::async_trait]
pub trait ConversationModel: Send + Sync {
    /// Produce the assistant reply for `user_text`, given prior `history`
    /// (`(role, text)` pairs, role ∈ {"user","assistant"}). Must never panic —
    /// failures map to `TurnReply { degraded: true, .. }`.
    async fn respond(&self, history: &[(String, String)], user_text: &str) -> TurnReply;
}

/// Live model: replays the conversation through Chord's unified inference path
/// via [`crate::intake::context::generate`] →
/// [`crate::intake::infer::infer_with_metrics`] (P5 backend routing). Flattens
/// the running history into a single prompt (the unified `generate` surface is
/// prompt-in/text-out, matching how S83's `context`/`agent` suites drive it).
pub struct LiveModel {
    client: reqwest::Client,
    model_name: String,
    timeout: Duration,
}

impl LiveModel {
    pub fn new(client: reqwest::Client, model_name: impl Into<String>, timeout: Duration) -> Self {
        LiveModel {
            client,
            model_name: model_name.into(),
            timeout,
        }
    }

    /// Flatten history + the new user turn into a single chat-style prompt.
    fn render_prompt(history: &[(String, String)], user_text: &str) -> String {
        let mut p = String::new();
        for (role, text) in history {
            let tag = if role == "assistant" { "Assistant" } else { "User" };
            p.push_str(tag);
            p.push_str(": ");
            p.push_str(text);
            p.push_str("\n\n");
        }
        p.push_str("User: ");
        p.push_str(user_text);
        p.push_str("\n\nAssistant:");
        p
    }
}

#[async_trait::async_trait]
impl ConversationModel for LiveModel {
    async fn respond(&self, history: &[(String, String)], user_text: &str) -> TurnReply {
        let prompt = Self::render_prompt(history, user_text);
        // UNIFIED PATH: context::generate → infer::infer_with_metrics (P5).
        let out = crate::intake::context::generate(
            &self.client,
            &self.model_name,
            &prompt,
            self.timeout,
        )
        .await;

        if let Some(err) = out.error {
            // timeout / transport / HTTP / OOM → degradation, not a crash.
            return TurnReply {
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
            // empty string at depth = truncation/refusal-shaped degradation.
            return TurnReply {
                text: String::new(),
                degraded: true,
                degrade_reason: Some("empty response (truncation/refusal)".into()),
            };
        }
        TurnReply {
            text: out.response,
            degraded: false,
            degrade_reason: None,
        }
    }
}

// ===========================================================================
// Coherence panel (judged)
// ===========================================================================

/// The single coherence trait the panel scores per sampled response.
pub const COHERENCE_TRAIT: &str = "coherence";

/// Build a coherence-rubric judge prompt for one sampled response. Ends with the
/// shared [`JSON_CONTRACT_SUFFIX`] so the judge harness's strict extractor
/// applies. The conversation context is summarized as the running topic; no infra
/// values appear (the corpus is abstract).
pub fn coherence_prompt(user_text: &str, response: &str) -> String {
    format!(
        "You are scoring an assistant's reply in a multi-turn conversation for \
COHERENCE and on-voice consistency. Rubric (integer 1-5):\n\
5 = fully on-topic, internally consistent, steady tone/voice;\n\
4 = mostly on-topic and consistent, minor wobble;\n\
3 = partially on-topic, some drift or tonal inconsistency;\n\
2 = largely off-topic or inconsistent;\n\
1 = incoherent, off-topic, or empty.\n\n\
User said:\n{user}\n\nAssistant replied:\n{resp}\n\n\
Score the single trait \"{trait}\". {suffix}",
        user = user_text.trim(),
        resp = if response.trim().is_empty() {
            "(empty response)"
        } else {
            response.trim()
        },
        trait = COHERENCE_TRAIT,
        suffix = JSON_CONTRACT_SUFFIX,
    )
}

// ===========================================================================
// Per-conversation replay (runner core)
// ===========================================================================

/// Result of replaying one conversation against one model.
#[derive(Debug, Clone)]
pub struct ConversationRun {
    pub conversation_id: String,
    pub recall: RecallResult,
    /// (user_text, response) sampled at `coherence_sample_turns` (only turns
    /// actually reached before degradation).
    pub coherence_samples: Vec<(String, String)>,
    /// Depth at which degradation was recorded, if any.
    pub degraded_at: Option<usize>,
}

/// Replay one conversation against `model`, evaluating recall probes and
/// collecting coherence samples. Stops at the first degraded turn, recording the
/// degradation depth (never panics).
pub async fn replay_conversation(
    model: &dyn ConversationModel,
    conv: &Conversation,
    threshold: f64,
) -> ConversationRun {
    let mut history: Vec<(String, String)> = Vec::new();
    let mut probe_outcomes: Vec<ProbeOutcome> = Vec::new();
    let mut coherence_samples: Vec<(String, String)> = Vec::new();
    let mut degraded_at: Option<usize> = None;

    for turn in &conv.turns {
        let reply = model.respond(&history, &turn.text).await;

        if reply.degraded {
            // Degradation at this depth: record and STOP (no crash, no further
            // probes). Truncation/timeout becomes the recorded ceiling region.
            degraded_at = Some(turn.index);
            // Probes at this very turn count as misses (the model produced nothing
            // usable), so the curve reflects the degradation honestly.
            for pr in &turn.probes {
                probe_outcomes.push(ProbeOutcome {
                    depth: turn.index,
                    fact_key: pr.key.clone(),
                    kind: pr.kind.clone(),
                    recalled: false,
                });
            }
            break;
        }

        // Evaluate recall probes for this turn.
        for pr in &turn.probes {
            probe_outcomes.push(ProbeOutcome {
                depth: turn.index,
                fact_key: pr.key.clone(),
                kind: pr.kind.clone(),
                recalled: probe_recalled(pr, &reply.text),
            });
        }

        // Collect coherence sample if this depth is sampled.
        if conv.coherence_sample_turns.contains(&turn.index) {
            coherence_samples.push((turn.text.clone(), reply.text.clone()));
        }

        // Extend history with this exchange for the next turn.
        history.push(("user".to_string(), turn.text.clone()));
        history.push(("assistant".to_string(), reply.text));
    }

    let recall = compute_recall(probe_outcomes, threshold, degraded_at);
    ConversationRun {
        conversation_id: conv.id.clone(),
        recall,
        coherence_samples,
        degraded_at,
    }
}

// ===========================================================================
// Aggregation across conversations → DimensionScore rows
// ===========================================================================

/// Dimension label written to `assistant_dimension_score.dimension`.
pub const DIMENSION: &str = "conversation_depth";

/// Metric names emitted by this dimension.
pub const METRIC_RECALL_CEILING: &str = "recall_ceiling_turns";
pub const METRIC_COHERENCE: &str = "coherence";

/// The full dim-1 outcome for one (model, backend): the deterministic recall axis
/// plus the judged coherence axis, ready to flatten into storage rows.
#[derive(Debug, Clone)]
pub struct Dim1Outcome {
    pub per_conversation: Vec<ConversationRun>,
    /// Max `recall_ceiling_turns` across conversations (the deepest sustained
    /// recall depth the model reached anywhere).
    pub recall_ceiling_turns: usize,
    /// Panel result over all sampled coherence responses (mean + SD), or `None`
    /// when no samples were judged.
    pub coherence: Option<PanelResult>,
}

impl Dim1Outcome {
    /// Flatten into `DimensionScore` rows for one (model, backend):
    ///   - one deterministic `recall_ceiling_turns` row (judge = "deterministic"),
    ///   - one coherence row per judged trait (judge = "panel" / single judge id).
    pub fn into_dimension_scores(
        &self,
        model_id: &ModelId,
        backend_tag: BackendTag,
    ) -> Vec<DimensionScore> {
        let mut rows = Vec::new();

        // Deterministic recall ceiling (single numeric metric, no SD, no judge).
        let recall_audit = serde_json::json!({
            "conversations": self
                .per_conversation
                .iter()
                .map(|r| serde_json::json!({
                    "id": r.conversation_id,
                    "recall_ceiling_turns": r.recall.recall_ceiling_turns,
                    "degraded_at": r.degraded_at,
                    "curve": r.recall.curve.iter().map(|p| serde_json::json!({
                        "depth": p.depth, "recalled": p.recalled, "total": p.total
                    })).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        })
        .to_string();
        rows.push(DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: METRIC_RECALL_CEILING.to_string(),
            value: self.recall_ceiling_turns as f64,
            std_dev: None,
            judge: "deterministic".to_string(),
            low_confidence: false,
            raw_json: Some(recall_audit),
        });

        // Judged coherence axis (mean + SD per trait), if any samples scored.
        if let Some(panel) = &self.coherence {
            rows.extend(panel.into_dimension_scores(model_id, backend_tag));
        }

        rows
    }
}

/// Run the full deterministic recall axis across every conversation in the
/// corpus, then score the collected coherence samples with the judge panel.
///
/// `judges` is the panel ([`super::judges::CliJudge::panel`] live, or mocks in
/// tests). Pure orchestration over the injected [`ConversationModel`] and judges
/// — no DB, no direct network (the model trait owns inference). Never panics.
pub async fn run_dim1(
    model: &dyn ConversationModel,
    judges: &[Box<dyn Judge>],
    corpus: &ConversationCorpus,
) -> Dim1Outcome {
    let mut per_conversation = Vec::with_capacity(corpus.conversations.len());
    let mut coherence_samples: Vec<(String, String)> = Vec::new();

    for conv in &corpus.conversations {
        let run = replay_conversation(model, conv, corpus.recall_threshold).await;
        coherence_samples.extend(run.coherence_samples.iter().cloned());
        per_conversation.push(run);
    }

    let recall_ceiling_turns = per_conversation
        .iter()
        .map(|r| r.recall.recall_ceiling_turns)
        .max()
        .unwrap_or(0);

    // Coherence: average each sampled response's judged score into one panel
    // result. We score each sample independently and pool the per-judge integers
    // so the final mean/SD reflects coherence across sampled depths.
    let coherence = if coherence_samples.is_empty() || judges.is_empty() {
        None
    } else {
        Some(score_coherence(judges, &coherence_samples).await)
    };

    Dim1Outcome {
        per_conversation,
        recall_ceiling_turns,
        coherence,
    }
}

/// Score coherence across sampled (user, response) pairs with the panel. Each
/// sample is judged independently; the per-judge integer scores are pooled so the
/// resulting `PanelResult` carries the mean + sample SD over all (judge × sample)
/// observations — the variance signal the spec wants preserved.
async fn score_coherence(
    judges: &[Box<dyn Judge>],
    samples: &[(String, String)],
) -> PanelResult {
    use super::JudgeOutcome;

    // Pool per-judge scores across samples: judge_id -> Vec<score>.
    let mut pooled: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut any_complied = false;

    for (user_text, response) in samples {
        let prompt = coherence_prompt(user_text, response);
        let pr = judges::run_panel(judges, DIMENSION, &prompt, &[COHERENCE_TRAIT]).await;
        warnings.extend(pr.warnings.iter().cloned());
        for outcome in &pr.outcomes {
            if let JudgeOutcome::Scored { judge, traits } = outcome {
                if let Some(v) = traits.get(COHERENCE_TRAIT) {
                    pooled.entry(judge.clone()).or_default().push(*v);
                    any_complied = true;
                }
            }
        }
    }

    if !any_complied {
        return PanelResult::aggregate(
            DIMENSION,
            vec![JudgeOutcome::Abstained {
                judge: "panel".to_string(),
                reason: "no judge scored any coherence sample".to_string(),
                raw: None,
            }],
            dedup(warnings),
        );
    }

    // Reduce each judge's pooled samples to that judge's mean coherence, rounded
    // to the nearest integer in [1,5] so the shared integer-based aggregation
    // (mean + sample SD across judges) applies unchanged.
    let outcomes: Vec<JudgeOutcome> = pooled
        .into_iter()
        .map(|(judge, scores)| {
            let mean = scores.iter().sum::<i64>() as f64 / scores.len() as f64;
            let rounded = mean.round().clamp(1.0, 5.0) as i64;
            let mut traits = BTreeMap::new();
            traits.insert(COHERENCE_TRAIT.to_string(), rounded);
            JudgeOutcome::Scored { judge, traits }
        })
        .collect();

    PanelResult::aggregate(DIMENSION, outcomes, dedup(warnings))
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
        // 5 conversations of increasing length per the spec.
        let lens: Vec<usize> = corpus.conversations.iter().map(|c| c.length_turns).collect();
        assert_eq!(lens, vec![5, 10, 20, 40, 80]);
    }

    #[test]
    fn validate_catches_probe_before_plant() {
        // Fact probed at a turn that is not after its plant_turn.
        let corpus: ConversationCorpus = serde_json::from_str(
            r#"{
              "recall_threshold": 0.6,
              "conversations": [{
                "id": "bad", "length_turns": 2, "coherence_sample_turns": [],
                "facts": [{"key":"x","plant_turn":2,"value":"v","statement":"s"}],
                "turns": [
                  {"index":1,"role":"user","text":"probe early","probes":[{"key":"x","kind":"verbatim","expect_substrings":["v"]}]},
                  {"index":2,"role":"user","text":"plant","plants":["x"]}
                ]
              }]
            }"#,
        )
        .unwrap();
        let problems = validate_corpus(&corpus);
        assert!(problems.iter().any(|p| p.contains("not after plant_turn")), "{problems:?}");
    }

    #[test]
    fn validate_catches_undeclared_and_unprobed() {
        let corpus: ConversationCorpus = serde_json::from_str(
            r#"{
              "recall_threshold": 0.6,
              "conversations": [{
                "id": "bad", "length_turns": 2, "coherence_sample_turns": [],
                "facts": [{"key":"y","plant_turn":1,"value":"v","statement":"s"}],
                "turns": [
                  {"index":1,"role":"user","text":"plant y","plants":["y"]},
                  {"index":2,"role":"user","text":"probe z","probes":[{"key":"z","kind":"verbatim","expect_substrings":["v"]}]}
                ]
              }]
            }"#,
        )
        .unwrap();
        let problems = validate_corpus(&corpus);
        assert!(problems.iter().any(|p| p.contains("undeclared fact 'z'")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("'y' never probed")), "{problems:?}");
    }

    #[test]
    fn probe_recalled_is_case_insensitive_substring() {
        let p = Probe {
            key: "city".into(),
            kind: "verbatim".into(),
            expect_substrings: vec!["Zubrovka".into()],
        };
        assert!(probe_recalled(&p, "You said the city is ZUBROVKA."));
        assert!(!probe_recalled(&p, "I don't recall."));
        assert!(!probe_recalled(&p, "")); // empty/refusal = miss
    }

    #[test]
    fn compute_recall_curve_and_ceiling() {
        // depth 4: 2/2 recalled (1.0), depth 8: 1/2 (0.5), depth 10: 0/1 (0.0)
        let probes = vec![
            ProbeOutcome { depth: 4, fact_key: "a".into(), kind: "v".into(), recalled: true },
            ProbeOutcome { depth: 4, fact_key: "b".into(), kind: "v".into(), recalled: true },
            ProbeOutcome { depth: 8, fact_key: "c".into(), kind: "v".into(), recalled: true },
            ProbeOutcome { depth: 8, fact_key: "d".into(), kind: "v".into(), recalled: false },
            ProbeOutcome { depth: 10, fact_key: "e".into(), kind: "v".into(), recalled: false },
        ];
        let r = compute_recall(probes, 0.6, None);
        assert_eq!(r.curve.len(), 3);
        assert_eq!(r.curve[0].rate(), 1.0);
        assert_eq!(r.curve[1].rate(), 0.5);
        assert_eq!(r.curve[2].rate(), 0.0);
        // ceiling = deepest depth with rate >= 0.6 → depth 4 (depth 8 is 0.5).
        assert_eq!(r.recall_ceiling_turns, 4);
    }

    #[test]
    fn compute_recall_no_depth_meets_threshold_is_zero_ceiling() {
        let probes = vec![ProbeOutcome {
            depth: 6,
            fact_key: "a".into(),
            kind: "v".into(),
            recalled: false,
        }];
        let r = compute_recall(probes, 0.6, None);
        assert_eq!(r.recall_ceiling_turns, 0);
    }

    #[test]
    fn coherence_prompt_ends_with_contract() {
        let p = coherence_prompt("hi", "hello there");
        assert!(p.trim_end().ends_with(JSON_CONTRACT_SUFFIX));
        assert!(p.contains("coherence"));
        // empty response handled without panic
        let p2 = coherence_prompt("hi", "   ");
        assert!(p2.contains("(empty response)"));
    }

    #[test]
    fn live_model_render_prompt_alternates_roles() {
        let hist = vec![
            ("user".to_string(), "first".to_string()),
            ("assistant".to_string(), "reply".to_string()),
        ];
        let p = LiveModel::render_prompt(&hist, "second");
        assert!(p.contains("User: first"));
        assert!(p.contains("Assistant: reply"));
        assert!(p.trim_end().ends_with("Assistant:"));
        assert!(p.find("first").unwrap() < p.find("second").unwrap());
    }

    // ── mock model for replay/runner unit coverage ──

    struct MockModel {
        /// Responses keyed by the user_text substring they should react to.
        scripted: BTreeMap<String, String>,
        /// Turn index at which to force degradation (None = never).
        degrade_at: Option<usize>,
        calls: std::sync::Mutex<usize>,
    }

    impl MockModel {
        fn new(scripted: BTreeMap<String, String>, degrade_at: Option<usize>) -> Self {
            MockModel { scripted, degrade_at, calls: std::sync::Mutex::new(0) }
        }
    }

    #[async_trait::async_trait]
    impl ConversationModel for MockModel {
        async fn respond(&self, _history: &[(String, String)], user_text: &str) -> TurnReply {
            let mut n = self.calls.lock().unwrap();
            *n += 1;
            let this_turn = *n;
            drop(n);
            if Some(this_turn) == self.degrade_at {
                return TurnReply {
                    text: String::new(),
                    degraded: true,
                    degrade_reason: Some("forced timeout".into()),
                };
            }
            // Echo any scripted answer whose key is a substring of the user text.
            for (k, v) in &self.scripted {
                if user_text.to_lowercase().contains(&k.to_lowercase()) {
                    return TurnReply { text: v.clone(), ..Default::default() };
                }
            }
            TurnReply { text: "Sure, here is a suggestion.".into(), ..Default::default() }
        }
    }

    fn tiny_conv() -> Conversation {
        serde_json::from_str(
            r#"{
              "id":"t","length_turns":4,"coherence_sample_turns":[2],
              "facts":[{"key":"city","plant_turn":1,"value":"Zubrovka","statement":"s"}],
              "turns":[
                {"index":1,"role":"user","text":"My city is Zubrovka","plants":["city"]},
                {"index":2,"role":"user","text":"any tips?"},
                {"index":3,"role":"user","text":"more tips?"},
                {"index":4,"role":"user","text":"which city did I name?","probes":[{"key":"city","kind":"verbatim","expect_substrings":["zubrovka"]}]}
              ]
            }"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn replay_scores_recall_hit() {
        let mut scripted = BTreeMap::new();
        scripted.insert("which city".to_string(), "You named Zubrovka.".to_string());
        let model = MockModel::new(scripted, None);
        let run = replay_conversation(&model, &tiny_conv(), 0.6).await;
        assert_eq!(run.degraded_at, None);
        assert_eq!(run.recall.recall_ceiling_turns, 4); // hit at depth 4
        assert_eq!(run.coherence_samples.len(), 1); // turn 2 sampled
    }

    #[tokio::test]
    async fn replay_degradation_stops_and_records_depth() {
        // Degrade at turn 3 → probe at turn 4 never evaluated; degraded_at = 3.
        let model = MockModel::new(BTreeMap::new(), Some(3));
        let run = replay_conversation(&model, &tiny_conv(), 0.6).await;
        assert_eq!(run.degraded_at, Some(3));
        // No probe outcomes recorded beyond depth 3; ceiling falls to 0 (no recall).
        assert_eq!(run.recall.recall_ceiling_turns, 0);
        // The run did NOT panic and produced a result — degradation, not a crash.
    }

    // ── scripted judges for the coherence axis ──

    struct FixedJudge {
        id: String,
        score: i64,
    }

    #[async_trait::async_trait]
    impl Judge for FixedJudge {
        fn id(&self) -> &str {
            &self.id
        }
        async fn invoke(&self, _prompt: &str, _attempt: u8) -> judges::JudgeReply {
            judges::JudgeReply::Text(format!("{{\"coherence\": {}}}", self.score))
        }
    }

    #[tokio::test]
    async fn run_dim1_produces_recall_and_coherence_rows() {
        let mut scripted = BTreeMap::new();
        scripted.insert("which city".to_string(), "Zubrovka, you said.".to_string());
        let model = MockModel::new(scripted, None);

        let corpus = ConversationCorpus {
            schema_version: "test".into(),
            recall_threshold: 0.6,
            conversations: vec![tiny_conv()],
        };
        let panel: Vec<Box<dyn Judge>> = vec![
            Box::new(FixedJudge { id: "claude".into(), score: 4 }),
            Box::new(FixedJudge { id: "gemini".into(), score: 5 }),
            Box::new(FixedJudge { id: "codex".into(), score: 3 }),
        ];
        let outcome = run_dim1(&model, &panel, &corpus).await;
        assert_eq!(outcome.recall_ceiling_turns, 4);
        let coh = outcome.coherence.as_ref().expect("coherence scored");
        assert_eq!(coh.complying, 3);
        // [4,5,3] → mean 4.0, SD 1.0
        let agg = &coh.aggregates[COHERENCE_TRAIT];
        assert!((agg.mean - 4.0).abs() < 1e-9);
        assert!((agg.std_dev.unwrap() - 1.0).abs() < 1e-9);

        let rows = outcome.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Gpu);
        // one recall_ceiling row + one coherence row
        assert!(rows.iter().any(|r| r.metric == METRIC_RECALL_CEILING && r.judge == "deterministic"));
        let coh_row = rows.iter().find(|r| r.metric == COHERENCE_TRAIT).unwrap();
        assert_eq!(coh_row.judge, "panel");
        assert_eq!(coh_row.dimension, DIMENSION);
        assert_eq!(coh_row.backend_tag, BackendTag::Gpu);
    }

    #[tokio::test]
    async fn run_dim1_no_judges_skips_coherence() {
        let model = MockModel::new(BTreeMap::new(), None);
        let corpus = ConversationCorpus {
            schema_version: "test".into(),
            recall_threshold: 0.6,
            conversations: vec![tiny_conv()],
        };
        let outcome = run_dim1(&model, &[], &corpus).await;
        assert!(outcome.coherence.is_none());
        // recall row still emitted.
        let rows = outcome.into_dimension_scores(&ModelId::from("m"), BackendTag::Cpu);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metric, METRIC_RECALL_CEILING);
    }
}
