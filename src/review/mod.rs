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
mod kg_context;
mod prompt;

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub use aggregate::{aggregate, ProviderResult};
pub use dispatch::ReviewConfig;
pub use prompt::{build_docs_prompt, build_prompt, parse_verdict, Role, Structure};

const ALLOWED_PROVIDERS: &[&str] = &["opus", "codex", "agy", "nemotron", "qwen_coder"];
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
            ProviderResult {
                provider,
                verdict: verdict.as_str().to_string(),
                reasoning,
                error: None,
            }
        }
        Err(reason) => ProviderResult {
            provider,
            verdict: "UNKNOWN".to_string(),
            reasoning: String::new(),
            error: Some(reason),
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

        Ok(json!({
            "structure": args["structure"],
            "providers": results,
            "aggregate_verdict": aggregate_verdict,
            "complete": complete,
            "kg_rebuild": kg_rebuild,
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
}
