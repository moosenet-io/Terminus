//! DLAND-05: one-shot backfill -- migrate an already-bloated repo README,
//! operator-reviewed (S119, spec `S119-docgen-landing-hierarchy`, Plane
//! project TERM).
//!
//! [`backfill_readme`] migrates a repo's hand-grown mega-README into the
//! concise landing + `docs/` hierarchy in ONE guarded pass: generate against
//! the existing README as context (reusing [`super::trigger::run_docgen_trigger`]'s
//! full sweep -> generate -> render flow) -> run the no-loss guard
//! ([`super::preserve::check_preservation`], DLAND-02) -> run the landing
//! gates ([`super::readme_layers::check_landing_length`]/
//! [`super::readme_layers::check_landing_links`], DLAND-03) -> place into a
//! WORKING COPY via [`super::place::place_docs`] (DLAND-01, itself fail-closed
//! gated by DLAND-03) -> emit a [`BackfillReport`] summary.
//!
//! ## Nothing reimplemented
//! Every step below calls an existing module -- this file is pure
//! orchestration:
//! - [`super::trigger::run_docgen_trigger`] (DLAND-04's own building block --
//!   PII sweep, generation, per-target render, versioning) is called with
//!   `place=false`: this module decides placement itself, only AFTER the
//!   no-loss guard has cleared, rather than letting the trigger place
//!   unconditionally.
//! - [`super::preserve::check_preservation`] (DLAND-02) is the sole no-loss
//!   guard -- no second coverage check.
//! - [`super::readme_layers::check_landing_length`] /
//!   [`super::readme_layers::check_landing_links`] (DLAND-03) are the sole
//!   landing lints -- surfaced here for the summary, and re-enforced
//!   fail-closed inside [`super::place::place_docs`] regardless.
//! - [`super::place::place_docs`] (DLAND-01) is the sole placement writer --
//!   atomic, idempotent, working-tree-only, no git/network.
//!
//! ## First cutover is operator-reviewed, never auto-committed
//! This module NEVER runs git (no add/commit/push) and makes NO forge
//! (Plane/Gitea/GitHub) call of any kind -- it only reads `target_root`'s
//! current `README.md` (if any) and writes a working copy via `place_docs`.
//! The result is handed to the normal build pipeline (worktree diff -> review
//! -> merge) for an operator to bless, exactly like every other change to a
//! tracked repo.
//!
//! ## Never place when the no-loss guard flags a drop (load-bearing)
//! If [`super::preserve::check_preservation`] reports ANY missing section,
//! [`backfill_readme`] returns immediately with the missing sections listed
//! in [`BackfillReport::missing`] and calls [`super::place::place_docs`] not
//! at all -- the working copy (and any existing `README.md` on disk) is left
//! completely untouched. A human must confirm the drop (by adjusting the
//! source content and re-running, or by accepting the loss out of band)
//! before this tool will ever place a cutover for that repo. See
//! `backfill_refuses_to_place_when_a_section_is_dropped` below for the
//! negative test asserting the old README is byte-for-byte untouched on
//! disk in this case.
//!
//! ## Idempotent, re-runnable
//! Like [`super::place::place_docs`] itself, re-running this against an
//! already-migrated repo either produces `GenerationOutcome::NoChange` (the
//! generator has nothing new to say) or a placement whose `written` list is
//! empty (byte-identical content already on disk) -- never a spurious diff.
//!
//! ## Edge cases (spec)
//! - A repo already concise (`GenerationOutcome::NoChange`) -> no-op,
//!   `summary` says so, `placed = false`.
//! - A repo with no `README.md` at `target_root` -> treated as first-doc
//!   generation (`existing_docs = None`); there is nothing to preserve, so
//!   the no-loss guard trivially passes (nothing to lose).

use std::collections::BTreeSet;
use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::config::DocTargetType;
use super::generate::{ChordDocGenerator, DocGenerator, GenerationOutcome};
use super::place::{place_docs, README_PATH};
use super::preserve::{check_preservation, Section};
use super::readme_layers::{check_landing_length, check_landing_links, landing_line_count};
use super::trigger::{run_docgen_trigger, TriggerOutcome};
use super::versioning::VersionStore;

// ---------------------------------------------------------------------------
// BackfillReport
// ---------------------------------------------------------------------------

/// The result of one [`backfill_readme`] call: what the OLD README looked
/// like, what the migration would produce (or did produce), and whether it
/// was actually placed into the working copy. This is DATA for an operator
/// to review before the normal build pipeline carries the working-copy
/// change through review/merge -- this module never commits/pushes/acts on
/// its own beyond writing the working copy itself.
#[derive(Debug, Clone, PartialEq)]
pub struct BackfillReport {
    /// Whether `target_root/README.md` existed before this call.
    pub old_readme_existed: bool,
    /// Line count of the OLD `README.md`, or `0` if none existed.
    pub old_readme_lines: usize,
    /// Line count of the NEW concise landing README, if generation actually
    /// produced one (`None` for `NoChange`/`Flagged`/`Skipped`/`Failed`, or
    /// when no `readme` target rendered at all).
    pub new_landing_lines: Option<usize>,
    /// [`super::preserve::PreservationReport::coverage_ratio`] -- `1.0` when
    /// there was nothing to lose (no old README, or generation didn't run).
    pub coverage_ratio: f32,
    /// Every OLD section the no-loss guard could not find the substance of
    /// in the new landing/docs. NON-EMPTY here means [`Self::placed`] is
    /// `false` -- see the module doc comment's "Never place when the
    /// no-loss guard flags a drop" section.
    pub missing: Vec<Section>,
    /// Repo-relative `docs/**` paths actually written this call (excludes
    /// `README.md` itself -- see [`Self::new_landing_lines`] for the
    /// README's own before/after). Empty whenever [`Self::placed`] is
    /// `false`, or when placement was a byte-identical no-op re-run.
    pub docs_files_created: Vec<String>,
    /// `true` iff the concise landing + docs tree were actually written (or
    /// already matched byte-for-byte) into `target_root`. `false` whenever
    /// the no-loss guard flagged a drop, a landing gate failed, generation
    /// produced nothing new, or the stage didn't run at all.
    pub placed: bool,
    /// DLAND-03 landing lint failures (over-length and/or dangling `docs/`
    /// link targets), surfaced here even though [`super::place::place_docs`]
    /// enforces the same gate fail-closed on its own. Non-empty only when
    /// [`Self::placed`] is `false` for this reason specifically.
    pub gate_failures: Vec<String>,
    /// A short, human-readable summary of what happened -- for logging and
    /// for an operator deciding whether to carry the resulting working-copy
    /// diff through the normal review/merge pipeline.
    pub summary: String,
}

impl BackfillReport {
    fn no_op(old_readme_existed: bool, old_readme_lines: usize, summary: String) -> Self {
        Self {
            old_readme_existed,
            old_readme_lines,
            new_landing_lines: None,
            coverage_ratio: 1.0,
            missing: Vec::new(),
            docs_files_created: Vec::new(),
            placed: false,
            gate_failures: Vec::new(),
            summary,
        }
    }
}

// ---------------------------------------------------------------------------
// backfill_readme
// ---------------------------------------------------------------------------

/// Migrate `target_root`'s current `README.md` (if any) into a concise
/// landing + `docs/` tree, in one guarded pass. See the module doc comment
/// for the full flow and the "never place on a dropped section" guarantee.
///
/// - `target_root`: the working-copy root (typically a worktree) whose
///   `README.md` is read as `existing_docs`/no-loss-guard input, and where a
///   successful migration is placed. The ONLY filesystem access this
///   function performs directly is that one read; everything else (the
///   actual placement) goes through [`super::place::place_docs`].
/// - Every other parameter mirrors [`super::trigger::run_docgen_trigger`]'s
///   own (this function calls it with `place=false`, deciding placement
///   itself only after the no-loss guard clears).
#[allow(clippy::too_many_arguments)]
pub async fn backfill_readme(
    generator: &dyn DocGenerator,
    version_store: &VersionStore,
    project: &str,
    module_path: &str,
    git_ref: &str,
    raw_feat_context: &str,
    project_config_raw: Option<&Value>,
    available_credential_keys: &BTreeSet<String>,
    generated_at: &str,
    target_root: &Path,
) -> BackfillReport {
    let old_readme = match std::fs::read_to_string(target_root.join(README_PATH)) {
        Ok(s) => Some(s),
        // Genuinely absent -> a first-ever doc, nothing to preserve, safe to place.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        // The README EXISTS but could not be read (non-UTF8, permissions, I/O
        // error). Treating this as "no old README" would let the backfill
        // OVERWRITE content it never got to preserve -- the exact no-loss
        // violation this tool exists to prevent. Refuse to place and hand it to
        // the operator to inspect, rather than silently clobbering it.
        Err(e) => {
            return BackfillReport {
                old_readme_existed: true,
                old_readme_lines: 0,
                new_landing_lines: None,
                coverage_ratio: 0.0,
                missing: Vec::new(),
                docs_files_created: Vec::new(),
                placed: false,
                gate_failures: Vec::new(),
                summary: format!(
                    "refused: the existing README.md at the target could not be read ({e}); \
not overwriting unreadable content -- an operator must inspect it before backfilling"
                ),
            };
        }
    };
    let old_readme_existed = old_readme.is_some();
    let old_readme_lines = old_readme.as_deref().map(landing_line_count).unwrap_or(0);

    let outcome = run_docgen_trigger(
        generator,
        version_store,
        project,
        module_path,
        git_ref,
        old_readme.as_deref(),
        raw_feat_context,
        project_config_raw,
        available_credential_keys,
        generated_at,
        false,
        None,
    )
    .await;

    match outcome {
        TriggerOutcome::Skipped { reason } => {
            BackfillReport::no_op(old_readme_existed, old_readme_lines, format!("backfill skipped: {reason}"))
        }
        TriggerOutcome::Failed { reason } => BackfillReport::no_op(
            old_readme_existed,
            old_readme_lines,
            format!("backfill failed before any placement was attempted: {reason}"),
        ),
        TriggerOutcome::Completed { generation, render, .. } => match generation {
            GenerationOutcome::NoChange => BackfillReport::no_op(
                old_readme_existed,
                old_readme_lines,
                "repo is already concise -- generation produced no doc-relevant change vs the \
current README; nothing to migrate"
                    .to_string(),
            ),
            GenerationOutcome::Flagged { reason } => BackfillReport::no_op(
                old_readme_existed,
                old_readme_lines,
                format!("generation was flagged, nothing to migrate: {reason}"),
            ),
            GenerationOutcome::Generated { .. } => {
                let render = match render {
                    Some(r) => r,
                    None => {
                        return BackfillReport::no_op(
                            old_readme_existed,
                            old_readme_lines,
                            "generation completed but no render was produced -- nothing to migrate"
                                .to_string(),
                        )
                    }
                };

                let landing = render
                    .rendered()
                    .find(|a| a.target_type == DocTargetType::Readme)
                    .and_then(|a| a.content.clone());

                let landing = match landing {
                    Some(l) => l,
                    None => {
                        return BackfillReport::no_op(
                            old_readme_existed,
                            old_readme_lines,
                            "no readme target rendered for this project's doc-target config -- \
nothing to migrate"
                                .to_string(),
                        )
                    }
                };

                let old_readme_str = old_readme.clone().unwrap_or_default();
                let preservation = check_preservation(&old_readme_str, &landing, &render.docs_tree);

                if !preservation.missing.is_empty() {
                    let count = preservation.missing.len();
                    return BackfillReport {
                        old_readme_existed,
                        old_readme_lines,
                        new_landing_lines: Some(landing_line_count(&landing)),
                        coverage_ratio: preservation.coverage_ratio,
                        missing: preservation.missing,
                        docs_files_created: Vec::new(),
                        placed: false,
                        gate_failures: Vec::new(),
                        summary: format!(
                            "no-loss guard flagged {count} section(s) whose substance was not found \
in the generated landing/docs -- placement refused; an operator must confirm the drop (or \
adjust the source content) before this tool will place a cutover for this repo"
                        ),
                    };
                }

                // Surface the DLAND-03 landing gates in the summary -- these are
                // ALSO enforced fail-closed inside `place_docs` below regardless
                // of whether we check them here first.
                let mut gate_failures = Vec::new();
                if let Err(e) = check_landing_length(&landing) {
                    gate_failures.push(e);
                }
                if let Err(dangling) = check_landing_links(&landing, &render.docs_tree) {
                    gate_failures.extend(dangling);
                }
                if !gate_failures.is_empty() {
                    return BackfillReport {
                        old_readme_existed,
                        old_readme_lines,
                        new_landing_lines: Some(landing_line_count(&landing)),
                        coverage_ratio: preservation.coverage_ratio,
                        missing: Vec::new(),
                        docs_files_created: Vec::new(),
                        placed: false,
                        gate_failures,
                        summary: "generated landing failed a DLAND-03 landing lint gate -- placement \
refused, nothing written"
                            .to_string(),
                    };
                }

                let placement = place_docs(target_root, &landing, &render.docs_tree);

                if !placement.gate_failures.is_empty() {
                    // Should not normally diverge from the pre-check above, but
                    // `place_docs` is the source of truth -- surface whatever it
                    // reports rather than assuming agreement.
                    return BackfillReport {
                        old_readme_existed,
                        old_readme_lines,
                        new_landing_lines: Some(landing_line_count(&landing)),
                        coverage_ratio: preservation.coverage_ratio,
                        missing: Vec::new(),
                        docs_files_created: Vec::new(),
                        placed: false,
                        gate_failures: placement.gate_failures,
                        summary: "generated landing failed the DLAND-03 placement gate -- nothing \
written"
                            .to_string(),
                    };
                }

                let placed = !placement.written.is_empty() || !placement.unchanged.is_empty();
                let docs_files_created: Vec<String> = placement
                    .written
                    .iter()
                    .filter(|p| p.as_str() != README_PATH)
                    .cloned()
                    .collect();

                let summary = if !placement.skipped.is_empty() {
                    format!(
                        "placement partially refused ({} entr(y/ies) skipped): {:?}",
                        placement.skipped.len(),
                        placement.skipped
                    )
                } else if placed {
                    format!(
                        "migrated README.md from {} line(s) to a {} line concise landing plus {} \
docs/** page(s); no-loss coverage {:.0}%",
                        old_readme_lines,
                        landing_line_count(&landing),
                        docs_files_created.len(),
                        preservation.coverage_ratio * 100.0
                    )
                } else {
                    "placement attempted but nothing was written or changed".to_string()
                };

                BackfillReport {
                    old_readme_existed,
                    old_readme_lines,
                    new_landing_lines: Some(landing_line_count(&landing)),
                    coverage_ratio: preservation.coverage_ratio,
                    missing: Vec::new(),
                    docs_files_created,
                    placed,
                    gate_failures: Vec::new(),
                    summary,
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------
// docgen_backfill tool
// ---------------------------------------------------------------------------

/// `docgen_backfill` -- the MCP-tool surface for a one-shot, operator-blessed
/// README-to-hierarchy migration (DLAND-05). Holds its own [`VersionStore`],
/// matching [`super::trigger::DocgenRun`]'s posture (version history
/// accumulates across calls for the lifetime of this tool instance).
pub struct DocgenBackfill {
    store: VersionStore,
}

impl DocgenBackfill {
    pub fn new() -> Self {
        Self { store: VersionStore::new() }
    }
}

impl Default for DocgenBackfill {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RustTool for DocgenBackfill {
    fn name(&self) -> &str {
        "docgen_backfill"
    }

    fn description(&self) -> &str {
        "One-shot backfill (DLAND-05): migrate an already-bloated repo README (Terminus, Chord, \
Muse, lumina-constellation, ...) into the concise landing + docs/ hierarchy in ONE guarded pass \
-- generate against the existing README as context, run the no-loss guard (DLAND-02), run the \
landing gates (DLAND-03), and place into a WORKING COPY at target_root for operator review. \
Refuses to place anything (README.md and every docs/** file, together) if the no-loss guard \
flags any dropped section, or if the generated landing fails a landing lint gate -- an operator \
must confirm before a real cutover lands. NEVER commits, pushes, or makes any Plane/Gitea/GitHub \
call -- working-copy write only; the normal build pipeline (review, merge) carries the result \
from there."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "spec_id": {
                    "type": "string",
                    "description": "The spec identifier this backfill belongs to (e.g. \"S119-docgen-landing-hierarchy\"), carried through for logging/observability."
                },
                "project": {
                    "type": "string",
                    "description": "The project/repo identifier this content belongs to (e.g. \"TERM\")."
                },
                "module_path": {
                    "type": "string",
                    "description": "Repo-relative module/path being migrated (often the repo root, e.g. \".\")."
                },
                "git_ref": {
                    "type": "string",
                    "description": "The commit/ref this backfill generation is tied to."
                },
                "feat_context": {
                    "type": "string",
                    "description": "The context describing this backfill (e.g. a note that this is a first-cutover migration). UNSWEPT -- this tool runs the mandatory PII sweep on it before anything else touches it."
                },
                "project_config": {
                    "type": "object",
                    "description": "The project's raw doc-target config, e.g. {\"targets\": [{\"type\": \"readme\"}]}. Must declare a \"readme\" target for a backfill to have anything to place."
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
                "target_root": {
                    "type": "string",
                    "description": "The working-copy root (typically a worktree) whose current README.md is read as migration input, and where a successful migration is placed. Required."
                }
            },
            "required": ["spec_id", "project", "module_path", "git_ref", "feat_context", "target_root"]
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
        let target_root = args
            .get("target_root")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("target_root is required and must not be empty".into()))?;
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

        let generator = ChordDocGenerator::from_env();
        let report = backfill_readme(
            &generator,
            &self.store,
            project,
            module_path,
            git_ref,
            feat_context,
            project_config,
            &available_credential_keys,
            generated_at,
            Path::new(target_root),
        )
        .await;

        let payload = json!({
            "spec_id": spec_id,
            "old_readme_existed": report.old_readme_existed,
            "old_readme_lines": report.old_readme_lines,
            "new_landing_lines": report.new_landing_lines,
            "coverage_ratio": report.coverage_ratio,
            "missing": report.missing.iter().map(|s| json!({
                "heading": s.heading,
                "reason": s.reason,
            })).collect::<Vec<_>>(),
            "docs_files_created": report.docs_files_created,
            "placed": report.placed,
            "gate_failures": report.gate_failures,
            "summary": report.summary,
        });
        Ok(serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(DocgenBackfill::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockDocGenerator {
        response: String,
        captured_prompt: Mutex<Option<String>>,
    }

    impl MockDocGenerator {
        fn new(response: impl Into<String>) -> Self {
            Self { response: response.into(), captured_prompt: Mutex::new(None) }
        }
    }

    #[async_trait]
    impl DocGenerator for MockDocGenerator {
        async fn generate(&self, prompt: &str) -> Result<String, ToolError> {
            *self.captured_prompt.lock().unwrap() = Some(prompt.to_string());
            Ok(self.response.clone())
        }
    }

    fn readme_config() -> Value {
        json!({"targets": [{"type": "readme"}]})
    }

    /// Per-call unique temp dir (pid + nanosecond timestamp) -- several
    /// tests in this module run concurrently, so pid alone isn't enough,
    /// matching `place.rs`'s/`trigger.rs`'s own test helper.
    fn unique_tmp_dir(label: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("docgen-backfill-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── Happy path: preserved multi-section README -> migrated + placed ──

    #[tokio::test]
    async fn backfill_migrates_a_preserved_multi_section_readme_and_places_it() {
        let root = unique_tmp_dir("happy-path");
        let old_readme = "# Widget\n\n\
## Install\n\nRun `cargo install widget_cli` to get started.\n\n\
## Configuration\n\nSet `WIDGET_PORT=8080` in your environment.\n";
        std::fs::write(root.join("README.md"), old_readme).unwrap();

        // The generated content carries BOTH old sections' stable tokens:
        // `cargo install widget_cli` under Quickstart (lands directly on the
        // concise landing's Quick Start section) and `WIDGET_PORT=8080`
        // under Reference (lands in docs/reference/api.md via the Diátaxis
        // split) -- so the no-loss guard finds everything covered.
        let generator = MockDocGenerator::new(
            "# Widget\n\nA widget factory that builds and configures widgets fast.\n\n\
## Quickstart\n\nRun `cargo install widget_cli` to get your first widget building.\n\n\
## Reference\n\nSet `WIDGET_PORT=8080` in your environment to configure the listen port.\n",
        );
        let store = VersionStore::new();

        let report = backfill_readme(
            &generator,
            &store,
            "TERM",
            ".",
            "backfill1",
            "one-shot backfill of the bloated README",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(report.old_readme_existed);
        assert!(report.old_readme_lines > 0);
        assert!(report.missing.is_empty(), "expected nothing missing: {:?}", report.missing);
        assert_eq!(report.coverage_ratio, 1.0);
        assert!(report.gate_failures.is_empty(), "{:?}", report.gate_failures);
        assert!(report.placed, "expected placement to happen: {}", report.summary);
        let new_lines = report.new_landing_lines.expect("a landing was generated");
        assert!(new_lines <= super::super::readme_layers::LANDING_MAX_LINES);
        assert!(!report.docs_files_created.is_empty());

        // Really landed on disk, not just reported.
        assert!(root.join("README.md").exists());
        assert!(root.join("docs/index.md").exists());
        assert!(root.join("docs/reference/api.md").exists());
        let new_readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert_ne!(new_readme, old_readme, "README.md should have been replaced with the concise landing");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Negative: a dropped section refuses placement, old README untouched ─

    #[tokio::test]
    async fn backfill_refuses_to_place_when_a_section_is_dropped() {
        let root = unique_tmp_dir("dropped-section");
        let old_readme = "# Widget\n\n\
## Install\n\nRun `cargo install widget_cli` to get started.\n\n\
## Telemetry\n\nSet `WIDGET_TELEMETRY_ENDPOINT` to opt in to the `submit_metrics()` reporter.\n";
        std::fs::write(root.join("README.md"), old_readme).unwrap();

        // The generation only covers Install's tokens -- Telemetry's env var
        // and function name appear nowhere in the new corpus.
        let generator = MockDocGenerator::new(
            "# Widget\n\nA widget factory.\n\n\
## Quickstart\n\nRun `cargo install widget_cli` to begin.\n",
        );
        let store = VersionStore::new();

        let report = backfill_readme(
            &generator,
            &store,
            "TERM",
            ".",
            "backfill2",
            "one-shot backfill dropping a section",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(!report.placed, "must never place when a section is dropped: {}", report.summary);
        assert_eq!(report.missing.len(), 1, "{:?}", report.missing);
        assert_eq!(report.missing[0].heading, "Telemetry");
        assert!(report.coverage_ratio < 1.0);
        assert!(report.docs_files_created.is_empty());

        // The old README on disk is completely untouched.
        let on_disk = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert_eq!(on_disk, old_readme, "a dropped section must never overwrite the old README");
        assert!(!root.join("docs").exists(), "no docs/ tree should have been written either");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── An EXISTING but unreadable README is never overwritten ──────────

    #[tokio::test]
    async fn backfill_refuses_to_place_when_the_existing_readme_is_unreadable() {
        // codex review: a present-but-unreadable README (here: invalid UTF-8)
        // must NOT be treated like an absent one -- overwriting it would lose
        // content the no-loss guard never got to inspect.
        let root = unique_tmp_dir("unreadable-readme");
        // Invalid UTF-8 bytes: read_to_string fails with InvalidData, not NotFound.
        std::fs::write(root.join("README.md"), [0xff, 0xfe, 0x00, 0x9f, 0x28]).unwrap();
        let before = std::fs::read(root.join("README.md")).unwrap();

        let generator = MockDocGenerator::new(
            "# Widget\n\n## Quickstart\n\nRun `cargo install widget_cli`.\n",
        );
        let store = VersionStore::new();

        let report = backfill_readme(
            &generator,
            &store,
            "TERM",
            ".",
            "backfill-unreadable",
            "one-shot backfill against an unreadable README",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(!report.placed, "must never place over an unreadable README: {}", report.summary);
        assert!(report.old_readme_existed);
        assert!(report.summary.contains("refused"), "summary: {}", report.summary);
        // The original bytes are byte-for-byte untouched; no docs/ tree written.
        assert_eq!(std::fs::read(root.join("README.md")).unwrap(), before);
        assert!(!root.join("docs").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Gate failure: an oversized landing refuses placement ────────────

    #[tokio::test]
    async fn backfill_refuses_to_place_on_a_landing_gate_failure() {
        let root = unique_tmp_dir("gate-failure");
        // No README.md at target_root -- first-doc case, nothing to lose, so
        // the no-loss guard trivially clears and the ONLY thing that can
        // block placement here is the DLAND-03 landing-length gate.
        let oversized_quickstart = "line\n".repeat(220);
        let generator = MockDocGenerator::new(format!(
            "# Widget\n\nA widget factory.\n\n## Quickstart\n\n{oversized_quickstart}"
        ));
        let store = VersionStore::new();

        let report = backfill_readme(
            &generator,
            &store,
            "TERM",
            ".",
            "backfill3",
            "one-shot backfill whose generation is oversized",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(!report.old_readme_existed);
        assert!(report.missing.is_empty(), "nothing to lose with no old README: {:?}", report.missing);
        assert!(!report.placed, "an oversized landing must never be placed: {}", report.summary);
        assert!(!report.gate_failures.is_empty());
        assert!(!root.join("README.md").exists(), "gate failure must write nothing at all");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Edge: already-concise repo (NoChange) is a clean no-op ───────────

    #[tokio::test]
    async fn backfill_is_a_noop_when_the_repo_is_already_concise() {
        let root = unique_tmp_dir("already-concise");
        let existing = "# Widget\n\nAlready fully migrated and concise.";
        std::fs::write(root.join("README.md"), existing).unwrap();

        // Generator echoes the existing content back verbatim -> NoChange.
        let generator = MockDocGenerator::new(existing.to_string());
        let store = VersionStore::new();

        let report = backfill_readme(
            &generator,
            &store,
            "TERM",
            ".",
            "backfill4",
            "backfill against an already-concise repo",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(!report.placed);
        assert!(report.new_landing_lines.is_none());
        assert!(report.missing.is_empty());
        assert!(report.summary.to_lowercase().contains("already concise") || report.summary.to_lowercase().contains("no doc-relevant change"));

        // Untouched on disk.
        let on_disk = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert_eq!(on_disk, existing);

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Edge: no doc-target config declared -> skip cleanly ──────────────

    #[tokio::test]
    async fn backfill_skips_cleanly_when_project_has_not_opted_in() {
        let root = unique_tmp_dir("no-config");
        let generator = MockDocGenerator::new("should never be produced".to_string());
        let store = VersionStore::new();

        let report = backfill_readme(
            &generator,
            &store,
            "TERM",
            ".",
            "backfill5",
            "backfill against a project with no doc-target config",
            None,
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(!report.placed);
        assert!(report.summary.to_lowercase().contains("skip"));

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Tool-level: registration + schema shape ──────────────────────────

    #[test]
    fn docgen_backfill_registers_with_valid_schema() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("docgen_backfill"));
        for info in reg.list() {
            if info.name == "docgen_backfill" {
                assert_eq!(info.parameters.get("type").and_then(Value::as_str), Some("object"));
                let required: Vec<&str> = info
                    .parameters
                    .get("required")
                    .and_then(Value::as_array)
                    .expect("required array")
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                assert!(required.contains(&"target_root"));
            }
        }
    }

    #[tokio::test]
    async fn docgen_backfill_tool_requires_target_root() {
        let tool = DocgenBackfill::new();
        let result = tool
            .execute(json!({
                "spec_id": "S119-docgen-landing-hierarchy",
                "project": "TERM",
                "module_path": ".",
                "git_ref": "abc123",
                "feat_context": "some context"
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }
}
