//! Docgen: the sovereign, in-house documentation engine (DOCGEN-01 scaffold,
//! S95, Plane TERM-143). Replaces Mintlify: triggered after every feat by
//! the build skill (DOCGEN-08, later), it reads what was actually built
//! (the merged diff + spec), deepens a project's documentation, and renders
//! variable output artifacts per project (README, wiki, PDF, Notion/
//! Obsidian notes, dev blog) as declared in that project's doc-target
//! config.
//!
//! ## Scope of THIS item (DOCGEN-01)
//! Module skeleton + core types + registration + the per-project doc-target
//! config schema ([`config`]) ONLY. No generation, rendering, or versioning
//! yet -- those land in DOCGEN-05/06/07. This item's single registered tool
//! ([`DocgenStatus`]) is a read-only config-inspection tool, mirroring how
//! `src/scribe/mod.rs` (SCRB-01) shipped `scribe_status` alongside its own
//! scaffold stubs.
//!
//! ## Reuse plan (reference only -- NOT reimplemented here)
//! Later docgen items reuse existing modules rather than duplicating them:
//!   - `crate::scribe::{inspect, vault}` -- reading a real worktree's
//!     current docs (DOCGEN-05) and writing versioned artifacts into the
//!     Obsidian-compatible knowledge vault (DOCGEN-07) reuse Scribe's
//!     existing worktree-inspection and vault-write plumbing rather than a
//!     second implementation of either.
//!   - `crate::github::pii` -- the PII sweep gate DOCGEN-02 puts in front of
//!     every inference request reuses the same sweep engine the git-public
//!     mirror already runs, not a second scanner.
//!   - Chord owns model routing (DOCGEN-03); this module never picks a
//!     model itself, per the S95 design overview's seam.
//!
//! ## Registration
//! Docgen registers on Chord's core `register_all()` (`src/registry.rs`, via
//! `crate::tools::register` -> `docgen::register`) -- the SAME single
//! registration path every other core tool (`plane`/`gitea`/`github`/
//! `scribe`) uses. There is no separate "personal-only" registry for it.
//!
//! ## The landing README's own output contract (DGRICH-05, S119)
//! This engine's OWN generated landing page follows a fixed 8-section
//! skeleton, deterministically assembled by [`readme_layers::build_landing_body`]
//! from [`prompts::RepoIdentity`] (Pass 1) + [`repo_facts::RepoFacts`]
//! (Pass 0) + the real emitted docs tree ([`render::docs_tree::DocsTreeFile`]):
//! (1) hero (`<h1>` + tagline + [`readme_layers::fact_row`] -- a single
//! computed line like `Rust · 410 modules · 53 MCP tools · 11.9k KG nodes ·
//! analyzed a1b2c3d`, which REPLACES the old hardcoded shields.io badge
//! row), (2) "What is `<name>`", (3) Architecture (the real derived
//! diagram, `subsystem_architecture_mermaid_source`, never the generic
//! `Client -> Core -> Output` template except as the explicit no-KG
//! fallback), (4) Subsystems/Features table (every row links to its real
//! `docs/reference/<subsystem>.md`), (5) Quick Start (points at
//! `docs/getting-started.md`, never inlined), (6) Documentation index
//! (generated from the ACTUAL emitted tree, one row per real page with its
//! real first-paragraph one-liner), (7) At a Glance (computed
//! function/struct/trait/module counts, workspace members, binaries --
//! never invented), (8) Contributing + License. The landing is gated
//! fail-closed by BOTH [`readme_layers::check_landing_length`]
//! (`LANDING_MAX_LINES = 300`) and [`readme_layers::check_landing_substance`]
//! (`LANDING_MIN_SUBSTANTIVE_LINES = 80`, counting non-blank/non-chrome
//! lines) -- a landing that is all chrome (the pre-DGRICH-05 ~50-61 line
//! bare failure mode) or that inlines everything (the pre-revision
//! 2000+-line bloat failure mode) is a structural gate failure either way.
//! This is additive to, and does not replace, the legacy per-module
//! landing ([`readme_layers::render_layered_readme`]/`build_layered_body`)
//! other renderers in this crate still call for projects with no
//! repo-level KG grounding.
//!
//! ## Config options, `docgen_facts`, and gate wiring (DGRICH-09, S119)
//! [`config::ProjectDocConfig`] gains three optional per-project knobs for
//! the repo-level rich pipeline: `subsystem_page_cap` (default 16, how many
//! per-subsystem reference pages Pass 2 generates), `landing_budget`
//! (default 300, this project's OWN concise-landing ceiling, checked in
//! ADDITION to the engine-wide `LANDING_MAX_LINES` cap), and `identity_hint`
//! (an operator tagline that wins over Pass 1's generated one when
//! present). [`DocgenFacts`] (`docgen_facts`) is a new READ-ONLY preview
//! tool: it dry-runs [`repo_facts::build_repo_facts`] for a project's
//! checkout and returns a grounding summary (subsystem rollup, node/edge
//! counts, `kg_grounded`, entry points, hotspots) with NO writes, so an
//! operator can sanity-check grounding before a real `docgen_backfill`.
//! Finally, the substance floor ([`readme_layers::check_landing_substance`]),
//! the generic-diagram lint ([`diagram::is_generic_placeholder`], via
//! [`quality::check_landing_diagram`]), and an identity-lint backstop
//! ([`quality::check_landing_identity`], re-running the same
//! anti-latch/symbol-existence lints Pass 1 already gates generation with)
//! are now folded into [`place::place_repo_docs`]'s own fail-closed gate
//! set, the single door every repo-level caller places a landing through --
//! superseding the DGRICH-07 stopgap that ran a subset of these checks
//! inline in `trigger::run_repo_level_trigger` before calling `place_docs`
//! directly.
//!
//! ## Secrets (S95 Pre-flight: `OPENROUTER_API_KEY`, `NOTION_TOKEN`, etc.)
//! This scaffold reads no secret VALUES at all -- see [`config`]'s module
//! doc comment. Vault key NAMES a target may need are named by
//! [`config::DocTargetType::credential_key`]; resolving them to actual
//! values via `vault::manager().get()` / `SecretManager::get()` is deferred
//! to the generation/render items that actually call out to Chord or a
//! target's API.

pub mod backfill;
pub mod changelog;
pub mod config;
pub mod crate_graph;
pub mod diagram;
pub mod drift;
pub mod generate;
pub mod mismatch;
pub mod pii_gate;
pub mod preserve;
pub mod place;
pub mod prompts;
pub mod quality;
pub mod readme_layers;
pub mod render;
pub mod repo_facts;
pub mod search_index;
pub mod svg_assets;
pub mod trigger;
pub mod versioning;

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub use config::{
    DocTargetConfig, DocTargetType, ProjectDocConfig, ResolvedDocTarget, DEFAULT_LANDING_BUDGET,
    DEFAULT_SUBSYSTEM_PAGE_CAP,
};
pub use generate::{
    all_symbol_names, generate_docs, generate_docs_for_module, generate_repo_docs, ChordDocGenerator,
    DocGenerator, GenerationOutcome, PassRecord, RepoDocsOutcome, SweptFeatContext,
};
pub use pii_gate::{sweep_input, sweep_input_for_routing, PiiGateOutcome, RoutingDestination};
pub use prompts::{
    anti_latch_lint, build_guides_prompt, build_repo_identity_prompt, build_subsystem_page_prompt,
    honest_command_lint, parse_file_blocks, parse_repo_identity, symbol_existence_lint,
    FeatureRow, GuideTopic, PromptParseError, RepoIdentity, SubsystemBrief,
};
pub use preserve::{check_preservation, PreservationReport, Section as PreservationSection};
// `Section` is re-exported under a `Preservation`-prefixed alias above to
// avoid ambiguity with any future generic `Section` type this module might
// re-export from elsewhere; `docgen::preserve::Section` remains the
// canonical unqualified name.
pub use place::{place_docs, place_repo_docs, PlacementReport, SkippedEntry, README_PATH};
pub use quality::{
    check_landing_diagram, check_landing_identity, lint_prose, run_quality_gate, JudgeScores,
    LintResult, ProseLintConfig, QualityScore, QualityScoreStore, QualityVerdict,
    DEFAULT_QUALITY_THRESHOLD,
};
pub use readme_layers::{
    build_landing_body, check_landing_length, check_landing_substance, deepen_layers, fact_row,
    landing_line_count, parse_layers, render_diataxis_set, render_layered_readme,
    substantive_line_count, DiataxisArtifact, DiataxisMode, ParsedLayers, CHANGELOG_PATH,
    DOCS_ARCHITECTURE_PATH, DOCS_GETTING_STARTED_PATH, DOCS_GUIDES_INDEX_PATH, DOCS_INDEX_PATH,
    DOCS_REFERENCE_INDEX_PATH, LANDING_MAX_LINES, LANDING_MIN_SUBSTANTIVE_LINES, LICENSE_PATH,
};
pub use render::docs_tree::{build_docs_tree, build_repo_docs_tree, first_paragraph, DocsTreeFile};
pub use render::{render_all, RenderContext, RenderOutcome, RenderedArtifact};
pub use repo_facts::{
    build_repo_facts, AtlasGraphSource, BinTarget, ConfigSurface, EntryPoints, GraphSource,
    LegacySection, ProseAnchors, RepoFacts, RepoScale, Subsystem, SubsystemEdge, SubsystemGraph,
    SymbolRef,
};
pub use trigger::{run_docgen_trigger, DocgenRun, TriggerOutcome};
pub use backfill::{backfill_readme, BackfillReport, DocgenBackfill};

/// `docgen_status` -- report how the doc engine would interpret a project's
/// declared (or absent) doc-target config: which targets it declares (or
/// the README-only default), and, when a set of available credential key
/// names is supplied, which targets are currently enabled vs. disabled for
/// missing credentials. Read-only; never mutates anything, never generates
/// or renders content -- purely a config-inspection tool for this scaffold
/// item.
pub struct DocgenStatus;

#[async_trait]
impl RustTool for DocgenStatus {
    fn name(&self) -> &str {
        "docgen_status"
    }

    fn description(&self) -> &str {
        "Report the doc engine's interpretation of a project's per-project doc-target \
config: which targets it declares (readme/wiki/pdf/notion/obsidian/blog), the \
README-only default applied when a project declares none, and -- if a list of \
available credential key names is supplied -- which declared targets are \
enabled vs. disabled for a missing credential. Config-inspection only; this \
scaffold item generates/renders nothing."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_config": {
                    "type": "object",
                    "description": "The project's raw doc-target config, e.g. {\"targets\": [{\"type\": \"readme\"}, {\"type\": \"notion\", \"options\": {\"database_id\": \"...\"}}]}. Omit (or pass no `targets` key) to see the README-only default that applies to an unconfigured project."
                },
                "available_credential_keys": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of runtime secret-store KEY NAMES (never values) currently known to be available, e.g. [\"NOTION_TOKEN\"]. When supplied, the response also reports which declared targets are enabled vs. disabled-for-missing-credential."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_config = args.get("project_config");
        let is_default = project_config
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("targets"))
            .and_then(Value::as_array)
            .map(|a| a.is_empty())
            .unwrap_or(true);

        let cfg = ProjectDocConfig::parse(project_config)?;

        let available: BTreeSet<String> = args
            .get("available_credential_keys")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        let resolved = cfg.resolve(&available);
        let targets_json: Vec<Value> = resolved
            .iter()
            .map(|r| {
                json!({
                    "type": r.target_type.as_str(),
                    "enabled": r.enabled,
                    "hint": r.hint,
                })
            })
            .collect();

        Ok(serde_json::to_string_pretty(&json!({
            "is_default_readme_only": is_default,
            "targets": targets_json,
        }))
        .unwrap_or_else(|_| "{}".to_string()))
    }
}

/// `docgen_facts` (DGRICH-09) -- a READ-ONLY preview tool: dry-run
/// [`build_repo_facts`] (DGRICH-01) for a project's checkout so an operator
/// can sanity-check what the rich pipeline's grounding actually looks like
/// (subsystem rollup, node/edge counts, entry points, hotspots,
/// `kg_grounded`) BEFORE committing to a real `docgen_backfill`. Performs
/// NO writes, NO generation, and NO placement -- it is a pure
/// read-then-summarize wrapper around the same deterministic, zero-LLM-call
/// builder the repo-level trigger path (DGRICH-07) already uses; this tool
/// adds no second grounding derivation.
pub struct DocgenFacts;

#[async_trait]
impl RustTool for DocgenFacts {
    fn name(&self) -> &str {
        "docgen_facts"
    }

    fn description(&self) -> &str {
        "Read-only preview: dry-run the rich doc engine's RepoFacts grounding (DGRICH-01) for \
a project's checkout WITHOUT generating or placing anything, so an operator can sanity-check \
what the pipeline actually sees (subsystem rollup with node counts, kg_grounded, entry points, \
top hotspots, edge-matrix size) before running docgen_backfill. Performs no writes of any kind; \
degrades cleanly (kg_grounded: false, no fabricated numbers) for a project with no Atlas KG \
entry, exactly like the real pipeline would."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "The project/repo identifier to look up in the Atlas KG store (e.g. \"TERM\")."
                },
                "target_root": {
                    "type": "string",
                    "description": "The repo checkout root to scan for entry points/config surface/prose anchors (e.g. a worktree path). Also accepted as `repo_path` for callers that prefer that name."
                },
                "repo_path": {
                    "type": "string",
                    "description": "Alias for `target_root`. Either name is accepted; `target_root` wins if both are supplied."
                },
                "git_ref": {
                    "type": "string",
                    "description": "The ref this preview is stamped with (display-only metadata; does not change what is derived). Defaults to \"HEAD\" if omitted."
                }
            },
            "required": ["project", "target_root"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project = args
            .get("project")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("project is required and must not be empty".into()))?;
        let target_root = args
            .get("target_root")
            .and_then(Value::as_str)
            .or_else(|| args.get("repo_path").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArgument(
                    "target_root (or repo_path) is required and must not be empty".into(),
                )
            })?;
        let git_ref = args
            .get("git_ref")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("HEAD");

        let graph_source = AtlasGraphSource::from_env();
        let facts = build_repo_facts(&graph_source, std::path::Path::new(target_root), project, git_ref)?;

        Ok(serde_json::to_string_pretty(&facts_summary(&facts)).unwrap_or_else(|_| "{}".to_string()))
    }
}

/// The JSON summary [`DocgenFacts`] returns -- deliberately a SUMMARY (node
/// counts, names, top-N lists), not the full identity/subsystem slices
/// [`RepoFacts::identity_slice`]/[`RepoFacts::subsystem_slice`] build for an
/// actual generation prompt; an operator sanity-checking grounding wants an
/// at-a-glance shape, not the exact swept prompt payload.
fn facts_summary(facts: &RepoFacts) -> Value {
    json!({
        "project_id": facts.project_id,
        "git_ref": facts.git_ref,
        "kg_grounded": facts.kg_grounded,
        "scale": {
            "node_count": facts.scale.node_count,
            "edge_count": facts.scale.edge_count,
            "by_kind": facts.scale.by_kind,
            "hotspots": facts.scale.hotspots.iter().map(|s| json!({
                "id": s.id,
                "kind": s.kind,
                "path": s.path,
                "rank": s.rank,
            })).collect::<Vec<_>>(),
        },
        "subsystems": facts.subsystems.iter().map(|s| json!({
            "name": s.name,
            "source_dir": s.source_dir,
            "node_count": s.node_count,
            "is_misc": s.is_misc,
            "top_symbols": s.top_symbols.iter().map(|sym| sym.id.clone()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "edge_matrix_size": facts.edge_matrix.edges.len(),
        "entry_points": {
            "bin_targets": facts.entry_points.bin_targets.iter().map(|b| json!({
                "name": b.name,
                "path": b.path,
            })).collect::<Vec<_>>(),
            "workspace_members": facts.entry_points.workspace_members,
            "entrypoint_symbols": facts.entry_points.entrypoint_symbols,
        },
        "config_surface_var_count": facts.config_surface.env_var_names.len(),
        "old_readme_section_count": facts.old_readme_sections.len(),
    })
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Docgen tools into the registry. Called from
/// `crate::tools::register` (`src/tools/mod.rs`), itself called from
/// `register_all()` (`src/registry.rs`) -- the same core registration path
/// `plane`/`gitea`/`github`/`scribe` use. Never called from any
/// personal-only path.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(DocgenStatus));
    let _ = registry.register(Box::new(DocgenFacts));
    mismatch::register(registry);
    changelog::register(registry);
    drift::register(registry);
    trigger::register(registry);
    backfill::register(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_TOOL_NAMES: &[&str] = &[
        "docgen_status",
        "docgen_facts",
        "docgen_mismatch_detect",
        "docgen_generate_changelog",
        "docgen_drift_check",
        "docgen_run",
        "docgen_backfill",
    ];

    #[test]
    fn registers_expected_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), EXPECTED_TOOL_NAMES.len());
        for name in EXPECTED_TOOL_NAMES {
            assert!(reg.contains(name), "missing tool: {name}");
        }
    }

    #[test]
    fn every_tool_has_a_valid_object_schema() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        for info in reg.list() {
            assert_eq!(
                info.parameters.get("type").and_then(Value::as_str),
                Some("object"),
                "tool {} parameters() must be a JSON Schema object",
                info.name
            );
        }
    }

    #[tokio::test]
    async fn docgen_status_reports_readme_only_default_with_no_args() {
        let tool = DocgenStatus;
        let out = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["is_default_readme_only"], json!(true));
        assert_eq!(parsed["targets"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["targets"][0]["type"], json!("readme"));
        assert_eq!(parsed["targets"][0]["enabled"], json!(true));
    }

    #[tokio::test]
    async fn docgen_status_reports_declared_targets() {
        let tool = DocgenStatus;
        let out = tool
            .execute(json!({
                "project_config": {"targets": [{"type": "readme"}, {"type": "wiki"}]}
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["is_default_readme_only"], json!(false));
        assert_eq!(parsed["targets"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn docgen_status_reports_disabled_target_for_missing_credential() {
        let tool = DocgenStatus;
        let out = tool
            .execute(json!({
                "project_config": {"targets": [{"type": "notion"}]}
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["targets"][0]["type"], json!("notion"));
        assert_eq!(parsed["targets"][0]["enabled"], json!(false));
        assert!(parsed["targets"][0]["hint"]
            .as_str()
            .unwrap()
            .contains("NOTION_TOKEN"));
    }

    /// Negative test: an unknown target type surfaces as a tool error, not
    /// a panic/crash.
    #[tokio::test]
    async fn docgen_status_returns_clear_error_for_unknown_target_type() {
        let tool = DocgenStatus;
        let result = tool
            .execute(json!({
                "project_config": {"targets": [{"type": "sharepoint"}]}
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    // ── DGRICH-09: docgen_facts ────────────────────────────────────────

    fn unique_facts_tmp_dir(label: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("docgen-facts-test-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Acceptance: a project with no Atlas KG entry degrades cleanly to
    /// `kg_grounded: false`, never an error -- and the empty checkout
    /// (no Cargo.toml) still produces a valid (empty) facts summary.
    #[tokio::test]
    async fn docgen_facts_degrades_cleanly_for_a_project_with_no_kg_entry() {
        let root = unique_facts_tmp_dir("no-kg");
        let tool = DocgenFacts;

        let out = tool
            .execute(json!({
                "project": "DOCGEN-FACTS-TEST-NO-SUCH-PROJECT",
                "target_root": root.to_string_lossy(),
            }))
            .await
            .expect("docgen_facts must not error for an ungrounded project");

        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["kg_grounded"], json!(false));
        assert_eq!(parsed["scale"]["node_count"], json!(0));
        assert_eq!(parsed["subsystems"].as_array().unwrap().len(), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    /// `docgen_facts` performs NO writes: the target_root's contents are
    /// byte-for-byte identical (empty) before and after the call.
    #[tokio::test]
    async fn docgen_facts_performs_no_writes() {
        let root = unique_facts_tmp_dir("no-writes");
        let tool = DocgenFacts;

        let before: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
        assert!(before.is_empty());

        let _ = tool
            .execute(json!({
                "project": "DOCGEN-FACTS-TEST-NO-WRITES",
                "target_root": root.to_string_lossy(),
            }))
            .await
            .unwrap();

        let after: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
        assert!(after.is_empty(), "docgen_facts must never write into target_root: {after:?}");
        assert!(!root.join("README.md").exists());
        assert!(!root.join("docs").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    /// `repo_path` is accepted as an alias for `target_root`.
    #[tokio::test]
    async fn docgen_facts_accepts_repo_path_as_an_alias_for_target_root() {
        let root = unique_facts_tmp_dir("repo-path-alias");
        let tool = DocgenFacts;

        let out = tool
            .execute(json!({
                "project": "DOCGEN-FACTS-TEST-ALIAS",
                "repo_path": root.to_string_lossy(),
            }))
            .await
            .expect("repo_path must be accepted as an alias for target_root");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["kg_grounded"], json!(false));

        std::fs::remove_dir_all(&root).ok();
    }

    /// Negative test: a missing required `project` is a clear tool error,
    /// not a panic.
    #[tokio::test]
    async fn docgen_facts_requires_project() {
        let tool = DocgenFacts;
        let result = tool.execute(json!({"target_root": "/tmp"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    /// Negative test: a missing `target_root`/`repo_path` is a clear tool
    /// error, not a panic.
    #[tokio::test]
    async fn docgen_facts_requires_target_root_or_repo_path() {
        let tool = DocgenFacts;
        let result = tool.execute(json!({"project": "TERM"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    fn docgen_facts_registers_with_a_valid_object_schema() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("docgen_facts"));
        for info in reg.list() {
            if info.name == "docgen_facts" {
                assert_eq!(info.parameters.get("type").and_then(Value::as_str), Some("object"));
                let required: Vec<&str> = info
                    .parameters
                    .get("required")
                    .and_then(Value::as_array)
                    .expect("required array")
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                assert!(required.contains(&"project"));
            }
        }
    }
}
