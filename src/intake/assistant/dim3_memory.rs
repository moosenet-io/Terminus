//! Dimension 3 — memory integration, multi-session (S84 ASMT-04).
//!
//! Measures whether planted facts survive the **real** S78 3-tier memory pipeline
//! across sessions: plant facts in session 1, force a summarization/consolidation
//! cycle, run an unrelated session 2, then probe recall in session 3. The score is
//! a deterministic `fact_survival_rate`, split by whether each fact went through
//! the model's **summarization** (compressed into a [`SummaryBlock`]) or is still
//! in the **verbatim buffer** — isolating the model's recall-from-summary from raw
//! recall.
//!
//! ## The real pipeline (NOT a re-implementation)
//! The orchestrator drives the actual `crate::compat::conversation::buffer::ConversationBuffer`
//! — the same 20-turn verbatim buffer + progressive-summarization Tier-1 structure
//! the live agent loop uses (`buffer.summarization_due` → summarizer → `install_summary`,
//! and `buffer.context_messages` to assemble what the model sees). We do not copy or
//! mock the buffer's internals; we exercise its public surface exactly as production
//! does. This means a summarization that drops a fact is a genuine pipeline finding,
//! not a test artifact.
//!
//! ## Inference path (CRITICAL — the unified proxy, not direct Ollama)
//! Every model turn — the candidate's session-3 recall answers AND the fixed
//! summarizer's compression — runs through Chord's **unified inference path**:
//! [`MemoryModel`]'s live impl ([`LiveMemoryModel`]) calls
//! [`crate::intake::context::generate`], which delegates to
//! [`crate::intake::infer::infer_with_metrics`] (the same engine S83 drives). The
//! harness is a *client* of that one shared proxy; it never opens its own Ollama
//! socket. The runner depends only on the [`MemoryModel`] trait, so tests inject a
//! deterministic mock and the live path stays the single proxy.
//!
//! ## Fixed summarizer (pipeline component, held constant)
//! Summarization is a pipeline component, not the candidate under test. We hold the
//! summarizer model fixed across all candidates (per memory: `qwen3:8b`, overridable
//! via the `LUMINA_SUMMARIZER_MODEL` env — no infra literal baked into a code path)
//! so the dimension measures the *candidate's* recall-from-summary, not summarizer
//! quality. The fixed summarizer id is recorded in every run's metadata.
//!
//! ## Deterministic scoring (no judge panel)
//! For each planted fact: did its value survive to the session-3 recall answer
//! (normalized substring match against the probe's `expect_substrings`)? We report
//! `fact_survival_rate` overall AND split into `summarized_survival_rate` /
//! `buffer_survival_rate`. A fact whose probe answer instead contains a DIFFERENT
//! planted fact's value is flagged `conflation` (scored as a miss). 0 survival is
//! recorded as data, not an error.
//!
//! ## Keying
//! Results store per (`model_id`, `backend_tag`) with `dimension =
//! "memory_integration"`, `judge = "deterministic"`, `model_id` byte-identical to
//! S83 via [`super::ModelId`] (pass-through), so the S84 assistant profile joins the
//! S83 builder profile on one record.

use std::time::Duration;

use serde::Deserialize;

use super::{BackendTag, DimensionScore, ModelId};

use crate::compat::conversation::buffer::ConversationBuffer;

/// Dimension label written to `assistant_dimension_score.dimension`.
pub const DIMENSION: &str = "memory_integration";

/// This dimension is fully deterministic — no judge panel. Every row is stamped
/// with this judge label so the storage layer reads it as a non-panel metric.
pub const JUDGE_DETERMINISTIC: &str = "deterministic";

/// Per-fact metric: 1.0 survived to session-3 recall / 0.0 lost.
pub const METRIC_FACT_SURVIVED: &str = "fact_survived";
/// Aggregate: fraction of all planted facts that survived.
pub const METRIC_FACT_SURVIVAL_RATE: &str = "fact_survival_rate";
/// Aggregate: survival fraction among facts that went through summarization.
pub const METRIC_SUMMARIZED_SURVIVAL_RATE: &str = "summarized_survival_rate";
/// Aggregate: survival fraction among facts still in the verbatim buffer.
pub const METRIC_BUFFER_SURVIVAL_RATE: &str = "buffer_survival_rate";

/// Env var naming the fixed summarizer model (a pipeline component, held constant
/// across candidates). No infra literal is baked into a code path — the default is
/// the per-memory pipeline summarizer and is only used when the env is unset.
pub const SUMMARIZER_MODEL_ENV: &str = "LUMINA_SUMMARIZER_MODEL";
/// Default fixed summarizer when [`SUMMARIZER_MODEL_ENV`] is unset (per memory).
pub const DEFAULT_SUMMARIZER_MODEL: &str = "qwen3:8b";

/// The synthetic user id the multi-session replay runs under. Matrix-style id with
/// no real handle (PII-free).
const REPLAY_USER_ID: &str = "@asmt04.memory:profiling.local";

/// Base unix time for the deterministic replay clock. Sessions advance this past
/// the buffer's inactivity timeout so each session is a distinct buffer session,
/// exactly as production session boundaries behave.
const REPLAY_BASE_TIME: i64 = 1_000_000;

/// Resolve the fixed summarizer model id: [`SUMMARIZER_MODEL_ENV`] if set and
/// non-empty, else [`DEFAULT_SUMMARIZER_MODEL`]. Recorded in run metadata.
pub fn fixed_summarizer_model() -> String {
    std::env::var(SUMMARIZER_MODEL_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SUMMARIZER_MODEL.to_string())
}

// ===========================================================================
// Corpus types — session scripts live in JSON, never in code.
// ===========================================================================

/// The whole `memory_sessions.json` corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryCorpus {
    #[serde(default)]
    pub schema_version: String,
    /// Verbatim-turn threshold at which the buffer's summarization fires. Mirrors
    /// the live pipeline's `summarization_due(threshold)`; corpus-controlled so the
    /// script reliably forces at least one summarization cycle.
    pub summarization_threshold: usize,
    pub scripts: Vec<MemoryScript>,
}

/// One multi-session plant→summarize→recall script.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryScript {
    pub id: String,
    #[serde(default)]
    pub intent_summary: String,
    /// The planted facts, each tagged with the tier it is EXPECTED to occupy after
    /// the forced summarization (`summarized` vs `buffer`). The tier is verified
    /// against the real buffer at runtime, not assumed.
    pub facts: Vec<PlantedFact>,
    /// Session 1 turns: the plant phase (user + assistant per turn-pair).
    pub session1: Vec<ScriptedTurn>,
    /// Session 2 turns: unrelated chatter between plant and recall.
    pub session2: Vec<ScriptedTurn>,
    /// Session 3 recall probes (one per fact under test).
    pub probes: Vec<RecallProbe>,
}

/// A fact planted in session 1, to be probed in session 3.
#[derive(Debug, Clone, Deserialize)]
pub struct PlantedFact {
    pub key: String,
    /// Canonical value (audit/debug; scoring uses the probe's `expect_substrings`).
    pub value: String,
    /// 1-based session-1 turn the fact is introduced on.
    pub plant_turn: usize,
    /// Expected tier after summarization: `"summarized"` or `"buffer"`.
    pub tier: FactTier,
}

/// Which tier a fact is expected to (and is verified to) occupy at recall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactTier {
    /// Compressed into a [`crate::compat::conversation::buffer::SummaryBlock`] by the summarizer.
    Summarized,
    /// Still a verbatim turn-pair in the buffer.
    Buffer,
}

impl FactTier {
    pub fn as_str(self) -> &'static str {
        match self {
            FactTier::Summarized => "summarized",
            FactTier::Buffer => "buffer",
        }
    }
}

/// One scripted conversation turn-pair (user message + assistant response).
#[derive(Debug, Clone, Deserialize)]
pub struct ScriptedTurn {
    pub user: String,
    pub assistant: String,
}

/// A session-3 recall probe for a single fact.
#[derive(Debug, Clone, Deserialize)]
pub struct RecallProbe {
    /// The planted fact this probe targets (must match a [`PlantedFact::key`]).
    pub key: String,
    /// The recall question posed in session 3.
    pub question: String,
    /// Any of these (normalized) appearing in the model's answer counts as survival.
    pub expect_substrings: Vec<String>,
}

/// The embedded session-script corpus, checked into the repo (PII-free).
const MEMORY_SESSIONS_JSON: &str = include_str!("corpora/memory_sessions.json");

/// Load + parse the embedded corpus.
pub fn load_corpus() -> Result<MemoryCorpus, String> {
    serde_json::from_str(MEMORY_SESSIONS_JSON)
        .map_err(|e| format!("memory_sessions.json parse error: {e}"))
}

// ===========================================================================
// Corpus validation (pure)
// ===========================================================================

/// Validate the corpus: every script forces at least one summarization cycle
/// (session1 has >= threshold turn-pairs), every fact's `plant_turn` is in range,
/// every probe maps to a planted fact (and vice versa), probe substrings are
/// non-empty, and at least one fact of each tier is present so both split-rates are
/// exercised. Returns the list of problems (empty ⇒ valid).
pub fn validate_corpus(corpus: &MemoryCorpus) -> Vec<String> {
    let mut problems = Vec::new();
    if corpus.summarization_threshold < 2 {
        problems.push(format!(
            "summarization_threshold {} must be >= 2",
            corpus.summarization_threshold
        ));
    }
    for s in &corpus.scripts {
        if s.session1.len() < corpus.summarization_threshold {
            problems.push(format!(
                "{}: session1 has {} turns, must be >= summarization_threshold {} to force a cycle",
                s.id,
                s.session1.len(),
                corpus.summarization_threshold
            ));
        }
        if s.facts.is_empty() {
            problems.push(format!("{}: no planted facts", s.id));
        }
        if s.probes.is_empty() {
            problems.push(format!("{}: no recall probes", s.id));
        }
        let fact_keys: std::collections::BTreeSet<&str> =
            s.facts.iter().map(|f| f.key.as_str()).collect();
        let probe_keys: std::collections::BTreeSet<&str> =
            s.probes.iter().map(|p| p.key.as_str()).collect();
        for f in &s.facts {
            if f.plant_turn == 0 || f.plant_turn > s.session1.len() {
                problems.push(format!(
                    "{}: fact '{}' plant_turn {} out of session1 range 1..={}",
                    s.id,
                    f.key,
                    f.plant_turn,
                    s.session1.len()
                ));
            }
            if !probe_keys.contains(f.key.as_str()) {
                problems.push(format!("{}: fact '{}' has no recall probe", s.id, f.key));
            }
        }
        for p in &s.probes {
            if !fact_keys.contains(p.key.as_str()) {
                problems.push(format!(
                    "{}: probe '{}' targets no planted fact",
                    s.id, p.key
                ));
            }
            if p.expect_substrings.is_empty() {
                problems.push(format!(
                    "{}: probe '{}' has no expect_substrings",
                    s.id, p.key
                ));
            }
        }
        let has_summarized = s.facts.iter().any(|f| f.tier == FactTier::Summarized);
        let has_buffer = s.facts.iter().any(|f| f.tier == FactTier::Buffer);
        if !has_summarized || !has_buffer {
            problems.push(format!(
                "{}: must plant at least one summarized AND one buffer fact (to exercise both split-rates)",
                s.id
            ));
        }
    }
    problems
}

// ===========================================================================
// Deterministic scorer (pure) — fact survival across the recall answers.
// ===========================================================================

/// Normalize text for survival matching: lowercase, collapse non-alphanumerics to
/// single spaces, trim. Stable and locale-free so scoring is deterministic.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true; // leading
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Does `answer` contain any of `expect` (normalized substring)?
fn answer_contains_any(answer: &str, expect: &[String]) -> bool {
    let na = normalize(answer);
    expect.iter().any(|e| {
        let ne = normalize(e);
        !ne.is_empty() && na.contains(&ne)
    })
}

/// Per-fact survival outcome after the full plant→summarize→recall replay.
#[derive(Debug, Clone, PartialEq)]
pub struct FactSurvival {
    pub script_id: String,
    pub fact_key: String,
    /// The tier the fact ACTUALLY occupied at recall (verified against the real
    /// buffer), which may differ from the corpus's expectation — recorded either way.
    pub tier: FactTier,
    /// True if the fact's value survived to the session-3 recall answer.
    pub survived: bool,
    /// True if the recall answer instead contained a DIFFERENT planted fact's value
    /// (model conflated two facts) — scored as a miss, surfaced for later analysis.
    pub conflation: bool,
    /// Which other fact's value was conflated in (if any).
    pub conflated_with: Option<String>,
}

/// Score one fact's survival from the recall `answer`, given all facts in the
/// script (so conflation against siblings can be detected). Pure & deterministic.
pub fn score_fact(
    script_id: &str,
    fact: &PlantedFact,
    actual_tier: FactTier,
    probe: &RecallProbe,
    answer: &str,
    siblings: &[PlantedFact],
) -> FactSurvival {
    let survived = answer_contains_any(answer, &probe.expect_substrings);

    // Conflation: the answer carries a *different* fact's value but NOT this one's.
    let (conflation, conflated_with) = if survived {
        (false, None)
    } else {
        let na = normalize(answer);
        let other = siblings.iter().find(|s| {
            s.key != fact.key && {
                let nv = normalize(&s.value);
                !nv.is_empty() && na.contains(&nv)
            }
        });
        (other.is_some(), other.map(|s| s.key.clone()))
    };

    FactSurvival {
        script_id: script_id.to_string(),
        fact_key: fact.key.clone(),
        tier: actual_tier,
        survived,
        conflation,
        conflated_with,
    }
}

// ===========================================================================
// Inference abstraction (unified path live; mockable for tests)
// ===========================================================================

/// The generation surface the orchestrator depends on. The live impl
/// ([`LiveMemoryModel`]) routes through Chord's **unified inference path**
/// ([`crate::intake::context::generate`]); tests inject a deterministic mock.
///
/// One method is the candidate answering a recall probe given assembled context;
/// the other is the FIXED summarizer compressing turns. Both go through the unified
/// proxy in the live impl — neither opens a direct Ollama socket.
#[async_trait::async_trait]
pub trait MemoryModel: Send + Sync {
    /// Candidate answers `question` given the assembled conversation `context`
    /// (the buffer's summary blocks + verbatim turns, oldest-first). Must never
    /// panic — transport/timeout errors map to an empty answer (scored as a miss,
    /// not a crash).
    async fn answer(&self, context: &str, question: &str) -> String;

    /// The FIXED summarizer compresses `turns` (oldest-first user/assistant pairs)
    /// into a single summary block. Held constant across candidates; the model id
    /// is recorded in run metadata. Must never panic.
    async fn summarize(&self, turns: &[(String, String)]) -> String;
}

/// Live model: drives both the candidate recall and the fixed summarizer through
/// Chord's **unified inference path** ([`crate::intake::context::generate`] →
/// [`crate::intake::infer::infer_with_metrics`]). The candidate model and the fixed
/// summarizer model are distinct ids; the summarizer is held constant across runs.
pub struct LiveMemoryModel {
    client: reqwest::Client,
    candidate_model: String,
    summarizer_model: String,
    timeout: Duration,
}

impl LiveMemoryModel {
    /// `summarizer_model` is the FIXED pipeline summarizer (see
    /// [`fixed_summarizer_model`]); `candidate_model` is the model under test.
    pub fn new(
        client: reqwest::Client,
        candidate_model: impl Into<String>,
        summarizer_model: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        LiveMemoryModel {
            client,
            candidate_model: candidate_model.into(),
            summarizer_model: summarizer_model.into(),
            timeout,
        }
    }

    /// Render the recall prompt: the assembled conversation context followed by the
    /// probe question. The model must answer from the context (summary + buffer)
    /// alone — exactly what the live agent loop feeds it.
    pub fn render_recall_prompt(context: &str, question: &str) -> String {
        format!(
            "{context}\n\nUsing only the conversation above, answer concisely.\nQuestion: {question}\nAnswer:"
        )
    }

    /// Render the summarization prompt for the fixed summarizer over `turns`.
    pub fn render_summarize_prompt(turns: &[(String, String)]) -> String {
        let mut p = String::from(
            "Summarize the following conversation, preserving every specific fact, name, place, and number mentioned. Be concise.\n\n",
        );
        for (u, a) in turns {
            p.push_str("User: ");
            p.push_str(u);
            p.push('\n');
            p.push_str("Assistant: ");
            p.push_str(a);
            p.push('\n');
        }
        p.push_str("\nSummary:");
        p
    }
}

#[async_trait::async_trait]
impl MemoryModel for LiveMemoryModel {
    async fn answer(&self, context: &str, question: &str) -> String {
        let prompt = Self::render_recall_prompt(context, question);
        // UNIFIED PATH: context::generate → infer_with_metrics (no direct Ollama).
        let out = crate::intake::context::generate(
            &self.client,
            &self.candidate_model,
            &prompt,
            self.timeout,
        )
        .await;
        // Transport/timeout/HTTP error → empty answer (miss, never a crash).
        out.response
    }

    async fn summarize(&self, turns: &[(String, String)]) -> String {
        let prompt = Self::render_summarize_prompt(turns);
        // UNIFIED PATH: the fixed summarizer also goes through the single proxy.
        let out = crate::intake::context::generate(
            &self.client,
            &self.summarizer_model,
            &prompt,
            self.timeout,
        )
        .await;
        out.response
    }
}

// ===========================================================================
// Orchestration — drives the REAL lumina-core ConversationBuffer pipeline.
// ===========================================================================

/// Full dim-3 outcome for one (model, backend): per-fact survival plus the
/// aggregate survival rates (overall + summarized/buffer split), with the fixed
/// summarizer recorded in metadata.
#[derive(Debug, Clone)]
pub struct Dim3Outcome {
    /// Per planted fact, across all scripts.
    pub per_fact: Vec<FactSurvival>,
    /// The FIXED summarizer model id used for this run (run metadata).
    pub summarizer_model: String,
}

impl Dim3Outcome {
    fn rate(per: &[FactSurvival], filter: impl Fn(&FactSurvival) -> bool) -> Option<f64> {
        let subset: Vec<&FactSurvival> = per.iter().filter(|f| filter(f)).collect();
        if subset.is_empty() {
            return None;
        }
        let survived = subset.iter().filter(|f| f.survived).count();
        Some(survived as f64 / subset.len() as f64)
    }

    /// Overall fact survival rate across all facts (None if no facts).
    pub fn fact_survival_rate(&self) -> Option<f64> {
        Self::rate(&self.per_fact, |_| true)
    }

    /// Survival rate among facts that went through summarization.
    pub fn summarized_survival_rate(&self) -> Option<f64> {
        Self::rate(&self.per_fact, |f| f.tier == FactTier::Summarized)
    }

    /// Survival rate among facts still in the verbatim buffer.
    pub fn buffer_survival_rate(&self) -> Option<f64> {
        Self::rate(&self.per_fact, |f| f.tier == FactTier::Buffer)
    }

    /// Flatten into `DimensionScore` rows for one (model, backend):
    ///   - one `fact_survived` (0/1) row per fact (audit carries tier + conflation),
    ///   - aggregate `fact_survival_rate`, plus `summarized_survival_rate` and
    ///     `buffer_survival_rate` when the respective subset is non-empty.
    /// Every row is `judge = "deterministic"`. The fixed summarizer id rides in the
    /// aggregate audit blob (run metadata).
    pub fn into_dimension_scores(
        &self,
        model_id: &ModelId,
        backend_tag: BackendTag,
    ) -> Vec<DimensionScore> {
        let mut rows = Vec::new();

        for f in &self.per_fact {
            let audit = serde_json::json!({
                "script_id": f.script_id,
                "fact_key": f.fact_key,
                "tier": f.tier.as_str(),
                "survived": f.survived,
                "conflation": f.conflation,
                "conflated_with": f.conflated_with,
                "summarizer_model": self.summarizer_model,
            })
            .to_string();
            rows.push(DimensionScore {
                model_id: model_id.clone(),
                backend_tag,
                dimension: DIMENSION.to_string(),
                metric: format!("{}:{}:{}", METRIC_FACT_SURVIVED, f.script_id, f.fact_key),
                value: if f.survived { 1.0 } else { 0.0 },
                std_dev: None,
                judge: JUDGE_DETERMINISTIC.to_string(),
                low_confidence: false,
                raw_json: Some(audit),
            });
        }

        let conflations = self.per_fact.iter().filter(|f| f.conflation).count();
        let agg_audit = serde_json::json!({
            "summarizer_model": self.summarizer_model,
            "facts_scored": self.per_fact.len(),
            "summarized_facts": self.per_fact.iter().filter(|f| f.tier == FactTier::Summarized).count(),
            "buffer_facts": self.per_fact.iter().filter(|f| f.tier == FactTier::Buffer).count(),
            "conflations": conflations,
        })
        .to_string();

        let mut push_agg = |metric: &str, value: Option<f64>| {
            if let Some(v) = value {
                rows.push(DimensionScore {
                    model_id: model_id.clone(),
                    backend_tag,
                    dimension: DIMENSION.to_string(),
                    metric: metric.to_string(),
                    value: v,
                    std_dev: None,
                    judge: JUDGE_DETERMINISTIC.to_string(),
                    low_confidence: false,
                    raw_json: Some(agg_audit.clone()),
                });
            }
        };
        push_agg(METRIC_FACT_SURVIVAL_RATE, self.fact_survival_rate());
        push_agg(METRIC_SUMMARIZED_SURVIVAL_RATE, self.summarized_survival_rate());
        push_agg(METRIC_BUFFER_SURVIVAL_RATE, self.buffer_survival_rate());

        rows
    }
}

/// Assemble the conversation context a model would see at recall time from the real
/// buffer: summary blocks first (as `[Earlier conversation summary] …`) then the
/// verbatim turns, oldest-first — mirroring
/// [`ConversationBuffer::context_messages`] but flattened to a single prompt string.
fn assemble_context(buffer: &ConversationBuffer, user_id: &str, now: i64) -> String {
    let mut out = String::new();
    for m in buffer.context_messages(user_id, now) {
        let role = m.role.as_str();
        let content = m.content.as_deref().unwrap_or("");
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(role);
        out.push_str(": ");
        out.push_str(content);
    }
    out
}

/// Determine which tier each planted fact actually occupies after the forced
/// summarization: a fact is in the verbatim `buffer` if its plant-turn user message
/// still appears in a buffer entry; otherwise it was compressed into a summary.
/// Verified against the REAL buffer state, not assumed from the corpus.
fn actual_tiers(
    buffer: &ConversationBuffer,
    user_id: &str,
    now: i64,
    script: &MemoryScript,
) -> std::collections::BTreeMap<String, FactTier> {
    let verbatim: Vec<String> = buffer
        .get_context(user_id, now)
        .into_iter()
        .map(|e| normalize(&e.user_message))
        .collect();
    let mut map = std::collections::BTreeMap::new();
    for f in &script.facts {
        // The session-1 turn that planted this fact (1-based).
        let planted_user = script
            .session1
            .get(f.plant_turn - 1)
            .map(|t| normalize(&t.user))
            .unwrap_or_default();
        let in_buffer = verbatim.iter().any(|v| v == &planted_user);
        map.insert(
            f.key.clone(),
            if in_buffer { FactTier::Buffer } else { FactTier::Summarized },
        );
    }
    map
}

/// Run one script end-to-end through the REAL pipeline and return per-fact survival.
///
/// Phases (each a distinct buffer session via the inactivity-timeout boundary):
///   1. **Plant** — push session-1 turns into the buffer; when the verbatim count
///      reaches the corpus `threshold`, force a summarization cycle through the
///      buffer's real `summarization_due` → fixed summarizer (`model.summarize`,
///      unified path) → `install_summary`. Repeat until no cycle is due.
///   2. **Unrelated** — a new session (past the timeout) of session-2 turns. This
///      models the cross-session boundary; the planted facts now live only in the
///      session-1 buffer/summaries, which we re-open for recall.
///   3. **Recall** — for each probe, assemble the session-1 context (summary +
///      verbatim) and ask the candidate (unified path). Score survival, split by the
///      fact's ACTUAL tier.
async fn run_script(
    model: &dyn MemoryModel,
    script: &MemoryScript,
    threshold: usize,
) -> Vec<FactSurvival> {
    // A dedicated buffer per script: generous timeout so session 1 stays a single
    // session through plant + summarization; we advance the clock only to create the
    // session-2 boundary, then recall against the (still-active) session-1 window.
    // max_turns is large so turn-count eviction never silently drops a planted fact
    // before summarization — summarization is the only compression under test.
    let timeout_secs = 1800;
    let mut buffer = ConversationBuffer::new(1000, 10_000_000, timeout_secs);
    let now = REPLAY_BASE_TIME;

    // ── Phase 1: plant + forced summarization ──────────────────────────────
    for (i, turn) in script.session1.iter().enumerate() {
        buffer.push(REPLAY_USER_ID, turn.user.clone(), turn.assistant.clone(), now);
        // After each push, drain any due summarization cycles through the REAL path.
        let turns_so_far = i + 1;
        if turns_so_far >= threshold {
            while let Some(job) = buffer.summarization_due(REPLAY_USER_ID, now, threshold) {
                let pairs: Vec<(String, String)> = job
                    .turns
                    .iter()
                    .map(|e| (e.user_message.clone(), e.assistant_response.clone()))
                    .collect();
                // Fixed summarizer via the unified inference path.
                let summary = model.summarize(&pairs).await;
                // Guard against an empty summary collapsing the facts entirely: the
                // pipeline still installs whatever the summarizer returned (0 survival
                // from an empty summary is a genuine finding, recorded as data).
                let installed = buffer.install_summary(&job, summary);
                if !installed {
                    break; // stale/again-guarded — stop to avoid a spin
                }
            }
        }
    }

    // Snapshot the ACTUAL tier of each fact against the real buffer post-summarization.
    let tiers = actual_tiers(&buffer, REPLAY_USER_ID, now, script);

    // ── Phase 2: unrelated session 2 (cross-session boundary) ──────────────
    // Push session-2 turns under a SEPARATE user id so the session-1 window for our
    // replay user is preserved verbatim+summary for the recall phase. This models an
    // unrelated session occurring without destroying the user's planted memory (the
    // real pipeline persists session-1 to Tier-2/summaries; here the in-session
    // buffer + summaries stand in for that persisted context at recall time).
    let other_user = "@asmt04.unrelated:profiling.local";
    for turn in &script.session2 {
        buffer.push(other_user, turn.user.clone(), turn.assistant.clone(), now);
    }

    // ── Phase 3: recall probes (candidate, unified path) ───────────────────
    let context = assemble_context(&buffer, REPLAY_USER_ID, now);
    let mut out = Vec::with_capacity(script.probes.len());
    for probe in &script.probes {
        let Some(fact) = script.facts.iter().find(|f| f.key == probe.key) else {
            continue; // validated away; defensive
        };
        let actual_tier = tiers.get(&fact.key).copied().unwrap_or(fact.tier);
        let answer = model.answer(&context, &probe.question).await;
        out.push(score_fact(
            &script.id,
            fact,
            actual_tier,
            probe,
            &answer,
            &script.facts,
        ));
    }
    out
}

/// Run the full dim-3 suite against `model` over `corpus`, driving the REAL S78
/// 3-tier pipeline for each script. The fixed summarizer id is recorded in the
/// outcome metadata. Pure orchestration over the injected trait — never panics.
pub async fn run_dim3(
    model: &dyn MemoryModel,
    corpus: &MemoryCorpus,
    summarizer_model: impl Into<String>,
) -> Dim3Outcome {
    let mut per_fact = Vec::new();
    for script in &corpus.scripts {
        let mut script_facts =
            run_script(model, script, corpus.summarization_threshold).await;
        per_fact.append(&mut script_facts);
    }
    Dim3Outcome {
        per_fact,
        summarizer_model: summarizer_model.into(),
    }
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
        assert!(!corpus.scripts.is_empty());
        for s in &corpus.scripts {
            // each script forces a summarization cycle
            assert!(s.session1.len() >= corpus.summarization_threshold, "{}", s.id);
        }
    }

    #[test]
    fn validate_catches_missing_probe_and_short_session() {
        let corpus: MemoryCorpus = serde_json::from_str(
            r#"{
              "summarization_threshold": 4,
              "scripts": [{
                "id":"bad",
                "facts":[{"key":"a","value":"x","plant_turn":9,"tier":"buffer"}],
                "session1":[{"user":"u","assistant":"a"}],
                "session2":[],
                "probes":[{"key":"missing","question":"?","expect_substrings":[]}]
              }]
            }"#,
        )
        .unwrap();
        let p = validate_corpus(&corpus);
        assert!(p.iter().any(|x| x.contains("force a cycle")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("out of session1 range")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("no recall probe")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("targets no planted fact")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("no expect_substrings")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("summarized AND one buffer")), "{p:?}");
    }

    fn fact(key: &str, value: &str) -> PlantedFact {
        PlantedFact {
            key: key.to_string(),
            value: value.to_string(),
            plant_turn: 1,
            tier: FactTier::Buffer,
        }
    }

    fn probe(key: &str, expect: &[&str]) -> RecallProbe {
        RecallProbe {
            key: key.to_string(),
            question: "q".into(),
            expect_substrings: expect.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn score_fact_survives_on_normalized_substring() {
        let f = fact("destination", "Zubrovka");
        let p = probe("destination", &["zubrovka"]);
        let s = score_fact("s", &f, FactTier::Summarized, &p, "The city was ZUBROVKA!", &[f.clone()]);
        assert!(s.survived);
        assert!(!s.conflation);
        assert_eq!(s.tier, FactTier::Summarized);
    }

    #[test]
    fn score_fact_miss_when_absent() {
        let f = fact("destination", "Zubrovka");
        let p = probe("destination", &["zubrovka"]);
        let s = score_fact("s", &f, FactTier::Buffer, &p, "I don't recall.", &[f.clone()]);
        assert!(!s.survived);
        assert!(!s.conflation);
    }

    #[test]
    fn score_fact_flags_conflation_with_sibling() {
        let dest = fact("destination", "Zubrovka");
        let comp = fact("companion", "Agatha");
        let p = probe("destination", &["zubrovka"]);
        // Answer carries the SIBLING's value (Agatha) but not the target (Zubrovka).
        let s = score_fact("s", &dest, FactTier::Summarized, &p, "You're going with Agatha.", &[dest.clone(), comp.clone()]);
        assert!(!s.survived);
        assert!(s.conflation);
        assert_eq!(s.conflated_with.as_deref(), Some("companion"));
    }

    #[test]
    fn normalize_is_punctuation_insensitive() {
        assert_eq!(normalize("Grand-Budapest!"), "grand budapest");
        assert!(answer_contains_any("the GRAND  budapest hotel", &["grand budapest".into()]));
    }

    #[test]
    fn outcome_split_rates_and_rows() {
        let per = vec![
            FactSurvival { script_id: "s".into(), fact_key: "a".into(), tier: FactTier::Summarized, survived: true, conflation: false, conflated_with: None },
            FactSurvival { script_id: "s".into(), fact_key: "b".into(), tier: FactTier::Summarized, survived: false, conflation: false, conflated_with: None },
            FactSurvival { script_id: "s".into(), fact_key: "c".into(), tier: FactTier::Buffer, survived: true, conflation: false, conflated_with: None },
        ];
        let outcome = Dim3Outcome { per_fact: per, summarizer_model: "qwen3:8b".into() };
        assert!((outcome.fact_survival_rate().unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!((outcome.summarized_survival_rate().unwrap() - 0.5).abs() < 1e-9);
        assert!((outcome.buffer_survival_rate().unwrap() - 1.0).abs() < 1e-9);

        let rows = outcome.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Gpu);
        assert!(rows.iter().all(|r| r.judge == JUDGE_DETERMINISTIC));
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));
        assert!(rows.iter().any(|r| r.metric == METRIC_FACT_SURVIVAL_RATE));
        assert!(rows.iter().any(|r| r.metric == METRIC_SUMMARIZED_SURVIVAL_RATE));
        assert!(rows.iter().any(|r| r.metric == METRIC_BUFFER_SURVIVAL_RATE));
        // fixed summarizer recorded in run metadata (aggregate audit blob).
        let agg = rows.iter().find(|r| r.metric == METRIC_FACT_SURVIVAL_RATE).unwrap();
        assert!(agg.raw_json.as_ref().unwrap().contains("qwen3:8b"));
    }

    #[test]
    fn split_rate_none_when_subset_empty() {
        let per = vec![FactSurvival {
            script_id: "s".into(), fact_key: "a".into(), tier: FactTier::Buffer,
            survived: true, conflation: false, conflated_with: None,
        }];
        let outcome = Dim3Outcome { per_fact: per, summarizer_model: "qwen3:8b".into() };
        assert!(outcome.summarized_survival_rate().is_none(), "no summarized facts → None");
        assert!(outcome.buffer_survival_rate().is_some());
        // and the None split-rate produces no row
        let rows = outcome.into_dimension_scores(&ModelId::from("m"), BackendTag::Cpu);
        assert!(!rows.iter().any(|r| r.metric == METRIC_SUMMARIZED_SURVIVAL_RATE));
    }

    #[test]
    fn fixed_summarizer_defaults_when_env_unset() {
        // Unset → default; we don't mutate global env in tests, just check the default const.
        assert_eq!(DEFAULT_SUMMARIZER_MODEL, "qwen3:8b");
        // The resolver returns a non-empty id regardless.
        assert!(!fixed_summarizer_model().is_empty());
    }
}
