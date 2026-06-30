//! Dimension 2 — tool chaining, conversational / implicit-intent (S84 ASMT-03).
//!
//! Measures multi-step tool chaining where the user's intent is **implicit across
//! turns** — the model must infer a 3–5 step chain (with data handoff between
//! steps) from a conversation that never names the tools to call. This is the new
//! axis. The **explicit-task** chains are NOT re-authored here: they are the S83
//! agent-scenario `multi_step` cases, **referenced by id** (see
//! [`s83_reference_ids`] / [`load_s83_referenced_explicit_scenarios`]) and loaded
//! from the S83 corpus at runtime. Only the conversational scenarios live in this
//! crate's [`corpora/toolchain_conversational.json`].
//!
//! ## Inference path (CRITICAL — the unified proxy, not direct Ollama)
//! Every model turn runs through Chord's **unified tool-dispatch path**:
//! [`ConversationToolModel::dispatch`]'s live impl ([`LiveToolModel`]) calls
//! [`crate::intake::context::chat_with_tools`], which posts to the normal
//! `/api/chat` tool-calling surface that [`crate::intake::infer`] /
//! `infer_with_metrics` front (the same engine S83's `agent` suite drives). The
//! harness is a *client* of that one shared proxy; it never opens its own Ollama
//! socket. The runner depends only on the [`ConversationToolModel`] trait, so
//! tests inject a deterministic mock and the live path stays the single proxy.
//!
//! ## Deterministic scoring (no judge panel)
//! Four per-scenario criteria, scored from the dispatched tool-call transcript:
//!   - **chain completed** — every expected tool was called at least once,
//!   - **correct tools** — only expected tools were called (no spurious/hallucinated),
//!   - **correct order** — expected tools appeared in the expected relative order,
//!   - **correct arg handoff** — the carried value from an earlier step shows up in
//!     the dependent step's arguments (case-insensitive substring),
//!   - **stopped appropriately** — no extra calls past the expected chain length.
//!
//! A scenario is a **clean pass** only when ALL of {completed, correct tools,
//! correct order, handoff, stopped} hold. **Wrong-order** (right tools, wrong
//! sequence) and **spurious-call** (an unexpected/hallucinated tool, or calls past
//! the stop point) are recorded as DISTINCT outcome flags so they never read as a
//! clean pass — partial credit via `chain_accuracy` (fraction of correct steps).
//!
//! ## Keying
//! Results store per (`model_id`, `backend_tag`) with
//! `dimension = "tool_chaining"`, `judge = "deterministic"`, `model_id`
//! byte-identical to S83 via [`super::ModelId`] (pass-through), so the S84
//! assistant profile joins the S83 builder profile on one record.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::{BackendTag, DimensionScore, ModelId};

// Reuse S83's tool-catalog construction so the live model sees the SAME tool
// specs the builder agent suite advertises (no divergent catalog).
use crate::intake::agent::{build_catalog, REAL_TOOLS};

/// Dimension label written to `assistant_dimension_score.dimension`.
pub const DIMENSION: &str = "tool_chaining";

/// This dimension is fully deterministic — no judge panel. Every row is stamped
/// with this judge label so the storage layer reads it as a non-panel metric.
pub const JUDGE_DETERMINISTIC: &str = "deterministic";

/// Per-scenario metric: 1.0 clean pass / 0.0 otherwise.
pub const METRIC_SCENARIO_PASS: &str = "scenario_pass";
/// Per-scenario metric: fraction of correct chain steps (partial credit).
pub const METRIC_CHAIN_ACCURACY: &str = "chain_accuracy";
/// Aggregate metric: clean-pass rate across all conversational scenarios.
pub const METRIC_CONVERSATIONAL_PASS_RATE: &str = "conversational_pass_rate";
/// Aggregate metric: mean chain-accuracy across all conversational scenarios.
pub const METRIC_MEAN_CHAIN_ACCURACY: &str = "mean_chain_accuracy";

/// Catalog size advertised to the model for a conversational chain. Large enough
/// to make tool selection non-trivial (matches the S83 multi_step band of 50).
const CONVERSATIONAL_CATALOG_SIZE: usize = 50;

// ===========================================================================
// Corpus types — conversational scenarios live in JSON, never in code.
// The S83 explicit scenarios are referenced by id only (no bodies here).
// ===========================================================================

/// The whole `toolchain_conversational.json` corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct ConversationalCorpus {
    #[serde(default)]
    pub schema_version: String,
    /// IDs of the S83 `multi_step` scenarios this dimension references for the
    /// explicit-task chains. These are loaded from the S83 corpus at runtime; the
    /// scenario BODIES are never duplicated into this file.
    #[serde(default)]
    pub s83_reference_ids: Vec<String>,
    /// The conversational / implicit-intent scenarios (the new axis).
    pub scenarios: Vec<ConversationalScenario>,
}

/// One conversational, implicit-intent chain scenario.
#[derive(Debug, Clone, Deserialize)]
pub struct ConversationalScenario {
    pub id: String,
    /// Human-readable description of the implied intent (audit only).
    #[serde(default)]
    pub intent_summary: String,
    /// The multi-turn user messages. Intent builds across these; no tool is named.
    pub turns: Vec<String>,
    /// The correct ordered tool chain (3–5 tools).
    pub expected_chain: Vec<String>,
    /// Data-handoff expectations between steps.
    #[serde(default)]
    pub handoffs: Vec<Handoff>,
    /// Expected number of tool calls; calls beyond this are "spurious / didn't stop".
    pub stop_after: usize,
    #[serde(default)]
    pub notes: String,
}

/// A data-handoff edge: a value produced/known at `from_step` must appear in the
/// arguments of `to_step`'s call.
#[derive(Debug, Clone, Deserialize)]
pub struct Handoff {
    pub from_step: usize,
    pub to_step: usize,
    #[serde(default)]
    pub from_tool: String,
    pub to_tool: String,
    /// Canonical carried value (audit/debug only; scoring uses `expect_arg_substrings`).
    #[serde(default)]
    pub carried_value: String,
    /// Any of these (lowercased) appearing in `to_tool`'s argument JSON counts as a
    /// correct handoff.
    pub expect_arg_substrings: Vec<String>,
}

/// The embedded conversational corpus, checked into the repo (PII-free).
const TOOLCHAIN_CONVERSATIONAL_JSON: &str = include_str!("corpora/toolchain_conversational.json");

/// Load + parse the embedded conversational corpus.
pub fn load_corpus() -> Result<ConversationalCorpus, String> {
    serde_json::from_str(TOOLCHAIN_CONVERSATIONAL_JSON)
        .map_err(|e| format!("toolchain_conversational.json parse error: {e}"))
}

/// The S83 scenario ids this dimension references for explicit chains. Convenience
/// accessor over the embedded corpus (returns `[]` if the corpus fails to parse —
/// callers that need parse errors should use [`load_corpus`]).
pub fn s83_reference_ids() -> Vec<String> {
    load_corpus().map(|c| c.s83_reference_ids).unwrap_or_default()
}

// ===========================================================================
// Corpus validation (pure)
// ===========================================================================

/// Validate the conversational corpus: every scenario has 3–5 expected tools, at
/// least one turn, a `stop_after` matching the chain length, handoff indices in
/// range with non-empty arg substrings, and every expected tool is a real catalog
/// tool (so the correct answer is reachable). Returns the list of problems
/// (empty ⇒ valid).
pub fn validate_corpus(corpus: &ConversationalCorpus) -> Vec<String> {
    let mut problems = Vec::new();
    let real: BTreeSet<&str> = REAL_TOOLS.iter().map(|(n, _)| *n).collect();

    for s in &corpus.scenarios {
        let n = s.expected_chain.len();
        if !(3..=5).contains(&n) {
            problems.push(format!(
                "{}: expected_chain has {} tools, must be 3-5 (sequential chain)",
                s.id, n
            ));
        }
        if s.turns.is_empty() {
            problems.push(format!("{}: no conversational turns", s.id));
        }
        if s.stop_after != n {
            problems.push(format!(
                "{}: stop_after {} != expected_chain length {}",
                s.id, s.stop_after, n
            ));
        }
        for t in &s.expected_chain {
            if !real.contains(t.as_str()) {
                problems.push(format!(
                    "{}: expected tool '{}' is not a real catalog tool (unreachable)",
                    s.id, t
                ));
            }
        }
        for h in &s.handoffs {
            if h.from_step >= n || h.to_step >= n {
                problems.push(format!(
                    "{}: handoff step out of range (from {} to {}, chain len {})",
                    s.id, h.from_step, h.to_step, n
                ));
            }
            if h.from_step >= h.to_step {
                problems.push(format!(
                    "{}: handoff from_step {} must precede to_step {}",
                    s.id, h.from_step, h.to_step
                ));
            }
            if h.to_step < n && s.expected_chain.get(h.to_step).map(|t| t != &h.to_tool).unwrap_or(false)
            {
                problems.push(format!(
                    "{}: handoff to_tool '{}' does not match expected_chain[{}]",
                    s.id, h.to_tool, h.to_step
                ));
            }
            if h.expect_arg_substrings.is_empty() {
                problems.push(format!(
                    "{}: handoff {}->{} has no expect_arg_substrings",
                    s.id, h.from_step, h.to_step
                ));
            }
        }
    }
    problems
}

// ===========================================================================
// Deterministic scorer (pure) — the heart of this dimension.
// ===========================================================================

/// One dispatched tool call from the model's transcript: tool name + raw argument
/// JSON (so the handoff check can scan the arguments).
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchedCall {
    pub tool: String,
    pub args: Value,
}

impl DispatchedCall {
    pub fn new(tool: impl Into<String>, args: Value) -> Self {
        DispatchedCall { tool: tool.into(), args }
    }

    /// Flatten the argument JSON to a lowercase string for substring handoff checks.
    fn args_lc(&self) -> String {
        self.args.to_string().to_lowercase()
    }
}

/// Distinct outcome class for a scenario — wrong-order and spurious-call are
/// recorded SEPARATELY from a clean pass (acceptance criterion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainOutcome {
    /// All criteria held: completed, correct tools, correct order, handoff, stopped.
    CleanPass,
    /// Right tools all called but in the wrong relative order (partial credit).
    WrongOrder,
    /// Called an unexpected/hallucinated tool, or kept calling past the stop point.
    SpuriousCall,
    /// Chain incomplete (a required tool was never called).
    Incomplete,
    /// A required data handoff was missing (right tools+order, arg not carried).
    HandoffMissing,
}

impl ChainOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            ChainOutcome::CleanPass => "clean_pass",
            ChainOutcome::WrongOrder => "wrong_order",
            ChainOutcome::SpuriousCall => "spurious_call",
            ChainOutcome::Incomplete => "incomplete",
            ChainOutcome::HandoffMissing => "handoff_missing",
        }
    }
    pub fn is_clean_pass(self) -> bool {
        matches!(self, ChainOutcome::CleanPass)
    }
}

/// The full deterministic score for one scenario replay.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainScore {
    pub scenario_id: String,
    /// Every expected tool was called at least once.
    pub completed: bool,
    /// Only expected tools were called (no spurious / hallucinated names).
    pub correct_tools: bool,
    /// Expected tools appeared in the expected relative order (subsequence).
    pub correct_order: bool,
    /// Every declared handoff's carried value appeared in the dependent call's args.
    pub correct_handoff: bool,
    /// The model stopped at/under the expected chain length (no extra calls).
    pub stopped: bool,
    /// Fraction of correct chain steps (partial credit), in [0,1].
    pub chain_accuracy: f64,
    /// The distinct outcome class (clean pass vs wrong-order vs spurious, etc.).
    pub outcome: ChainOutcome,
    /// Names of tools the model called that were NOT in the expected chain.
    pub spurious_tools: Vec<String>,
}

impl ChainScore {
    /// A scenario passes (1.0) only on a clean pass.
    pub fn passed(&self) -> bool {
        self.outcome.is_clean_pass()
    }
}

/// Subsequence-order check: do `expected` tools appear, each in turn, somewhere in
/// `dispatched` (allowing other calls between)? Pure. (Mirrors S83 `score_multi_step`
/// order semantics so the explicit and conversational paths agree.)
fn is_in_order(expected: &[String], dispatched: &[String]) -> bool {
    let mut it = dispatched.iter();
    for want in expected {
        if !it.any(|d| d == want) {
            return false;
        }
    }
    true
}

/// Score one conversational scenario against the model's dispatched calls. Pure
/// and deterministic. The four+1 criteria are evaluated independently so the
/// outcome class can distinguish wrong-order and spurious-call from a clean pass.
pub fn score_chain(scenario: &ConversationalScenario, dispatched: &[DispatchedCall]) -> ChainScore {
    let expected: Vec<String> = scenario.expected_chain.clone();
    let expected_set: BTreeSet<&str> = expected.iter().map(|s| s.as_str()).collect();
    let called_names: Vec<String> = dispatched.iter().map(|c| c.tool.clone()).collect();

    // (a) chain completed — every expected tool called at least once.
    let completed = expected
        .iter()
        .all(|t| called_names.iter().any(|c| c == t));

    // (b) correct tools — no call outside the expected set (spurious/hallucinated).
    let spurious_tools: Vec<String> = called_names
        .iter()
        .filter(|c| !expected_set.contains(c.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>() // dedup
        .into_iter()
        .collect();
    let correct_tools = spurious_tools.is_empty();

    // (c) correct order — expected tools appear in the expected relative order.
    let correct_order = is_in_order(&expected, &called_names);

    // (d) correct arg handoff — each declared handoff's value appears in the
    //     dependent step's call arguments. We match the handoff's `to_tool` to the
    //     model's call of that tool (first occurrence) and scan its args.
    let correct_handoff = scenario.handoffs.iter().all(|h| {
        match dispatched.iter().find(|c| c.tool == h.to_tool) {
            None => false, // dependent tool never called → handoff cannot hold
            Some(call) => {
                let lc = call.args_lc();
                h.expect_arg_substrings
                    .iter()
                    .any(|s| lc.contains(&s.to_lowercase()))
            }
        }
    });

    // (e) stopped appropriately — total calls did not exceed the expected length.
    let stopped = dispatched.len() <= scenario.stop_after;

    // chain_accuracy: fraction of expected tools called in the right relative
    // position. We walk the dispatched calls and count, greedily, how many of the
    // expected tools were satisfied in order — partial credit for a partial chain.
    let chain_accuracy = if expected.is_empty() {
        0.0
    } else {
        let mut idx = 0usize;
        for name in &called_names {
            if idx < expected.len() && name == &expected[idx] {
                idx += 1;
            }
        }
        idx as f64 / expected.len() as f64
    };

    // Distinct outcome classification — precedence chosen so a clean pass is only
    // ever reported when ALL criteria hold, and the failure modes the spec calls
    // out (wrong-order, spurious-call) are surfaced separately.
    let outcome = if completed && correct_tools && correct_order && correct_handoff && stopped {
        ChainOutcome::CleanPass
    } else if !correct_tools || !stopped {
        // Hallucinated/unexpected tool, or didn't stop (extra calls past the chain).
        ChainOutcome::SpuriousCall
    } else if !completed {
        ChainOutcome::Incomplete
    } else if !correct_order {
        // All right tools called, no spurious, but in the wrong sequence.
        ChainOutcome::WrongOrder
    } else {
        // Completed, correct tools, correct order, stopped — only the handoff failed.
        ChainOutcome::HandoffMissing
    };

    ChainScore {
        scenario_id: scenario.id.clone(),
        completed,
        correct_tools,
        correct_order,
        correct_handoff,
        stopped,
        chain_accuracy,
        outcome,
        spurious_tools,
    }
}

// ===========================================================================
// Inference abstraction (unified path live; mockable for tests)
// ===========================================================================

/// The tool-dispatch surface the runner depends on. The live impl
/// ([`LiveToolModel`]) routes through Chord's unified tool-dispatch path; tests
/// inject a deterministic mock.
#[async_trait::async_trait]
pub trait ConversationToolModel: Send + Sync {
    /// Given the full conversational `turns` (intent built across them) and the
    /// advertised tool `catalog`, return the ordered list of tool calls the model
    /// dispatched. Must never panic — transport/timeout errors map to an empty
    /// dispatch (scored as an incomplete chain, not a crash).
    async fn dispatch(&self, turns: &[String], catalog: &Value) -> Vec<DispatchedCall>;
}

/// Live model: drives the scenario through Chord's **unified tool-dispatch path**
/// via [`crate::intake::context::chat_with_tools`] (the normal `/api/chat`
/// tool-calling surface fronted by `infer`/`infer_with_metrics`). The multi-turn
/// conversation is flattened into a single user message preserving turn order, so
/// the model must infer the implicit chain from the built-up intent — exactly how
/// S83's `agent` suite invokes the same proxy.
pub struct LiveToolModel {
    client: reqwest::Client,
    model_name: String,
    timeout: Duration,
}

impl LiveToolModel {
    pub fn new(client: reqwest::Client, model_name: impl Into<String>, timeout: Duration) -> Self {
        LiveToolModel { client, model_name: model_name.into(), timeout }
    }

    /// Flatten the conversational turns into a single prompt preserving order. The
    /// intent is left IMPLICIT (we do not inject "call these tools"); the model
    /// must infer the chain.
    pub fn render_prompt(turns: &[String]) -> String {
        let mut p = String::new();
        for (i, t) in turns.iter().enumerate() {
            if i > 0 {
                p.push_str("\n\n");
            }
            p.push_str(t);
        }
        p
    }
}

#[async_trait::async_trait]
impl ConversationToolModel for LiveToolModel {
    async fn dispatch(&self, turns: &[String], catalog: &Value) -> Vec<DispatchedCall> {
        let prompt = Self::render_prompt(turns);
        // UNIFIED PATH: context::chat_with_tools → /api/chat (infer/infer_with_metrics).
        let out = crate::intake::context::chat_with_tools(
            &self.client,
            &self.model_name,
            &prompt,
            catalog,
            self.timeout,
        )
        .await;
        // Transport/timeout/HTTP error or no calls → empty dispatch (incomplete
        // chain, never a crash). The tool_calls are (name, args) in order.
        out.tool_calls
            .into_iter()
            .map(|(name, args)| DispatchedCall::new(name, args))
            .collect()
    }
}

// ===========================================================================
// Runner (orchestration over the trait; no DB, no direct network)
// ===========================================================================

/// Full dim-2 outcome for one (model, backend): per-conversational-scenario score
/// plus the aggregate pass-rate and mean chain-accuracy, ready to flatten into
/// storage rows.
#[derive(Debug, Clone)]
pub struct Dim2Outcome {
    /// Per conversational scenario.
    pub per_scenario: Vec<ChainScore>,
    /// IDs of the S83 explicit scenarios referenced (loaded separately by the
    /// caller); recorded for audit so the join to S83 is explicit.
    pub s83_referenced_ids: Vec<String>,
    /// S83 ids that were referenced but missing from the S83 corpus at runtime
    /// (explicit chains skipped with a logged note — conversational chains still run).
    pub s83_missing_ids: Vec<String>,
}

impl Dim2Outcome {
    /// Clean-pass rate across conversational scenarios.
    pub fn conversational_pass_rate(&self) -> f64 {
        if self.per_scenario.is_empty() {
            return 0.0;
        }
        let pass = self.per_scenario.iter().filter(|s| s.passed()).count();
        pass as f64 / self.per_scenario.len() as f64
    }

    /// Mean chain-accuracy across conversational scenarios.
    pub fn mean_chain_accuracy(&self) -> f64 {
        if self.per_scenario.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.per_scenario.iter().map(|s| s.chain_accuracy).sum();
        sum / self.per_scenario.len() as f64
    }

    /// Flatten into `DimensionScore` rows for one (model, backend):
    ///   - one `scenario_pass` (0/1) + one `chain_accuracy` row per scenario,
    ///   - one aggregate `conversational_pass_rate` + one `mean_chain_accuracy` row.
    /// Every row is `judge = "deterministic"` (no panel). The per-scenario rows
    /// carry a redacted audit blob recording the DISTINCT outcome class so
    /// wrong-order / spurious-call are never lost behind a pass/fail bit.
    pub fn into_dimension_scores(
        &self,
        model_id: &ModelId,
        backend_tag: BackendTag,
    ) -> Vec<DimensionScore> {
        let mut rows = Vec::new();

        for s in &self.per_scenario {
            let audit = serde_json::json!({
                "scenario_id": s.scenario_id,
                "outcome": s.outcome.as_str(),
                "completed": s.completed,
                "correct_tools": s.correct_tools,
                "correct_order": s.correct_order,
                "correct_handoff": s.correct_handoff,
                "stopped": s.stopped,
                "spurious_tools": s.spurious_tools,
                "chain_accuracy": s.chain_accuracy,
            })
            .to_string();

            rows.push(DimensionScore {
                model_id: model_id.clone(),
                backend_tag,
                dimension: DIMENSION.to_string(),
                metric: format!("{}:{}", METRIC_SCENARIO_PASS, s.scenario_id),
                value: if s.passed() { 1.0 } else { 0.0 },
                std_dev: None,
                judge: JUDGE_DETERMINISTIC.to_string(),
                low_confidence: false,
                raw_json: Some(audit.clone()),
            });
            rows.push(DimensionScore {
                model_id: model_id.clone(),
                backend_tag,
                dimension: DIMENSION.to_string(),
                metric: format!("{}:{}", METRIC_CHAIN_ACCURACY, s.scenario_id),
                value: s.chain_accuracy,
                std_dev: None,
                judge: JUDGE_DETERMINISTIC.to_string(),
                low_confidence: false,
                raw_json: Some(audit),
            });
        }

        // Aggregate rows (audit records the S83 reference set + any missing ids).
        let agg_audit = serde_json::json!({
            "s83_referenced_ids": self.s83_referenced_ids,
            "s83_missing_ids": self.s83_missing_ids,
            "scenarios_scored": self.per_scenario.len(),
        })
        .to_string();
        rows.push(DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: METRIC_CONVERSATIONAL_PASS_RATE.to_string(),
            value: self.conversational_pass_rate(),
            std_dev: None,
            judge: JUDGE_DETERMINISTIC.to_string(),
            low_confidence: false,
            raw_json: Some(agg_audit.clone()),
        });
        rows.push(DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: METRIC_MEAN_CHAIN_ACCURACY.to_string(),
            value: self.mean_chain_accuracy(),
            std_dev: None,
            judge: JUDGE_DETERMINISTIC.to_string(),
            low_confidence: false,
            raw_json: Some(agg_audit),
        });

        rows
    }
}

/// Run the conversational dim-2 axis against `model` over `corpus`. For each
/// scenario, builds the S83-shared tool catalog (so the correct tools are
/// reachable amid decoys), dispatches through the unified path, and scores
/// deterministically. `s83_missing_ids` lets the caller record which referenced
/// explicit scenarios were absent (skipped-with-note) without duplicating them.
/// Pure orchestration over the injected trait — never panics.
pub async fn run_dim2(
    model: &dyn ConversationToolModel,
    corpus: &ConversationalCorpus,
    s83_missing_ids: Vec<String>,
) -> Dim2Outcome {
    let mut per_scenario = Vec::with_capacity(corpus.scenarios.len());

    for sc in &corpus.scenarios {
        let catalog = build_catalog(CONVERSATIONAL_CATALOG_SIZE, &sc.expected_chain);
        let dispatched = model.dispatch(&sc.turns, &catalog).await;
        per_scenario.push(score_chain(sc, &dispatched));
    }

    Dim2Outcome {
        per_scenario,
        s83_referenced_ids: corpus.s83_reference_ids.clone(),
        s83_missing_ids,
    }
}

// ===========================================================================
// S83 explicit-scenario reference (BY ID — bodies never duplicated)
// ===========================================================================

/// Resolve the S83-referenced explicit `multi_step` scenarios by **id** from the
/// S83 agent-scenario corpus (`agent-scenarios.json` under `INTAKE_CORPUS_DIR`).
/// Returns `(found, missing_ids)`: the found scenarios are the live S83 bodies
/// (loaded, not copied); `missing_ids` are referenced ids absent from the corpus
/// (the runner skips those explicit chains with a logged note and still runs the
/// conversational chains — EDGE CASE in the spec).
///
/// This is the SINGLE source of the explicit scenarios. Re-authoring any S83
/// scenario body inside `toolchain_conversational.json` would defeat the purpose;
/// the test `no_s83_scenario_bodies_duplicated` enforces that.
pub fn load_s83_referenced_explicit_scenarios(
    reference_ids: &[String],
) -> (Vec<crate::intake::agent::Scenario>, Vec<String>) {
    let dir = crate::intake::code::corpus_dir();
    let all = match crate::intake::agent::read_scenarios(&dir) {
        Ok(s) => s,
        Err(_) => {
            // Corpus not present → all referenced ids are "missing"; conversational
            // chains still run. Not a crash.
            return (Vec::new(), reference_ids.to_vec());
        }
    };
    let by_id: std::collections::BTreeMap<&str, &crate::intake::agent::Scenario> =
        all.iter().map(|s| (s.id.as_str(), s)).collect();

    let mut found = Vec::new();
    let mut missing = Vec::new();
    for id in reference_ids {
        match by_id.get(id.as_str()) {
            Some(s) => found.push((*s).clone()),
            None => missing.push(id.clone()),
        }
    }
    (found, missing)
}

// ===========================================================================
// Tests (pure unit coverage; integration lives in tests/intake/)
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(tool: &str, args: Value) -> DispatchedCall {
        DispatchedCall::new(tool, args)
    }

    fn trip_scenario() -> ConversationalScenario {
        load_corpus()
            .unwrap()
            .scenarios
            .into_iter()
            .find(|s| s.id == "cv-trip-pack-01")
            .unwrap()
    }

    #[test]
    fn embedded_corpus_parses_and_validates() {
        let corpus = load_corpus().expect("corpus parses");
        let problems = validate_corpus(&corpus);
        assert!(problems.is_empty(), "corpus problems: {problems:?}");
        // 3–5 conversational scenarios per the spec.
        assert!((3..=5).contains(&corpus.scenarios.len()));
        // Every scenario is a 3–5 step chain.
        for s in &corpus.scenarios {
            assert!((3..=5).contains(&s.expected_chain.len()), "{}", s.id);
        }
        // References S83 explicit scenarios by id (non-empty).
        assert!(!corpus.s83_reference_ids.is_empty());
    }

    #[test]
    fn validate_catches_bad_chain_and_handoff() {
        let corpus: ConversationalCorpus = serde_json::from_str(
            r#"{
              "scenarios": [{
                "id":"bad","turns":["hi"],
                "expected_chain":["weather","not_a_real_tool"],
                "stop_after":3,
                "handoffs":[{"from_step":1,"to_step":0,"to_tool":"weather","expect_arg_substrings":[]}]
              }]
            }"#,
        )
        .unwrap();
        let p = validate_corpus(&corpus);
        assert!(p.iter().any(|x| x.contains("must be 3-5")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("not a real catalog tool")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("stop_after")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("must precede")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("no expect_arg_substrings")), "{p:?}");
    }

    #[test]
    fn clean_pass_when_all_criteria_hold() {
        let sc = trip_scenario(); // weather -> google_calendar_week -> reminder_set
        let dispatched = vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("google_calendar_week", json!({"query": "this weekend"})),
            call("reminder_set", json!({"query": "pack for the weekend in Zubrovka"})),
        ];
        let s = score_chain(&sc, &dispatched);
        assert_eq!(s.outcome, ChainOutcome::CleanPass);
        assert!(s.passed());
        assert!((s.chain_accuracy - 1.0).abs() < 1e-9);
        assert!(s.spurious_tools.is_empty());
    }

    #[test]
    fn wrong_order_is_distinct_from_clean_pass() {
        let sc = trip_scenario();
        // Right tools, no spurious, but reminder called BEFORE calendar.
        let dispatched = vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("reminder_set", json!({"query": "pack for the weekend"})),
            call("google_calendar_week", json!({"query": "this weekend"})),
        ];
        let s = score_chain(&sc, &dispatched);
        assert_eq!(s.outcome, ChainOutcome::WrongOrder);
        assert!(!s.passed());
        assert!(s.completed);
        assert!(s.correct_tools);
        assert!(!s.correct_order);
        // partial credit: weather satisfied, then order breaks.
        assert!(s.chain_accuracy < 1.0 && s.chain_accuracy > 0.0);
    }

    #[test]
    fn spurious_call_is_distinct_from_clean_pass() {
        let sc = trip_scenario();
        // Correct chain PLUS a hallucinated/unexpected tool.
        let dispatched = vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("google_calendar_week", json!({"query": "weekend"})),
            call("reminder_set", json!({"query": "pack for the weekend in Zubrovka"})),
            call("book_flight", json!({"query": "Zubrovka"})), // spurious
        ];
        let s = score_chain(&sc, &dispatched);
        assert_eq!(s.outcome, ChainOutcome::SpuriousCall);
        assert!(!s.passed());
        assert!(!s.correct_tools);
        assert!(!s.stopped); // 4 > stop_after 3
        assert_eq!(s.spurious_tools, vec!["book_flight".to_string()]);
    }

    #[test]
    fn incomplete_chain_when_tool_missing() {
        let sc = trip_scenario();
        let dispatched = vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("google_calendar_week", json!({"query": "weekend"})),
            // reminder_set never called
        ];
        let s = score_chain(&sc, &dispatched);
        assert_eq!(s.outcome, ChainOutcome::Incomplete);
        assert!(!s.completed);
        assert!((s.chain_accuracy - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn handoff_missing_when_value_not_carried() {
        let sc = trip_scenario();
        // Correct tools + order + stopped, but the destination never reaches the
        // reminder args (no "pack"/"weekend" carried into reminder_set).
        let dispatched = vec![
            call("weather", json!({"query": "Zubrovka"})),
            call("google_calendar_week", json!({"query": "this weekend"})),
            call("reminder_set", json!({"query": "do the thing"})),
        ];
        let s = score_chain(&sc, &dispatched);
        assert_eq!(s.outcome, ChainOutcome::HandoffMissing);
        assert!(!s.passed());
        assert!(s.completed && s.correct_tools && s.correct_order && s.stopped);
        assert!(!s.correct_handoff);
    }

    #[test]
    fn empty_dispatch_is_incomplete_not_crash() {
        let sc = trip_scenario();
        let s = score_chain(&sc, &[]);
        assert_eq!(s.outcome, ChainOutcome::Incomplete);
        assert_eq!(s.chain_accuracy, 0.0);
    }

    #[test]
    fn live_render_prompt_preserves_turn_order_and_implicit_intent() {
        let turns = vec!["first turn".to_string(), "second turn".to_string()];
        let p = LiveToolModel::render_prompt(&turns);
        assert!(p.find("first").unwrap() < p.find("second").unwrap());
        // Implicit: we never inject a "call these tools" instruction.
        assert!(!p.to_lowercase().contains("call these"));
    }

    // ── mock model for the runner path ──

    struct MockToolModel {
        plan: std::collections::BTreeMap<String, Vec<DispatchedCall>>,
    }

    #[async_trait::async_trait]
    impl ConversationToolModel for MockToolModel {
        async fn dispatch(&self, turns: &[String], _catalog: &Value) -> Vec<DispatchedCall> {
            // Key off the first turn's text to pick the scripted plan.
            let key = turns.first().cloned().unwrap_or_default();
            self.plan.get(&key).cloned().unwrap_or_default()
        }
    }

    #[tokio::test]
    async fn run_dim2_scores_conversational_scenarios_and_aggregates() {
        let corpus = load_corpus().unwrap();
        // Script a clean pass for the trip scenario, leave the rest empty (incomplete).
        let trip = corpus.scenarios.iter().find(|s| s.id == "cv-trip-pack-01").unwrap();
        let mut plan = std::collections::BTreeMap::new();
        plan.insert(
            trip.turns[0].clone(),
            vec![
                call("weather", json!({"query": "Zubrovka"})),
                call("google_calendar_week", json!({"query": "weekend"})),
                call("reminder_set", json!({"query": "pack for the weekend in Zubrovka"})),
            ],
        );
        let model = MockToolModel { plan };

        let outcome = run_dim2(&model, &corpus, vec![]).await;
        assert_eq!(outcome.per_scenario.len(), corpus.scenarios.len());
        // Exactly one clean pass (the trip scenario).
        let passes = outcome.per_scenario.iter().filter(|s| s.passed()).count();
        assert_eq!(passes, 1);
        let expected_rate = 1.0 / corpus.scenarios.len() as f64;
        assert!((outcome.conversational_pass_rate() - expected_rate).abs() < 1e-9);

        // Storage rows: per-scenario pass + accuracy, plus the two aggregates.
        let rows = outcome.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Gpu);
        assert!(rows.iter().all(|r| r.judge == JUDGE_DETERMINISTIC));
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));
        assert!(rows
            .iter()
            .any(|r| r.metric == METRIC_CONVERSATIONAL_PASS_RATE));
        assert!(rows.iter().any(|r| r.metric == METRIC_MEAN_CHAIN_ACCURACY));
        // A per-scenario pass row for the trip carries value 1.0.
        let trip_pass = rows
            .iter()
            .find(|r| r.metric == format!("{}:{}", METRIC_SCENARIO_PASS, "cv-trip-pack-01"))
            .unwrap();
        assert_eq!(trip_pass.value, 1.0);
    }

    #[test]
    fn s83_reference_ids_are_present_and_nonempty() {
        let ids = s83_reference_ids();
        assert!(!ids.is_empty());
        // They look like S83 multi_step ids.
        assert!(ids.iter().all(|i| i.starts_with("ms-")));
    }
}
