//! DGRICH-08: invert backfill -- the rich, KG-grounded pipeline is now the
//! PRIMARY product; verbatim relocation of the old README is a no-loss
//! BACKSTOP only (S119, spec `S119-dgrich-rich-doc-generator`, design
//! `fable-docgen-redesign.md` §5, Plane project TERM).
//!
//! ## History: DLAND-05 -> DLAND-RELOC -> DGRICH-08
//! [`backfill_readme`] originally (DLAND-05) asked an LLM to rewrite the
//! whole README, which was lossy by construction (an LLM asked for a concise
//! landing summarizes). DLAND-RELOC fixed the loss by giving up on
//! generation for the `docs/` tree entirely: the LLM produced only the
//! landing's hero/quick-start, and EVERY old `## ` section was copied
//! verbatim into its own `docs/reference/<slug>.md` page. That guaranteed
//! no-loss, but it also guaranteed every sub-page was exactly as good as the
//! old bloated README's section -- unorganized, unexplained, not
//! reference-shaped -- because generation was never really in the loop for
//! the `docs/` tree at all.
//!
//! DGRICH-08 inverts this again, all the way this time: [`backfill_readme`]
//! now runs the SAME rich, KG-grounded pipeline DGRICH-07's repo-level
//! trigger mode runs (`RepoFacts` -> `generate_repo_docs` -> `build_landing_body`
//! -> `build_repo_docs_tree`) as the PRIMARY product. The old README is fed
//! in as `RepoFacts`' own Pass-0 input #7 (`old_readme_sections`, "legacy
//! claims to verify") -- it is grounding material for the generator, not the
//! output. Verbatim relocation survives, but ONLY as the no-loss BACKSTOP:
//! after generation, [`super::preserve::check_preservation`] runs against
//! the generated landing + docs tree, and any old section whose substance
//! generation did NOT cover is relocated VERBATIM (byte-exact, via the same
//! [`old_readme_parts`] byte-offset slicer DLAND-RELOC introduced) to
//! `docs/legacy/<slug>.md`, linked from `reference/index.md`
//! ([`super::render::docs_tree::build_repo_docs_tree`]'s `legacy_pages`
//! parameter). No-loss stays true by construction -- coverage is always
//! 1.0 -- but now by EXCEPTION, not as the whole output.
//!
//! ## Reuse plan (nothing reimplemented)
//! - [`super::repo_facts::build_repo_facts`] (DGRICH-01) -- the sole
//!   deterministic grounding-layer builder; the old README already reaches it
//!   as `old_readme_sections` via [`super::preserve::split_old_sections`].
//! - [`super::generate::generate_repo_docs`] (DGRICH-03) -- the sole
//!   Passes-1-3 orchestrator over the unchanged `DocGenerator` seam.
//! - [`super::readme_layers::build_landing_body`] (DGRICH-05) -- the sole
//!   rich-landing assembler.
//! - [`super::render::docs_tree::build_repo_docs_tree`] (DGRICH-06) -- the
//!   sole rich docs-tree renderer; its `legacy_pages` parameter is exactly
//!   what this module populates.
//! - [`super::preserve::check_preservation`] (DLAND-02) -- the sole no-loss
//!   guard, run TWICE here: once to find generation's coverage gap, once
//!   (backstop-verified, never just trusted) after legacy relocation closes
//!   it.
//! - [`old_readme_parts`]/[`slugify`] -- this module's own byte-offset
//!   slicer, kept EXACTLY as DLAND-RELOC built it (byte-exact verbatim
//!   slices are still what a legacy page needs), now invoked only for the
//!   sections the no-loss guard actually flags, not every section.
//! - [`super::place::place_docs`] (DLAND-01) -- the sole placement writer.
//!
//! ## First cutover is operator-reviewed, never auto-committed
//! Unchanged: this module never runs git and makes no Plane/Gitea/GitHub
//! call -- it only reads `target_root`'s current `README.md` (if any) and
//! writes a working copy via `place_docs`, for the normal build pipeline
//! (worktree diff -> review -> merge) to carry from there.
//!
//! ## Safe fallback when generation is flagged/degraded
//! When the identity pass never succeeds (Chord unreachable, a parse/lint
//! violation surviving retry), [`super::generate::generate_repo_docs`]
//! returns `identity: None` and this module assembles a minimal, honest
//! landing rather than fabricating one. Because that minimal landing covers
//! almost nothing, essentially every old section is uncovered by generation
//! and therefore relocates to the legacy backstop -- exactly today's
//! (DLAND-RELOC) all-verbatim behavior, reached here as an emergent
//! consequence of the SAME coverage check, not a special case.
//!
//! ## No-loss is proven, never merely trusted
//! Even though legacy relocation makes coverage 1.0 by construction, the
//! no-loss guard is re-run against the FINAL landing + docs tree (including
//! the legacy pages) before placement -- "never trust a guarantee is
//! self-enforcing when a cheap, already-existing check can verify it for
//! free" (the same posture DLAND-RELOC's own module doc comment stated).
//!
//! ## Idempotent, re-runnable
//! Like [`super::place::place_docs`] itself, re-running this against an
//! already-migrated repo produces a placement whose `written` list is empty
//! (byte-identical content already on disk) -- never a spurious diff.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::diagram::{is_generic_placeholder, subsystem_architecture_mermaid_source};
use super::generate::{generate_repo_docs, DocGenerator, FallbackDocGenerator};
use super::pii_gate::sweep_input;
use super::place::{place_docs, README_PATH};
use super::preserve::{check_preservation, Section};
use super::readme_layers::{build_landing_body, check_landing_substance, fact_row, landing_line_count};
use super::render::docs_tree::{build_repo_docs_tree, DocsTreeFile};
use super::repo_facts::{build_repo_facts, AtlasGraphSource, GraphSource, RepoFacts};
use super::trigger::declares_no_targets;
use super::versioning::{ArtifactKey, VersionStore};

// ---------------------------------------------------------------------------
// No-loss backstop: byte-offset slicer (DLAND-RELOC, unchanged) + labeling
// ---------------------------------------------------------------------------

/// Turn a heading (or the "no headings" fallback label) into a filesystem-
/// and URL-safe slug: lowercase, non-alphanumeric runs collapsed to a single
/// `-`, leading/trailing `-` trimmed. Never returns an empty string for
/// non-empty input containing at least one alphanumeric character; callers
/// handle the fully-degenerate case (a heading with NO alphanumeric
/// characters at all) with a fallback label before calling this.
fn slugify(heading: &str) -> String {
    let mut slug = String::with_capacity(heading.len());
    let mut last_was_dash = false;
    for c in heading.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// If `trimmed` (a line with leading whitespace already removed) opens or closes
/// a code fence, return its marker `(char, run_length)` -- a run of at least 3
/// backticks or tildes. Otherwise `None`.
fn fence_marker(trimmed: &str) -> Option<(char, usize)> {
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == ch).count();
    if len >= 3 {
        Some((ch, len))
    } else {
        None
    }
}

/// Split the OLD README into (verbatim preamble, [(heading, VERBATIM source
/// slice)]) using EXACT byte offsets. Each section's slice is the original
/// source from its `## ` line through just before the next `## ` line (or
/// EOF), copied byte-for-byte -- so a relocated legacy page contains the
/// section's exact authored heading, blank lines, and body, not a
/// reconstruction from parsed fields (which would normalise whitespace /
/// heading formatting). Deliberately distinct from
/// [`super::preserve::split_old_sections`] (the line-based, whitespace-
/// trimming parser the no-loss guard uses to COMPARE tokens) precisely
/// because relocation needs byte-exact source spans; both agree on where
/// `## ` sections begin, tracking fenced code blocks so a literal `## `
/// line inside a fence is never mistaken for a real section boundary.
fn old_readme_parts(old: &str) -> (String, Vec<(String, String)>) {
    let mut starts: Vec<(usize, String)> = Vec::new();
    let mut offset = 0usize;
    let mut fence: Option<(char, usize)> = None;
    for line in old.split_inclusive('\n') {
        let trimmed = line.trim_start();
        // CommonMark: a code fence may be indented at most 3 spaces; 4+ spaces
        // of indentation is an indented code block, not a fence marker.
        let indent = line.len() - trimmed.len();
        let marker = if indent <= 3 { fence_marker(trimmed) } else { None };
        if let Some((ch, len)) = marker {
            match fence {
                None => fence = Some((ch, len)),
                Some((open_ch, open_len)) => {
                    if ch == open_ch && len >= open_len {
                        fence = None;
                    }
                    // else: a different/short marker line -- still inside the fence.
                }
            }
        } else if fence.is_none() {
            if let Some(rest) = trimmed.strip_prefix("## ") {
                starts.push((offset, rest.trim().to_string()));
            }
        }
        offset += line.len();
    }
    if starts.is_empty() {
        // No `## ` headings anywhere: the whole document is one verbatim
        // section (no separate preamble).
        if old.trim().is_empty() {
            return (String::new(), Vec::new());
        }
        return (String::new(), vec![(String::new(), old.to_string())]);
    }
    let preamble = old[..starts[0].0].trim().to_string();
    let mut sections = Vec::with_capacity(starts.len());
    for i in 0..starts.len() {
        let start = starts[i].0;
        let end = if i + 1 < starts.len() { starts[i + 1].0 } else { old.len() };
        sections.push((starts[i].1.clone(), old[start..end].to_string()));
    }
    (preamble, sections)
}

/// Every OLD-README section, labeled EXACTLY the way
/// [`check_preservation`] labels its `missing`/`covered` entries (same
/// duplicate-heading disambiguation, same order), paired with the
/// filesystem-safe slug [`old_readme_parts`]' sections get assigned in
/// original document order and each section's byte-exact verbatim content.
/// Building this labeling in lockstep with `check_preservation`'s own is
/// what lets a [`Section::heading`] reported as `missing` be matched back to
/// its verbatim source unambiguously -- see [`backfill_readme_with_graph_source`]
/// for the filter that does that matching.
fn label_and_slug_old_sections(old_readme: &str) -> Vec<(String, String, String)> {
    let (_preamble, sections) = old_readme_parts(old_readme);

    let mut total_counts: HashMap<&str, usize> = HashMap::new();
    for (heading, _) in &sections {
        *total_counts.entry(heading.as_str()).or_insert(0) += 1;
    }

    let mut seen_counts: HashMap<&str, usize> = HashMap::new();
    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(sections.len());

    for (heading, content) in &sections {
        let occurrence = {
            let c = seen_counts.entry(heading.as_str()).or_insert(0);
            *c += 1;
            *c
        };
        let is_duplicate = total_counts.get(heading.as_str()).copied().unwrap_or(0) > 1;

        let label = if heading.trim().is_empty() {
            "(whole document)".to_string()
        } else if is_duplicate {
            format!("{heading} (#{occurrence})")
        } else {
            heading.clone()
        };

        let heading_label_for_slug = if heading.trim().is_empty() { "Overview".to_string() } else { heading.clone() };
        let base_slug = {
            let s = slugify(&heading_label_for_slug);
            if s.is_empty() { "section".to_string() } else { s }
        };
        let slug_count = slug_counts.entry(base_slug.clone()).or_insert(0);
        *slug_count += 1;
        let slug = if *slug_count == 1 { base_slug } else { format!("{base_slug}-{slug_count}") };

        out.push((label, slug, content.clone()));
    }

    out
}

/// A safe, honest landing for when the identity pass (Pass 1) never
/// succeeds -- own copy of the same fallback
/// [`super::trigger`]'s repo-level trigger uses (that module keeps its copy
/// private; duplicated here rather than exposed cross-module for a ~15-line
/// helper). Never fabricates a `RepoIdentity`, never unwraps/panics.
fn minimal_landing(facts: &RepoFacts, docs_tree: &[DocsTreeFile]) -> String {
    let mut out = String::new();
    out.push_str(&format!("<h1 align=\"center\">{}</h1>\n\n", facts.project_id));
    out.push_str(
        "<p align=\"center\"><em>Documentation generation did not complete this round -- see \
the pass ledger for details.</em></p>\n\n",
    );
    out.push_str(&format!("<p align=\"center\">{}</p>\n\n", fact_row(facts)));
    out.push_str("---\n\n");
    out.push_str("## Documentation\n\n");
    if docs_tree.is_empty() {
        out.push_str("_No documentation pages were generated this round._\n");
    } else {
        out.push_str("| Page |\n|---|\n");
        for f in docs_tree {
            out.push_str(&format!("| {} |\n", f.path));
        }
    }
    out.push_str("\n## Contributing\n\nSee the project's build pipeline docs for the contribution process.\n\n");
    out.push_str("## License\n\nSee LICENSE.\n");
    out
}

// ---------------------------------------------------------------------------
// BackfillReport
// ---------------------------------------------------------------------------

/// The result of one [`backfill_readme`] call: what the OLD README looked
/// like, what the rich pipeline produced, how much of that the model itself
/// covered vs. how much fell back to the verbatim legacy backstop, and
/// whether it was actually placed into the working copy. This is DATA for an
/// operator to review before the normal build pipeline carries the
/// working-copy change through review/merge -- this module never
/// commits/pushes/acts on its own beyond writing the working copy itself.
#[derive(Debug, Clone, PartialEq)]
pub struct BackfillReport {
    /// Whether `target_root/README.md` existed before this call.
    pub old_readme_existed: bool,
    /// Line count of the OLD `README.md`, or `0` if none existed.
    pub old_readme_lines: usize,
    /// Line count of the NEW rich landing README, when the pipeline actually
    /// assembled one (always, once the opt-in gate and the initial reads
    /// pass -- even a fully flagged generation still assembles a
    /// [`minimal_landing`]). `None` only for skip/refusal outcomes that never
    /// reached generation at all.
    pub new_landing_lines: Option<usize>,
    /// [`super::preserve::PreservationReport::coverage_ratio`] of the FINAL
    /// landing + docs tree (i.e. AFTER the legacy backstop closed any
    /// generation gap) -- `1.0` whenever nothing was lost, which is the
    /// no-loss guarantee this tool exists to prove by construction.
    pub coverage_ratio: f32,
    /// Every OLD section the no-loss guard could not find the substance of
    /// even AFTER legacy relocation. Should be empty in practice (relocation
    /// makes coverage 1.0 by construction) -- kept as the negative-safety-net
    /// report, never silently dropped. Non-empty here means [`Self::placed`]
    /// is `false`.
    pub missing: Vec<Section>,
    /// Repo-relative `docs/**` paths actually written this call (excludes
    /// `README.md` itself). Empty whenever [`Self::placed`] is `false`, or
    /// when placement was a byte-identical no-op re-run.
    pub docs_files_created: Vec<String>,
    /// `true` iff the rich landing + docs tree were actually written (or
    /// already matched byte-for-byte) into `target_root`. `false` whenever a
    /// no-loss/landing/placement gate withheld the cutover, or the stage
    /// didn't run at all (skip/refusal).
    pub placed: bool,
    /// Landing-gate failures (substance floor, generic-diagram lint, or the
    /// DLAND-03 length/link gates `place_docs` itself enforces) that
    /// withheld placement. Non-empty only when [`Self::placed`] is `false`
    /// for this reason specifically.
    pub gate_failures: Vec<String>,
    /// DGRICH-08: how many OLD README sections the rich pipeline's OWN
    /// generated landing + docs tree covered, with no legacy backstop
    /// needed. The ideal/ordinary case approaches "every section" as the
    /// generator gets better grounded.
    pub covered_by_generation: usize,
    /// DGRICH-08: how many OLD README sections fell back to a verbatim
    /// `docs/legacy/<slug>.md` page because generation did not cover them.
    /// `0` is the ideal case. A fully flagged/degraded generation round
    /// relocates every section -- the safe fallback to the pre-DGRICH-08
    /// (DLAND-RELOC) all-verbatim behavior, reached here as an emergent
    /// consequence of the same coverage check rather than a special case.
    pub relocated_to_legacy: usize,
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
            covered_by_generation: 0,
            relocated_to_legacy: 0,
            summary,
        }
    }
}

// ---------------------------------------------------------------------------
// backfill_readme
// ---------------------------------------------------------------------------

/// Migrate `target_root`'s current `README.md` (if any) by running the FULL
/// rich, KG-grounded doc-generation pipeline against `target_root` as the
/// PRIMARY product, with the old README relocated verbatim to
/// `docs/legacy/<slug>.md` ONLY for sections generation didn't cover. See the
/// module doc comment for the full flow and the no-loss guarantee.
///
/// - `target_root`: the working-copy root (typically a worktree) whose
///   `README.md` is read as no-loss-guard input (and, via
///   [`super::repo_facts::build_repo_facts`], as `RepoFacts`' Pass-0 legacy
///   input) and where a successful migration is placed. `target_root` is
///   also the checkout `RepoFacts` scans for entry points/config
///   surface/prose anchors -- unlike DGRICH-07's repo-level trigger mode,
///   this function does NOT gate on "looks like a full checkout"
///   ([`super::repo_facts::build_repo_facts`] degrades every checkout-scan
///   source gracefully when it isn't one): backfill always runs the rich
///   pipeline, never a legacy per-module fallback.
/// - `module_path`/`available_credential_keys` are accepted for call-site
///   compatibility with the existing `docgen_backfill` tool schema but are
///   not consumed by the rich pipeline (no per-target multi-format
///   rendering happens here, only the landing + docs tree `place_docs`
///   writes).
/// - `raw_feat_context` is still swept unconditionally
///   ([`super::pii_gate::sweep_input`]) even though the rich pipeline's
///   identity pass never sees a diff by design (the anti-latch rule,
///   DGRICH-02) -- the tool's contract promises the sweep runs regardless.
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
    let graph_source = AtlasGraphSource::from_env();
    backfill_readme_with_graph_source(
        generator,
        &graph_source,
        version_store,
        project,
        module_path,
        git_ref,
        raw_feat_context,
        project_config_raw,
        available_credential_keys,
        generated_at,
        target_root,
    )
    .await
}

/// The real body of [`backfill_readme`], parameterized over a
/// [`GraphSource`] so tests can inject a fixture graph -- exactly the same
/// seam shape [`super::trigger::run_docgen_trigger`]'s own
/// `run_docgen_trigger_with_graph_source` uses. Not `pub`: the public,
/// signature-frozen entry point is [`backfill_readme`] above, which always
/// supplies the real [`AtlasGraphSource`].
#[allow(clippy::too_many_arguments)]
async fn backfill_readme_with_graph_source(
    generator: &dyn DocGenerator,
    graph_source: &dyn GraphSource,
    version_store: &VersionStore,
    project: &str,
    _module_path: &str,
    git_ref: &str,
    raw_feat_context: &str,
    project_config_raw: Option<&Value>,
    _available_credential_keys: &BTreeSet<String>,
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
                covered_by_generation: 0,
                relocated_to_legacy: 0,
                summary: format!(
                    "refused: the existing README.md at the target could not be read ({e}); \
not overwriting unreadable content -- an operator must inspect it before backfilling"
                ),
            };
        }
    };
    let old_readme_existed = old_readme.is_some();
    let old_readme_str = old_readme.clone().unwrap_or_default();
    let old_readme_lines = old_readme.as_deref().map(landing_line_count).unwrap_or(0);

    // Opt-in gate: a project with no doc-target config at all has not opted
    // in to the backfill stage either -- same "declares nothing" test the
    // post-feat trigger stage uses, reused (not re-implemented) so the two
    // gates can never disagree.
    if declares_no_targets(project_config_raw) {
        return BackfillReport::no_op(
            old_readme_existed,
            old_readme_lines,
            format!(
                "backfill skipped: project '{project}' has no doc-target config declared -- the \
backfill stage is opt-in (like mirror_ready) and this project has not opted in"
            ),
        );
    }

    // DOCGEN-02: unconditional PII sweep of the caller-supplied feat context.
    // The rich pipeline's identity pass never looks at a diff by design (the
    // anti-latch rule, DGRICH-02), so this result is not threaded any
    // further -- but the tool's contract promises the sweep always runs, and
    // an unsweepable context is still a real failure to surface.
    if let Err(e) = sweep_input(raw_feat_context) {
        return BackfillReport::no_op(
            old_readme_existed,
            old_readme_lines,
            format!("backfill failed before any placement was attempted: PII sweep of feat context failed: {e}"),
        );
    }

    let facts = match build_repo_facts(graph_source, target_root, project, git_ref) {
        Ok(f) => f,
        Err(e) => {
            return BackfillReport::no_op(
                old_readme_existed,
                old_readme_lines,
                format!("backfill failed before any placement was attempted: could not build RepoFacts: {e}"),
            );
        }
    };

    // Passes 1-3: the rich pipeline, run as the PRIMARY product. The old
    // README already reached `facts.old_readme_sections` as Pass-0 input #7
    // ("legacy claims to verify") via `build_repo_facts` -- grounding, not
    // the output.
    let outcome = generate_repo_docs(generator, &facts, project, git_ref).await;
    let docs_tree_no_legacy = build_repo_docs_tree(project, &facts, &outcome, &[]);

    let landing = match &outcome.identity {
        Some(identity) => build_landing_body(identity, &facts, &docs_tree_no_legacy),
        None => minimal_landing(&facts, &docs_tree_no_legacy),
    };

    // No-loss guard, pass 1: what did the rich generation ITSELF cover,
    // before any backstop relocation?
    let pres_gen = check_preservation(&old_readme_str, &landing, &docs_tree_no_legacy);
    let missing_labels: BTreeSet<&str> = pres_gen.missing.iter().map(|s| s.heading.as_str()).collect();

    // DGRICH-08 backstop: every OLD section generation did NOT cover is
    // relocated VERBATIM to docs/legacy/<slug>.md, reusing the exact same
    // byte-offset slicer + slugging DLAND-RELOC always used -- now invoked
    // only for the gap, not the whole document.
    let legacy_pages: Vec<(String, String)> = label_and_slug_old_sections(&old_readme_str)
        .into_iter()
        .filter(|(label, _, _)| missing_labels.contains(label.as_str()))
        .map(|(_, slug, content)| (slug, content))
        .collect();

    let docs_tree = if legacy_pages.is_empty() {
        docs_tree_no_legacy
    } else {
        build_repo_docs_tree(project, &facts, &outcome, &legacy_pages)
    };

    // No-loss guard, pass 2: the backstop must make coverage 1.0 BY
    // CONSTRUCTION (every relocated section's exact original bytes are now
    // part of the corpus) -- re-verified here rather than merely trusted.
    let preservation = check_preservation(&old_readme_str, &landing, &docs_tree);

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
            covered_by_generation: pres_gen.covered.len(),
            relocated_to_legacy: legacy_pages.len(),
            summary: format!(
                "no-loss guard flagged {count} section(s) whose substance was not found even \
after verbatim legacy relocation -- placement refused; this should be unreachable given \
verbatim relocation, an operator must inspect this repo's old README before retrying"
            ),
        };
    }

    // Extra Pass-5 gates ahead of DGRICH-09 (mirrors DGRICH-07's repo-level
    // trigger door): the substance floor and the generic-diagram lint aren't
    // folded into `place_docs`'s own fail-closed set yet, so this door runs
    // them itself rather than shipping a latch-prone/near-empty landing.
    //
    // CRITICAL: these gates apply ONLY to a RICH landing (identity present). A
    // DEGRADED run (identity == None) uses the deliberately-sparse
    // `minimal_landing` AND has relocated ALL old content verbatim to legacy
    // (no-loss holds) -- that is the safe fallback to pre-DGRICH-08 behavior and
    // MUST still place. Gating it on the substance floor would refuse placement
    // exactly when the fallback is supposed to guarantee it (codex review
    // finding: "generation-flagged -> all relocate" must not be blocked).
    let mut gate_failures: Vec<String> = Vec::new();
    if outcome.identity.is_some() {
        if let Err(e) = check_landing_substance(&landing) {
            gate_failures.push(e);
        }
        if facts.kg_grounded {
            let generic = subsystem_architecture_mermaid_source(&facts)
                .map(|s| is_generic_placeholder(s.as_str()))
                .unwrap_or(true);
            if generic {
                gate_failures.push(
                    "architecture diagram lint (DGRICH-04 is_generic_placeholder): the derived \
diagram is the generic template or has fewer than 5 real subsystem nodes -- withholding the \
cutover rather than shipping a latch-prone landing"
                        .to_string(),
                );
            }
        }
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
            covered_by_generation: pres_gen.covered.len(),
            relocated_to_legacy: legacy_pages.len(),
            summary: "assembled landing failed a Pass-5 landing gate -- placement refused, \
nothing written"
                .to_string(),
        };
    }

    let placement = place_docs(target_root, &landing, &docs_tree);

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
            covered_by_generation: pres_gen.covered.len(),
            relocated_to_legacy: legacy_pages.len(),
            summary: "assembled landing failed the DLAND-03 placement gate -- nothing written"
                .to_string(),
        };
    }

    let placed = !placement.written.is_empty() || !placement.unchanged.is_empty();
    let docs_files_created: Vec<String> =
        placement.written.iter().filter(|p| p.as_str() != README_PATH).cloned().collect();

    if placed {
        let key = ArtifactKey::new(project.to_string(), "readme".to_string());
        let _ = version_store.store_version(key, landing.clone(), git_ref.to_string(), generated_at.to_string());
    }

    let summary = if !placement.skipped.is_empty() {
        format!(
            "placement partially refused ({} entr(y/ies) skipped): {:?}",
            placement.skipped.len(),
            placement.skipped
        )
    } else if placed {
        format!(
            "rich pipeline generated the landing + docs/ tree ({} old README section(s) covered \
by generation, {} relocated verbatim to docs/legacy/ as the no-loss backstop); no-loss coverage \
{:.0}%",
            pres_gen.covered.len(),
            legacy_pages.len(),
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
        covered_by_generation: pres_gen.covered.len(),
        relocated_to_legacy: legacy_pages.len(),
        summary,
    }
}

// ---------------------------------------------------------------------------
// docgen_backfill tool
// ---------------------------------------------------------------------------

/// `docgen_backfill` -- the MCP-tool surface for a one-shot, operator-blessed
/// README-to-hierarchy migration (DLAND-05, inverted by DGRICH-08). Holds its
/// own [`VersionStore`], matching [`super::trigger::DocgenRun`]'s posture
/// (version history accumulates across calls for the lifetime of this tool
/// instance).
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
        "One-shot backfill (DLAND-05, inverted by DGRICH-08): migrate an already-bloated repo \
README (Terminus, Chord, Muse, lumina-constellation, ...) by running the FULL rich, \
KG-grounded doc-generation pipeline (RepoFacts -> identity -> per-subsystem reference pages -> \
guides -> derived architecture diagram -> assembled landing) against the repo as the PRIMARY \
product -- the old README is fed in as grounding (Pass-0 legacy input), not the output. Any old \
section the rich pipeline's own landing/docs did not cover is relocated VERBATIM (byte-exact) to \
docs/legacy/<slug>.md and linked from the reference index, as a no-loss BACKSTOP only -- so no \
information is lost by construction, but the docs are no longer just a mechanical copy of the old \
README's sections. Still runs the no-loss guard (DLAND-02, re-verified after the backstop) and \
the landing gates as a fail-closed set, and places into a WORKING COPY at target_root for \
operator review. Refuses to place anything (README.md and every docs/** file, together) if the \
no-loss guard flags any dropped section even after legacy relocation (should not be reachable), \
or if the assembled landing fails a landing gate -- an operator must confirm before a real \
cutover lands. NEVER commits, pushes, or makes any Plane/Gitea/GitHub call -- working-copy write \
only; the normal build pipeline (review, merge) carries the result from there."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "spec_id": {
                    "type": "string",
                    "description": "The spec identifier this backfill belongs to (e.g. \"S119-dgrich-rich-doc-generator\"), carried through for logging/observability."
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
                    "description": "The project's raw doc-target config, e.g. {\"targets\": [{\"type\": \"readme\"}]}. Must declare a \"readme\" target for a backfill to have anything to place. DGRICH-09: for the repo-level rich pipeline this object also accepts the same optional tuning knobs docgen_run does -- \"subsystem_page_cap\", \"landing_budget\", \"identity_hint\" -- see docgen_run's parameter description for details; all default cleanly when omitted."
                },
                "available_credential_keys": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Runtime secret-store KEY NAMES (never values) currently available. Accepted for call-site compatibility; not consumed by the rich pipeline."
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

        // DGDG-01: falls back to a configured OpenRouter model when local
        // Chord/GPU inference is jammed; behaves exactly like a bare
        // `ChordDocGenerator::from_env()` when no fallback is configured.
        let generator = FallbackDocGenerator::from_env();
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
            "covered_by_generation": report.covered_by_generation,
            "relocated_to_legacy": report.relocated_to_legacy,
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

    use crate::scribe::graph::{Confidence, EdgeKind, KgEdge, KgNode, KnowledgeGraph, NodeKind};
    use super::super::repo_facts::{FixtureGraphSource, NoGraphSource};

    fn node(id: &str, kind: NodeKind, path: &str) -> KgNode {
        let name = id.rsplit("::").next().unwrap_or(id).to_string();
        KgNode::new(id, kind, name, path)
    }

    /// Five subsystems (`a`..`e`), each a 30-node hub-and-leaves group (the
    /// same proven shape `repo_facts.rs`'s/`generate.rs`'s own fixtures use)
    /// -- enough real subsystems that DGRICH-04's derived architecture
    /// diagram is never the generic fallback (`is_generic_placeholder`
    /// requires >=5 real subsystem nodes), so a happy-path backfill's
    /// Pass-5 gates can actually clear and placement can be observed
    /// end-to-end on disk.
    fn five_subsystem_graph() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("BF");
        for name in ["a", "b", "c", "d", "e"] {
            let hub = format!("crate::{name}::Hub::run");
            g.insert_node(node(&hub, NodeKind::Function, &format!("src/{name}/hub.rs")));
            for i in 0..29 {
                let leaf = format!("crate::{name}::f{i}");
                g.insert_node(node(&leaf, NodeKind::Function, &format!("src/{name}/f{i}.rs")));
                g.insert_edge(KgEdge::new(&leaf, &hub, EdgeKind::Calls, Confidence::Extracted)).unwrap();
            }
        }
        g
    }

    /// A deliberately long, 3-paragraph `what_is` (16 lines per paragraph) --
    /// real generated prose would never be this repetitive, but the
    /// DGRICH-05 substance floor (`check_landing_substance`,
    /// `LANDING_MIN_SUBSTANTIVE_LINES = 80`) is orthogonal to what DGRICH-08
    /// itself needs to prove; padding it out here deterministically clears
    /// that floor so the coverage/relocation assertions below can also
    /// assert on the REAL on-disk placement, rather than leaving placement
    /// success to chance on exact line-count arithmetic.
    fn verbose_what_is() -> String {
        let mut paragraphs = Vec::new();
        for p in 0..3 {
            let lines: Vec<String> = (0..16)
                .map(|i| format!("Fixture filler sentence {p}-{i} describing the widget factory in more detail."))
                .collect();
            paragraphs.push(lines.join("\n"));
        }
        paragraphs.join("\n\n")
    }

    fn valid_identity_json() -> String {
        let names = ["a", "b", "c", "d", "e"];
        let subsystems: Vec<Value> = names
            .iter()
            .map(|n| json!({"name": n, "one_liner": format!("Subsystem {n} handles its part."), "role": "core"}))
            .collect();
        let feature_rows: Vec<Value> = names
            .iter()
            .map(|n| json!({"feature": format!("{n} processing"), "description": format!("Processes work via subsystem {n}."), "subsystem": n}))
            .collect();
        json!({
            "tagline": "Widget Factory is a hub combining five subsystems.",
            "what_is": verbose_what_is(),
            "audience": "Fixture test operators.",
            "subsystems": subsystems,
            "feature_rows": feature_rows,
            "guide_topics": [{"title": "Run the fixture", "grounding": "crate::a::Hub::run"}]
        })
        .to_string()
    }

    const GOOD_GUIDES: &str = "\
=== FILE: docs/getting-started.md ===
Clone the repo, then run `crate::a::Hub::run`.

=== FILE: docs/guides/run-the-fixture.md ===
# Run the fixture
1. Build the fixture.
2. Call `crate::a::Hub::run`.
";

    /// Scripted, prompt-dispatching `DocGenerator` -- mirrors `generate.rs`'s
    /// own `ScriptedGenerator` test seam (private there, so a small
    /// equivalent is duplicated here rather than exposed cross-module for a
    /// test-only type).
    struct ScriptedGenerator {
        identity_response: String,
        guides_response: String,
    }

    impl ScriptedGenerator {
        fn new(identity_response: impl Into<String>, guides_response: impl Into<String>) -> Self {
            Self { identity_response: identity_response.into(), guides_response: guides_response.into() }
        }
    }

    #[async_trait]
    impl DocGenerator for ScriptedGenerator {
        async fn generate(&self, prompt: &str) -> Result<String, ToolError> {
            if prompt.contains("Write a JSON object with EXACTLY these keys") {
                return Ok(self.identity_response.clone());
            }
            if prompt.contains("You are writing the operator guides") {
                return Ok(self.guides_response.clone());
            }
            const MARKER: &str = "reference page for the `";
            if let Some(idx) = prompt.find(MARKER) {
                let rest = &prompt[idx + MARKER.len()..];
                if let Some(end) = rest.find('`') {
                    let name = &rest[..end];
                    return Ok(format!(
                        "# {name}\n\n## Key types and functions\n\
`crate::{name}::Hub::run` is the entry point.\n\n\
## How it connects\nCalled by its own leaves.\n\n\
## Notes and gaps\nNothing else to cover here.\n"
                    ));
                }
            }
            Ok(String::new())
        }
    }

    /// A generator that always fails -- forces `generate_repo_docs`'s
    /// identity pass to `Flagged`/`None` (both attempts error out),
    /// exercising DGRICH-08's "generation flagged -> all sections relocate"
    /// edge case.
    struct FailingGenerator;

    #[async_trait]
    impl DocGenerator for FailingGenerator {
        async fn generate(&self, _prompt: &str) -> Result<String, ToolError> {
            Err(ToolError::Http("backend unreachable".to_string()))
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

    // ── TEST PLAN: old README fully covered by generation -> nothing
    // relocated, coverage 1.0, no legacy pages ──────────────────────────

    #[tokio::test]
    async fn old_readme_fully_covered_by_generation_relocates_nothing() {
        let root = unique_tmp_dir("fully-covered");
        // This section's only stable tokens (`crate`, `Hub`, `run`) are
        // exactly what subsystem `a`'s generated reference page also states
        // verbatim -- generation covers it with no legacy backstop needed.
        let old_readme = "# Widget\n\n## Alpha\n\nSee `crate::a::Hub::run` for details.\n";
        std::fs::write(root.join("README.md"), old_readme).unwrap();

        let graph_source = FixtureGraphSource(five_subsystem_graph());
        let generator = ScriptedGenerator::new(valid_identity_json(), GOOD_GUIDES);
        let store = VersionStore::new();

        let report = backfill_readme_with_graph_source(
            &generator,
            &graph_source,
            &store,
            "BF",
            ".",
            "abc123",
            "one-shot backfill of an already-covered README",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(report.gate_failures.is_empty(), "{:?}", report.gate_failures);
        assert!(report.placed, "{}", report.summary);
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        assert_eq!(report.coverage_ratio, 1.0);
        assert_eq!(report.relocated_to_legacy, 0, "{}", report.summary);
        assert_eq!(report.covered_by_generation, 1);
        assert!(!root.join("docs/legacy").exists(), "no legacy backstop should be needed");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── TEST PLAN: a section with unique substance not regenerated ->
    // relocated verbatim to legacy/<slug>.md, linked, coverage stays 1.0 ──

    #[tokio::test]
    async fn a_section_not_covered_by_generation_is_relocated_verbatim_to_legacy() {
        let root = unique_tmp_dir("partial-cover");
        let old_readme = "# Widget\n\n\
## Alpha\n\nSee `crate::a::Hub::run` for details.\n\n\
## Legacy Notes\n\nSet `WIDGET_LEGACY_FLAG_XYZ` to enable legacy mode.\n";
        std::fs::write(root.join("README.md"), old_readme).unwrap();

        let graph_source = FixtureGraphSource(five_subsystem_graph());
        let generator = ScriptedGenerator::new(valid_identity_json(), GOOD_GUIDES);
        let store = VersionStore::new();

        let report = backfill_readme_with_graph_source(
            &generator,
            &graph_source,
            &store,
            "BF",
            ".",
            "abc123",
            "one-shot backfill with one uncovered legacy section",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(report.gate_failures.is_empty(), "{:?}", report.gate_failures);
        assert!(report.placed, "{}", report.summary);
        assert!(report.missing.is_empty(), "no-loss guard must be satisfied after the backstop: {:?}", report.missing);
        assert_eq!(report.coverage_ratio, 1.0);
        // Both counts surfaced (TEST PLAN item 3): one section covered by
        // generation, one relocated to the legacy backstop.
        assert_eq!(report.covered_by_generation, 1, "{}", report.summary);
        assert_eq!(report.relocated_to_legacy, 1, "{}", report.summary);

        // The legacy page really landed, verbatim, and is linked.
        let legacy_path = root.join("docs/legacy/legacy-notes.md");
        assert!(legacy_path.exists(), "expected a legacy backstop page on disk");
        let legacy_content = std::fs::read_to_string(&legacy_path).unwrap();
        assert!(legacy_content.contains("WIDGET_LEGACY_FLAG_XYZ"), "{legacy_content}");
        let reference_index = std::fs::read_to_string(root.join("docs/reference/index.md")).unwrap();
        assert!(reference_index.contains("legacy-notes"), "{reference_index}");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── TEST PLAN: empty old README -> no legacy, coverage 1.0 ──────────

    #[tokio::test]
    async fn empty_old_readme_has_no_legacy_pages_and_trivial_full_coverage() {
        let root = unique_tmp_dir("no-old-readme");
        // Deliberately no README.md written at target_root at all.

        let graph_source = FixtureGraphSource(five_subsystem_graph());
        let generator = ScriptedGenerator::new(valid_identity_json(), GOOD_GUIDES);
        let store = VersionStore::new();

        let report = backfill_readme_with_graph_source(
            &generator,
            &graph_source,
            &store,
            "BF",
            ".",
            "abc123",
            "backfill against a repo with no old README at all",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(!report.old_readme_existed);
        assert_eq!(report.old_readme_lines, 0);
        assert_eq!(report.coverage_ratio, 1.0);
        assert!(report.missing.is_empty());
        assert_eq!(report.relocated_to_legacy, 0);
        assert_eq!(report.covered_by_generation, 0);
        assert!(report.gate_failures.is_empty(), "{:?}", report.gate_failures);
        assert!(report.placed, "{}", report.summary);
        assert!(!root.join("docs/legacy").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    // ── TEST PLAN: generation flagged -> all sections relocate ─────────

    #[tokio::test]
    async fn generation_flagged_relocates_every_old_section_to_legacy() {
        let root = unique_tmp_dir("generation-flagged");
        let old_readme = "# Widget\n\n\
## Alpha\n\nSee `crate::a::Hub::run` for details.\n\n\
## Legacy Notes\n\nSet `WIDGET_LEGACY_FLAG_XYZ` to enable legacy mode.\n";
        std::fs::write(root.join("README.md"), old_readme).unwrap();

        // The graph is still grounded (facts.kg_grounded stays true), but
        // EVERY generator call fails, so the identity pass never succeeds
        // and Passes 2/3 are skipped entirely -- `outcome.identity` is
        // `None`, forcing the `minimal_landing` fallback, which covers
        // essentially nothing.
        let graph_source = FixtureGraphSource(five_subsystem_graph());
        let generator = FailingGenerator;
        let store = VersionStore::new();

        let report = backfill_readme_with_graph_source(
            &generator,
            &graph_source,
            &store,
            "BF",
            ".",
            "abc123",
            "backfill whose generation is entirely flagged",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        // The safe fallback: nothing covered by generation, everything
        // relocated verbatim to the backstop, coverage 1.0.
        assert_eq!(report.covered_by_generation, 0, "{}", report.summary);
        assert_eq!(report.relocated_to_legacy, 2, "{}", report.summary);
        assert_eq!(report.coverage_ratio, 1.0, "the backstop must still make coverage 1.0");
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        // A DEGRADED run (identity == None) MUST still place: the substance /
        // generic-diagram gates are RICH-landing-only, so the deliberately-sparse
        // minimal_landing + full verbatim relocation is never gate-withheld
        // (codex review finding: "generation-flagged -> all relocate" must place).
        assert!(report.placed, "degraded fallback must place, not be gate-withheld: {}", report.summary);
        assert!(report.gate_failures.is_empty(), "no gate should fire on the degraded path: {:?}", report.gate_failures);

        std::fs::remove_dir_all(&root).ok();
    }

    // ── An EXISTING but unreadable README is never overwritten ──────────

    #[tokio::test]
    async fn backfill_refuses_to_place_when_the_existing_readme_is_unreadable() {
        let root = unique_tmp_dir("unreadable-readme");
        // Invalid UTF-8 bytes: read_to_string fails with InvalidData, not NotFound.
        std::fs::write(root.join("README.md"), [0xff, 0xfe, 0x00, 0x9f, 0x28]).unwrap();
        let before = std::fs::read(root.join("README.md")).unwrap();

        let generator = FailingGenerator;
        let store = VersionStore::new();

        let report = backfill_readme_with_graph_source(
            &generator,
            &NoGraphSource,
            &store,
            "BF",
            ".",
            "abc123",
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

    // ── Edge: no doc-target config declared -> skip cleanly ──────────────

    #[tokio::test]
    async fn backfill_skips_cleanly_when_project_has_not_opted_in() {
        let root = unique_tmp_dir("no-config");
        let generator = FailingGenerator;
        let store = VersionStore::new();

        let report = backfill_readme_with_graph_source(
            &generator,
            &NoGraphSource,
            &store,
            "BF",
            ".",
            "abc123",
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
                "spec_id": "S119-dgrich-rich-doc-generator",
                "project": "BF",
                "module_path": ".",
                "git_ref": "abc123",
                "feat_context": "some context"
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }
}
