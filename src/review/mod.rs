//! `review_run` — multi-provider, multi-structure code/change review tool.
//!
//! Dispatches a review prompt to 1-5 providers concurrently, in one of four
//! structures (`single`, `adversarial_pair`, `panel_majority`,
//! `panel_unanimous`), and aggregates their verdicts into one answer.
//!
//! ## Providers
//!   - `opus`, `codex`, `agy` — CLI-backed. Reached over loopback HTTP via the
//!     `review-daemon` binary (`src/bin/review_daemon/`), which is the ONLY
//!     place in this codebase permitted to spawn these processes (see
//!     `src/tool.rs`'s no-subprocess-in-tool contract, and `src/dgem/mod.rs`
//!     for the established precedent of this daemon-over-loopback-HTTP shape).
//!   - `nemotron`, `qwen_coder` — dispatched directly to OpenRouter's
//!     chat-completions endpoint via `reqwest`. Both are genuinely
//!     frontier-class free-tier models (see `dispatch.rs` for the current
//!     model tags and the rationale for each).
//!
//! A single provider's failure/timeout/auth-error degrades that provider's
//! entry to `"unavailable: <reason>"` rather than failing the whole tool call;
//! the aggregate result's `complete` flag reflects whether every requested
//! provider actually answered.
//!
//! ## Config (env)
//!   - `REVIEW_DAEMON_URL`   — review-daemon base URL (default `http://127.0.0.1:8790`) // pii-test-fixture
//!   - `REVIEW_DAEMON_TOKEN` — bearer token matching the daemon's own config;
//!                             if unset, `opus`/`codex`/`agy` all degrade to
//!                             `"unavailable: REVIEW_DAEMON_TOKEN not configured"`
//!   - `OPENROUTER_API_KEY`  — OpenRouter key for `nemotron`/`qwen_coder`; if
//!                             unset, those two degrade similarly

mod aggregate;
// REVCAP-01 PART A: the reviewer-provider capacity core (two-tier cooldown/
// shelve state machine + the frontier capacity gate). `pub(crate)` for the
// same reason as `dispatch`/`kg_context` below -- `review_provider_status`
// (this module, further down) and `execute()`'s capacity gate both need it,
// and it has no reason to be reachable outside this crate.
pub(crate) mod capacity;
// `pub(crate)` (was module-private): DOCGEN-10's mismatch detector
// (`crate::tools::docgen::mismatch`) reuses `is_daemon_provider` /
// `openrouter_model_for` / the model-tag constants directly rather than
// re-declaring the opus/codex/agy-vs-nemotron/qwen_coder provider-routing
// table a second time -- one source of truth for "which providers go
// through the daemon vs. OpenRouter", not two.
pub(crate) mod dispatch;
pub(crate) mod free_pool;
// `pub(crate)` (was module-private): CXEG-02's `cortex_scope`
// (`crate::cortex::scope`) reuses `derive_changed_files` directly so it and
// `review_run`'s KGREV-01 grounding agree on which files a `diff`/
// `changed_files` input touches, rather than re-implementing the same
// CSV/array/unified-diff parsing a second time.
pub(crate) mod kg_context;
mod prompt;
// CXEG-07: the Tier-C consistency/elegance lens. Module stays private --
// `execute()` below calls into it directly, strictly AFTER `aggregate()` has
// already fixed `aggregate_verdict`/`complete` (see `consistency`'s module
// doc for why that ordering is the load-bearing advisory-only safety
// property). CXEG-10's calibration harness gets a narrow, purpose-built
// door (`run_consistency_lens_dry` below) instead of a wider `pub mod` --
// same S9 rationale as everywhere else in this crate: one sanctioned way in.
mod consistency;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::cortex::house_style::HouseStyleCache;
use crate::cortex::review::compute_review as cortex_compute_review;
use crate::cortex::waiver;
use crate::cortex::CortexConfig;
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::scribe::graph::findings_store::{FindingsStore, NewFinding, RecordOutcome, ScopeKind};
use crate::scribe::graph::model::KnowledgeGraph;
use crate::scribe::graph::store::GraphStore;
use crate::scribe::graph::vec_embed::EmbedClient;
use crate::scribe::ScribeConfig;
use crate::tool::{RustTool, ToolOutput};

pub use aggregate::{aggregate, Finding, ProviderResult};
pub use dispatch::ReviewConfig;
pub use prompt::{build_docs_prompt, build_prompt, parse_findings, parse_verdict, Role, Structure};
// CXEG-10: the calibration harness needs to name the CXEG-07 lens's result
// types to read back what it would have flagged. Re-exported rather than
// widening `consistency` itself to `pub` -- see the `mod consistency;` note
// above.
pub use consistency::{ConsistencyFinding, ConsistencyRun};

const ALLOWED_PROVIDERS: &[&str] =
    &["opus", "codex", "agy", "nemotron", "qwen_coder", "free", "claude-fable-5", "gpt56", "diffusion"];
/// Routine review panel cap (per one `review_run` call).
const MAX_PROVIDERS: usize = 5;
/// The Epic capstone runs the widest possible panel once per build, so it may
/// exceed the routine 5-provider cap (opus+codex+agy+free+claude-fable-5+gpt56 = 6,
/// with headroom for the local nemotron/qwen_coder lenses).
const MAX_PROVIDERS_EPIC: usize = 8;

/// The provider-count cap for a given structure (Epic gets the wider panel).
fn max_providers_for(structure: Structure) -> usize {
    if structure == Structure::Epic {
        MAX_PROVIDERS_EPIC
    } else {
        MAX_PROVIDERS
    }
}

/// KGREV-02: process-wide set of `project_id`s with an in-flight KG rebuild.
/// A re-review of the SAME project short-circuits (see `execute()`'s top-of-
/// function lock check) while its entry is present; a different project is
/// never blocked by another's rebuild. Pattern mirrors
/// `src/sysversion/mod.rs`'s `OnceLock<Mutex<..>>` process-wide cache.
static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn in_flight() -> &'static Mutex<HashSet<String>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// RAII guard that removes `project_id` from [`in_flight()`] when dropped, on
/// every path (normal return, early `?`, or panic-unwind) -- this is what
/// guarantees the lock never deadlocks a project.
struct InFlightGuard(String);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut set = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        set.remove(&self.0);
    }
}

/// KGREV-02 post-aggregate hook: on a successful, complete review pass with
/// both `project_id` and `repo_path` present in `context`, incrementally
/// rebuild that project's Atlas graph for the changed files, holding the
/// per-project lock ([`in_flight()`]) for the duration so a concurrent
/// re-review of the same project short-circuits rather than referencing a
/// mid-rebuild graph. Always returns a `kg_rebuild` value to merge into the
/// tool result -- never propagates a rebuild error (the review already
/// passed; a rebuild failure is reported, not fatal).
async fn maybe_rebuild(run_hooks: bool, context: &Value) -> Value {
    if !run_hooks {
        return json!({"ran": false, "reason": "review did not pass"});
    }
    let Some(project_id) = context.get("project_id").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return json!({"ran": false, "reason": "no project_id"});
    };
    let Some(repo_path) = context.get("repo_path").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return json!({"ran": false, "reason": "no repo_path"});
    };

    // Insert BEFORE constructing the guard, then build the guard -- so the
    // guard's Drop always has a corresponding entry to remove.
    in_flight().lock().unwrap_or_else(|e| e.into_inner()).insert(project_id.clone());
    let _guard = InFlightGuard(project_id.clone());

    let changed_files = kg_context::derive_changed_files(context);
    let rebuild_args = json!({
        "project_id": project_id,
        "repo_path": repo_path,
        "incremental": true,
        "changed_files": changed_files,
    });

    match crate::scribe::graph::build::ScribeKgBuild.execute_structured(rebuild_args).await {
        Ok(out) => {
            let mut result = json!({"ran": true, "ok": true});
            if let Some(structured) = out.structured {
                if let Some(map) = result.as_object_mut() {
                    for key in ["nodes", "edges", "clusters", "mode"] {
                        if let Some(v) = structured.get(key) {
                            map.insert(key.to_string(), v.clone());
                        }
                    }
                }
            }
            result
        }
        Err(e) => {
            tracing::warn!("KGREV-02: incremental KG rebuild failed for '{project_id}': {e}");
            json!({"ran": true, "ok": false, "error": e.to_string()})
        }
    }
    // `_guard` drops here, releasing the lock on every path above.
}

/// KGREV-03 post-rebuild hook: on a successful, complete review pass whose
/// `context` also carries doc params (`project` + `spec_id`, required;
/// `module_path` / `git_ref` / `project_config`, optional), drive a doc
/// refresh through the ONE sanctioned doc-generation door -- `docgen_run`
/// (`crate::tools::docgen::trigger::DocgenRun`), called in-process via its
/// `RustTool::execute_structured`. Must run AFTER [`maybe_rebuild`] so the
/// doc engine sees the freshly-rebuilt graph/state. Always returns a
/// `scribe_docs` value to merge into the tool result -- never propagates a
/// docgen error (the review already passed; a doc-gen failure is reported,
/// not fatal). Most reviews won't supply doc params at all; this wire only
/// fires for real merge-time reviews that do (S9: no ad-hoc doc path).
///
/// ## DLAND-04: capstone-APPROVE places the generated docs
/// When `context` also carries a `repo_path` (the merged feat's working
/// copy -- the same field [`maybe_rebuild`] already reads), this hook asks
/// `docgen_run` to PLACE its output (`place: true`, `target_root:
/// repo_path`) as well as generate it, so a passing epic capstone lands the
/// concise README + `docs/` tree directly into the working tree as a
/// normal working-tree change the pipeline's own git stages carry (DLAND-01,
/// gated fail-closed by DLAND-03) -- no new forge/git door is added here,
/// this is still the same in-process `docgen_run` call, just with two more
/// fields in its args. When `repo_path` is absent, placement is skipped
/// exactly like before this item (generation-only). Non-blocking either
/// way: a placement failure surfaces inside the returned `docgen`/`outcome`
/// JSON like any other docgen-internal failure -- it never changes this
/// hook's own non-fatal contract.
async fn maybe_scribe_docs(run_hooks: bool, context: &Value) -> Value {
    if !run_hooks {
        return json!({"ran": false, "reason": "not an approved pass"});
    }
    let Some(project) = context.get("project").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return json!({"ran": false, "reason": "no doc params"});
    };
    let Some(spec_id) = context.get("spec_id").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return json!({"ran": false, "reason": "no doc params"});
    };

    // `docgen_run` requires non-empty `module_path`/`git_ref`; a review's
    // context may not carry either, so fall back to inert placeholders
    // rather than failing the doc wire outright -- `docgen_run` itself is
    // opt-in per project (no `project_config` -> clean `Skipped`), so an
    // unspecified module/ref never causes a spurious doc generation.
    let module_path = context.get("module_path").and_then(|v| v.as_str()).unwrap_or(".").to_string();
    let git_ref = context.get("git_ref").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
    let feat_context = context.get("diff").and_then(|v| v.as_str()).unwrap_or_default().to_string();

    let mut docgen_args = json!({
        "spec_id": spec_id,
        "project": project,
        "module_path": module_path,
        "git_ref": git_ref,
        "feat_context": feat_context,
    });
    if let Some(project_config) = context.get("project_config") {
        docgen_args["project_config"] = project_config.clone();
    }
    // DLAND-04: this hook only fires on a passing epic capstone
    // (`should_run_docgen`) -- when the review context also names a
    // `repo_path`, land the generated docs into that working tree too, not
    // just generate+version them. Absent `repo_path` -> generation-only,
    // exactly the pre-DLAND-04 behavior.
    if let Some(repo_path) = context.get("repo_path").and_then(|v| v.as_str()) {
        docgen_args["place"] = json!(true);
        docgen_args["target_root"] = json!(repo_path);
    }

    // The ONLY doc-generation door: the existing `docgen_run` tool, called
    // in-process. No direct doc-gen HTTP/Chord call is made here (S9).
    match crate::tools::docgen::trigger::DocgenRun::default().execute_structured(docgen_args).await {
        Ok(out) => match serde_json::from_str::<Value>(&out.text) {
            Ok(parsed) => {
                let mut result = json!({"ran": true});
                if let Some(map) = result.as_object_mut() {
                    if let Some(outcome) = parsed.get("outcome") {
                        map.insert("outcome".to_string(), outcome.clone());
                    }
                    map.insert("docgen".to_string(), parsed);
                }
                result
            }
            Err(e) => {
                tracing::warn!("KGREV-03: docgen_run for '{project}' returned non-JSON output: {e}");
                json!({"ran": true, "ok": false, "error": e.to_string()})
            }
        },
        Err(e) => {
            tracing::warn!("KGREV-03: docgen_run failed for '{project}': {e}");
            json!({"ran": true, "ok": false, "error": e.to_string()})
        }
    }
}

// ── KGFIND-03: capture-only findings recording ─────────────────────────────

/// KGFIND-03: collapse findings that are the same issue reported by more than
/// one provider -- same `(category, file, symbol)`, regardless of description
/// wording -- into one, keeping the first occurrence (in provider order). Pure,
/// fully unit-testable without a store or a graph.
fn dedup_across_providers(results: &[ProviderResult]) -> Vec<Finding> {
    // Collapse a SINGLE review's cross-provider duplicates by (category, file,
    // symbol) — NOT including the description text: two reviewers describing the
    // same issue at the same location in different words must count as ONE
    // occurrence, not two. The description wording is intentionally out of the
    // key. The finer "is this really the same issue" judgement across reviews is
    // the store's semantic (embedding) dedup within the (project, scope,
    // category) bucket; this hook-level collapse only prevents one review's
    // agreeing reviewers from double-counting recurrence.
    let mut seen: HashSet<(String, Option<String>, Option<String>)> = HashSet::new();
    let mut out = Vec::new();
    for r in results {
        for f in &r.findings {
            let key = (f.category.clone(), f.file.clone(), f.symbol.clone());
            if seen.insert(key) {
                out.push(f.clone());
            }
        }
    }
    out
}

/// KGFIND-03: resolve the KG scope a finding concerns. Prefers an exact node
/// match by id, then by name among the graph's currently-valid nodes; falls
/// back to the finding's file (path scope), then the project itself (global
/// scope). `graph` is `None` when no store/graph is available for the
/// project -- symbol findings then fall back to path/global just like a
/// finding with no symbol at all. Pure, no I/O.
fn resolve_scope(finding: &Finding, graph: Option<&KnowledgeGraph>, project_id: &str) -> (ScopeKind, String) {
    if let Some(symbol) = finding.symbol.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(g) = graph {
            if let Some(node) = g.get_node(symbol) {
                return (ScopeKind::Node, node.id.clone());
            }
            if let Some(node) = g.current_nodes().find(|n| n.name == symbol) {
                return (ScopeKind::Node, node.id.clone());
            }
        }
    }
    if let Some(file) = finding.file.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return (ScopeKind::Path, file.to_string());
    }
    (ScopeKind::Global, project_id.to_string())
}

/// Build a single finding's provenance entry from the review `context` plus
/// the aggregate verdict and the set of reviewing providers -- `pr`,
/// `review` (falling back to `spec_id`), and `git_ref` are carried through
/// when present; anything absent from `context` is simply omitted rather
/// than stored as null.
fn build_finding_provenance(context: &Value, verdict: &str, providers: &[String]) -> Value {
    let mut obj = serde_json::Map::new();
    if let Some(pr) = context.get("pr") {
        obj.insert("pr".to_string(), pr.clone());
    }
    if let Some(review) = context.get("review") {
        obj.insert("review".to_string(), review.clone());
    } else if let Some(spec_id) = context.get("spec_id") {
        obj.insert("spec_id".to_string(), spec_id.clone());
    }
    if let Some(git_ref) = context.get("git_ref") {
        obj.insert("git_ref".to_string(), git_ref.clone());
    }
    obj.insert("verdict".to_string(), json!(verdict));
    obj.insert("providers".to_string(), json!(providers));
    obj.insert("recorded_at".to_string(), json!(chrono::Utc::now().to_rfc3339()));
    Value::Object(obj)
}

/// CXEG-10: calibration-only entry point for the Tier-C consistency lens in
/// dry/capture-only mode. Delegates directly to `consistency::maybe_run` --
/// the EXACT SAME function `execute()` above calls after `aggregate()` --
/// with no other code path involved (S9: one door). The critical property
/// for calibration is what this function does NOT do: unlike `execute()`,
/// it never folds the returned findings into `maybe_record_findings`, so a
/// caller that only ever calls this (as `cortex_calibrate` does) can never
/// write to the KGFIND store, structurally rather than by a flag that could
/// drift. `panel_results` may be empty (calibration replays no live
/// correctness panel) -- `consistency::maybe_run` only reads it to detect
/// cross-source disagreement on the SAME `(category, file, symbol)` anchor,
/// which degrades cleanly to "no disagreement observed" when there is no
/// panel to compare against.
pub async fn run_consistency_lens_dry(
    context: &Value,
    criteria: &str,
    panel_results: &[ProviderResult],
    review_cfg: &ReviewConfig,
    cortex_config: &CortexConfig,
    house_style_cache: &HouseStyleCache,
) -> ConsistencyRun {
    consistency::maybe_run(context, criteria, panel_results, review_cfg, cortex_config, house_style_cache).await
}

/// KGFIND-03 post-aggregate hook: record the providers' structured findings
/// onto the Atlas KG findings store, anchored to scope, deduped across
/// providers and (best-effort) semantically deduped by the store itself.
/// Fires on ANY verdict -- findings matter most on `REQUEST_CHANGES` -- unlike
/// [`maybe_rebuild`]/[`maybe_scribe_docs`], which only run on a clean
/// `APPROVE`. CAPTURE ONLY: never mints a rule, never promotes, never blocks;
/// always returns a `findings_recorded` value to merge into the tool result,
/// and never propagates a store/embedding error (a findings failure must
/// never change the verdict or fail the review call).
///
/// `context` is expected to already carry a `"verdict"` field (the caller
/// stamps this in just before invoking the hook, mirroring how
/// `kg_context::inject` stamps `"knowledge_graph"` in-place) -- this keeps the
/// function's signature `(results, context)` while still giving provenance
/// access to the aggregate verdict.
async fn maybe_record_findings(results: &[ProviderResult], context: &Value) -> Value {
    let Some(project_id) = context.get("project_id").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return json!({"recorded": false, "reason": "no project_id"});
    };

    let deduped = dedup_across_providers(results);
    if deduped.is_empty() {
        return json!({"recorded": false, "reason": "no findings"});
    }

    let store = match FindingsStore::from_env().await {
        Ok(s) => s,
        Err(e) => {
            return json!({"recorded": false, "reason": format!("findings store unavailable: {e}")});
        }
    };

    // Best-effort graph load for scope resolution -- absence (no store, no
    // graph for this project) just means symbol findings fall back to
    // path/global scope rather than node scope; never a hard failure.
    let graph = GraphStore::from_config(&ScribeConfig::from_env()).load(&project_id).ok().flatten();

    let embed_client = EmbedClient::from_env();
    let verdict = context.get("verdict").and_then(|v| v.as_str()).unwrap_or("UNKNOWN").to_string();
    let providers: Vec<String> = results.iter().map(|r| r.provider.clone()).collect();

    let mut created = 0u32;
    let mut recurred = 0u32;
    let mut errors = 0u32;

    for finding in &deduped {
        let (scope_kind, scope_ref) = resolve_scope(finding, graph.as_ref(), &project_id);

        // Best-effort embedding: on failure, record with no embedding so the
        // store falls back to exact-text dedup rather than losing the finding.
        let embedding = match embed_client.embed(&finding.description).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("KGFIND-03: embed failed for a finding, recording without embedding: {e}");
                None
            }
        };

        let provenance = build_finding_provenance(context, &verdict, &providers);
        let new_finding = NewFinding {
            project_id: project_id.clone(),
            category: finding.category.clone(),
            severity: finding.severity.clone(),
            scope_kind,
            scope_ref,
            description: finding.description.clone(),
            provenance,
        };

        match store.record(new_finding, embedding).await {
            Ok(RecordOutcome::Created(_)) => created += 1,
            Ok(RecordOutcome::Recurred { .. }) => recurred += 1,
            Err(e) => {
                tracing::warn!("KGFIND-03: failed to record a finding for '{project_id}': {e}");
                errors += 1;
            }
        }
    }

    json!({"recorded": true, "created": created, "recurred": recurred, "errors": errors})
}

// ── CXEG-08: Stage-5b risk-gate escalation (governance only, no new scoring) ─
//
// See the module's execute() call site for the load-bearing safety property:
// this ONLY ever widens the dispatched `providers` panel BEFORE dispatch --
// it never reads or sets `aggregate_verdict`/`complete`, so a `high`
// `cortex_review` band can never itself flip a verdict. Fail-open throughout:
// any failure to compute a risk band, or to look up a waiver, degrades to "no
// escalation" rather than blocking or altering the correctness gate.

/// Decision computed by [`maybe_escalate`] BEFORE dispatch; finalized into
/// the result's `"escalation"` block by [`finalize_escalation`] AFTER
/// dispatch (once the added provider's own outcome, if any, is known).
#[derive(Debug)]
struct EscalationDecision {
    escalated: bool,
    added_provider: Option<String>,
    band: String,
    risk_score: Option<f64>,
    waived: bool,
    waiver: Option<Value>,
    escalation_degraded: bool,
    reason: String,
}

/// CXEG-08: consult `cortex_review`'s risk band for this change and, on
/// `"high"` (and not actively waived), widen `providers` in place by
/// appending `CortexConfig::escalation_add_provider` -- never removes,
/// reorders, or otherwise touches the panel the caller already asked for.
///
/// Fail-open at every step:
/// - `escalation_enabled == false`, no `project_id`, or no derivable changed
///   files -> no escalation, `providers` untouched.
/// - `cortex_review` itself never errors (it degrades internally to
///   `configured:false`/`band:"unknown"`), so an ungraphed project simply
///   reads as `band != "high"` here -- no escalation, `providers` untouched.
///   This is exactly the "cortex_review unavailable -> gate proceeds on the
///   correctness verdict alone" contract: nothing downstream ever blocks on
///   it.
/// - An active waiver for `HIGH_RISK_BAND_RULE`/this change's scope
///   suppresses escalation; an expired one does not.
/// - A waiver LOOKUP failure (store unconfigured/unreachable) is treated as
///   "no active waiver found" -- logged, never propagated as an error.
/// - `Structure::AdversarialPair`'s panel is fixed at exactly 2 providers
///   (`defend`/`attack`); widening it would misassign roles, so escalation
///   is recorded (`escalated:true`) but `providers` is left untouched, with
///   `escalation_degraded:true`.
/// - An invalid `escalation_add_provider` (not in `ALLOWED_PROVIDERS`), or a
///   panel already at `MAX_PROVIDERS` that doesn't already include the
///   configured add-provider, degrades the same way -- `escalated:true`,
///   `escalation_degraded:true`, base panel proceeds untouched. Escalation
///   can never deadlock dispatch.
async fn maybe_escalate(
    structure: Structure,
    context: &Value,
    providers: &mut Vec<String>,
    cortex_config: &CortexConfig,
) -> EscalationDecision {
    let no_escalation = |band: &str, risk_score: Option<f64>, reason: &str| EscalationDecision {
        escalated: false,
        added_provider: None,
        band: band.to_string(),
        risk_score,
        waived: false,
        waiver: None,
        escalation_degraded: false,
        reason: reason.to_string(),
    };

    if !cortex_config.escalation_enabled {
        return no_escalation("unknown", None, "disabled");
    }

    let Some(project_id) = context.get("project_id").and_then(Value::as_str).map(str::to_string) else {
        return no_escalation("unknown", None, "no_project_id");
    };

    let changed_files = kg_context::derive_changed_files(context);
    if changed_files.is_empty() {
        return no_escalation("unknown", None, "no_changed_files");
    }

    let review = cortex_compute_review(&project_id, &changed_files, cortex_config, false).await;
    let band = review.get("band").and_then(Value::as_str).unwrap_or("unknown").to_string();
    let risk_score = review.get("risk_score").and_then(Value::as_f64);

    if band != "high" {
        return no_escalation(&band, risk_score, "band_not_high");
    }

    let requested_scope = changed_files.join(",");
    let active = match waiver::active_waiver(&project_id, waiver::HIGH_RISK_BAND_RULE, &requested_scope).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                "CXEG-08: waiver lookup failed for '{project_id}' ({e}) -- treating as no active \
                 waiver (fail-open on the waiver lookup itself; the escalation/waiver layer never \
                 blocks the correctness gate either way)"
            );
            None
        }
    };

    if let Some(w) = active {
        return EscalationDecision {
            escalated: false,
            added_provider: None,
            band,
            risk_score,
            waived: true,
            waiver: Some(w.to_json()),
            escalation_degraded: false,
            reason: "active_waiver_suppressed_escalation".to_string(),
        };
    }

    if structure == Structure::AdversarialPair {
        return EscalationDecision {
            escalated: true,
            added_provider: None,
            band,
            risk_score,
            waived: false,
            waiver: None,
            escalation_degraded: true,
            reason: "adversarial_pair panel is fixed at 2 providers (defend/attack); cannot widen".to_string(),
        };
    }

    let add_provider = cortex_config.escalation_add_provider.clone();
    if !ALLOWED_PROVIDERS.contains(&add_provider.as_str()) {
        return EscalationDecision {
            escalated: true,
            added_provider: None,
            band,
            risk_score,
            waived: false,
            waiver: None,
            escalation_degraded: true,
            reason: format!("configured escalation_add_provider '{add_provider}' is not a valid provider"),
        };
    }

    let already_present = providers.contains(&add_provider);
    if !already_present && providers.len() >= MAX_PROVIDERS {
        return EscalationDecision {
            escalated: true,
            added_provider: None,
            band,
            risk_score,
            waived: false,
            waiver: None,
            escalation_degraded: true,
            reason: "panel already at MAX_PROVIDERS; could not widen".to_string(),
        };
    }

    if !already_present {
        providers.push(add_provider.clone());
    }

    EscalationDecision {
        escalated: true,
        added_provider: Some(add_provider),
        band,
        risk_score,
        waived: false,
        waiver: None,
        escalation_degraded: false,
        reason: if already_present {
            "high band; configured add-provider was already in the panel".to_string()
        } else {
            "high band; panel widened by one provider".to_string()
        },
    }
}

/// Fold in whether the escalation's `added_provider` (if any) actually came
/// back degraded (an `error` on its `ProviderResult`) once dispatch has run.
/// Never touches `aggregate_verdict`/`complete` -- purely descriptive.
fn finalize_escalation(decision: EscalationDecision, results: &[ProviderResult]) -> Value {
    let mut escalation_degraded = decision.escalation_degraded;
    if let Some(provider) = &decision.added_provider {
        if results.iter().find(|r| &r.provider == provider).map(|r| r.error.is_some()).unwrap_or(true) {
            escalation_degraded = true;
        }
    }

    let mut out = json!({
        "escalated": decision.escalated,
        "band": decision.band,
        "risk_score": decision.risk_score,
        "waived": decision.waived,
        "escalation_degraded": escalation_degraded,
        "reason": decision.reason,
        "advisory_only": true,
    });
    if let Some(w) = decision.waiver {
        out["waiver"] = w;
    }
    if let Some(provider) = decision.added_provider {
        out["added_provider"] = json!(provider);
    }
    out
}

pub struct ReviewRun {
    // CXEG-07: shared across calls so the Tier-C consistency lens's
    // exemplar profiles benefit from `HouseStyleCache`'s own
    // generation-keyed memoization (see `cortex::house_style`'s module
    // doc) instead of recomputing from scratch on every `review_run` call.
    house_style_cache: Arc<HouseStyleCache>,
}

impl ReviewRun {
    pub fn new() -> Self {
        Self { house_style_cache: Arc::new(HouseStyleCache::new()) }
    }
}

impl Default for ReviewRun {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse + validate `review_run`'s input schema. Returns the structure, the
/// (order-preserving) provider list, criteria, and context object.
fn parse_input(args: &Value) -> Result<(Structure, Vec<String>, String, Value), ToolError> {
    let structure_str = args["structure"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArgument("'structure' is required".into()))?;
    let structure = Structure::parse(structure_str).ok_or_else(|| {
        ToolError::InvalidArgument(format!(
            "'structure' must be one of single|adversarial_pair|panel_majority|panel_unanimous|epic, got '{structure_str}'"
        ))
    })?;

    let providers_val = args["providers"]
        .as_array()
        .ok_or_else(|| ToolError::InvalidArgument("'providers' must be a non-empty array".into()))?;
    let provider_cap = max_providers_for(structure);
    if providers_val.is_empty() || providers_val.len() > provider_cap {
        return Err(ToolError::InvalidArgument(format!(
            "'providers' must have between 1 and {provider_cap} entries, got {}",
            providers_val.len()
        )));
    }
    let mut providers = Vec::with_capacity(providers_val.len());
    for p in providers_val {
        let name = p
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("each entry in 'providers' must be a string".into()))?;
        if !ALLOWED_PROVIDERS.contains(&name) {
            return Err(ToolError::InvalidArgument(format!(
                "unknown provider '{name}', must be one of {ALLOWED_PROVIDERS:?}"
            )));
        }
        providers.push(name.to_string());
    }
    if structure == Structure::AdversarialPair && providers.len() != 2 {
        return Err(ToolError::InvalidArgument(
            "'adversarial_pair' requires exactly 2 providers (defend, attack)".into(),
        ));
    }

    let criteria = args["criteria"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArgument("'criteria' is required".into()))?
        .to_string();

    let context = args.get("context").cloned().unwrap_or_else(|| json!({}));

    Ok((structure, providers, criteria, context))
}

/// Whether the KGREV-02 graph rebuild should fire. The Atlas graph is refreshed
/// on every completed review pass (APPROVE — the review-time complement to the
/// Stage 7c merge-time `scribe_kg_build`) AND at the end of every completed Epic
/// capstone audit (advisory — fires regardless of the audit's verdict). Pure/testable.
fn should_run_kg_rebuild(structure: Structure, aggregate_verdict: &str, complete: bool) -> bool {
    complete && (structure == Structure::Epic || aggregate_verdict == "APPROVE")
}

/// Whether the KGREV-03 doc engine (`docgen_run`) should fire. `docgen_run` is
/// TOKEN-INTENSIVE, so it runs ONLY after a **PASSING Epic capstone** — a completed
/// epic audit whose verdict is APPROVE (the whole build audited clean). It NEVER
/// runs on a per-item/per-merge review, and NEVER on an epic that surfaced findings
/// (REQUEST_CHANGES). Pure/testable.
fn should_run_docgen(structure: Structure, aggregate_verdict: &str, complete: bool) -> bool {
    complete && structure == Structure::Epic && aggregate_verdict == "APPROVE"
}

fn role_for(structure: Structure, index: usize) -> Role {
    match structure {
        Structure::AdversarialPair => {
            if index == 0 {
                Role::Defend
            } else {
                Role::Attack
            }
        }
        // Every provider in an Epic capstone is a whole-repo auditor.
        Structure::Epic => Role::Auditor,
        _ => Role::Reviewer,
    }
}

/// REVCAP-01 PART B: whether `provider` is currently STANDING IN for a down
/// frontier reviewer -- i.e. `provider` is itself an up FRONTIER provider
/// (opus/codex/agy) while at least one OTHER frontier provider is down
/// (Shelved, or Cooldown not yet elapsed per [`capacity::ReviewerStatus::is_down`]).
/// Reuses the exact "an up frontier alternate exists" definition PART A's own
/// `capacity_substitutions` annotation (in `execute()`, above) already
/// established for this codebase, rather than inventing a second notion of
/// "substitute". A non-frontier provider (nemotron/qwen_coder/free/gpt56) is
/// NEVER intensive-eligible -- only opus/codex/agy can stand in for each other.
/// `provider` itself being down does NOT count (that's just the requested,
/// degrading provider -- not a substitute for anything). Backward compatible:
/// on an empty/healthy registry (nothing has ever recorded a cap) this is
/// `false` for every provider, so a normal panel's dispatch opts are completely
/// unaffected -- byte-for-byte the pre-PART-B behavior.
fn is_intensive_substitute(provider: &str, now: std::time::SystemTime) -> bool {
    if !capacity::FRONTIER.contains(&provider) {
        return false;
    }
    let reg = capacity::registry();
    if reg.is_down(provider, now) {
        return false;
    }
    capacity::FRONTIER.iter().any(|other| *other != provider && reg.is_down(other, now))
}

/// Route `provider` to the right transport (daemon / free-pool / direct
/// OpenRouter) and return its raw reply text, or a human-readable
/// `"unavailable: ..."` degrade reason. Single source (S9) for that routing
/// table: [`run_one_provider`] (the correctness panel) and CXEG-07's
/// `consistency::maybe_run` (the Tier-C lens's dedicated pinned-provider
/// dispatch) both call this rather than each re-deriving which transport a
/// provider name maps to.
async fn dispatch_provider_raw(
    cfg: &ReviewConfig,
    provider: &str,
    prompt_text: &str,
    daemon_opts: &dispatch::DaemonOpts,
) -> Result<String, String> {
    if dispatch::is_daemon_provider(provider) {
        cfg.dispatch_daemon(provider, prompt_text, daemon_opts).await
    } else if provider == "free" {
        // Seamless free-tier: round-robin the daily-curated free-model pool
        // with 429 failover (see free_pool). Used as the tail of a 3-5 provider
        // panel, after the sub/OAuth providers.
        cfg.dispatch_free_pool(prompt_text).await
    } else if provider == "diffusion" {
        // TERM-DIFF-01: LOCAL, offline, zero-cost lens -- routed to Chord's
        // DiffusionGemma serve, not OpenRouter/daemon, so it gets its own arm
        // ahead of the `openrouter_model_for` check rather than being folded
        // into that table.
        cfg.dispatch_diffusion(prompt_text).await
    } else if let Some(model) = dispatch::openrouter_model_for(provider) {
        // Credit guard: refuse a PAID model (e.g. gpt56 = gpt-5.6-luna) when the
        // OpenRouter balance is below the floor, so a paid lens can never bottom
        // out the account. Free models pass through untouched.
        cfg.guard_paid_model(model).await?;
        cfg.dispatch_openrouter(model, prompt_text).await
    } else {
        // Unreachable given parse_input's validation for the correctness
        // panel, but fail safe rather than panic if it ever were -- and a
        // real, reachable path for the Tier-C lens, whose provider name is
        // NOT validated against `ALLOWED_PROVIDERS` (see
        // `consistency::ConsistencyReviewConfig::from_env`).
        Err(format!("unavailable: unknown provider '{provider}'"))
    }
}

async fn run_one_provider(
    cfg: ReviewConfig,
    provider: String,
    prompt_text: String,
    daemon_opts: dispatch::DaemonOpts,
) -> ProviderResult {
    let raw = dispatch_provider_raw(&cfg, &provider, &prompt_text, &daemon_opts).await;

    match raw {
        Ok(text) => {
            // REVCAP-01: a genuine reply demonstrates the provider is up --
            // self-populate the capacity registry at zero extra dispatch cost
            // (clears any stale cap and resets the consecutive-cap/backoff
            // counters). See `record_dispatch_outcome`'s doc for why this
            // lives in one place shared with the error arm below.
            record_dispatch_outcome(&provider, &Ok(()));
            let (verdict, reasoning) = parse_verdict(&text);
            let findings = parse_findings(&text);
            ProviderResult {
                provider,
                verdict: verdict.as_str().to_string(),
                reasoning,
                error: None,
                findings,
            }
        }
        Err(reason) => {
            record_dispatch_outcome(&provider, &Err(reason.clone()));
            ProviderResult {
                provider,
                verdict: "UNKNOWN".to_string(),
                reasoning: String::new(),
                error: Some(reason),
                findings: Vec::new(),
            }
        }
    }
}

/// REVCAP-01: record a real dispatch outcome into the process-global
/// [`capacity::registry`] -- the exact hook that makes the registry
/// self-populating at zero extra provider usage (no separate probe dispatch
/// needed for `review_run`'s own callers; only `review_provider_status`'s
/// opt-in `probe:true` spends an extra dispatch). Classifies an error into
/// rate-limit (two-tier cooldown/shelve) vs. timeout (latency, never a cap)
/// vs. any other dispatch error (tracked as a diagnostic, still available).
/// A success clears any cap state outright.
fn record_dispatch_outcome(provider: &str, outcome: &Result<(), String>) {
    let now = std::time::SystemTime::now();
    match outcome {
        Ok(()) => capacity::registry().update(provider, |s| s.mark_success()),
        Err(reason) => {
            if capacity::is_rate_limit_error(reason) {
                let horizon = capacity::parse_horizon_secs(reason);
                capacity::registry()
                    .update(provider, |s| s.mark_rate_limited(horizon, now, reason.clone()));
            } else if capacity::is_timeout_error(reason) {
                capacity::registry().update(provider, |s| s.mark_latency(reason.clone()));
            } else {
                capacity::registry().update(provider, |s| s.mark_error(reason.clone()));
            }
        }
    }
}

#[async_trait]
impl RustTool for ReviewRun {
    fn name(&self) -> &str {
        "review_run"
    }

    fn description(&self) -> &str {
        "Run a multi-provider code/change review. 'structure' is one of single, \
adversarial_pair, panel_majority, panel_unanimous, epic. 'epic' is the whole-repo \
Epic Review capstone (run ONCE at the end of a build): every provider audits the \
entire codebase against the contracts and emits findings — ADVISORY (never gates a \
merge), and a COMPLETED epic audit drives the KG refresh + doc engine (docgen) at \
its end regardless of findings. 'providers' (1-5) picks from \
opus, codex, agy (CLI-backed via review-daemon), nemotron, qwen_coder (OpenRouter, \
frontier-class free-tier models), diffusion (LOCAL, offline, zero-cost -- routed \
through Chord's DiffusionGemma model, no external API/credits involved). \
'criteria' is the acceptance criteria text; \
'context' is a free-form JSON object (diff/files/description). Providers are \
dispatched concurrently; a single provider's failure degrades that entry rather \
than failing the whole call."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "structure": {
                    "type": "string",
                    "enum": ["single", "adversarial_pair", "panel_majority", "panel_unanimous", "epic"]
                },
                "providers": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_PROVIDERS_EPIC,
                    "items": {
                        "type": "string",
                        "enum": ALLOWED_PROVIDERS
                    }
                },
                "criteria": {
                    "type": "string",
                    "description": "Free-text acceptance criteria the change must satisfy."
                },
                "context": {
                    "type": "object",
                    "description": "Free-form context (diff/files/description/etc)."
                }
            },
            "required": ["structure", "providers", "criteria"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let (structure, mut providers, criteria, mut context) = parse_input(&args)?;

        // KGREV-02: a project with an in-flight incremental KG rebuild (from
        // a just-approved prior review) short-circuits here -- a re-review
        // must never reference a mid-rebuild graph. Dispatches no providers.
        if let Some(project_id) = context.get("project_id").and_then(|v| v.as_str()) {
            let locked = in_flight().lock().unwrap_or_else(|e| e.into_inner()).contains(project_id);
            if locked {
                return Ok(json!({
                    "structure": args["structure"],
                    "providers": [],
                    "aggregate_verdict": "UNKNOWN",
                    "complete": false,
                    "locked": true,
                    "reason": format!("KG rebuild in progress for {project_id}; retry when ready"),
                })
                .to_string());
            }
        }

        // KGREV-01: best-effort, backward-compatible KG grounding -- a no-op
        // unless `context.project_id` is present AND a matching graph exists.
        kg_context::inject(&mut context);
        // KGRULE-04: best-effort, backward-compatible active-rules injection
        // -- closes the loop between rule crystallization/promotion
        // (KGRULE-01..03) and enforcement by surfacing the rules the system
        // has learned to every reviewer. A no-op (context byte-for-byte
        // unchanged) when the rules store is unconfigured, matching
        // `kg_context::inject`'s own degrade contract. Must run here (in the
        // async `execute()` body) rather than inside the sync `inject()`
        // above, since `RulesStore` is sqlx-backed and awaits its queries.
        kg_context::inject_active_rules(&mut context).await;
        let cfg = ReviewConfig::from_env();
        // CXEG-04's config is reused here (Stage-5b escalation) AND further
        // below (CXEG-07's consistency lens) -- computed once, not twice.
        let cortex_config = CortexConfig::from_env();

        // CXEG-08: Stage-5b risk-gate escalation. Runs BEFORE dispatch (not
        // after aggregate(), unlike CXEG-07's consistency lens) because its
        // ONLY effect is widening the `providers` panel that gets dispatched
        // below -- it never reads or mutates `aggregate_verdict`/`complete`
        // itself, so risk structurally cannot flip the verdict: the extra
        // reviewer's own correctness opinion is what (if anything) moves the
        // outcome, exactly like any other panel member. Fail-open throughout
        // (see `maybe_escalate`'s doc): any failure to compute a risk band or
        // waiver just means no escalation, never a blocked/degraded panel.
        let escalation_decision = maybe_escalate(structure, &context, &mut providers, &cortex_config).await;

        // REVCAP-01 PART A: capacity gate. Consult the self-populating
        // capacity registry (fed by `record_dispatch_outcome` on every past
        // real dispatch) BEFORE spending a dispatch on this call. If >= 2 of
        // the FRONTIER providers (codex/agy/opus) are currently down
        // (Shelved, or Cooldown not yet elapsed), a panel must not silently
        // return a degraded verdict -- return a distinct, structurally
        // detectable PAUSE outcome instead (`aggregate_verdict:
        // "CAPACITY_PAUSED"` + `capacity_paused: true`), dispatching NO
        // providers. Backward-compat: an empty/fresh registry (nothing ever
        // recorded a cap) always reads as "0 down" here, so this is a total
        // no-op until the registry has real data -- identical to today's
        // behavior.
        let now = std::time::SystemTime::now();
        let (paused, down_providers) =
            capacity::frontier_capacity_paused(capacity::registry(), now);
        if paused {
            let earliest = capacity::earliest_recovery(capacity::registry(), &down_providers)
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64());
            return Ok(json!({
                "structure": args["structure"],
                "providers": [],
                "aggregate_verdict": "CAPACITY_PAUSED",
                "complete": false,
                "capacity_paused": true,
                "down_providers": down_providers,
                "earliest_recovery_unix": earliest,
                "reason": "insufficient adversarial capacity: 2 or more frontier reviewer \
                           providers (codex/agy/opus) are currently capped; refusing to return \
                           a degraded verdict",
            })
            .to_string());
        }
        // Fewer than 2 frontier providers down: proceed, but ANNOTATE (never
        // alter) when a requested frontier provider is itself down and an
        // available frontier substitute exists -- the actual intensity
        // escalation (swapping dispatch params) is PART B; here we only
        // record the substitution note for the caller's visibility.
        let capacity_substitutions: Vec<Value> = providers
            .iter()
            .filter(|p| {
                capacity::FRONTIER.contains(&p.as_str()) && capacity::registry().is_down(p, now)
            })
            .filter_map(|down_provider| {
                capacity::FRONTIER
                    .iter()
                    .find(|alt| {
                        !providers.contains(&alt.to_string())
                            && !capacity::registry().is_down(alt, now)
                    })
                    .map(|alt| {
                        json!({
                            "requested": down_provider,
                            "available_substitute": alt,
                            "applied": false,
                        })
                    })
            })
            .collect();

        let mut set = tokio::task::JoinSet::new();
        // Tracks each spawned task's tokio::task::Id back to its (index,
        // provider name), so a task panic (JoinError) can still be attributed
        // to the RIGHT slot instead of a fabricated trailing index -- which
        // matters for adversarial_pair, where index 0/1 = defend/attack.
        let mut id_to_slot: std::collections::HashMap<tokio::task::Id, (usize, String)> =
            std::collections::HashMap::with_capacity(providers.len());
        // Epic capstone auditors run in EXPLORE mode (read-only tools + repo cwd)
        // with progress/stall detection; every other structure keeps the routine
        // (tools-off, fixed-budget) daemon behavior unchanged.
        let daemon_opts = if structure == Structure::Epic {
            let repo_path = context
                .get("repo_path")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            dispatch::DaemonOpts::epic(repo_path)
        } else {
            dispatch::DaemonOpts::routine()
        };
        for (idx, provider) in providers.iter().enumerate() {
            let mut role = role_for(structure, idx);
            // REVCAP-01 PART B: a provider standing in for a currently-DOWN
            // frontier reviewer gets the INTENSIVE preset (raised effort + a
            // longer wall-clock budget -- the proven fix for the 120s-timeout
            // failure of a genuinely deep review) and the harder,
            // refutation-first `IntensiveReviewer` role, instead of the
            // structure's normal opts/role. Deliberately scoped to the plain
            // panel roles (`Structure::Single`/`PanelMajority`/`PanelUnanimous`,
            // where `role_for` returns `Role::Reviewer`) -- `AdversarialPair`'s
            // defend/attack verdict sentinels (`APPROVE`/`REQUEST_CHANGES` vs.
            // `REFUTED`/`NOT_REFUTED`) are load-bearing for
            // `aggregate_adversarial_pair`, and the Epic capstone already runs
            // every provider in its own explore+long-budget `epic()` preset --
            // overriding either would change verdict semantics or double-apply
            // intensity, not "review harder". Any panel where no frontier is
            // down (the common case, and every pre-PART-B call site) is
            // completely unaffected: `is_intensive_substitute` is `false` for
            // every provider, so this is a no-op there.
            let opts = if role == Role::Reviewer && is_intensive_substitute(provider, now) {
                role = Role::IntensiveReviewer;
                dispatch::DaemonOpts::intensive()
            } else {
                daemon_opts.clone()
            };
            let prompt_text = build_prompt(role, &criteria, &context);
            let cfg = cfg.clone();
            let provider = provider.clone();
            let provider_for_map = provider.clone();
            let handle = set.spawn(async move {
                let result = run_one_provider(cfg, provider, prompt_text, opts).await;
                (idx, result)
            });
            id_to_slot.insert(handle.id(), (idx, provider_for_map));
        }

        let mut indexed: Vec<(usize, ProviderResult)> = Vec::with_capacity(providers.len());
        while let Some(joined) = set.join_next_with_id().await {
            match joined {
                Ok((_id, pair)) => indexed.push(pair),
                Err(join_err) => {
                    // A spawned task panicking is not expected, but must not
                    // take the whole tool call down -- degrade instead, at
                    // the correct (idx, provider) slot.
                    let (idx, provider) = id_to_slot
                        .get(&join_err.id())
                        .cloned()
                        .unwrap_or((indexed.len(), "unknown".to_string()));
                    indexed.push((
                        idx,
                        ProviderResult {
                            provider,
                            verdict: "UNKNOWN".to_string(),
                            reasoning: String::new(),
                            error: Some(format!("unavailable: task join error: {join_err}")),
                            findings: Vec::new(),
                        },
                    ));
                }
            }
        }
        indexed.sort_by_key(|(idx, _)| *idx);
        let results: Vec<ProviderResult> = indexed.into_iter().map(|(_, r)| r).collect();

        let (aggregate_verdict, complete) = aggregate(structure, &results);

        // CXEG-07: Tier-C consistency/elegance lens. Runs strictly AFTER the
        // line above -- `aggregate_verdict`/`complete` are already fixed and
        // nothing below can reach back and change them (the load-bearing
        // safety property; see `consistency`'s module doc). Advisory-only,
        // never an `Err`: disabled/unconfigured/degraded all resolve to an
        // empty, labeled `ConsistencyRun` rather than affecting this call.
        // `cortex_config` was already computed above (before dispatch) for
        // CXEG-08's escalation decision; reused here rather than recomputed.
        let consistency_run =
            consistency::maybe_run(&context, &criteria, &results, &cfg, &cortex_config, &self.house_style_cache).await;

        // The post-review hooks (KG refresh + doc engine) fire on a completed
        // APPROVE pass — OR, for the Epic capstone, on ANY completed audit. The
        // capstone is advisory: it surfaces findings but never "fails", so its END
        // (a completed whole-repo audit) is what drives the graph refresh and the
        // doc engine — the operator-visible "capstone → doc engine" hook — even
        // when the audit reports work-items. A non-epic review keeps the strict
        // APPROVE gate.
        // KGREV-02: incrementally rebuild the project's KG on a completed pass
        // (or a completed epic audit) — best-effort, never fails the review; see
        // `maybe_rebuild` for the lock semantics.
        let kg_rebuild =
            maybe_rebuild(should_run_kg_rebuild(structure, &aggregate_verdict, complete), &context)
                .await;

        // KGREV-03: the doc engine is TOKEN-INTENSIVE, so it fires ONLY after a
        // PASSING Epic capstone (a completed epic audit that verdicts APPROVE),
        // never per-merge — sequenced after the KG rebuild so docs see the fresh
        // graph. Best-effort through the sanctioned `docgen_run` door (S9).
        let scribe_docs =
            maybe_scribe_docs(should_run_docgen(structure, &aggregate_verdict, complete), &context)
                .await;

        // KGFIND-03: best-effort, capture-only recording of structured
        // findings onto the Atlas KG findings store. Unlike the two hooks
        // above, this fires on ANY verdict (not just APPROVE) -- findings
        // matter most on REQUEST_CHANGES. Stamp the verdict into a context
        // copy so `maybe_record_findings` can attach it to provenance without
        // widening its `(results, context)` signature. Never affects the
        // verdict or errors the call.
        let mut findings_context = context.clone();
        if let Value::Object(map) = &mut findings_context {
            map.insert("verdict".to_string(), json!(aggregate_verdict));
        }

        // CXEG-07: fold the consistency lens's (subjective-flagged,
        // cross-source-merged) findings through the SAME KGFIND-03 record
        // path as the correctness panel -- no second findings-access path
        // (S9). Placed FIRST in `results_for_findings` so
        // `dedup_across_providers`'s (category, file, symbol) first-wins
        // collapse keeps the richer, disagreement-flagged entry over a
        // correctness reviewer's own plain duplicate tag of the same anchor,
        // rather than silently losing the `subjective` flag to dedup order.
        let mut results_for_findings: Vec<ProviderResult> = Vec::with_capacity(results.len() + 1);
        if !consistency_run.findings.is_empty() {
            results_for_findings.push(ProviderResult {
                provider: "consistency_lens".to_string(),
                verdict: "ADVISORY".to_string(),
                reasoning: String::new(),
                error: None,
                findings: consistency_run
                    .findings
                    .iter()
                    .cloned()
                    .map(|cf| Finding { subjective: Some(cf.subjective), ..cf.finding })
                    .collect(),
            });
        }
        results_for_findings.extend(results.iter().cloned());
        let findings_recorded = maybe_record_findings(&results_for_findings, &findings_context).await;

        // CXEG-08: finalize the escalation block now that dispatch has
        // actually happened -- `escalation_decision` was computed BEFORE
        // dispatch (it only widens `providers`); this step just folds in
        // whether the ADDED provider itself came back degraded, so
        // `escalation_degraded:true` reflects reality without ever touching
        // `aggregate_verdict`/`complete` above.
        let escalation = finalize_escalation(escalation_decision, &results);

        Ok(json!({
            "structure": args["structure"],
            "providers": results,
            "aggregate_verdict": aggregate_verdict,
            "complete": complete,
            "kg_rebuild": kg_rebuild,
            "scribe_docs": scribe_docs,
            "findings_recorded": findings_recorded,
            "escalation": escalation,
            "capacity_substitutions": capacity_substitutions,
            "consistency": {
                "status": consistency_run.status,
                "provider": consistency_run.provider,
                "degraded": consistency_run.degraded,
                "advisory_only": true,
                "findings_count": consistency_run.findings.len(),
                "subjective_count": consistency_run.findings.iter().filter(|f| f.subjective).count(),
            },
        })
        .to_string())
    }
}

/// `openrouter_credits` — the OpenRouter spend/credits tracker. Reports the
/// remaining balance, total granted, and total usage (USD) from OpenRouter's cost
/// API, plus the paid-dispatch floor below which the credit guard refuses a paid
/// model. Lets an operator SEE spend before/after a paid capstone lens (gpt56) runs,
/// and is the same signal the pre-flight guard uses so a paid review never bottoms
/// out the account.
struct OpenRouterCredits;

#[async_trait::async_trait]
impl RustTool for OpenRouterCredits {
    fn name(&self) -> &str {
        "openrouter_credits"
    }

    fn description(&self) -> &str {
        "Report OpenRouter spend + remaining credits (USD) from the cost API: \
         remaining, total_granted, total_usage, and the paid-dispatch floor below \
         which review_run refuses a paid model (e.g. gpt56 = gpt-5.6-luna) to avoid \
         bottoming out the account. Takes no arguments."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, _args: Value) -> Result<ToolOutput, ToolError> {
        let cfg = ReviewConfig::from_env();
        match cfg.openrouter_credits().await {
            Ok((remaining, granted, usage)) => {
                let floor = dispatch::min_openrouter_credits();
                let structured = json!({
                    "remaining_usd": remaining,
                    "total_granted_usd": granted,
                    "total_usage_usd": usage,
                    "paid_dispatch_floor_usd": floor,
                    "below_floor": remaining < floor,
                });
                let text = format!(
                    "OpenRouter credits: ${remaining:.4} remaining (granted ${granted:.4}, used ${usage:.4}); \
                     paid-dispatch floor ${floor:.2}{}",
                    if remaining < floor { " — BELOW FLOOR: paid models (gpt56) are refused" } else { "" }
                );
                Ok(ToolOutput::with_structured(text, structured))
            }
            Err(e) => Ok(ToolOutput::text_only(format!("openrouter_credits unavailable: {e}"))),
        }
    }
}

/// The providers `review_provider_status` reports on -- every provider name
/// `review_run` itself accepts EXCEPT the two capstone-only lenses
/// (`claude-fable-5`, `gpt56`), which aren't part of the routine
/// panel/capacity-gate story this tool exists for. `diffusion` (local,
/// zero-cost, Chord-routed) IS part of the routine panel story, so it's
/// included here like the other non-capstone-only providers.
const STATUS_PROVIDERS: &[&str] =
    &["opus", "codex", "agy", "free", "nemotron", "qwen_coder", "diffusion"];

/// `review_provider_status` — REVCAP-01 PART A: report the reviewer-provider
/// capacity registry's current state per provider (cached, zero extra
/// dispatch cost by default), or optionally refresh it with a minimal probe
/// dispatch per provider (`probe: true`).
struct ReviewProviderStatus;

impl ReviewProviderStatus {
    /// Minimal probe dispatch: a trivial prompt, routine daemon opts, routed
    /// through the SAME `dispatch_provider_raw` transport table `review_run`
    /// itself uses (S9: one dispatch routing table, not a second one for
    /// probing) -- its outcome is folded into the registry via the same
    /// `record_dispatch_outcome` hook a real review uses.
    async fn probe_one(cfg: &ReviewConfig, provider: &str) {
        let daemon_opts = dispatch::DaemonOpts::routine();
        let raw = dispatch_provider_raw(cfg, provider, "ping", &daemon_opts).await;
        record_dispatch_outcome(provider, &raw.map(|_| ()));
    }
}

#[async_trait::async_trait]
impl RustTool for ReviewProviderStatus {
    fn name(&self) -> &str {
        "review_provider_status"
    }

    fn description(&self) -> &str {
        "Report each review provider's (opus, codex, agy, free, nemotron, qwen_coder) \
         current capacity-registry state: available/cooldown/shelved/latency/error, the \
         absolute capped_until time, the reset horizon, provenance, and consecutive-cap \
         count. Default returns the CACHED registry state (no extra provider dispatch). \
         Pass 'probe: true' to refresh every provider with one minimal dispatch first."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "probe": {
                    "type": "boolean",
                    "description": "If true, dispatch one minimal probe per provider to refresh \
                                     the registry before reporting (spends real provider usage)."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let probe = args.get("probe").and_then(Value::as_bool).unwrap_or(false);
        if probe {
            let cfg = ReviewConfig::from_env();
            for provider in STATUS_PROVIDERS {
                Self::probe_one(&cfg, provider).await;
            }
        }

        let now = std::time::SystemTime::now();
        let providers: Vec<Value> = STATUS_PROVIDERS
            .iter()
            .map(|name| {
                let s = capacity::registry().get(name);
                let capped_until = s
                    .cooldown_until
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64());
                json!({
                    "provider": s.name,
                    "status": s.last_status,
                    "available": s.available,
                    "capped_until": capped_until,
                    "horizon_secs": s.last_horizon_secs,
                    "consecutive_caps": s.consecutive_caps,
                    "backoff_secs": s.backoff_secs,
                    "source": s.source,
                })
            })
            .collect();
        let (paused, down_providers) =
            capacity::frontier_capacity_paused(capacity::registry(), now);

        let structured = json!({
            "probed": probe,
            "providers": providers,
            "frontier_capacity_paused": paused,
            "frontier_down": down_providers,
        });
        Ok(ToolOutput::with_structured(structured.to_string(), structured))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry
        .register(Box::new(ReviewRun::new()))
        .expect("review_run must register cleanly");
    registry
        .register(Box::new(OpenRouterCredits))
        .expect("openrouter_credits must register cleanly");
    registry
        .register(Box::new(ReviewProviderStatus))
        .expect("review_provider_status must register cleanly");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ReviewRun {
        ReviewRun::new()
    }

    #[test]
    fn epic_gets_a_wider_provider_cap() {
        // The routine panel stays capped at 5; the once-per-build capstone may run
        // the full royal panel (opus+codex+agy+free+claude-fable-5+gpt56).
        assert_eq!(max_providers_for(Structure::PanelUnanimous), MAX_PROVIDERS);
        assert_eq!(max_providers_for(Structure::Single), MAX_PROVIDERS);
        assert_eq!(max_providers_for(Structure::Epic), MAX_PROVIDERS_EPIC);
        assert!(MAX_PROVIDERS_EPIC >= 6);
    }

    #[test]
    fn epic_structure_maps_every_provider_to_auditor() {
        assert_eq!(role_for(Structure::Epic, 0), Role::Auditor);
        assert_eq!(role_for(Structure::Epic, 3), Role::Auditor);
        // non-epic panels stay plain reviewers
        assert_eq!(role_for(Structure::PanelUnanimous, 0), Role::Reviewer);
    }

    #[test]
    fn kg_rebuild_fires_on_completed_pass_or_epic_audit() {
        // KG refresh: on a completed APPROVE review (review-time complement to the
        // merge-time refresh) OR on ANY completed epic audit (advisory).
        assert!(should_run_kg_rebuild(Structure::PanelUnanimous, "APPROVE", true));
        assert!(!should_run_kg_rebuild(Structure::PanelUnanimous, "REQUEST_CHANGES", true));
        assert!(!should_run_kg_rebuild(Structure::PanelUnanimous, "APPROVE", false));
        assert!(should_run_kg_rebuild(Structure::Epic, "REQUEST_CHANGES", true));
        assert!(should_run_kg_rebuild(Structure::Epic, "APPROVE", true));
        assert!(!should_run_kg_rebuild(Structure::Epic, "REQUEST_CHANGES", false));
    }

    #[test]
    fn docgen_fires_only_after_a_passing_epic_capstone() {
        // Token-intensive: ONLY a completed epic audit that verdicts APPROVE.
        assert!(should_run_docgen(Structure::Epic, "APPROVE", true));
        // NOT a failing/advisory epic (surfaced findings) ...
        assert!(!should_run_docgen(Structure::Epic, "REQUEST_CHANGES", true));
        // ... NOT an incomplete epic ...
        assert!(!should_run_docgen(Structure::Epic, "APPROVE", false));
        // ... and NEVER a per-item/per-merge review, even a clean APPROVE.
        assert!(!should_run_docgen(Structure::PanelUnanimous, "APPROVE", true));
        assert!(!should_run_docgen(Structure::Single, "APPROVE", true));
    }

    #[tokio::test]
    async fn rejects_unknown_structure() {
        let args = json!({"structure": "bogus", "providers": ["opus"], "criteria": "x"});
        let err = tool().execute(args).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn rejects_unknown_provider_name() {
        let args = json!({"structure": "single", "providers": ["gpt5"], "criteria": "x"});
        let err = tool().execute(args).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn rejects_empty_providers() {
        let args = json!({"structure": "single", "providers": [], "criteria": "x"});
        let err = tool().execute(args).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── CXEG-10: run_consistency_lens_dry wiring ────────────────────────────

    fn calibration_cortex_config(enable_tier_c: bool) -> CortexConfig {
        CortexConfig {
            risk_score_threshold: 7.0,
            enable_tier_b: false,
            enable_tier_c,
            elegance_advisory_only: true,
            dup_cosine: 0.85,
            atlas_database_url: None,
            max_blast_nodes: crate::cortex::scope::DEFAULT_MAX_BLAST_NODES,
            tier_b_percentile: 90.0,
            house_style_exemplars_k: crate::cortex::house_style::DEFAULT_EXEMPLARS_K,
            risk_weight_centrality_spike: 2.0,
            risk_weight_complexity_spike: 1.5,
            risk_weight_fan_out_explosion: 1.5,
            risk_weight_community_boundary_crossing: 2.5,
            risk_weight_semantic_duplication: 10.0,
            risk_weight_recurrence: 1.0,
            risk_band_elevated_cut: 4.0,
            audit_clone_timeout_secs: 60,
            audit_max_clone_bytes: 200_000_000,
            escalation_enabled: true,
            escalation_add_provider: "agy".to_string(),
            crystallize_min_recurrence: crate::cortex::crystallize::DEFAULT_MIN_RECURRENCE,
        }
    }

    #[tokio::test]
    async fn run_consistency_lens_dry_disabled_makes_no_network_call_and_returns_empty() {
        // enable_tier_c: false short-circuits before any provider dispatch or
        // graph load -- this is the same first check `consistency::maybe_run`
        // performs, exercised here through the calibration door instead of
        // `execute()`. A disabled run must carry zero findings.
        let ctx = json!({"project_id": "TERM", "changed_files": ["src/lib.rs"]});
        let run = run_consistency_lens_dry(
            &ctx,
            "calibration replay",
            &[],
            &ReviewConfig::from_env(),
            &calibration_cortex_config(false),
            &HouseStyleCache::new(),
        )
        .await;
        assert_eq!(run.status, "disabled");
        assert!(run.findings.is_empty());
    }

    #[tokio::test]
    async fn run_consistency_lens_dry_missing_project_id_degrades_cleanly() {
        let ctx = json!({"changed_files": ["src/lib.rs"]});
        let run = run_consistency_lens_dry(
            &ctx,
            "calibration replay",
            &[],
            &ReviewConfig::from_env(),
            &calibration_cortex_config(true),
            &HouseStyleCache::new(),
        )
        .await;
        assert_eq!(run.status, "no_project_id");
        assert!(run.findings.is_empty());
    }

    /// CXEG-10 acceptance criterion: the dry/calibration path writes NOTHING
    /// to the KGFIND `FindingsStore`. Proven two ways, both infra-free (no
    /// live DB, mirroring how every other test in this module avoids live
    /// infra):
    ///
    /// 1. **Compile-time shape proof.** `run_consistency_lens_dry` returns a
    ///    `ConsistencyRun`, and the EXHAUSTIVE destructure below (which fails
    ///    to compile if a field is ever added) shows that type carries no
    ///    store-write acknowledgment channel at all — no `recorded` marker,
    ///    no store handle, nothing a caller could mistake for "a write
    ///    happened." Contrast `execute()`'s own result, which threads
    ///    `maybe_record_findings`' `{"recorded": ...}` value into a
    ///    `"findings_recorded"` field: the dry wrapper deliberately has no
    ///    such path.
    /// 2. **Source-scan proof.** The wrapper's own body delegates solely to
    ///    `consistency::maybe_run` and never names `maybe_record_findings`
    ///    (the ONLY function in this crate that writes to `FindingsStore`)
    ///    nor `FindingsStore` itself — so no future edit can quietly add a
    ///    write without tripping this assertion. Same self-scanning posture
    ///    as `cortex_calibrate`'s `no_direct_http_client` test.
    #[tokio::test]
    async fn run_consistency_lens_dry_never_records_findings() {
        // (1) Behavioral run through the dry door. `enable_tier_c=false`
        // short-circuits before any dispatch, so this is hermetic; the point
        // here is the SHAPE of what comes back, exercised on the real return
        // value rather than asserted only in prose.
        let ctx = json!({"project_id": "TERM", "changed_files": ["src/lib.rs"]});
        let run = run_consistency_lens_dry(
            &ctx,
            "calibration replay",
            &[],
            &ReviewConfig::from_env(),
            &calibration_cortex_config(false),
            &HouseStyleCache::new(),
        )
        .await;
        // Exhaustive destructure: if a store-write/`recorded` field is ever
        // added to `ConsistencyRun`, THIS LINE stops compiling — forcing a
        // deliberate re-review of whether the dry path can still claim to be
        // write-free. There is intentionally no such field today.
        let ConsistencyRun { status, provider: _, degraded: _, findings } = run;
        assert_eq!(status, "disabled");
        assert!(findings.is_empty());

        // (2) Structural source-scan: isolate the dry wrapper's own function
        // body (from its signature up to the doc comment that precedes
        // `maybe_record_findings`) and assert it never reaches the record
        // hook or the store. `maybe_record_findings` is the single function
        // in this crate that performs a `FindingsStore` write, so a wrapper
        // that names neither cannot write.
        let src = include_str!("mod.rs");
        let start = src
            .find("pub async fn run_consistency_lens_dry(")
            .expect("dry wrapper must be defined in this file");
        // The wrapper is immediately followed by `maybe_record_findings`'s
        // doc block; slice up to that boundary so we scan ONLY the wrapper.
        let rest = &src[start..];
        let end = rest
            .find("/// KGFIND-03 post-aggregate hook")
            .expect("maybe_record_findings' doc block must follow the dry wrapper");
        let body = &rest[..end];
        assert!(
            body.contains("consistency::maybe_run"),
            "the dry wrapper must delegate to consistency::maybe_run (the one lens door)"
        );
        assert!(
            !body.contains("maybe_record_findings"),
            "the dry wrapper must NOT call the findings-record hook -- that is the whole point of dry mode"
        );
        assert!(
            !body.contains("FindingsStore"),
            "the dry wrapper must NOT touch the FindingsStore directly either"
        );
    }

    #[tokio::test]
    async fn rejects_too_many_providers() {
        let args = json!({
            "structure": "panel_majority",
            "providers": ["opus", "codex", "agy", "nemotron", "qwen_coder", "opus"],
            "criteria": "x"
        });
        let err = tool().execute(args).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn adversarial_pair_requires_exactly_two_providers() {
        let args = json!({"structure": "adversarial_pair", "providers": ["opus"], "criteria": "x"});
        let err = tool().execute(args).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn missing_criteria_is_rejected() {
        let args = json!({"structure": "single", "providers": ["opus"]});
        let err = tool().execute(args).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    /// End-to-end through `execute()` with no daemon/OpenRouter configured
    /// (env vars unset in the test process): every provider must degrade
    /// cleanly rather than the tool call erroring out, proving the
    /// error-degradation contract at the `RustTool::execute` boundary.
    #[tokio::test]
    #[serial_test::serial]
    async fn execute_degrades_all_providers_when_unconfigured_and_still_returns_ok() {
        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        std::env::remove_var("OPENROUTER_API_KEY");
        let args = json!({
            "structure": "panel_majority",
            "providers": ["opus", "nemotron"],
            "criteria": "must compile",
            "context": {"diff": "+ fn x() {}"}
        });
        let out = tool().execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["complete"], false);
        assert_eq!(parsed["providers"].as_array().unwrap().len(), 2);
        for p in parsed["providers"].as_array().unwrap() {
            assert!(p["error"].is_string(), "expected a degrade reason, got {p}");
        }
    }

    // ── KGREV-02: rebuild-on-pass hook + per-project re-review lock ────────

    fn clear_in_flight(project_id: &str) {
        in_flight().lock().unwrap_or_else(|e| e.into_inner()).remove(project_id);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn locked_project_short_circuits() {
        let project_id = "KGREV02-LOCKED";
        in_flight().lock().unwrap_or_else(|e| e.into_inner()).insert(project_id.to_string());

        let args = json!({
            "structure": "single",
            "providers": ["opus"],
            "criteria": "x",
            "context": {"project_id": project_id}
        });
        let out = tool().execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        clear_in_flight(project_id);

        assert_eq!(parsed["locked"], true, "{parsed}");
        assert_eq!(parsed["providers"].as_array().unwrap().len(), 0, "{parsed}");
        assert_eq!(parsed["aggregate_verdict"], "UNKNOWN", "{parsed}");
        assert_eq!(parsed["complete"], false, "{parsed}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn approve_triggers_rebuild_and_releases_lock() {
        let store_dir = std::env::temp_dir().join(format!("kgrev02-store-{}", std::process::id()));
        let repo_dir = std::env::temp_dir().join(format!("kgrev02-repo-{}", std::process::id()));
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("lib.rs"), "pub fn hello() {}\n").unwrap();
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        std::env::set_var("SCRIBE_ALLOWED_REPO_ROOTS", repo_dir.to_string_lossy().to_string());

        let project_id = "KGREV02-APPROVE";
        let context = json!({
            "project_id": project_id,
            "repo_path": repo_dir.to_string_lossy().to_string(),
        });
        let kg_rebuild = maybe_rebuild(true, &context).await;

        assert_eq!(kg_rebuild["ran"], true, "{kg_rebuild}");
        assert_eq!(kg_rebuild["ok"], true, "{kg_rebuild}");
        assert!(
            in_flight().lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "lock must be released after rebuild"
        );

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
        std::env::remove_var("SCRIBE_ALLOWED_REPO_ROOTS");
        let _ = std::fs::remove_dir_all(&store_dir);
        let _ = std::fs::remove_dir_all(&repo_dir);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rebuild_error_does_not_fail_review() {
        let store_dir = std::env::temp_dir().join(format!("kgrev02-errstore-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        // Empty allowed roots -> any repo_path is default-denied.
        std::env::set_var("SCRIBE_ALLOWED_REPO_ROOTS", "");

        let project_id = "KGREV02-ERROR";
        let context = json!({
            "project_id": project_id,
            "repo_path": "/nonexistent/bogus/repo/path",
        });
        let kg_rebuild = maybe_rebuild(true, &context).await;

        assert_eq!(kg_rebuild["ran"], true, "{kg_rebuild}");
        assert_eq!(kg_rebuild["ok"], false, "{kg_rebuild}");
        assert!(kg_rebuild["error"].is_string(), "{kg_rebuild}");
        assert!(
            in_flight().lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "lock must be released even on rebuild error"
        );

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
        std::env::remove_var("SCRIBE_ALLOWED_REPO_ROOTS");
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_project_id_no_lock_no_rebuild() {
        let context = json!({"diff": "+ fn x() {}"});
        let kg_rebuild = maybe_rebuild(true, &context).await;
        assert_eq!(kg_rebuild["ran"], false, "{kg_rebuild}");
        assert!(
            in_flight().lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "no lock should ever be taken without a project_id"
        );

        // Backward compatible: full execute() path with no project_id still
        // dispatches providers normally (degrading without daemon/OpenRouter
        // config) rather than short-circuiting.
        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        std::env::remove_var("OPENROUTER_API_KEY");
        let args = json!({
            "structure": "single",
            "providers": ["opus"],
            "criteria": "x",
            "context": {"diff": "+ fn x() {}"}
        });
        let out = tool().execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["locked"], Value::Null, "{parsed}");
        assert_eq!(parsed["providers"].as_array().unwrap().len(), 1, "{parsed}");
        assert_eq!(parsed["kg_rebuild"]["ran"], false, "{parsed}");
    }

    // -- KGREV-03: scribe_docs wiring ----------------------------------------

    #[tokio::test]
    #[serial_test::serial]
    async fn approve_with_doc_params_runs_docgen() {
        // No project_config -> docgen_run's own opt-in gate skips cleanly
        // before touching Chord; no doc-target credentials/backend needed.
        let context = json!({
            "project": "TERM",
            "spec_id": "S112-review-kg-integration",
            "diff": "+ fn hello() {}",
        });
        let scribe_docs = maybe_scribe_docs(true, &context).await;

        assert_eq!(scribe_docs["ran"], true, "{scribe_docs}");
        let outcome = scribe_docs["outcome"].as_str().unwrap_or_default();
        assert!(
            outcome == "failed" || outcome == "skipped",
            "expected outcome failed|skipped, got {scribe_docs}"
        );

        // The overall review result stays Ok and the verdict is unaffected.
        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        std::env::remove_var("OPENROUTER_API_KEY");
        let args = json!({
            "structure": "single",
            "providers": ["opus"],
            "criteria": "x",
            "context": {
                "diff": "+ fn hello() {}",
                "project": "TERM",
                "spec_id": "S112-review-kg-integration",
            }
        });
        let out = tool().execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        // With no REVIEW_DAEMON_TOKEN configured, the single provider
        // degrades to UNKNOWN/incomplete, so `maybe_scribe_docs`'s APPROVE
        // gate is correctly never reached from the full `execute()` path --
        // this just confirms `execute()` stays Ok end-to-end with the new
        // `scribe_docs` field always present in the result shape.
        assert_eq!(parsed["scribe_docs"]["ran"], false, "{parsed}");
        assert!(parsed["aggregate_verdict"].is_string(), "{parsed}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn approve_without_doc_params_skips_docgen() {
        let context = json!({"diff": "+ fn x() {}"});
        let scribe_docs = maybe_scribe_docs(true, &context).await;
        assert_eq!(scribe_docs["ran"], false, "{scribe_docs}");
        assert_eq!(scribe_docs["reason"], "no doc params", "{scribe_docs}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn non_approve_skips_docgen() {
        let context = json!({
            "project": "TERM",
            "spec_id": "S112-review-kg-integration",
        });
        let scribe_docs = maybe_scribe_docs(false, &context).await;
        assert_eq!(scribe_docs["ran"], false, "{scribe_docs}");

        let scribe_docs = maybe_scribe_docs(false, &context).await;
        assert_eq!(scribe_docs["ran"], false, "{scribe_docs}");
    }

    // ── KGFIND-03: cross-provider dedup + scope resolution (pure) ──────────

    fn finding(category: &str, file: Option<&str>, symbol: Option<&str>, description: &str) -> Finding {
        Finding {
            category: category.to_string(),
            severity: "medium".to_string(),
            file: file.map(str::to_string),
            symbol: symbol.map(str::to_string),
            description: description.to_string(),
            subjective: None,
        }
    }

    fn provider_with(provider: &str, findings: Vec<Finding>) -> ProviderResult {
        ProviderResult {
            provider: provider.to_string(),
            verdict: "REQUEST_CHANGES".to_string(),
            reasoning: String::new(),
            error: None,
            findings,
        }
    }

    #[test]
    fn dedup_across_providers_collapses_same_issue_from_two_providers() {
        let results = vec![
            provider_with(
                "opus",
                vec![finding("bug", Some("src/a.rs"), Some("crate::a::foo"), "off-by-one error in loop bound")],
            ),
            provider_with(
                "codex",
                vec![finding(
                    "bug",
                    Some("src/a.rs"),
                    Some("crate::a::foo"),
                    "Off-by-one error in loop bound.",
                )],
            ),
        ];
        let deduped = dedup_across_providers(&results);
        assert_eq!(deduped.len(), 1, "{deduped:?}");
        assert_eq!(deduped[0].description, "off-by-one error in loop bound");
    }

    #[test]
    fn dedup_across_providers_collapses_same_location_despite_different_wording() {
        // Two reviewers describe the SAME issue at the same (category, file,
        // symbol) in COMPLETELY different words — they must still collapse to one
        // occurrence (the description is intentionally out of the dedup key).
        let results = vec![
            provider_with(
                "opus",
                vec![finding("bug", Some("src/a.rs"), Some("crate::a::foo"), "loop can index one past the slice end")],
            ),
            provider_with(
                "codex",
                vec![finding("bug", Some("src/a.rs"), Some("crate::a::foo"), "the counter exceeds the array length")],
            ),
        ];
        let deduped = dedup_across_providers(&results);
        assert_eq!(deduped.len(), 1, "differently-worded same-location findings must collapse: {deduped:?}");
        assert_eq!(deduped[0].description, "loop can index one past the slice end");
    }

    #[test]
    fn dedup_across_providers_keeps_distinct_issues_separate() {
        let results = vec![
            provider_with("opus", vec![finding("bug", Some("src/a.rs"), Some("crate::a::foo"), "off-by-one")]),
            provider_with(
                "codex",
                vec![finding("style", Some("src/b.rs"), Some("crate::b::Bar"), "missing doc comment")],
            ),
        ];
        let deduped = dedup_across_providers(&results);
        assert_eq!(deduped.len(), 2, "{deduped:?}");
    }

    #[test]
    fn dedup_across_providers_empty_when_no_findings() {
        let results = vec![provider_with("opus", vec![]), provider_with("codex", vec![])];
        assert!(dedup_across_providers(&results).is_empty());
    }

    fn graph_with_node() -> KnowledgeGraph {
        use crate::scribe::graph::model::{KgNode, NodeKind};
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs"));
        g
    }

    #[test]
    fn resolve_scope_symbol_matching_node_id_is_node_scope() {
        let g = graph_with_node();
        let f = finding("bug", Some("src/a.rs"), Some("crate::a::foo"), "d");
        let (kind, ref_) = resolve_scope(&f, Some(&g), "TERM");
        assert_eq!(kind, ScopeKind::Node);
        assert_eq!(ref_, "crate::a::foo");
    }

    #[test]
    fn resolve_scope_symbol_matching_node_name_is_node_scope() {
        let g = graph_with_node();
        let f = finding("bug", Some("src/a.rs"), Some("foo"), "d");
        let (kind, ref_) = resolve_scope(&f, Some(&g), "TERM");
        assert_eq!(kind, ScopeKind::Node);
        assert_eq!(ref_, "crate::a::foo");
    }

    #[test]
    fn resolve_scope_symbol_not_in_graph_falls_back_to_path() {
        let g = graph_with_node();
        let f = finding("bug", Some("src/z.rs"), Some("crate::z::nope"), "d");
        let (kind, ref_) = resolve_scope(&f, Some(&g), "TERM");
        assert_eq!(kind, ScopeKind::Path);
        assert_eq!(ref_, "src/z.rs");
    }

    #[test]
    fn resolve_scope_symbol_not_in_graph_no_file_falls_back_to_global() {
        let g = graph_with_node();
        let f = finding("bug", None, Some("crate::z::nope"), "d");
        let (kind, ref_) = resolve_scope(&f, Some(&g), "TERM");
        assert_eq!(kind, ScopeKind::Global);
        assert_eq!(ref_, "TERM");
    }

    #[test]
    fn resolve_scope_no_symbol_but_file_is_path_scope() {
        let f = finding("bug", Some("src/a.rs"), None, "d");
        let (kind, ref_) = resolve_scope(&f, None, "TERM");
        assert_eq!(kind, ScopeKind::Path);
        assert_eq!(ref_, "src/a.rs");
    }

    #[test]
    fn resolve_scope_neither_symbol_nor_file_is_global_scope() {
        let f = finding("bug", None, None, "d");
        let (kind, ref_) = resolve_scope(&f, None, "TERM");
        assert_eq!(kind, ScopeKind::Global);
        assert_eq!(ref_, "TERM");
    }

    #[test]
    fn resolve_scope_no_graph_available_symbol_falls_back_to_path() {
        let f = finding("bug", Some("src/a.rs"), Some("crate::a::foo"), "d");
        let (kind, ref_) = resolve_scope(&f, None, "TERM");
        assert_eq!(kind, ScopeKind::Path);
        assert_eq!(ref_, "src/a.rs");
    }

    // ── KGFIND-03: maybe_record_findings hook (capture-only, non-blocking) ─

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_record_findings_no_project_id_is_noop() {
        let results = vec![provider_with("opus", vec![finding("bug", Some("src/a.rs"), None, "d")])];
        let context = json!({"diff": "+ fn x() {}"});
        let out = maybe_record_findings(&results, &context).await;
        assert_eq!(out["recorded"], false, "{out}");
        assert_eq!(out["reason"], "no project_id", "{out}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_record_findings_no_findings_at_all_is_noop() {
        let results = vec![provider_with("opus", vec![]), provider_with("codex", vec![])];
        let context = json!({"project_id": "TERM"});
        let out = maybe_record_findings(&results, &context).await;
        assert_eq!(out["recorded"], false, "{out}");
        assert_eq!(out["reason"], "no findings", "{out}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_record_findings_project_and_findings_but_store_unset_is_noop() {
        // Mirrors the NotConfigured-skip pattern used elsewhere in this
        // workspace: never attempt a live DB connection from a unit test --
        // if a real DSN happens to be configured in this process, skip.
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let results = vec![provider_with(
            "opus",
            vec![finding("bug", Some("src/a.rs"), None, "off-by-one in loop bound")],
        )];
        let context = json!({"project_id": "TERM", "verdict": "REQUEST_CHANGES"});
        let out = maybe_record_findings(&results, &context).await;
        assert_eq!(out["recorded"], false, "{out}");

        // The review's own verdict handling is entirely untouched by this
        // hook -- confirmed by exercising the full `execute()` path with the
        // same context shape and no daemon/OpenRouter config: the call stays
        // `Ok` end-to-end with `findings_recorded` always present.
        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        std::env::remove_var("OPENROUTER_API_KEY");
        let args = json!({
            "structure": "single",
            "providers": ["opus"],
            "criteria": "x",
            "context": {"project_id": "TERM", "diff": "+ fn x() {}"}
        });
        let out = tool().execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert!(parsed["findings_recorded"].is_object(), "{parsed}");
        assert!(parsed["aggregate_verdict"].is_string(), "{parsed}");
    }

    // ── CXEG-08: Stage-5b risk-gate escalation ──────────────────────────────

    fn escalation_cfg(enabled: bool, add_provider: &str, force_high: bool) -> CortexConfig {
        CortexConfig {
            risk_score_threshold: if force_high { 0.0 } else { 7.0 },
            enable_tier_b: false,
            enable_tier_c: false,
            elegance_advisory_only: true,
            dup_cosine: 0.85,
            atlas_database_url: None,
            max_blast_nodes: crate::cortex::scope::DEFAULT_MAX_BLAST_NODES,
            tier_b_percentile: 90.0,
            house_style_exemplars_k: crate::cortex::house_style::DEFAULT_EXEMPLARS_K,
            risk_weight_centrality_spike: 2.0,
            risk_weight_complexity_spike: 1.5,
            risk_weight_fan_out_explosion: 1.5,
            risk_weight_community_boundary_crossing: 2.5,
            risk_weight_semantic_duplication: 10.0,
            risk_weight_recurrence: 1.0,
            risk_band_elevated_cut: if force_high { 0.0 } else { 4.0 },
            audit_clone_timeout_secs: 60,
            audit_max_clone_bytes: 200_000_000,
            crystallize_min_recurrence: crate::cortex::crystallize::DEFAULT_MIN_RECURRENCE,
            escalation_enabled: enabled,
            escalation_add_provider: add_provider.to_string(),
        }
    }

    fn seed_tiny_graph(project_id: &str, path: &str) {
        use crate::scribe::graph::model::{KgNode, KnowledgeGraph, NodeKind};
        use crate::scribe::graph::store::GraphStore;
        use crate::scribe::ScribeConfig;

        let store = GraphStore::from_config(&ScribeConfig::from_env());
        let mut g = KnowledgeGraph::new(project_id);
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", path));
        g.recompute_degrees();
        store.save(project_id, &g).unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_disabled_is_a_noop() {
        let mut providers = vec!["opus".to_string()];
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(false, "codex", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;
        assert!(!decision.escalated, "{decision:?}");
        assert_eq!(decision.reason, "disabled");
        assert_eq!(providers, vec!["opus".to_string()], "disabled escalation must never touch the panel");
    }

    #[tokio::test]
    async fn maybe_escalate_no_project_id_is_a_noop() {
        let mut providers = vec!["opus".to_string()];
        let context = json!({"changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "codex", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;
        assert!(!decision.escalated, "{decision:?}");
        assert_eq!(decision.reason, "no_project_id");
        assert_eq!(providers.len(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_fail_open_when_cortex_review_unavailable() {
        // No stored Atlas graph -> cortex_review degrades internally to
        // configured:false/band:"unknown" (never an Err) -- the fail-open
        // contract: escalation must read this as "band not high", never
        // block/alter the correctness gate.
        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-nograph-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let mut providers = vec!["opus".to_string()];
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "codex", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;
        assert!(!decision.escalated, "{decision:?}");
        assert_eq!(decision.band, "unknown");
        assert_eq!(providers.len(), 1, "no graph -> no escalation -> panel untouched");

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_high_band_widens_panel_and_sets_escalated_true() {
        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-high-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph("TERM", "src/a.rs");

        let mut providers = vec!["opus".to_string()];
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "codex", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;

        assert!(decision.escalated, "{decision:?}");
        assert_eq!(decision.band, "high");
        assert_eq!(decision.added_provider, Some("codex".to_string()));
        assert_eq!(providers, vec!["opus".to_string(), "codex".to_string()], "panel must widen by exactly one provider");
        assert!(!decision.waived);

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_does_not_duplicate_an_already_present_add_provider() {
        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-dup-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph("TERM", "src/a.rs");

        let mut providers = vec!["opus".to_string(), "codex".to_string()];
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "codex", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;

        assert!(decision.escalated, "{decision:?}");
        assert_eq!(providers.len(), 2, "the configured add-provider is already present -- must not duplicate");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_adversarial_pair_panel_is_never_widened() {
        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-adv-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph("TERM", "src/a.rs");

        let mut providers = vec!["opus".to_string(), "codex".to_string()];
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "agy", true);
        let decision = maybe_escalate(Structure::AdversarialPair, &context, &mut providers, &cfg).await;

        assert!(decision.escalated, "{decision:?}");
        assert!(decision.escalation_degraded, "{decision:?}");
        assert_eq!(decision.added_provider, None);
        assert_eq!(providers.len(), 2, "adversarial_pair's fixed defend/attack panel must never be widened");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_invalid_add_provider_degrades_without_widening() {
        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-badprovider-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph("TERM", "src/a.rs");

        let mut providers = vec!["opus".to_string()];
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "gpt5-not-a-real-provider", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;

        assert!(decision.escalated, "{decision:?}");
        assert!(decision.escalation_degraded, "{decision:?}");
        assert_eq!(providers.len(), 1, "an invalid configured add-provider must never be pushed onto the panel");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_panel_already_at_max_degrades_without_widening() {
        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-maxpanel-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph("TERM", "src/a.rs");

        let mut providers = vec!["opus".to_string(), "codex".to_string(), "agy".to_string(), "nemotron".to_string(), "qwen_coder".to_string()];
        assert_eq!(providers.len(), MAX_PROVIDERS);
        let context = json!({"project_id": "TERM", "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "free", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;

        assert!(decision.escalated, "{decision:?}");
        assert!(decision.escalation_degraded, "{decision:?}");
        assert_eq!(providers.len(), MAX_PROVIDERS, "a full panel must never be exceeded to widen it");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_active_waiver_suppresses_escalation() {
        if std::env::var("ATLAS_DATABASE_URL").is_err() {
            return; // no live Atlas DB in this test process -- skip cleanly
        }
        let project_id = format!("TERM-CXEG08-WAIVE-{}", uuid::Uuid::new_v4());
        crate::cortex::waiver::record_waiver(
            &project_id,
            crate::cortex::waiver::HIGH_RISK_BAND_RULE,
            "*",
            "accepted risk for this sprint",
            "test-author",
            None,
        )
        .await
        .expect("record_waiver");

        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-waived-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph(&project_id, "src/a.rs");

        let mut providers = vec!["opus".to_string()];
        let context = json!({"project_id": project_id, "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "codex", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;

        assert!(!decision.escalated, "{decision:?}");
        assert!(decision.waived, "{decision:?}");
        assert_eq!(decision.reason, "active_waiver_suppressed_escalation");
        assert_eq!(providers.len(), 1, "a waived escalation must never widen the panel");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn maybe_escalate_expired_waiver_does_not_suppress_escalation() {
        if std::env::var("ATLAS_DATABASE_URL").is_err() {
            return; // no live Atlas DB in this test process -- skip cleanly
        }
        let project_id = format!("TERM-CXEG08-EXPIRED-{}", uuid::Uuid::new_v4());
        let expiry = chrono::Utc::now() - chrono::Duration::hours(1);
        crate::cortex::waiver::record_waiver(
            &project_id,
            crate::cortex::waiver::HIGH_RISK_BAND_RULE,
            "*",
            "expired waiver, should no longer apply",
            "test-author",
            Some(expiry),
        )
        .await
        .expect("record_waiver");

        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-expired-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph(&project_id, "src/a.rs");

        let mut providers = vec!["opus".to_string()];
        let context = json!({"project_id": project_id, "changed_files": ["src/a.rs"]});
        let cfg = escalation_cfg(true, "codex", true);
        let decision = maybe_escalate(Structure::PanelMajority, &context, &mut providers, &cfg).await;

        assert!(decision.escalated, "an EXPIRED waiver must not suppress escalation: {decision:?}");
        assert!(!decision.waived);
        assert_eq!(providers.len(), 2);

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn escalation_never_sets_changes_requested_from_risk_alone() {
        // A high band alone must never drive `aggregate_verdict` -- it is
        // computed by `aggregate()` purely from actual panel results, with
        // no risk/escalation input at all (see `execute()`'s call-site
        // comment). Here, every provider degrades (no daemon/OpenRouter
        // configured), so if risk COULD flip the verdict this would surface
        // as something other than "UNKNOWN"/incomplete.
        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-noflip-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph("TERM", "src/a.rs");
        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        std::env::remove_var("OPENROUTER_API_KEY");

        let args = json!({
            "structure": "panel_majority",
            "providers": ["opus"],
            "criteria": "must compile",
            "context": {"project_id": "TERM", "changed_files": ["src/a.rs"]}
        });
        let out = tool().execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["aggregate_verdict"], "UNKNOWN", "{parsed}");
        assert_eq!(parsed["complete"], false, "{parsed}");
        assert_ne!(parsed["aggregate_verdict"], "REQUEST_CHANGES", "risk alone must never set REQUEST_CHANGES: {parsed}");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn execute_reports_escalation_degraded_when_added_provider_is_unavailable() {
        // A default (non-forced) CortexConfig::from_env() picks up this
        // test's real threshold, so force a high band via a stored graph +
        // CORTEX_RISK_SCORE_THRESHOLD/CORTEX_RISK_BAND_ELEVATED_CUT env
        // overrides (execute() calls `CortexConfig::from_env()` internally,
        // not the test's own `escalation_cfg`).
        //
        // Hermeticity (test-gate flake fix): this test needs BOTH the base
        // panel provider (`opus`) and the escalated provider (`codex`) to be
        // dispatchable (not capacity-paused) yet have the `codex` dispatch
        // itself fail as "unavailable". Two ambient conditions previously
        // broke that on the gate host and nowhere else:
        //   1. `capacity::registry()` is a process-global singleton shared
        //      with every other test in this binary (see the REVCAP-A
        //      section below, which documents the same hazard). If an
        //      earlier test left `opus`/`codex`/`agy` marked down/rate
        //      limited (e.g. it panicked before its own cleanup ran), the
        //      REVCAP-01 frontier-capacity gate can pause dispatch entirely
        //      before either provider is even attempted, so `providers` come
        //      back with fewer than 2 entries -- unrelated to the thing this
        //      test actually exercises. Force all three FRONTIER providers
        //      back to a known-good `mark_success` state up front, exactly
        //      like the REVCAP-A tests do.
        //   2. `remove_var("REVIEW_DAEMON_TOKEN")` only guarantees "no token"
        //      if the ambient process env didn't already have a real one --
        //      on the compiler-gate host, which also runs the review daemon,
        //      it can. Don't rely on the token being absent: point
        //      `REVIEW_DAEMON_URL` at a closed local port so the daemon
        //      dispatch fails at the transport layer regardless of whether a
        //      token happens to be configured, making "codex unavailable"
        //      true unconditionally rather than incidentally.
        capacity::registry().update("opus", |s| s.mark_success());
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());

        let store_dir = std::env::temp_dir().join(format!("review-cxeg08-degraded-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        seed_tiny_graph("TERM", "src/a.rs");
        std::env::set_var("CORTEX_RISK_SCORE_THRESHOLD", "0.0");
        std::env::set_var("CORTEX_RISK_BAND_ELEVATED_CUT", "0.0");
        std::env::set_var("CORTEX_ESCALATION_ADD_PROVIDER", "codex");
        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        std::env::remove_var("OPENROUTER_API_KEY");
        // Guaranteed-unreachable: nothing listens on loopback port 1, so this
        // fails fast (connection refused) rather than depending on ambient
        // token state, and rather than risking a slow real-network timeout.
        std::env::set_var("REVIEW_DAEMON_URL", "http://127.0.0.1:1");

        let args = json!({
            "structure": "panel_majority",
            "providers": ["opus"],
            "criteria": "must compile",
            "context": {"project_id": "TERM", "changed_files": ["src/a.rs"]}
        });
        let out = tool().execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["escalation"]["escalated"], true, "{parsed}");
        assert_eq!(parsed["escalation"]["added_provider"], "codex", "{parsed}");
        assert_eq!(
            parsed["escalation"]["escalation_degraded"], true,
            "codex must degrade (no token / unreachable daemon), not deadlock: {parsed}"
        );
        // No deadlock: the call still completed and returned both providers.
        assert_eq!(parsed["providers"].as_array().unwrap().len(), 2, "{parsed}");

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
        std::env::remove_var("CORTEX_RISK_SCORE_THRESHOLD");
        std::env::remove_var("CORTEX_RISK_BAND_ELEVATED_CUT");
        std::env::remove_var("CORTEX_ESCALATION_ADD_PROVIDER");
        std::env::remove_var("REVIEW_DAEMON_URL");
        // Leave the frontier providers in a known-good state for whichever
        // test runs next (same discipline as the REVCAP-A section).
        capacity::registry().update("opus", |s| s.mark_success());
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
    }

    // ── REVCAP-01 PART A: capacity gate + review_provider_status wiring ────
    //
    // `#[serial_test::serial]` on every test in this section: they mutate the
    // process-global `capacity::registry()` singleton (shared with every
    // other test in this binary), same reason `free_pool`'s tests use it.

    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_gate_pauses_review_run_and_dispatches_no_providers() {
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
        let now = std::time::SystemTime::now();
        // >1800s -> Shelved, well within the "down" window for the gate check.
        capacity::registry()
            .update("codex", |s| s.mark_rate_limited(Some(3000), now, "test shelve"));
        capacity::registry()
            .update("agy", |s| s.mark_rate_limited(Some(3000), now, "test shelve"));

        let args = json!({
            "structure": "panel_majority",
            "providers": ["codex", "agy", "opus"],
            "criteria": "x"
        });
        let out = tool().execute(args).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(v["aggregate_verdict"], "CAPACITY_PAUSED", "{v}");
        assert_eq!(v["capacity_paused"], true, "{v}");
        assert_eq!(v["complete"], false, "{v}");
        assert!(v["providers"].as_array().unwrap().is_empty(), "gate must dispatch nothing: {v}");
        let down: Vec<String> = v["down_providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        assert!(
            down.contains(&"codex".to_string()) && down.contains(&"agy".to_string()),
            "{v}"
        );
        assert!(v["earliest_recovery_unix"].as_f64().is_some(), "{v}");

        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_gate_proceeds_when_only_one_frontier_provider_is_down() {
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
        let now = std::time::SystemTime::now();
        capacity::registry()
            .update("codex", |s| s.mark_rate_limited(Some(3000), now, "test shelve"));

        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        let args = json!({"structure": "single", "providers": ["agy"], "criteria": "x"});
        let out = tool().execute(args).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();

        // Only 1 frontier provider down -> proceeds normally (dispatches `agy`,
        // which degrades on the missing daemon token, but that's an ordinary
        // per-provider degrade, NOT the capacity-pause outcome).
        assert_ne!(v["aggregate_verdict"], "CAPACITY_PAUSED", "{v}");
        assert!(v["capacity_paused"].is_null(), "{v}");
        assert_eq!(v["providers"].as_array().unwrap().len(), 1, "{v}");

        capacity::registry().update("codex", |s| s.mark_success());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_gate_is_a_no_op_on_an_empty_fresh_registry_backward_compat() {
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
        capacity::registry().update("opus", |s| s.mark_success());

        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        let args = json!({"structure": "single", "providers": ["codex"], "criteria": "x"});
        let out = tool().execute(args).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();

        assert_ne!(v["aggregate_verdict"], "CAPACITY_PAUSED", "{v}");
        assert!(v["capacity_paused"].is_null(), "{v}");
        assert_eq!(v["providers"].as_array().unwrap().len(), 1, "{v}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn review_run_records_a_real_dispatch_error_into_the_capacity_registry() {
        capacity::registry().update("codex", |s| s.mark_success());
        // -> "unavailable: REVIEW_DAEMON_TOKEN not configured"
        std::env::remove_var("REVIEW_DAEMON_TOKEN");

        let args = json!({"structure": "single", "providers": ["codex"], "criteria": "x"});
        let _ = tool().execute(args).await.unwrap();

        // Not a rate-limit/timeout phrase, so this must record as a plain
        // Error status (still available), never a Cooldown/Shelved cap.
        let s = capacity::registry().get("codex");
        assert_eq!(s.last_status, capacity::CapStatus::Error, "{s:?}");
        assert!(s.available, "a non-cap dispatch error must never mark the provider down");

        capacity::registry().update("codex", |s| s.mark_success());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn review_provider_status_reports_cached_state_without_dispatching() {
        capacity::registry().update("codex", |s| s.mark_success());
        let now = std::time::SystemTime::now();
        capacity::registry().update("agy", |s| s.mark_rate_limited(Some(3000), now, "test cap"));

        let out = ReviewProviderStatus.execute_structured(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();

        assert_eq!(v["probed"], false, "{v}");
        let providers = v["providers"].as_array().unwrap();
        assert_eq!(providers.len(), STATUS_PROVIDERS.len(), "{v}");
        let agy = providers.iter().find(|p| p["provider"] == "agy").unwrap();
        assert_eq!(agy["status"], "shelved", "{agy}"); // 3000s > SHELVE_THRESHOLD_SECS
        assert!(agy["capped_until"].as_f64().is_some(), "{agy}");
        let codex = providers.iter().find(|p| p["provider"] == "codex").unwrap();
        assert_eq!(codex["status"], "available", "{codex}");

        capacity::registry().update("agy", |s| s.mark_success());
    }

    // ── REVCAP-01 PART B: intensive-substitute per-provider selection ──────
    //
    // `#[serial_test::serial]`, same reason as the PART A section above: these
    // mutate the process-global `capacity::registry()` singleton.

    #[test]
    #[serial_test::serial]
    fn is_intensive_substitute_true_for_an_up_frontier_peer_of_a_down_frontier() {
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
        capacity::registry().update("opus", |s| s.mark_success());
        let now = std::time::SystemTime::now();
        capacity::registry().update("codex", |s| s.mark_rate_limited(Some(3000), now, "shelved"));

        // opus and agy are both up frontier providers while codex is down ->
        // either is eligible to review harder in codex's place.
        assert!(is_intensive_substitute("opus", now), "opus should substitute for shelved codex");
        assert!(is_intensive_substitute("agy", now), "agy should substitute for shelved codex");

        capacity::registry().update("codex", |s| s.mark_success());
    }

    #[test]
    #[serial_test::serial]
    fn is_intensive_substitute_false_for_the_down_provider_itself() {
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
        let now = std::time::SystemTime::now();
        capacity::registry().update("codex", |s| s.mark_rate_limited(Some(3000), now, "shelved"));

        // A down provider dispatched anyway is not "standing in" for anything --
        // it's just the requested, degrading provider.
        assert!(!is_intensive_substitute("codex", now));

        capacity::registry().update("codex", |s| s.mark_success());
    }

    #[test]
    #[serial_test::serial]
    fn is_intensive_substitute_false_for_a_non_frontier_provider() {
        capacity::registry().update("codex", |s| s.mark_success());
        let now = std::time::SystemTime::now();
        capacity::registry().update("codex", |s| s.mark_rate_limited(Some(3000), now, "shelved"));

        // nemotron/qwen_coder/free/gpt56 are never frontier -- never intensive.
        assert!(!is_intensive_substitute("nemotron", now));
        assert!(!is_intensive_substitute("free", now));

        capacity::registry().update("codex", |s| s.mark_success());
    }

    #[test]
    #[serial_test::serial]
    fn is_intensive_substitute_false_on_an_empty_or_healthy_registry_backward_compat() {
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
        capacity::registry().update("opus", |s| s.mark_success());
        let now = std::time::SystemTime::now();

        // Nothing down anywhere -> every provider (frontier or not) reads as a
        // normal, non-substitute dispatch -- identical to pre-PART-B behavior.
        assert!(!is_intensive_substitute("opus", now));
        assert!(!is_intensive_substitute("codex", now));
        assert!(!is_intensive_substitute("agy", now));
        assert!(!is_intensive_substitute("nemotron", now));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn substitute_provider_is_dispatched_with_the_intensive_role_and_records_normally() {
        // End-to-end through `execute()`: with codex shelved and opus/agy up, a
        // panel of [codex, opus] must NOT hit the >=2-down capacity gate (only 1
        // frontier down) and must proceed to dispatch -- opus (the up frontier
        // peer) is the intensive substitute. No REVIEW_DAEMON_TOKEN is configured
        // here, so both providers degrade on dispatch (no real subprocess is
        // spawned, per this task's hard contract) -- this test asserts the run
        // completes normally (no capacity pause) and codex's degrade is recorded
        // as a plain dispatch error, exactly like the PART A behavior it builds
        // on; the opts/role selection itself is covered directly by the
        // `is_intensive_substitute` unit tests above (dispatch internals are not
        // observable without a real daemon).
        capacity::registry().update("codex", |s| s.mark_success());
        capacity::registry().update("agy", |s| s.mark_success());
        capacity::registry().update("opus", |s| s.mark_success());
        let now = std::time::SystemTime::now();
        capacity::registry().update("codex", |s| s.mark_rate_limited(Some(3000), now, "shelved"));

        std::env::remove_var("REVIEW_DAEMON_TOKEN");
        let args = json!({"structure": "panel_majority", "providers": ["codex", "opus"], "criteria": "x"});
        let out = tool().execute(args).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();

        assert_ne!(v["aggregate_verdict"], "CAPACITY_PAUSED", "{v}");
        assert!(v["capacity_paused"].is_null(), "{v}");
        assert_eq!(v["providers"].as_array().unwrap().len(), 2, "{v}");

        capacity::registry().update("codex", |s| s.mark_success());
    }
}
