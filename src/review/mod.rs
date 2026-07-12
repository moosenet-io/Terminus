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
// `pub(crate)` (was module-private): DOCGEN-10's mismatch detector
// (`crate::tools::docgen::mismatch`) reuses `is_daemon_provider` /
// `openrouter_model_for` / the model-tag constants directly rather than
// re-declaring the opus/codex/agy-vs-nemotron/qwen_coder provider-routing
// table a second time -- one source of truth for "which providers go
// through the daemon vs. OpenRouter", not two.
pub(crate) mod dispatch;
pub(crate) mod free_pool;
mod kg_context;
mod prompt;

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::scribe::graph::findings_store::{FindingsStore, NewFinding, RecordOutcome, ScopeKind};
use crate::scribe::graph::model::KnowledgeGraph;
use crate::scribe::graph::store::GraphStore;
use crate::scribe::graph::vec_embed::EmbedClient;
use crate::scribe::ScribeConfig;
use crate::tool::RustTool;

pub use aggregate::{aggregate, Finding, ProviderResult};
pub use dispatch::ReviewConfig;
pub use prompt::{build_docs_prompt, build_prompt, parse_findings, parse_verdict, Role, Structure};

const ALLOWED_PROVIDERS: &[&str] = &["opus", "codex", "agy", "nemotron", "qwen_coder", "free"];
const MAX_PROVIDERS: usize = 5;

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
async fn maybe_rebuild(aggregate_verdict: &str, complete: bool, context: &Value) -> Value {
    if aggregate_verdict != "APPROVE" || !complete {
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
async fn maybe_scribe_docs(aggregate_verdict: &str, complete: bool, context: &Value) -> Value {
    if aggregate_verdict != "APPROVE" || !complete {
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

/// Normalize a finding description for cross-provider dedup: collapse
/// whitespace, lowercase, and cap to a short prefix so two reviewers'
/// near-identical phrasing of the same issue still collide on the same key.
/// Pure, no I/O.
fn normalize_desc_prefix(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let trimmed = collapsed.trim_end_matches(['.', '!', '?']);
    trimmed.chars().take(80).collect()
}

/// KGFIND-03: collapse findings that are the same issue reported by more than
/// one provider -- same `(category, file, symbol)` plus a near-identical
/// description prefix -- into one, keeping the first occurrence (in provider
/// order). Pure, fully unit-testable without a store or a graph.
fn dedup_across_providers(results: &[ProviderResult]) -> Vec<Finding> {
    let mut seen: HashSet<(String, Option<String>, Option<String>, String)> = HashSet::new();
    let mut out = Vec::new();
    for r in results {
        for f in &r.findings {
            let key = (
                f.category.clone(),
                f.file.clone(),
                f.symbol.clone(),
                normalize_desc_prefix(&f.description),
            );
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

pub struct ReviewRun;

impl ReviewRun {
    pub fn new() -> Self {
        Self
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
            "'structure' must be one of single|adversarial_pair|panel_majority|panel_unanimous, got '{structure_str}'"
        ))
    })?;

    let providers_val = args["providers"]
        .as_array()
        .ok_or_else(|| ToolError::InvalidArgument("'providers' must be a non-empty array".into()))?;
    if providers_val.is_empty() || providers_val.len() > MAX_PROVIDERS {
        return Err(ToolError::InvalidArgument(format!(
            "'providers' must have between 1 and {MAX_PROVIDERS} entries, got {}",
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

fn role_for(structure: Structure, index: usize) -> Role {
    match structure {
        Structure::AdversarialPair => {
            if index == 0 {
                Role::Defend
            } else {
                Role::Attack
            }
        }
        _ => Role::Reviewer,
    }
}

async fn run_one_provider(cfg: ReviewConfig, provider: String, prompt_text: String) -> ProviderResult {
    let raw = if dispatch::is_daemon_provider(&provider) {
        cfg.dispatch_daemon(&provider, &prompt_text).await
    } else if provider == "free" {
        // Seamless free-tier: round-robin the daily-curated free-model pool
        // with 429 failover (see free_pool). Used as the tail of a 3-5 provider
        // panel, after the sub/OAuth providers.
        cfg.dispatch_free_pool(&prompt_text).await
    } else if let Some(model) = dispatch::openrouter_model_for(&provider) {
        cfg.dispatch_openrouter(model, &prompt_text).await
    } else {
        // Unreachable given parse_input's validation, but fail safe rather
        // than panic if it ever were.
        Err(format!("unavailable: unknown provider '{provider}'"))
    };

    match raw {
        Ok(text) => {
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
        Err(reason) => ProviderResult {
            provider,
            verdict: "UNKNOWN".to_string(),
            reasoning: String::new(),
            error: Some(reason),
            findings: Vec::new(),
        },
    }
}

#[async_trait]
impl RustTool for ReviewRun {
    fn name(&self) -> &str {
        "review_run"
    }

    fn description(&self) -> &str {
        "Run a multi-provider code/change review. 'structure' is one of single, \
adversarial_pair, panel_majority, panel_unanimous. 'providers' (1-5) picks from \
opus, codex, agy (CLI-backed via review-daemon), nemotron, qwen_coder (OpenRouter, \
frontier-class free-tier models). 'criteria' is the acceptance criteria text; \
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
                    "enum": ["single", "adversarial_pair", "panel_majority", "panel_unanimous"]
                },
                "providers": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_PROVIDERS,
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
        let (structure, providers, criteria, mut context) = parse_input(&args)?;

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
        let cfg = ReviewConfig::from_env();

        let mut set = tokio::task::JoinSet::new();
        // Tracks each spawned task's tokio::task::Id back to its (index,
        // provider name), so a task panic (JoinError) can still be attributed
        // to the RIGHT slot instead of a fabricated trailing index -- which
        // matters for adversarial_pair, where index 0/1 = defend/attack.
        let mut id_to_slot: std::collections::HashMap<tokio::task::Id, (usize, String)> =
            std::collections::HashMap::with_capacity(providers.len());
        for (idx, provider) in providers.iter().enumerate() {
            let role = role_for(structure, idx);
            let prompt_text = build_prompt(role, &criteria, &context);
            let cfg = cfg.clone();
            let provider = provider.clone();
            let provider_for_map = provider.clone();
            let handle = set.spawn(async move {
                let result = run_one_provider(cfg, provider, prompt_text).await;
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

        // KGREV-02: on a successful, complete pass, incrementally rebuild the
        // project's KG (best-effort, never fails the review); see
        // `maybe_rebuild` for the lock semantics.
        let kg_rebuild = maybe_rebuild(&aggregate_verdict, complete, &context).await;

        // KGREV-03: after the KG rebuild (so docs see the refreshed graph),
        // best-effort doc refresh through the sanctioned `docgen_run` door;
        // see `maybe_scribe_docs` for the gating/S9 rationale.
        let scribe_docs = maybe_scribe_docs(&aggregate_verdict, complete, &context).await;

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
        let findings_recorded = maybe_record_findings(&results, &findings_context).await;

        Ok(json!({
            "structure": args["structure"],
            "providers": results,
            "aggregate_verdict": aggregate_verdict,
            "complete": complete,
            "kg_rebuild": kg_rebuild,
            "scribe_docs": scribe_docs,
            "findings_recorded": findings_recorded,
        })
        .to_string())
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry
        .register(Box::new(ReviewRun::new()))
        .expect("review_run must register cleanly");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ReviewRun {
        ReviewRun::new()
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
        let kg_rebuild = maybe_rebuild("APPROVE", true, &context).await;

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
        let kg_rebuild = maybe_rebuild("APPROVE", true, &context).await;

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
        let kg_rebuild = maybe_rebuild("APPROVE", true, &context).await;
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
        let scribe_docs = maybe_scribe_docs("APPROVE", true, &context).await;

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
        let scribe_docs = maybe_scribe_docs("APPROVE", true, &context).await;
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
        let scribe_docs = maybe_scribe_docs("REQUEST_CHANGES", true, &context).await;
        assert_eq!(scribe_docs["ran"], false, "{scribe_docs}");

        let scribe_docs = maybe_scribe_docs("UNKNOWN", false, &context).await;
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
}
