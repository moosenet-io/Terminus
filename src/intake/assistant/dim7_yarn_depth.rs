//! Dimension "yarn_context_depth" — YaRN context-extension quality
//! degradation for ASSISTANT candidates (S86 YaRN-assistant extension).
//!
//! ## What this measures
//! For a model served with YaRN rope-scaling (`--rope-scaling yarn
//! --rope-scale N --yarn-orig-ctx <native_ctx> -c <extended_ctx>`), this
//! harness probes assistant quality — NOT code quality — at increasing
//! **actual context-token depths**: native baseline, then 30%/60%/100% of the
//! YaRN-extended target context ([`DepthRung`]). This mirrors the coder-side
//! YaRN validation discipline (probe at rising depth, find where quality
//! collapses rather than trusting the advertised extended-context number,
//! and STOP probing the moment it collapses instead of grinding to the
//! ceiling) but scores it with dim1's ASSISTANT primitives: a planted-fact
//! recall check ([`super::dim1_conversation::probe_recalled`]) and a judged
//! coherence score ([`super::dim1_conversation::coherence_prompt`] +
//! [`super::judges::run_panel`]), instead of dim1's byte-identical
//! recall/coherence *scoring* logic, applied at token depth rather than turn
//! depth.
//!
//! ## Why a separate dimension, not folded into dim1
//! dim1_conversation's depth axis is conversation TURNS on a fixed corpus,
//! and it runs for EVERY model in the standard six-dimension suite
//! ([`super::runner::SUITE_DIMENSIONS`]). YaRN-depth probing needs actual
//! CONTEXT-TOKEN depth computed from a model's own `native_ctx` /
//! `extended_ctx` (which differ per model and only apply to the subset of
//! nominated models flagged `yarn_capable` in `nominations.json`). Folding
//! this into dim1 would either force every model through token-padded
//! prompts it doesn't need, or silently change dim1's byte-identical corpus
//! contract with S83/MINT. A sibling dimension keeps dim1 untouched and
//! writes its own rows under a distinct `dimension = "yarn_context_depth"`
//! label — joinable the same way, on `(model_id, backend_tag, mem_config)`.
//!
//! This dimension is intentionally NOT added to `SUITE_DIMENSIONS`: it is
//! driven explicitly (per yarn-capable model, with that model's own
//! native/extended context) via [`run_yarn_depth`] /
//! [`run_yarn_depth_and_write`], not by the generic per-model suite loop.
//!
//! ## Persistence / mem_config
//! [`run_yarn_depth_and_write`] writes through the SAME
//! [`super::runner::ScoreSink`] abstraction the standard dimensions use, so
//! whichever sink the caller passes (the live [`super::runner::PgScoreSink`]
//! or a test double) is respected — including its `mem_config` tagging via
//! [`super::schema::insert_dimension_score_with_category_and_mem_config`].
//! This module never talks to Postgres directly.
//!
//! ## Degradation, never a crash
//! Exactly like dim1: a timeout, truncation, transport error, or empty
//! response at a probed depth is recorded as degradation at that depth, the
//! ladder stops there, and nothing here panics or aborts the run.

use serde::Deserialize;

use super::dim1_conversation::{coherence_prompt, probe_recalled, ConversationModel, Probe, COHERENCE_TRAIT};
use super::judges::{self, Judge};
use super::runner::ScoreSink;
use super::{BackendTag, DimensionScore, ModelId};

/// Dimension label written to `assistant_dimension_score.dimension`.
pub const DIMENSION: &str = "yarn_context_depth";

/// Deterministic summary metric: the deepest token target the model cleared
/// without collapsing (0 ⇒ collapsed at or before the native baseline).
pub const METRIC_USABLE_CEILING: &str = "usable_ceiling_tokens";

/// A judged coherence mean below this (on the shared 1-5 rubric) counts as
/// "weak" for collapse purposes at a given depth. This threshold is new to
/// this harness (there is no coder-side constant to reuse — the coder-side
/// YaRN validator's `combined_score`/`WEAK_BASELINE_THRESHOLD` pattern
/// described for this task was not found anywhere in this repository as of
/// commit a7de7ad; see the module-level doc and the build report). Chosen
/// as the midpoint below which the rubric's own text ("largely off-topic or
/// inconsistent") applies.
pub const WEAK_COHERENCE_THRESHOLD: f64 = 2.0;

/// Rough chars-per-token used only to size filler padding; this is a sizing
/// heuristic, not a tokenizer, and is documented as such everywhere it's used.
const APPROX_CHARS_PER_TOKEN: usize = 4;

// ===========================================================================
// Corpus (facts + filler live in JSON, never in code — matches dim1)
// ===========================================================================

/// The whole `yarn_depth_facts.json` corpus: one planted fact + filler text
/// used to pad a single-turn prompt out to a target context-token depth.
#[derive(Debug, Clone, Deserialize)]
pub struct YarnDepthCorpus {
    #[serde(default)]
    pub schema_version: String,
    pub fact_key: String,
    pub fact_statement: String,
    pub expect_substrings: Vec<String>,
    pub probe_question: String,
    pub filler_paragraph: String,
}

const YARN_DEPTH_JSON: &str = include_str!("corpora/yarn_depth_facts.json");

/// Load + parse the embedded corpus.
pub fn load_corpus() -> Result<YarnDepthCorpus, String> {
    serde_json::from_str(YARN_DEPTH_JSON).map_err(|e| format!("yarn_depth_facts.json parse error: {e}"))
}

/// Validate corpus integrity. Returns the list of problems (empty ⇒ valid).
pub fn validate_corpus(corpus: &YarnDepthCorpus) -> Vec<String> {
    let mut problems = Vec::new();
    if corpus.fact_key.trim().is_empty() {
        problems.push("fact_key is empty".to_string());
    }
    if corpus.fact_statement.trim().is_empty() {
        problems.push("fact_statement is empty".to_string());
    }
    if corpus.expect_substrings.is_empty() {
        problems.push("expect_substrings is empty".to_string());
    }
    if corpus.probe_question.trim().is_empty() {
        problems.push("probe_question is empty".to_string());
    }
    if corpus.filler_paragraph.trim().is_empty() {
        problems.push("filler_paragraph is empty".to_string());
    }
    problems
}

// ===========================================================================
// Depth ladder
// ===========================================================================

/// One rung of the YaRN depth ladder: native baseline, then 30%/60%/100% of
/// the model's YaRN-**extended** target context (not of native) — the same
/// ladder shape the coder-side YaRN validation harness is described as using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepthRung {
    Native,
    Pct30,
    Pct60,
    Pct100,
}

impl DepthRung {
    /// Probing order: shallowest first, so a collapse stops the ladder before
    /// wasting time/inference on deeper, doomed probes.
    pub const ORDER: [DepthRung; 4] =
        [DepthRung::Native, DepthRung::Pct30, DepthRung::Pct60, DepthRung::Pct100];

    pub fn label(self) -> &'static str {
        match self {
            DepthRung::Native => "native",
            DepthRung::Pct30 => "pct30",
            DepthRung::Pct60 => "pct60",
            DepthRung::Pct100 => "pct100",
        }
    }

    /// Target context-token depth for this rung.
    pub fn token_target(self, native_ctx: usize, extended_ctx: usize) -> usize {
        match self {
            DepthRung::Native => native_ctx,
            DepthRung::Pct30 => ((extended_ctx as f64) * 0.30).round() as usize,
            DepthRung::Pct60 => ((extended_ctx as f64) * 0.60).round() as usize,
            DepthRung::Pct100 => extended_ctx,
        }
    }
}

/// Estimate token count from character length. A sizing heuristic only —
/// good enough to land padded prompts in the right neighborhood of a target
/// depth; never used for anything scoring-relevant.
pub fn approx_tokens(s: &str) -> usize {
    s.len() / APPROX_CHARS_PER_TOKEN
}

/// Build a single-turn prompt: the planted fact up front, filler padding
/// repeated until the prompt is near `target_tokens`, then the probe
/// question at the end (so recall must survive the whole padded span).
pub fn build_padded_prompt(corpus: &YarnDepthCorpus, target_tokens: usize) -> String {
    let mut prompt = String::new();
    prompt.push_str(&corpus.fact_statement);
    prompt.push('\n');

    // Reserve headroom for the fact + question themselves so padding doesn't
    // push the total well past target_tokens.
    let reserved_tokens =
        approx_tokens(&corpus.fact_statement) + approx_tokens(&corpus.probe_question) + 32;
    let pad_tokens = target_tokens.saturating_sub(reserved_tokens);
    let pad_chars = pad_tokens * APPROX_CHARS_PER_TOKEN;

    let unit = corpus.filler_paragraph.trim();
    if !unit.is_empty() {
        while prompt.len() < pad_chars {
            prompt.push_str(unit);
            prompt.push(' ');
        }
    }

    prompt.push('\n');
    prompt.push_str(&corpus.probe_question);
    prompt
}

// ===========================================================================
// Per-rung outcome + ladder result
// ===========================================================================

/// Outcome of probing one depth rung.
#[derive(Debug, Clone)]
pub struct YarnDepthPoint {
    pub rung: DepthRung,
    pub token_target: usize,
    pub recalled: bool,
    /// Judged coherence mean (1-5), `None` when no judges configured or none
    /// complied.
    pub coherence: Option<f64>,
    pub degraded: bool,
    pub degrade_reason: Option<String>,
    /// True ⇒ this rung is where quality collapsed (degradation, a recall
    /// miss, or weak/unconfirmed coherence). The ladder stops here.
    pub collapsed: bool,
}

/// The full YaRN-depth ladder result for one (model, backend, native/extended
/// context pair).
#[derive(Debug, Clone)]
pub struct YarnDepthOutcome {
    /// Rungs actually probed, ascending depth, stopping at the first collapse.
    pub points: Vec<YarnDepthPoint>,
    /// Deepest token target cleared WITHOUT collapsing (the real usable
    /// ceiling — may be far below the advertised `extended_ctx`). `0` ⇒
    /// collapsed at or before the native baseline.
    pub usable_ceiling_tokens: usize,
    /// True ⇒ the ladder stopped before reaching `Pct100` because of a
    /// collapse (the "don't grind past collapse" discipline).
    pub stopped_early: bool,
}

/// Run the YaRN depth ladder against `model`, using `judges` for the
/// coherence axis (empty ⇒ recall-only collapse detection, matching dim1's
/// "no judges ⇒ skip coherence" behavior). Never panics: inference failures
/// at a rung are recorded as degradation at that rung and the ladder stops.
pub async fn run_yarn_depth(
    model: &dyn ConversationModel,
    judges: &[Box<dyn Judge>],
    corpus: &YarnDepthCorpus,
    native_ctx: usize,
    extended_ctx: usize,
) -> YarnDepthOutcome {
    let mut points = Vec::with_capacity(DepthRung::ORDER.len());
    let mut usable_ceiling_tokens = 0usize;
    let mut stopped_early = false;

    for rung in DepthRung::ORDER {
        let token_target = rung.token_target(native_ctx, extended_ctx);
        let prompt = build_padded_prompt(corpus, token_target);
        let reply = model.respond(&[], &prompt).await;

        if reply.degraded {
            points.push(YarnDepthPoint {
                rung,
                token_target,
                recalled: false,
                coherence: None,
                degraded: true,
                degrade_reason: reply.degrade_reason.clone(),
                collapsed: true,
            });
            stopped_early = true;
            break;
        }

        let probe = Probe {
            key: corpus.fact_key.clone(),
            kind: "yarn-depth".to_string(),
            expect_substrings: corpus.expect_substrings.clone(),
        };
        let recalled = probe_recalled(&probe, &reply.text);

        let coherence = if judges.is_empty() {
            None
        } else {
            let judge_prompt = coherence_prompt(&corpus.probe_question, &reply.text);
            let panel = judges::run_panel(judges, DIMENSION, &judge_prompt, &[COHERENCE_TRAIT]).await;
            panel.aggregates.get(COHERENCE_TRAIT).map(|a| a.mean)
        };

        // Collapse: a recall miss always collapses. When judges are
        // configured, a weak or unconfirmed (no judge complied) coherence
        // score ALSO collapses — an assistant that "remembers" the fact but
        // has gone incoherent is not a usable depth either. When no judges
        // are configured, collapse is recall-only (mirrors dim1's
        // no-judges-skip-coherence rule).
        let collapsed = if judges.is_empty() {
            !recalled
        } else {
            !recalled || coherence.map(|c| c < WEAK_COHERENCE_THRESHOLD).unwrap_or(true)
        };

        points.push(YarnDepthPoint {
            rung,
            token_target,
            recalled,
            coherence,
            degraded: false,
            degrade_reason: None,
            collapsed,
        });

        if collapsed {
            stopped_early = true;
            break;
        }
        usable_ceiling_tokens = token_target;
    }

    YarnDepthOutcome {
        points,
        usable_ceiling_tokens,
        stopped_early,
    }
}

impl YarnDepthOutcome {
    /// Flatten into `DimensionScore` rows for one (model, backend):
    ///   - one deterministic `usable_ceiling_tokens` row (audit carries every
    ///     probed rung),
    ///   - one deterministic `recall_<rung>` row per probed rung,
    ///   - one `coherence_<rung>` row per probed rung that was judged.
    pub fn into_dimension_scores(
        &self,
        model_id: &ModelId,
        backend_tag: BackendTag,
        native_ctx: usize,
        extended_ctx: usize,
    ) -> Vec<DimensionScore> {
        let mut rows = Vec::new();

        let audit = serde_json::json!({
            "native_ctx": native_ctx,
            "extended_ctx": extended_ctx,
            "stopped_early": self.stopped_early,
            "points": self.points.iter().map(|p| serde_json::json!({
                "rung": p.rung.label(),
                "token_target": p.token_target,
                "recalled": p.recalled,
                "coherence": p.coherence,
                "degraded": p.degraded,
                "degrade_reason": p.degrade_reason,
                "collapsed": p.collapsed,
            })).collect::<Vec<_>>(),
        })
        .to_string();

        rows.push(DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: METRIC_USABLE_CEILING.to_string(),
            value: self.usable_ceiling_tokens as f64,
            std_dev: None,
            judge: "deterministic".to_string(),
            low_confidence: false,
            raw_json: Some(audit),
        });

        for p in &self.points {
            rows.push(DimensionScore {
                model_id: model_id.clone(),
                backend_tag,
                dimension: DIMENSION.to_string(),
                metric: format!("recall_{}", p.rung.label()),
                value: if p.recalled { 1.0 } else { 0.0 },
                std_dev: None,
                judge: "deterministic".to_string(),
                low_confidence: false,
                raw_json: None,
            });

            if let Some(c) = p.coherence {
                rows.push(DimensionScore {
                    model_id: model_id.clone(),
                    backend_tag,
                    dimension: DIMENSION.to_string(),
                    metric: format!("coherence_{}", p.rung.label()),
                    value: c,
                    std_dev: None,
                    judge: "panel".to_string(),
                    low_confidence: false,
                    raw_json: None,
                });
            }
        }

        rows
    }
}

/// Run the ladder AND persist its rows through `sink` — the same
/// [`ScoreSink`] abstraction [`super::runner`] uses, so the live
/// [`super::runner::PgScoreSink`] (which tags `mem_config` via
/// [`super::schema::insert_dimension_score_with_category_and_mem_config`])
/// applies unchanged, and tests can inject an in-memory sink.
pub async fn run_yarn_depth_and_write(
    model: &dyn ConversationModel,
    judges: &[Box<dyn Judge>],
    corpus: &YarnDepthCorpus,
    native_ctx: usize,
    extended_ctx: usize,
    model_id: &ModelId,
    backend_tag: BackendTag,
    sink: &dyn ScoreSink,
) -> Result<YarnDepthOutcome, String> {
    let outcome = run_yarn_depth(model, judges, corpus, native_ctx, extended_ctx).await;
    let rows = outcome.into_dimension_scores(model_id, backend_tag, native_ctx, extended_ctx);
    sink.write(&rows).await?;
    Ok(outcome)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn embedded_corpus_parses_and_validates() {
        let corpus = load_corpus().expect("corpus parses");
        let problems = validate_corpus(&corpus);
        assert!(problems.is_empty(), "corpus problems: {problems:?}");
    }

    #[test]
    fn validate_catches_missing_fields() {
        let bad = YarnDepthCorpus {
            schema_version: "t".into(),
            fact_key: "".into(),
            fact_statement: "".into(),
            expect_substrings: vec![],
            probe_question: "".into(),
            filler_paragraph: "".into(),
        };
        let problems = validate_corpus(&bad);
        assert_eq!(problems.len(), 5, "{problems:?}");
    }

    #[test]
    fn depth_rung_targets_ladder_native_then_pct_of_extended() {
        let native = 8_192;
        let extended = 100_000;
        assert_eq!(DepthRung::Native.token_target(native, extended), 8_192);
        assert_eq!(DepthRung::Pct30.token_target(native, extended), 30_000);
        assert_eq!(DepthRung::Pct60.token_target(native, extended), 60_000);
        assert_eq!(DepthRung::Pct100.token_target(native, extended), 100_000);
    }

    #[test]
    fn padded_prompt_lands_near_target_tokens() {
        let corpus = load_corpus().unwrap();
        for target in [500usize, 2_000, 8_000] {
            let prompt = build_padded_prompt(&corpus, target);
            let got = approx_tokens(&prompt);
            // Heuristic padding: allow it to land within one filler unit of
            // the target (never wildly over/under).
            let filler_tokens = approx_tokens(corpus.filler_paragraph.trim()).max(1);
            assert!(
                got + filler_tokens >= target,
                "target {target} not reached: got {got} tokens"
            );
        }
        // The planted fact and probe question are always present, however
        // small the target.
        let tiny = build_padded_prompt(&corpus, 0);
        assert!(tiny.contains(&corpus.fact_statement));
        assert!(tiny.contains(&corpus.probe_question));
    }

    // ── mock model + judges for ladder unit coverage ──

    struct MockModel {
        /// Rung label -> canned reply.
        replies: BTreeMap<String, String>,
        /// Rung label at which to force degradation.
        degrade_at: Option<String>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl MockModel {
        fn new(replies: BTreeMap<String, String>, degrade_at: Option<&str>) -> Self {
            MockModel {
                replies,
                degrade_at: degrade_at.map(|s| s.to_string()),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// Which rung this call corresponds to, inferred from call order
        /// (the ladder always probes in [`DepthRung::ORDER`] order).
        fn rung_for_call(n: usize) -> &'static str {
            DepthRung::ORDER[n].label()
        }
    }

    #[async_trait::async_trait]
    impl ConversationModel for MockModel {
        async fn respond(
            &self,
            _history: &[(String, String)],
            _user_text: &str,
        ) -> super::super::dim1_conversation::TurnReply {
            use super::super::dim1_conversation::TurnReply;
            let mut calls = self.calls.lock().unwrap();
            let idx = calls.len();
            let rung = Self::rung_for_call(idx).to_string();
            calls.push(rung.clone());
            drop(calls);

            if self.degrade_at.as_deref() == Some(rung.as_str()) {
                return TurnReply {
                    text: String::new(),
                    degraded: true,
                    degrade_reason: Some("forced timeout at depth".into()),
                };
            }
            let text = self
                .replies
                .get(&rung)
                .cloned()
                .unwrap_or_else(|| "I don't recall.".to_string());
            TurnReply {
                text,
                degraded: false,
                degrade_reason: None,
            }
        }
    }

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

    fn strong_panel() -> Vec<Box<dyn Judge>> {
        vec![
            Box::new(FixedJudge { id: "claude".into(), score: 5 }),
            Box::new(FixedJudge { id: "gemini".into(), score: 4 }),
        ]
    }

    fn weak_panel() -> Vec<Box<dyn Judge>> {
        vec![
            Box::new(FixedJudge { id: "claude".into(), score: 1 }),
            Box::new(FixedJudge { id: "gemini".into(), score: 1 }),
        ]
    }

    #[tokio::test]
    async fn full_ladder_clears_all_rungs_when_recall_and_coherence_hold() {
        let corpus = load_corpus().unwrap();
        let mut replies = BTreeMap::new();
        for rung in DepthRung::ORDER {
            replies.insert(
                rung.label().to_string(),
                "The waypoint codeword was Halcyon-Reef-42.".to_string(),
            );
        }
        let model = MockModel::new(replies, None);
        let panel = strong_panel();

        let outcome = run_yarn_depth(&model, &panel, &corpus, 8_192, 100_000).await;
        assert_eq!(outcome.points.len(), 4);
        assert!(!outcome.stopped_early);
        assert_eq!(outcome.usable_ceiling_tokens, 100_000);
        assert!(outcome.points.iter().all(|p| p.recalled && !p.collapsed));
    }

    #[tokio::test]
    async fn ladder_stops_at_recall_miss_and_records_ceiling_below_it() {
        let corpus = load_corpus().unwrap();
        let mut replies = BTreeMap::new();
        replies.insert("native".to_string(), "The waypoint codeword was Halcyon-Reef-42.".to_string());
        replies.insert("pct30".to_string(), "The waypoint codeword was Halcyon-Reef-42.".to_string());
        replies.insert("pct60".to_string(), "I'm not sure what codeword you mean.".to_string()); // miss
        replies.insert("pct100".to_string(), "The waypoint codeword was Halcyon-Reef-42.".to_string());
        let model = MockModel::new(replies, None);
        let panel = strong_panel();

        let outcome = run_yarn_depth(&model, &panel, &corpus, 8_192, 100_000).await;
        // Stops at pct60 (the miss); pct100 never probed.
        assert_eq!(outcome.points.len(), 3);
        assert!(outcome.stopped_early);
        assert_eq!(outcome.points.last().unwrap().rung, DepthRung::Pct60);
        assert!(outcome.points.last().unwrap().collapsed);
        // Ceiling is the last rung BEFORE the collapse (pct30's target).
        let pct30_target = DepthRung::Pct30.token_target(8_192, 100_000);
        assert_eq!(outcome.usable_ceiling_tokens, pct30_target);
    }

    #[tokio::test]
    async fn ladder_stops_on_inference_degradation_never_panics() {
        let corpus = load_corpus().unwrap();
        let mut replies = BTreeMap::new();
        replies.insert("native".to_string(), "The waypoint codeword was Halcyon-Reef-42.".to_string());
        let model = MockModel::new(replies, Some("pct30"));
        let panel = strong_panel();

        let outcome = run_yarn_depth(&model, &panel, &corpus, 8_192, 100_000).await;
        assert_eq!(outcome.points.len(), 2);
        assert!(outcome.stopped_early);
        let last = outcome.points.last().unwrap();
        assert!(last.degraded);
        assert!(last.collapsed);
        assert_eq!(last.degrade_reason.as_deref(), Some("forced timeout at depth"));
    }

    #[tokio::test]
    async fn weak_coherence_collapses_even_when_recall_hits() {
        let corpus = load_corpus().unwrap();
        let mut replies = BTreeMap::new();
        for rung in DepthRung::ORDER {
            // Recall always hits, but the panel below scores coherence weak.
            replies.insert(
                rung.label().to_string(),
                "Halcyon-Reef-42 uh yes maybe wait what were we talking about".to_string(),
            );
        }
        let model = MockModel::new(replies, None);
        let panel = weak_panel(); // mean 1.0 < WEAK_COHERENCE_THRESHOLD

        let outcome = run_yarn_depth(&model, &panel, &corpus, 8_192, 100_000).await;
        assert_eq!(outcome.points.len(), 1, "should collapse at the first rung (native)");
        assert!(outcome.stopped_early);
        assert_eq!(outcome.usable_ceiling_tokens, 0);
        assert!(outcome.points[0].recalled);
        assert!(outcome.points[0].collapsed);
        assert!(outcome.points[0].coherence.unwrap() < WEAK_COHERENCE_THRESHOLD);
    }

    #[tokio::test]
    async fn no_judges_configured_skips_coherence_collapse_check() {
        let corpus = load_corpus().unwrap();
        let mut replies = BTreeMap::new();
        for rung in DepthRung::ORDER {
            replies.insert(rung.label().to_string(), "Halcyon-Reef-42, incoherent babble".to_string());
        }
        let model = MockModel::new(replies, None);

        let outcome = run_yarn_depth(&model, &[], &corpus, 8_192, 100_000).await;
        assert_eq!(outcome.points.len(), 4);
        assert!(!outcome.stopped_early);
        assert!(outcome.points.iter().all(|p| p.coherence.is_none()));
    }

    #[tokio::test]
    async fn into_dimension_scores_emits_ceiling_recall_and_coherence_rows() {
        let corpus = load_corpus().unwrap();
        let panel = strong_panel();

        // Stop after pct30 by making pct60 miss.
        let mut replies = BTreeMap::new();
        replies.insert("native".to_string(), "The waypoint codeword was Halcyon-Reef-42.".to_string());
        replies.insert("pct30".to_string(), "The waypoint codeword was Halcyon-Reef-42.".to_string());
        replies.insert("pct60".to_string(), "no idea".to_string());
        let model = MockModel::new(replies, None);

        let outcome = run_yarn_depth(&model, &panel, &corpus, 8_192, 100_000).await;
        let rows = outcome.into_dimension_scores(&ModelId::from("smollm3:3b"), BackendTag::Gpu, 8_192, 100_000);

        assert!(rows
            .iter()
            .any(|r| r.metric == METRIC_USABLE_CEILING && r.dimension == DIMENSION && r.judge == "deterministic"));
        assert!(rows.iter().any(|r| r.metric == "recall_native"));
        assert!(rows.iter().any(|r| r.metric == "recall_pct30"));
        assert!(rows.iter().any(|r| r.metric == "recall_pct60"));
        assert!(rows.iter().any(|r| r.metric == "coherence_native"));
        assert!(!rows.iter().any(|r| r.metric == "recall_pct100"), "pct100 never probed");
        assert!(rows.iter().all(|r| r.backend_tag == BackendTag::Gpu));
    }

    // ── run_yarn_depth_and_write: proves the ScoreSink/mem_config path ──

    struct MemSink {
        rows: std::sync::Mutex<Vec<DimensionScore>>,
    }

    #[async_trait::async_trait]
    impl ScoreSink for MemSink {
        async fn write(&self, rows: &[DimensionScore]) -> Result<(), String> {
            self.rows.lock().unwrap().extend_from_slice(rows);
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_yarn_depth_and_write_persists_through_the_shared_sink() {
        let corpus = load_corpus().unwrap();
        let mut replies = BTreeMap::new();
        for rung in DepthRung::ORDER {
            replies.insert(
                rung.label().to_string(),
                "The waypoint codeword was Halcyon-Reef-42.".to_string(),
            );
        }
        let model = MockModel::new(replies, None);
        let panel = strong_panel();
        let sink = MemSink { rows: std::sync::Mutex::new(Vec::new()) };

        let model_id = ModelId::from("smollm3:3b");
        let outcome = run_yarn_depth_and_write(
            &model,
            &panel,
            &corpus,
            65_536,
            131_072,
            &model_id,
            BackendTag::Gpu,
            &sink,
        )
        .await
        .expect("write succeeds");

        assert_eq!(outcome.usable_ceiling_tokens, 131_072);
        let rows = sink.rows.lock().unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.model_id == model_id));
        assert!(rows.iter().any(|r| r.metric == METRIC_USABLE_CEILING));
    }
}
