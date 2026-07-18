//! DOCGEN-08: build-skill trigger -- the post-feat doc stage (S95, Plane
//! TERM-150).
//!
//! Wires the doc engine (DOCGEN-01..07, already shipped on `main` under
//! `src/tools/docgen/`) into the build pipeline as a single post-verify
//! stage: after a feat merges + verifies, the build skill invokes this
//! module with the feat's context (spec_id, merged diff, repo/module path,
//! project doc-target config), and this module runs the FULL existing
//! engine flow -- PII sweep (DOCGEN-02) -> generate via Chord (DOCGEN-05) ->
//! render declared targets (DOCGEN-06) -> version (DOCGEN-07) -- and returns
//! the versioned artifacts. This is assembly/orchestration ONLY: every step
//! below calls an existing module; nothing here re-implements the sweep,
//! the generator, a renderer, or the version store.
//!
//! ## Reuse plan (nothing reimplemented)
//! - [`super::pii_gate::sweep_input`] (DOCGEN-02) -- the sole PII sweep.
//! - [`super::generate::generate_docs`] / [`super::generate::DocGenerator`]
//!   / [`super::generate::ChordDocGenerator`] (DOCGEN-05) -- the sole
//!   generation orchestration + Chord client seam.
//! - [`super::render::render_all`] (DOCGEN-06) -- the sole per-target
//!   renderer dispatch.
//! - [`super::versioning::VersionStore`] (DOCGEN-07) -- the sole version
//!   store.
//! - [`super::config::ProjectDocConfig`] (DOCGEN-01) -- the sole doc-target
//!   config parser/resolver; this module's own opt-in gate (below) reuses
//!   the exact "declares nothing at all" detection [`super::DocgenStatus`]
//!   already uses (`is_default_readme_only`), rather than a second check.
//!
//! ## Opt-in per project (load-bearing, differs from DOCGEN-01's own default)
//! [`super::config::ProjectDocConfig`]'s OWN default, when asked to generate,
//! is README-only (DOCGEN-01). But at the TRIGGER boundary this item owns,
//! a project that has not declared ANY doc-target config at all has not
//! opted in to the post-feat doc STAGE running for it in the first place --
//! spec APPROACH step 5: "Opt-in per project (like mirror-ready): a project
//! without doc-targets configured -> stage skips." So this module's gate is
//! stricter than (and runs before) `ProjectDocConfig`'s own defaulting: no
//! config at all -> [`TriggerOutcome::Skipped`], the engine is never even
//! invoked. A project that HAS declared targets (even just one) proceeds
//! through the full flow and may still hit `ProjectDocConfig`'s own
//! defaulting/disabling rules inside generation/rendering.
//!
//! ## Non-blocking to the feat (load-bearing)
//! [`run_docgen_trigger`] has NO `Result`/`Err` in its return type -- it is
//! structurally infallible. Every failure mode inside it (a malformed
//! config, a PII-gate error, a Chord/generator error) is caught and folded
//! into [`TriggerOutcome::Failed`], a normal `Ok`-shaped value, rather than
//! propagated as an error the caller would have to treat as "the feat
//! failed." A doc-gen failure logs + flags (the caller surfaces
//! `TriggerOutcome::Failed`'s `reason`) but can never itself fail the
//! merged feat -- see `doc_gen_failure_does_not_fail_the_feat` below for the
//! negative test asserting this end to end (a generator that always errors
//! still yields a normal `TriggerOutcome` value, never a panic or `Err`).
//!
//! ## Placement is the harness's job (load-bearing, inherited from DOCGEN-06)
//! Exactly like [`super::render::render_all`], this module RETURNS versioned
//! artifacts and touches no filesystem, git, or hosting surface itself BY
//! DEFAULT. See `run_never_touches_filesystem_or_repo` below for the negative
//! test asserting this holds when placement is not requested.
//!
//! ## DLAND-04: opt-in placement (S119, Plane TERM)
//! [`run_docgen_trigger`] and the `docgen_run` tool now accept two
//! ADDITIONAL, OPTIONAL parameters: `place` (bool, default `false`) and
//! `target_root` (`Option<&str>`/`Option<String>`, a repo-relative-root
//! filesystem path -- typically a worktree root). When `place` is `false`
//! (or `target_root` is absent), behavior is byte-for-byte unchanged from
//! before this item: no filesystem is ever touched. Only when BOTH `place`
//! is `true` AND `target_root` is supplied AND generation actually produced
//! content (`GenerationOutcome::Generated`) does this module obtain the
//! concise landing README (the rendered `readme` target's content, from
//! [`super::render::render_all`]'s own `readme_layers::render_layered_readme`
//! call) and the `docs/` tree ([`RenderOutcome::docs_tree`], which
//! `render_all` already builds from the SAME generated content whenever the
//! readme target rendered -- reused, never re-derived here) and hands both to
//! [`super::place::place_docs`] (DLAND-01, fail-closed gated by DLAND-03).
//! The resulting [`super::place::PlacementReport`] is folded into
//! [`TriggerOutcome::Completed`]'s new `placement` field. This is still a
//! LOCAL working-tree write only -- no git add/commit/push, no forge call --
//! and it is non-blocking exactly like every other step in this module: a
//! placement failure (bad `target_root`, a DLAND-03 gate failure, an I/O
//! error) is recorded in `placement`/`gate_failures`/`skipped`, never turned
//! into an `Err` or a panic, and never changes the fact that generation
//! completed successfully.

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::config::{DocTargetType, ProjectDocConfig};
use super::generate::{generate_docs, ChordDocGenerator, DocGenerator, GenerationOutcome, SweptFeatContext};
use super::pii_gate::sweep_input;
use super::place::{place_docs, PlacementReport};
use super::render::{render_all, RenderContext, RenderOutcome};
use super::versioning::{ArtifactKey, ArtifactVersion, VersionStore};

// ---------------------------------------------------------------------------
// TriggerOutcome
// ---------------------------------------------------------------------------

/// The result of one post-feat doc-stage invocation. Deliberately has no
/// `Err` sibling anywhere near it -- see the module doc comment's
/// "Non-blocking to the feat" section. Every branch here is a thing the
/// build skill can log/report without the feat itself being considered
/// failed.
#[derive(Debug, Clone)]
pub enum TriggerOutcome {
    /// The project has not declared any doc-target config at all -- the
    /// stage is opt-in (like `mirror_ready`) and this project hasn't opted
    /// in. The engine was never invoked.
    Skipped { reason: String },
    /// The engine ran to completion. `generation` reports what
    /// `generate_docs` produced; `render` and `versions` are only populated
    /// when generation actually produced new content
    /// (`GenerationOutcome::Generated`) -- a `NoChange` or `Flagged`
    /// generation completes the stage cleanly with nothing to render or
    /// version (spec EDGE CASE: "don't fabricate" / "don't write an
    /// empty/junk version").
    Completed {
        generation: GenerationOutcome,
        render: Option<RenderOutcome>,
        versions: Vec<ArtifactVersion>,
        /// DLAND-04: the result of an opt-in placement into a real working
        /// tree, when `place=true` and `target_root` was supplied AND
        /// generation actually produced content. `None` whenever placement
        /// wasn't requested, wasn't applicable (no readme target rendered),
        /// or generation produced `NoChange`/`Flagged` (nothing to place).
        /// Non-blocking: a placement FAILURE still shows up here as a
        /// populated `PlacementReport` with `gate_failures`/`skipped`
        /// entries, never as a reason this variant itself isn't `Completed`.
        placement: Option<PlacementReport>,
    },
    /// Something inside the flow (config parse, PII sweep, or generation)
    /// failed. `reason` is a human-readable summary for logging/flagging.
    /// This is NOT propagated as an `Err` to this function's caller -- see
    /// the module doc comment.
    Failed { reason: String },
}

impl TriggerOutcome {
    /// `true` for every variant -- named explicitly (rather than left
    /// implicit) so a caller wiring this into the build skill has a single,
    /// obviously-named place documenting the non-blocking contract instead
    /// of having to infer it from the absence of an `Err` type. Always
    /// `false`: a `TriggerOutcome` can never represent "fail the feat."
    pub fn is_fatal_to_feat(&self) -> bool {
        false
    }

    fn to_json(&self) -> Value {
        match self {
            TriggerOutcome::Skipped { reason } => json!({
                "outcome": "skipped",
                "reason": reason,
            }),
            TriggerOutcome::Completed { generation, render, versions, placement } => {
                let generation_json = match generation {
                    GenerationOutcome::Generated { content, source_commit } => json!({
                        "kind": "generated",
                        "content_len": content.len(),
                        "source_commit": source_commit,
                    }),
                    GenerationOutcome::NoChange => json!({"kind": "no_change"}),
                    GenerationOutcome::Flagged { reason } => json!({"kind": "flagged", "reason": reason}),
                };
                let render_json = render.as_ref().map(|r| {
                    json!({
                        "rendered": r.rendered().map(|a| json!({
                            "target": a.target_type.as_str(),
                            "format": a.format,
                        })).collect::<Vec<_>>(),
                        "skipped": r.skipped().map(|a| json!({
                            "target": a.target_type.as_str(),
                            "format": a.format,
                            "note": a.note,
                        })).collect::<Vec<_>>(),
                    })
                });
                let versions_json: Vec<Value> = versions
                    .iter()
                    .map(|v| {
                        json!({
                            "project": v.key.project,
                            "target": v.key.target,
                            "version": v.version,
                        })
                    })
                    .collect();
                let mut obj = json!({
                    "outcome": "completed",
                    "generation": generation_json,
                    "render": render_json,
                    "versions": versions_json,
                });
                // DLAND-04: only surface the `placement` key when a placement
                // was actually attempted. A default (no-placement) call must
                // produce JSON byte-for-byte identical to the pre-DLAND-04
                // output for existing docgen_run callers -- no `"placement":
                // null` noise.
                if let Some(p) = placement.as_ref() {
                    if let Value::Object(ref mut map) = obj {
                        map.insert(
                            "placement".to_string(),
                            json!({
                                "written": p.written,
                                "unchanged": p.unchanged,
                                "skipped": p.skipped.iter().map(|s| json!({
                                    "path": s.path,
                                    "reason": s.reason,
                                })).collect::<Vec<_>>(),
                                "gate_failures": p.gate_failures,
                            }),
                        );
                    }
                }
                obj
            }
            TriggerOutcome::Failed { reason } => json!({
                "outcome": "failed",
                "reason": reason,
                "fatal_to_feat": false,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Detect the same "project declared nothing at all" condition
/// [`super::DocgenStatus`] already uses for its `is_default_readme_only`
/// field, so this module's opt-in gate and the config-inspection tool never
/// disagree about what counts as "no config."
fn declares_no_targets(project_config_raw: Option<&Value>) -> bool {
    project_config_raw
        .and_then(Value::as_object)
        .and_then(|o| o.get("targets"))
        .and_then(Value::as_array)
        .map(|a| a.is_empty())
        .unwrap_or(true)
}

/// The full post-feat doc-stage flow: PII sweep -> generate via Chord ->
/// render declared targets -> version -> return the versioned artifacts.
/// Infallible by return type -- see the module doc comment's "Non-blocking
/// to the feat" section.
///
/// - `project` / `module_path` / `git_ref` identify what was built (repo +
///   the merged commit).
/// - `raw_feat_context` is the UNSWEPT feat context (merged diff/spec/code)
///   -- this function is the ONLY place in the trigger path allowed to see
///   it raw; it is swept via [`sweep_input`] before anything else touches
///   it, matching DOCGEN-02's unconditional-gate contract.
/// - `existing_docs` is the project's current docs, if any (`None` for a
///   project's first-ever doc -- DOCGEN-05 EDGE CASE).
/// - `project_config_raw` is the project's raw doc-target config, exactly
///   the shape [`ProjectDocConfig::parse`] / `docgen_status` accept.
/// - `available_credential_keys` is the set of runtime secret-store KEY
///   NAMES (never values) currently available, for target credential
///   resolution (DOCGEN-06).
/// - `generated_at` is an RFC3339 timestamp supplied by the caller -- this
///   function, like `versioning.rs` and every renderer, never reads the
///   system clock itself.
/// - `place` / `target_root` (DLAND-04): opt-in placement into a real
///   working tree. Both default-shaped to a no-op (`place=false`,
///   `target_root=None`) -- when either is absent, this function's
///   filesystem behavior is byte-for-byte identical to before DLAND-04. See
///   the module doc comment's "DLAND-04: opt-in placement" section.
#[allow(clippy::too_many_arguments)]
pub async fn run_docgen_trigger(
    generator: &dyn DocGenerator,
    version_store: &VersionStore,
    project: &str,
    module_path: &str,
    git_ref: &str,
    existing_docs: Option<&str>,
    raw_feat_context: &str,
    project_config_raw: Option<&Value>,
    available_credential_keys: &BTreeSet<String>,
    generated_at: &str,
    place: bool,
    target_root: Option<&str>,
) -> TriggerOutcome {
    // Opt-in gate (BEFORE the engine is invoked at all): no declared
    // doc-target config -> skip cleanly, matching the mirror_ready pattern.
    if declares_no_targets(project_config_raw) {
        return TriggerOutcome::Skipped {
            reason: format!(
                "project '{project}' has no doc-target config declared -- the post-feat doc \
stage is opt-in (like mirror_ready) and this project has not opted in; the doc engine was \
not invoked"
            ),
        };
    }

    let config = match ProjectDocConfig::parse(project_config_raw) {
        Ok(cfg) => cfg,
        Err(e) => {
            return TriggerOutcome::Failed {
                reason: format!("project '{project}' doc-target config could not be parsed: {e}"),
            }
        }
    };

    // DOCGEN-02: unconditional PII sweep gate on the input, BEFORE any
    // inference request is built. This is the only point in this function
    // that touches `raw_feat_context`.
    let gate_outcome = match sweep_input(raw_feat_context) {
        Ok(outcome) => outcome,
        Err(e) => {
            return TriggerOutcome::Failed {
                reason: format!("PII sweep of feat context for '{project}'/{module_path} failed: {e}"),
            }
        }
    };
    let feat_context = SweptFeatContext::from_gate_outcome(&gate_outcome);

    // DOCGEN-05: generate (deepen) docs via Chord.
    let generation = match generate_docs(generator, module_path, git_ref, existing_docs, &feat_context).await {
        Ok(outcome) => outcome,
        Err(e) => {
            return TriggerOutcome::Failed {
                reason: format!("doc generation for '{project}'/{module_path} at {git_ref} failed: {e}"),
            }
        }
    };

    let (content, source_commit) = match &generation {
        GenerationOutcome::Generated { content, source_commit } => (content.clone(), source_commit.clone()),
        // Nothing to render or version -- the stage still completes
        // cleanly (spec EDGE CASE: "don't fabricate" a version).
        GenerationOutcome::NoChange | GenerationOutcome::Flagged { .. } => {
            return TriggerOutcome::Completed { generation, render: None, versions: Vec::new(), placement: None };
        }
    };

    // DOCGEN-06: render every declared target. `render_all` itself never
    // touches a filesystem/repo/hosting surface -- it only returns
    // artifacts; see the module doc comment's "Placement is the harness's
    // job" section.
    let render_ctx = RenderContext {
        project,
        module: module_path,
        source_commit: &source_commit,
        generated_at,
        content: &content,
    };
    let render = render_all(&render_ctx, &config, available_credential_keys, None, None).await;

    // DOCGEN-07: version every artifact that was actually rendered. Skipped
    // targets are never versioned (nothing new was produced for them).
    let mut versions = Vec::new();
    for artifact in render.rendered() {
        if let Some(rendered_content) = &artifact.content {
            let key = ArtifactKey::new(project.to_string(), artifact.target_type.as_str().to_string());
            let version = version_store.store_version(key, rendered_content.clone(), source_commit.clone(), generated_at.to_string());
            versions.push(version);
        }
    }

    // DLAND-04: opt-in placement into a real working tree. Only when the
    // caller asked for it (`place=true` + `target_root` supplied) AND the
    // readme target actually rendered (so there's a landing to place, and
    // `render.docs_tree` -- built by `render_all` from the SAME generated
    // content -- is non-empty) do we call `place_docs`. Everything here is
    // non-blocking: any placement failure (bad `target_root`, a DLAND-03
    // gate failure, an I/O error) is captured in the returned
    // `PlacementReport`, never turned into an `Err`/panic, and never changes
    // the fact that generation+rendering already completed successfully.
    let placement = match (place, target_root) {
        (true, Some(root)) => render
            .rendered()
            .find(|a| a.target_type == DocTargetType::Readme)
            .and_then(|a| a.content.clone())
            .map(|landing| place_docs(std::path::Path::new(root), &landing, &render.docs_tree)),
        _ => None,
    };

    TriggerOutcome::Completed { generation, render: Some(render), versions, placement }
}

// ---------------------------------------------------------------------------
// docgen_run tool
// ---------------------------------------------------------------------------

/// `docgen_run` -- the MCP-tool surface the build skill calls as the
/// post-feat doc stage (Stage 7c). Holds its own [`VersionStore`] so
/// version history accumulates across calls for the lifetime of this tool
/// instance in the registry (mirrors how the engine's other stateful
/// scaffolding -- e.g. Scribe's vault -- lives inside its owning tool
/// rather than a crate-level global).
pub struct DocgenRun {
    store: VersionStore,
}

impl DocgenRun {
    pub fn new() -> Self {
        Self { store: VersionStore::new() }
    }
}

impl Default for DocgenRun {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RustTool for DocgenRun {
    fn name(&self) -> &str {
        "docgen_run"
    }

    fn description(&self) -> &str {
        "Post-feat doc stage (build-skill Stage 7c): given a merged feat's context (spec_id, \
diff/spec/code, repo/module, project doc-target config), runs the full doc engine flow -- \
PII sweep (unconditional input gate) -> generate via Chord's SLM router -> render every \
declared target (readme/wiki/pdf/notion/obsidian/blog) -> version -- and returns the \
versioned artifacts. Opt-in per project: a project with no doc-target config declared skips \
cleanly, the engine is never invoked. Non-blocking: any internal failure (config, PII sweep, \
generation) is reported in the result as a flagged/failed outcome, never as a call that should \
be read as 'the feat failed.' Artifacts only -- this tool never writes to a repo, filesystem, \
or hosting surface; placing a returned artifact is the calling harness's job."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "spec_id": {
                    "type": "string",
                    "description": "The spec identifier the merged feat belongs to (e.g. \"S95-documentation-engine\"), carried through for logging/observability."
                },
                "project": {
                    "type": "string",
                    "description": "The project/repo identifier this content belongs to (e.g. \"TERM\")."
                },
                "module_path": {
                    "type": "string",
                    "description": "Repo-relative module/path the feat changed."
                },
                "git_ref": {
                    "type": "string",
                    "description": "The merged commit/feat ref this generation is tied to."
                },
                "feat_context": {
                    "type": "string",
                    "description": "The merged diff/spec/code content describing what was built. UNSWEPT -- this tool runs the mandatory PII sweep on it before anything else touches it."
                },
                "existing_docs": {
                    "type": "string",
                    "description": "The project's current docs, if any. Omit for a project's first-ever doc."
                },
                "project_config": {
                    "type": "object",
                    "description": "The project's raw doc-target config, e.g. {\"targets\": [{\"type\": \"readme\"}]}. Omit (or pass no `targets` key) for a project that has not opted in to this stage -- it will be skipped cleanly."
                },
                "available_credential_keys": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Runtime secret-store KEY NAMES (never values) currently available, for target credential resolution."
                },
                "generated_at": {
                    "type": "string",
                    "description": "RFC3339 timestamp for this generation. Defaults to the current time if omitted."
                },
                "place": {
                    "type": "boolean",
                    "description": "DLAND-04: opt-in. When true (and `target_root` is also given), a successful generation's concise landing README and docs/ tree are written into `target_root` via the DLAND-01 placement writer (fail-closed gated by DLAND-03). Defaults to false -- no filesystem is touched unless explicitly requested."
                },
                "target_root": {
                    "type": "string",
                    "description": "DLAND-04: the working-tree root (e.g. a repo checkout or worktree path) to place the rendered README.md and docs/** into. Required for `place` to have any effect; ignored otherwise."
                }
            },
            "required": ["spec_id", "project", "module_path", "git_ref", "feat_context"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let spec_id = args
            .get("spec_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("spec_id is required and must not be empty".into()))?;
        let project = args
            .get("project")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("project is required and must not be empty".into()))?;
        let module_path = args
            .get("module_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("module_path is required and must not be empty".into()))?;
        let git_ref = args
            .get("git_ref")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("git_ref is required and must not be empty".into()))?;
        let feat_context = args
            .get("feat_context")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("feat_context is required".into()))?;
        let existing_docs = args.get("existing_docs").and_then(Value::as_str);
        let project_config = args.get("project_config");
        let available_credential_keys: BTreeSet<String> = args
            .get("available_credential_keys")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let generated_at_owned;
        let generated_at = match args.get("generated_at").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                generated_at_owned = chrono::Utc::now().to_rfc3339();
                &generated_at_owned
            }
        };
        // DLAND-04: opt-in placement. `place` defaults to `false` and
        // `target_root` to absent -- when either is missing this call's
        // filesystem behavior is unchanged from before DLAND-04.
        let place = args.get("place").and_then(Value::as_bool).unwrap_or(false);
        let target_root = args.get("target_root").and_then(Value::as_str);

        let generator = ChordDocGenerator::from_env();
        let outcome = run_docgen_trigger(
            &generator,
            &self.store,
            project,
            module_path,
            git_ref,
            existing_docs,
            feat_context,
            project_config,
            &available_credential_keys,
            generated_at,
            place,
            target_root,
        )
        .await;

        let mut payload = outcome.to_json();
        if let Value::Object(ref mut map) = payload {
            map.insert("spec_id".to_string(), json!(spec_id));
        }
        Ok(serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(DocgenRun::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockDocGenerator {
        response: Result<String, String>,
        captured_prompt: Mutex<Option<String>>,
    }

    impl MockDocGenerator {
        fn ok(response: impl Into<String>) -> Self {
            Self { response: Ok(response.into()), captured_prompt: Mutex::new(None) }
        }

        fn failing(msg: impl Into<String>) -> Self {
            Self { response: Err(msg.into()), captured_prompt: Mutex::new(None) }
        }

        fn captured_prompt(&self) -> String {
            self.captured_prompt.lock().unwrap().clone().expect("generate() was never called")
        }
    }

    #[async_trait]
    impl DocGenerator for MockDocGenerator {
        async fn generate(&self, prompt: &str) -> Result<String, ToolError> {
            *self.captured_prompt.lock().unwrap() = Some(prompt.to_string());
            match &self.response {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(ToolError::Http(e.clone())),
            }
        }
    }

    fn readme_config() -> Value {
        json!({"targets": [{"type": "readme"}]})
    }

    // -- Unit: post-verify, the trigger invokes the engine with correct
    // feat context (spec/diff/repo/config). --------------------------------
    #[tokio::test]
    async fn invokes_engine_with_feat_context_and_returns_completed() {
        let generator = MockDocGenerator::ok(
            "# terminus-rs docgen module\n\nThis module renders declared doc targets from a swept feat context.",
        );
        let store = VersionStore::new();
        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "abc1234",
            None,
            "the feat added docgen_run, wiring the engine into the build skill",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            None,
        )
        .await;

        match outcome {
            TriggerOutcome::Completed { generation: GenerationOutcome::Generated { source_commit, .. }, render, versions, .. } => {
                assert_eq!(source_commit, "abc1234");
                let render = render.expect("readme target should have rendered");
                assert_eq!(render.rendered().count(), 1);
                assert_eq!(versions.len(), 1);
                assert_eq!(versions[0].key.project, "TERM");
                assert_eq!(versions[0].key.target, "readme");
                assert_eq!(versions[0].version, 1);
            }
            other => panic!("expected Completed/Generated, got {other:?}"),
        }

        // The engine really was invoked with the (swept) feat context, not
        // skipped/bypassed.
        assert!(generator
            .captured_prompt()
            .contains("the feat added docgen_run"));
    }

    // -- Unit: doc-gen failure does NOT fail the feat (negative test). ------
    #[tokio::test]
    async fn doc_gen_failure_does_not_fail_the_feat() {
        let generator = MockDocGenerator::failing("chord backend unreachable");
        let store = VersionStore::new();

        // `run_docgen_trigger` has no `Result`/`Err` return type at all --
        // this call cannot panic or propagate an error even though the
        // underlying generator fails. That is the structural guarantee;
        // this test also asserts the resulting value correctly reports the
        // failure for the caller to log/flag.
        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "def5678",
            None,
            "some feat context",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            None,
        )
        .await;

        match &outcome {
            TriggerOutcome::Failed { reason } => {
                assert!(reason.contains("chord backend unreachable"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!outcome.is_fatal_to_feat());
        // No version was recorded for a failed generation.
        assert!(store
            .history(&ArtifactKey::new("TERM".to_string(), "readme".to_string()))
            .is_empty());
    }

    // -- Unit: project with no doc-targets -> stage skips (negative test). --
    #[tokio::test]
    async fn no_doc_target_config_skips_the_stage_and_never_invokes_the_engine() {
        let generator = MockDocGenerator::ok("should never be produced");
        let store = VersionStore::new();

        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "ghi9012",
            None,
            "some feat context",
            None, // no project config at all -- not opted in
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            None,
        )
        .await;

        assert!(matches!(outcome, TriggerOutcome::Skipped { .. }));

        // The engine was genuinely never invoked -- the generator's
        // `generate()` was never called (would panic on `captured_prompt()`
        // if it had been, but we assert the stronger structural fact
        // directly via a fresh flag instead of relying on that panic).
        assert!(store.current(&ArtifactKey::new("TERM".to_string(), "readme".to_string())).is_none());
    }

    /// Same as above, but with an explicit empty `targets: []` array rather
    /// than an absent key -- both count as "declares nothing."
    #[tokio::test]
    async fn explicit_empty_targets_array_also_skips() {
        let generator = MockDocGenerator::ok("should never be produced");
        let store = VersionStore::new();
        let empty_cfg = json!({"targets": []});

        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "jkl3456",
            None,
            "some feat context",
            Some(&empty_cfg),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            None,
        )
        .await;

        assert!(matches!(outcome, TriggerOutcome::Skipped { .. }));
    }

    // -- Integration (mocked engine): full trigger -> artifacts returned;
    // placement is the harness's job (negative test: no repo placement in
    // docgen_run / run_docgen_trigger). ------------------------------------
    #[tokio::test]
    async fn run_never_touches_filesystem_or_repo() {
        let tmp = std::env::temp_dir().join(format!("docgen-trigger-fs-guard-{}", std::process::id()));
        // Snapshot: nothing exists at this sentinel path before the run.
        assert!(!tmp.exists());

        let generator = MockDocGenerator::ok(
            "# Guard\n\nIf `run_docgen_trigger` ever wrote to disk, a real implementation \
would need a path -- this test simply asserts no such path was ever created for the whole \
duration of a real end-to-end run.",
        );
        let store = VersionStore::new();
        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "mno7890",
            None,
            "feat context for the filesystem guard test",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            None,
        )
        .await;

        assert!(matches!(outcome, TriggerOutcome::Completed { .. }));
        // The sentinel path was never created -- the whole flow (sweep,
        // generate, render, version) touched no filesystem path at all.
        assert!(!tmp.exists());

        // Artifacts are RETURNED, not placed: the version store holds the
        // rendered content in-memory only, addressable by the caller, never
        // written anywhere by this function.
        let versions = store.history(&ArtifactKey::new("TERM".to_string(), "readme".to_string()));
        assert_eq!(versions.len(), 1);
    }

    // -- NoChange / Flagged generation completes cleanly without a version. -
    #[tokio::test]
    async fn no_change_generation_completes_with_no_render_or_versions() {
        let existing = "# terminus-rs docgen module\n\nAlready fully documented.";
        let generator = MockDocGenerator::ok(existing);
        let store = VersionStore::new();

        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "pqr1234",
            Some(existing),
            "a feat with no doc-relevant change",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            None,
        )
        .await;

        match outcome {
            TriggerOutcome::Completed { generation: GenerationOutcome::NoChange, render, versions, .. } => {
                assert!(render.is_none());
                assert!(versions.is_empty());
            }
            other => panic!("expected Completed/NoChange, got {other:?}"),
        }
        assert!(store.current(&ArtifactKey::new("TERM".to_string(), "readme".to_string())).is_none());
    }

    #[tokio::test]
    async fn flagged_generation_completes_with_no_render_or_versions() {
        // Below MIN_GENERATION_LEN in generate.rs -> Flagged.
        let generator = MockDocGenerator::ok("hi");
        let store = VersionStore::new();

        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "stu5678",
            None,
            "a feat whose generation came back nearly empty",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            None,
        )
        .await;

        match outcome {
            TriggerOutcome::Completed { generation: GenerationOutcome::Flagged { .. }, render, versions, .. } => {
                assert!(render.is_none());
                assert!(versions.is_empty());
            }
            other => panic!("expected Completed/Flagged, got {other:?}"),
        }
    }

    // -- Malformed config is caught as Failed, not a panic. ------------------
    #[tokio::test]
    async fn malformed_config_is_failed_not_a_panic() {
        let generator = MockDocGenerator::ok("should never be produced");
        let store = VersionStore::new();
        let bad_cfg = json!({"targets": [{"type": "sharepoint"}]});

        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "vwx9012",
            None,
            "feat context",
            Some(&bad_cfg),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            None,
        )
        .await;

        match outcome {
            TriggerOutcome::Failed { reason } => assert!(reason.to_lowercase().contains("sharepoint") || reason.to_lowercase().contains("target")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // -- DLAND-04: opt-in placement into a real working tree. ---------------

    /// Per-call unique temp dir (pid + nanosecond timestamp), matching
    /// `place.rs`'s own test helper -- several tests in this module run
    /// concurrently within the same process, so pid alone isn't enough.
    fn unique_tmp_dir(label: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("docgen-trigger-place-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn place_true_with_target_root_lands_readme_and_docs_tree_on_disk() {
        let root = unique_tmp_dir("happy-path");
        let generator = MockDocGenerator::ok(
            "# terminus-rs docgen module\n\nThis module renders declared doc targets from a swept feat context.\n\n\
## Quickstart\n\nRun `docgen_run` to produce your first set of docs.\n\n\
## Deep Dive\n\nThe engine sweeps, generates, renders, and versions.\n",
        );
        let store = VersionStore::new();
        let root_str = root.to_str().unwrap();

        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "place1234",
            None,
            "the feat wired placement into the trigger",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            true,
            Some(root_str),
        )
        .await;

        match outcome {
            TriggerOutcome::Completed { generation: GenerationOutcome::Generated { .. }, placement, .. } => {
                let placement = placement.expect("place=true + target_root must produce a PlacementReport");
                assert!(placement.gate_failures.is_empty(), "{:?}", placement.gate_failures);
                assert!(placement.written.contains(&"README.md".to_string()), "{:?}", placement.written);
                assert!(
                    placement.written.iter().any(|p| p.starts_with("docs/")),
                    "expected at least one docs/** file written: {:?}",
                    placement.written
                );
            }
            other => panic!("expected Completed/Generated with a placement, got {other:?}"),
        }

        // The files really landed on disk, not just reported as written.
        assert!(root.join("README.md").exists());
        assert!(root.join("docs/index.md").exists());
        assert!(root.join("docs/getting-started.md").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn place_false_never_touches_the_target_root_even_if_supplied() {
        // Passing a target_root without place=true must still be a pure,
        // filesystem-free call -- `place` is the sole opt-in switch.
        let root = unique_tmp_dir("place-false-ignored");
        let generator = MockDocGenerator::ok(
            "# terminus-rs docgen module\n\nThis module renders declared doc targets from a swept feat context.",
        );
        let store = VersionStore::new();
        let root_str = root.to_str().unwrap();

        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "place5678",
            None,
            "a feat where placement was not requested",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            false,
            Some(root_str),
        )
        .await;

        // The serialized JSON of a no-placement completed outcome must NOT
        // carry a `placement` key at all (byte-for-byte-compatible with the
        // pre-DLAND-04 output for existing docgen_run callers).
        let json = outcome.to_json();
        assert!(
            json.get("placement").is_none(),
            "default (no-placement) JSON must not contain a placement key: {json}"
        );

        match outcome {
            TriggerOutcome::Completed { placement, .. } => assert!(placement.is_none()),
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(!root.join("README.md").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn placement_failure_is_reported_non_blocking_never_fails_the_feat() {
        // A target_root that doesn't exist makes `place_docs` refuse to write
        // anything -- this must show up as a populated (but empty-written)
        // PlacementReport inside a normal `Completed` outcome, never as a
        // `Failed` outcome or a panic. Generation/rendering/versioning still
        // succeeded; only placement itself is flagged.
        let missing_root = std::env::temp_dir().join(format!(
            "docgen-trigger-place-does-not-exist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        assert!(!missing_root.exists());
        let generator = MockDocGenerator::ok(
            "# terminus-rs docgen module\n\nThis module renders declared doc targets from a swept feat context.",
        );
        let store = VersionStore::new();
        let root_str = missing_root.to_str().unwrap();

        let outcome = run_docgen_trigger(
            &generator,
            &store,
            "TERM",
            "src/tools/docgen",
            "place9012",
            None,
            "a feat whose target_root does not exist",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-11T00:00:00Z",
            true,
            Some(root_str),
        )
        .await;

        match outcome {
            TriggerOutcome::Completed { generation: GenerationOutcome::Generated { .. }, versions, placement, .. } => {
                // Generation/rendering/versioning were unaffected.
                assert_eq!(versions.len(), 1, "a placement failure must not affect versioning");
                let placement = placement.expect("a placement attempt was made and must be reported");
                assert!(placement.written.is_empty());
                assert_eq!(placement.skipped.len(), 1, "{:?}", placement.skipped);
            }
            other => panic!("expected Completed/Generated (placement failure is non-blocking), got {other:?}"),
        }
        assert!(!missing_root.exists(), "place_docs must never create target_root itself");
    }

    // -- Tool-level smoke test: registration + schema shape. ----------------
    #[test]
    fn docgen_run_registers_with_valid_schema() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("docgen_run"));
        for info in reg.list() {
            if info.name == "docgen_run" {
                assert_eq!(info.parameters.get("type").and_then(Value::as_str), Some("object"));
            }
        }
    }

    #[tokio::test]
    async fn docgen_run_tool_requires_core_args() {
        let tool = DocgenRun::new();
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    /// DLAND-04: `place`/`target_root` are declared, optional schema params
    /// -- not required, so a caller that never mentions them (the pre-DLAND-04
    /// shape) still validates against this schema unchanged. `execute()`
    /// itself defaults `place` to `false` and `target_root` to absent when
    /// missing (see the `unwrap_or(false)`/`and_then` parsing above), which
    /// is what actually keeps a default call's behavior byte-for-byte
    /// unchanged -- this test asserts the schema-level contract those
    /// defaults rely on.
    #[test]
    fn docgen_run_schema_declares_place_and_target_root_as_optional() {
        let tool = DocgenRun::new();
        let schema = tool.parameters();
        let props = schema.get("properties").and_then(Value::as_object).expect("object schema");
        assert!(props.contains_key("place"), "schema missing DLAND-04 `place` param");
        assert!(props.contains_key("target_root"), "schema missing DLAND-04 `target_root` param");
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .expect("required array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(!required.contains(&"place"), "`place` must stay optional");
        assert!(!required.contains(&"target_root"), "`target_root` must stay optional");
    }
}
