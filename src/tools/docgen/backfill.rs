//! DLAND-05 / DLAND-RELOC: one-shot backfill -- migrate an already-bloated
//! repo README, operator-reviewed (S119, spec `S119-docgen-landing-hierarchy`,
//! Plane project TERM).
//!
//! ## DLAND-RELOC (mechanical relocation, supersedes DLAND-05's LLM-only flow)
//! A live run proved the original DLAND-05 flow (LLM-regenerate the whole
//! README) is lossy BY CONSTRUCTION: an LLM asked to write a concise landing
//! naturally SUMMARIZES (a 2285-line README became a 61-line landing) and
//! produces no `docs/` tree of its own, so [`super::preserve::check_preservation`]
//! correctly withheld every cutover. [`backfill_readme`] now separates the two
//! jobs the old flow conflated:
//! - The LLM is used ONLY to produce the concise top-page landing (hero +
//!   quick start) -- still via [`super::trigger::run_docgen_trigger`] with
//!   `place=false`, but its (necessarily empty, since it never saw the old
//!   README's sections rendered as its own docs) `docs_tree` is IGNORED.
//! - The `docs/` tree is instead built MECHANICALLY, directly from the OLD
//!   README: [`old_readme_parts`] (a byte-offset slicer -- see its doc for why
//!   this is deliberately distinct from the no-loss guard's line-based
//!   `split_old_sections`) splits it into a
//!   preamble plus one entry per top-level `## ` section, and each section's
//!   heading + body is copied VERBATIM into its own `docs/reference/<slug>.md`
//!   page (see [`build_docs_tree_from_old_readme`]). Verbatim copying makes
//!   the no-loss guarantee true BY CONSTRUCTION, not by hoping an LLM's
//!   paraphrase happens to keep every stable token.
//! - The final landing is the LLM's hero/quick-start text with any
//!   LLM-authored `## Documentation`-shaped section and any `docs/…` links
//!   stripped (they would dangle against the mechanical tree), followed by a
//!   mechanically-built `## Documentation` section linking to the mechanical
//!   `docs/index.md` hub -- see [`assemble_final_landing`].
//!
//! [`super::preserve::check_preservation`] (DLAND-02) and the DLAND-03 landing
//! gates ([`super::readme_layers::check_landing_length`]/
//! [`super::readme_layers::check_landing_links`]) still run exactly as
//! before, and [`super::place::place_docs`] (DLAND-01) is still the sole
//! placement writer -- this item changes WHAT is checked/placed, not the
//! guard/gate/writer machinery itself.
//!
//! ## Nothing reimplemented
//! - [`super::trigger::run_docgen_trigger`] is the sole generation
//!   orchestration (PII sweep, Chord call, per-target render) -- called with
//!   `place=false` so this module decides placement itself.
//! - [`old_readme_parts`] is this module's own byte-offset section slicer for
//!   VERBATIM relocation -- deliberately separate from the no-loss guard's
//!   line-based `split_old_sections` (which normalises whitespace to compare
//!   tokens); see [`old_readme_parts`] for why byte-exact slices are required.
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
//! ## No-loss is now true by construction, the guard stays as a backstop
//! Because every old `##` section is copied VERBATIM into
//! `docs/reference/*.md`, [`super::preserve::check_preservation`] should
//! always report `missing` empty / `coverage_ratio` `1.0` for this flow. The
//! "refuse to place if `missing` is non-empty" check is KEPT regardless --
//! never trust a guarantee is self-enforcing when a cheap, already-existing
//! check can verify it for free; see `mechanical_relocation_preserves_every_old_section`
//! below for the positive proof and the module's negative safety net.
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
//!   the no-loss guard trivially passes (nothing to lose), and the
//!   mechanical `docs/` tree built from an empty old README is just an
//!   (empty-contents) index page.
//! - An old README with NO `## ` headings at all -> [`super::preserve::split_old_sections`]'s
//!   own EDGE CASE applies: the whole body becomes ONE section (labeled
//!   "Overview" for its `docs/reference/overview.md` page here), so it is
//!   still relocated, never dropped.

use std::collections::{BTreeSet, HashMap};
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
use super::readme_layers::{
    check_landing_length, check_landing_links, landing_line_count, DOCS_INDEX_PATH, LANDING_MAX_LINES,
};
use super::render::docs_tree::DocsTreeFile;
use super::trigger::{run_docgen_trigger, TriggerOutcome};
use super::versioning::VersionStore;

// ---------------------------------------------------------------------------
// Mechanical relocation (DLAND-RELOC): old README sections -> docs/ verbatim
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

/// Mechanically build the `docs/` tree from the OLD README: a preamble (if
/// any, verbatim) plus one `docs/reference/<slug>.md` page per top-level
/// `## ` section, each holding that section's EXACT ORIGINAL SOURCE BYTES --
/// no paraphrasing, no summarizing, not even an appended newline. Section
/// boundaries come from [`old_readme_parts`], a purpose-built BYTE-OFFSET
/// slicer: it deliberately differs from [`super::preserve::split_old_sections`]
/// (the line-based, whitespace-trimming parser the no-loss guard uses to
/// COMPARE tokens) precisely because relocation needs byte-exact source spans,
/// not parsed/normalised heading+body fields. Both agree on where `## `
/// sections begin. Slugs are de-duplicated with a numeric suffix on collision
/// (`install`, `install-2`, ...). Returns `docs/index.md` (a hub page: short
/// title + verbatim preamble + a link list to every reference page, by its
/// original heading text) first, followed by the reference pages in original
/// section order.
///
/// An old README with NO `## ` headings at all still produces exactly one
/// reference page (the whole document, verbatim, labeled "Overview") -- so
/// nothing is ever silently dropped for lack of section structure.
/// Split the OLD README into (verbatim preamble, [(heading, VERBATIM source
/// slice)]) using EXACT byte offsets. Each section's slice is the original
/// source from its `## ` line through just before the next `## ` line (or EOF),
/// copied byte-for-byte -- so a relocated docs page contains the section's exact
/// authored heading, blank lines, and body, not a reconstruction from parsed
/// fields (which would normalise whitespace / heading formatting). This is what
/// makes the relocation truly loss-free.
fn old_readme_parts(old: &str) -> (String, Vec<(String, String)>) {
    let mut starts: Vec<(usize, String)> = Vec::new();
    let mut offset = 0usize;
    // Track fenced code blocks so a literal `## ` line INSIDE a ``` / ~~~ fence
    // (e.g. a shell/markdown example) is never mistaken for a real section
    // boundary -- otherwise a code example would be split across pages, breaking
    // its fence and inventing a bogus reference page.
    let mut in_fence = false;
    for line in old.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence {
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

fn build_docs_tree_from_old_readme(old_readme: &str) -> Vec<DocsTreeFile> {
    let (preamble, sections) = old_readme_parts(old_readme);

    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    // (slug, heading label for the index link list, VERBATIM page content)
    let mut pages: Vec<(String, String, String)> = Vec::with_capacity(sections.len());

    for (heading, slice) in &sections {
        let heading_label =
            if heading.trim().is_empty() { "Overview".to_string() } else { heading.clone() };
        let base_slug = {
            let s = slugify(&heading_label);
            if s.is_empty() { "section".to_string() } else { s }
        };
        let count = slug_counts.entry(base_slug.clone()).or_insert(0);
        *count += 1;
        let slug = if *count == 1 { base_slug } else { format!("{base_slug}-{count}") };

        // The page IS the exact original source slice -- byte-for-byte, no
        // trailing newline appended, no reconstruction. Whatever bytes the
        // author wrote for this section are exactly what the docs page holds.
        pages.push((slug, heading_label, slice.clone()));
    }

    let mut docs_tree = Vec::with_capacity(pages.len() + 1);

    let mut index = String::from("# Documentation Index\n\n");
    if !preamble.is_empty() {
        index.push_str(&preamble);
        index.push_str("\n\n");
    }
    if pages.is_empty() {
        index.push_str("_Nothing was relocated from the old README -- it had no content to preserve._\n");
    } else {
        index.push_str("## Contents\n\n");
        for (slug, heading_label, _) in &pages {
            index.push_str(&format!("- [{heading_label}](reference/{slug}.md)\n"));
        }
    }
    docs_tree.push(DocsTreeFile { path: DOCS_INDEX_PATH.to_string(), content: index });

    for (slug, _heading_label, content) in pages {
        docs_tree.push(DocsTreeFile { path: format!("docs/reference/{slug}.md"), content });
    }

    docs_tree
}

/// Remove a top-level `## Documentation`-named section (heading text through
/// the next `## ` heading or end of content) from `landing`, if present.
/// The LLM's own landing may have authored a Documentation section pointing
/// at docs paths it invented (and never actually rendered anywhere, per the
/// module doc comment) -- those links would dangle against the mechanical
/// `docs/` tree, so this strips the whole section rather than leaving a
/// broken link behind. A single linear pass over `landing.lines()`.
fn strip_documentation_section(landing: &str) -> String {
    let mut out = String::with_capacity(landing.len());
    let mut skipping = false;
    for line in landing.lines() {
        let trimmed = line.trim_start();
        if let Some(title) = trimmed.strip_prefix("## ") {
            skipping = title.trim().eq_ignore_ascii_case("documentation");
            if skipping {
                continue;
            }
        }
        if skipping {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Strip any remaining markdown link whose target starts with `docs/`,
/// keeping the link's LABEL text (so the surrounding prose still reads
/// sensibly) but dropping the `(docs/...)` target -- any such link would
/// dangle against the mechanically-built `docs/` tree, which never contains
/// whatever path the LLM invented. Char-boundary-safe single forward scan
/// (matching this crate's existing link-scanning style in
/// `readme_layers::extract_link_targets`), never a regex dependency.
fn strip_dangling_docs_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        if text.as_bytes()[i] == b'[' {
            if let Some(rel_close_bracket) = text[i + 1..].find(']') {
                let close_bracket = i + 1 + rel_close_bracket;
                if text[close_bracket + 1..].starts_with('(') {
                    let paren_start = close_bracket + 1;
                    if let Some(rel_close_paren) = text[paren_start + 1..].find(')') {
                        let close_paren = paren_start + 1 + rel_close_paren;
                        let target = &text[paren_start + 1..close_paren];
                        if target.starts_with("docs/") {
                            out.push_str(&text[i + 1..close_bracket]);
                            i = close_paren + 1;
                            continue;
                        }
                    }
                }
            }
        }
        let ch = text[i..].chars().next().expect("i < text.len() guarantees a char at i");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Collapse runs of 2+ consecutive blank lines down to a single blank line.
/// Stripping a whole `## Documentation` section (or several dangling links)
/// out of the middle of a document tends to leave extra blank lines behind;
/// left alone they'd eat into the [`LANDING_MAX_LINES`] budget for nothing.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Trim `body`'s TAIL (never the supplied `doc_section`) so that
/// `body` + a blank-line separator + `doc_section` fits within
/// [`LANDING_MAX_LINES`] lines total. The Documentation section (the one
/// piece of content that makes the mechanical `docs/` tree reachable at all)
/// is never dropped, even from a very long LLM hero/quick-start -- only
/// trailing prose above it is cut.
fn enforce_landing_length(body: &str, doc_section: &str) -> String {
    // `body` (trimmed) + one blank separator line + `doc_section` (trimmed) + a
    // trailing newline -- account for the separator explicitly in the budget.
    let doc_lines = doc_section.trim_end().lines().count();
    let budget = LANDING_MAX_LINES.saturating_sub(doc_lines + 1);
    let body_trimmed = body.trim_end();
    let body_lines: Vec<&str> = body_trimmed.lines().collect();
    let final_body = if body_lines.len() > budget {
        body_lines[..budget].join("\n")
    } else {
        body_trimmed.to_string()
    };
    format!("{}\n\n{}\n", final_body.trim_end(), doc_section.trim_end())
}

/// Assemble the FINAL landing: the LLM's concise hero/quick-start content,
/// with any LLM-authored `## Documentation` section and any dangling
/// `docs/...` links stripped (see [`strip_documentation_section`]/
/// [`strip_dangling_docs_links`]), followed by a mechanically-built
/// `## Documentation` section linking to the mechanical `docs/index.md` hub
/// plus up to 3 of its reference pages -- every `docs/` link this function
/// emits is a path taken DIRECTLY from `docs_tree`, so [`check_landing_links`]
/// always passes against it. Stays within [`LANDING_MAX_LINES`] via
/// [`enforce_landing_length`].
fn assemble_final_landing(llm_landing: &str, docs_tree: &[DocsTreeFile]) -> String {
    let stripped = collapse_blank_lines(&strip_dangling_docs_links(&strip_documentation_section(llm_landing)));

    let mut doc_section = String::from("## Documentation\n\n");
    doc_section
        .push_str(&format!("See [the documentation index]({DOCS_INDEX_PATH}) for the full reference.\n"));
    let top_pages: Vec<&DocsTreeFile> =
        docs_tree.iter().filter(|f| f.path.starts_with("docs/reference/")).take(3).collect();
    if !top_pages.is_empty() {
        doc_section.push('\n');
        for f in top_pages {
            let heading = f.content.lines().next().unwrap_or("").trim_start_matches('#').trim();
            let label = if heading.is_empty() { f.path.as_str() } else { heading };
            doc_section.push_str(&format!("- [{label}]({})\n", f.path));
        }
    }

    enforce_landing_length(&stripped, &doc_section)
}

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

                // The LLM's landing is used ONLY for its hero/quick-start text --
                // its own (necessarily empty here, since it never rendered the old
                // README's sections as its own docs) `docs_tree` is IGNORED below;
                // see the module doc comment's "DLAND-RELOC" section.
                let llm_landing = render
                    .rendered()
                    .find(|a| a.target_type == DocTargetType::Readme)
                    .and_then(|a| a.content.clone());

                let llm_landing = match llm_landing {
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

                // Mechanical relocation: the docs/ tree is built DIRECTLY from the
                // OLD README's sections (verbatim), never from the LLM's own
                // (empty) docs_tree -- see `build_docs_tree_from_old_readme`.
                let docs_tree = build_docs_tree_from_old_readme(&old_readme_str);
                let landing = assemble_final_landing(&llm_landing, &docs_tree);

                // No-loss guard (DLAND-02): kept as a backstop even though
                // verbatim relocation makes coverage 1.0 BY CONSTRUCTION -- see
                // the module doc comment's "No-loss is now true by construction"
                // section.
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
                        summary: format!(
                            "no-loss guard flagged {count} section(s) whose substance was not found \
in the mechanically-relocated landing/docs -- placement refused; this should be unreachable for a \
verbatim relocation, an operator must inspect this repo's old README before retrying"
                        ),
                    };
                }

                // Surface the DLAND-03 landing gates in the summary -- these are
                // ALSO enforced fail-closed inside `place_docs` below regardless
                // of whether we check them here first. `assemble_final_landing`
                // is built to satisfy both already (every docs/ link it emits
                // comes straight from `docs_tree`, and it trims to
                // `LANDING_MAX_LINES`), so these should not fire in practice.
                let mut gate_failures = Vec::new();
                if let Err(e) = check_landing_length(&landing) {
                    gate_failures.push(e);
                }
                if let Err(dangling) = check_landing_links(&landing, &docs_tree) {
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
                        summary: "assembled landing failed a DLAND-03 landing lint gate -- placement \
refused, nothing written"
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
                        summary: "assembled landing failed the DLAND-03 placement gate -- nothing \
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
                        "mechanically relocated README.md from {} line(s) to a {} line concise \
landing plus {} docs/** page(s) (every old section copied verbatim); no-loss coverage {:.0}%",
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
        "One-shot backfill (DLAND-05, mechanically relocated per DLAND-RELOC): migrate an \
already-bloated repo README (Terminus, Chord, Muse, lumina-constellation, ...) into a concise \
landing + docs/ hierarchy in ONE guarded pass. The LLM is used ONLY to produce the concise \
top-page landing (hero + quick start); the docs/ tree is built MECHANICALLY by copying every \
old top-level (##) section's heading + body VERBATIM into its own docs/reference/<slug>.md \
page (no paraphrasing, no summarizing), so no information is lost by construction. Still runs \
the no-loss guard (DLAND-02) and the landing gates (DLAND-03) as a backstop, and places into a \
WORKING COPY at target_root for operator review. Refuses to place anything (README.md and every \
docs/** file, together) if the no-loss guard flags any dropped section (should not be reachable \
given verbatim relocation), or if the assembled landing fails a landing lint gate -- an operator \
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

    // ── Happy path: bloated multi-section README -> mechanically relocated ──
    //
    // DLAND-RELOC: this replaces the old DLAND-05
    // `backfill_migrates_a_preserved_multi_section_readme_and_places_it` test.
    // That test relied on the MOCK LLM output happening to also carry every
    // old section's stable tokens under its own headings -- i.e. it tested
    // that a paraphrase-preserving LLM output passes the no-loss guard, not
    // that the tool itself guarantees no loss. Under mechanical relocation the
    // guarantee no longer depends on what the LLM says at all: every old
    // section is copied VERBATIM into its own `docs/reference/*.md` page
    // regardless of what the mock LLM landing contains. This test proves
    // exactly that -- four sections, each with a distinctive token the mock
    // LLM landing never mentions.
    #[tokio::test]
    async fn backfill_mechanically_relocates_every_old_section_verbatim_no_loss() {
        let root = unique_tmp_dir("happy-path");
        let old_readme = "# Widget\n\nA widget factory, in prose the LLM never sees again.\n\n\
## Install\n\nRun `cargo install widget_cli`. Requires `WIDGET_TOOLCHAIN_V3`.\n\n\
## Configuration\n\nSet `WIDGET_PORT=8080` in your environment.\n\n\
## Telemetry\n\nSet `WIDGET_TELEMETRY_ENDPOINT` to opt in to the `submit_metrics()` reporter.\n\n\
## Troubleshooting\n\nIf `widget_diagnose()` reports `ERR_WIDGET_JAMMED`, reset the feed tray.\n";
        std::fs::write(root.join("README.md"), old_readme).unwrap();

        // The mock LLM landing carries NONE of the four sections' distinctive
        // tokens -- if this tool still depended on the LLM's paraphrase to
        // preserve substance (the old DLAND-05 behavior), this would be
        // exactly the scenario that gets withheld as a drop. Mechanical
        // relocation must place it anyway, because the docs/ tree comes from
        // the OLD README directly, not from this text.
        let generator = MockDocGenerator::new(
            "# Widget\n\nA friendly widget factory that just works.\n\n\
## Quickstart\n\nGrab the CLI and build your first widget in minutes.\n",
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
        assert!(new_lines <= LANDING_MAX_LINES);
        assert_eq!(report.docs_files_created.len(), 5, "{:?}", report.docs_files_created); // index + 4 sections

        // Really landed on disk, not just reported -- one reference page per
        // old section, each named from its own heading.
        assert!(root.join("README.md").exists());
        assert!(root.join("docs/index.md").exists());
        assert!(root.join("docs/reference/install.md").exists());
        assert!(root.join("docs/reference/configuration.md").exists());
        assert!(root.join("docs/reference/telemetry.md").exists());
        assert!(root.join("docs/reference/troubleshooting.md").exists());

        // No-loss end to end: EVERY old section's distinctive token survives
        // verbatim in its placed page -- nothing was dropped or paraphrased.
        let install = std::fs::read_to_string(root.join("docs/reference/install.md")).unwrap();
        assert!(install.contains("WIDGET_TOOLCHAIN_V3"), "{install}");
        let config = std::fs::read_to_string(root.join("docs/reference/configuration.md")).unwrap();
        assert!(config.contains("WIDGET_PORT=8080"), "{config}");
        let telemetry = std::fs::read_to_string(root.join("docs/reference/telemetry.md")).unwrap();
        assert!(telemetry.contains("WIDGET_TELEMETRY_ENDPOINT") && telemetry.contains("submit_metrics()"), "{telemetry}");
        let troubleshooting = std::fs::read_to_string(root.join("docs/reference/troubleshooting.md")).unwrap();
        assert!(
            troubleshooting.contains("widget_diagnose()") && troubleshooting.contains("ERR_WIDGET_JAMMED"),
            "{troubleshooting}"
        );

        // The index hub links to every relocated page.
        let index = std::fs::read_to_string(root.join("docs/index.md")).unwrap();
        for heading in ["Install", "Configuration", "Telemetry", "Troubleshooting"] {
            assert!(index.contains(heading), "index missing a link for {heading}: {index}");
        }

        // The final landing every docs/ link resolves against the placed tree.
        let new_readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert_ne!(new_readme, old_readme, "README.md should have been replaced with the concise landing");
        let docs_tree_on_disk = build_docs_tree_from_old_readme(old_readme);
        assert!(check_landing_links(&new_readme, &docs_tree_on_disk).is_ok(), "{new_readme}");
        assert!(new_readme.contains("## Documentation"));
        assert!(new_readme.contains(DOCS_INDEX_PATH));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn relocated_pages_are_byte_exact_slices_of_the_old_readme_source() {
        // A relocated docs page must contain the section's ORIGINAL source
        // bytes (sub-headings, code fences, exact spacing), not a reconstruction
        // from parsed heading/body fields.
        let old = "# Title\n\nIntro.\n\n## Config\n\nSet it up:\n\n### Advanced\n\n```toml\nport = 8080\n```\n\nDone.\n\n## Next\n\nMore.\n";
        let tree = build_docs_tree_from_old_readme(old);
        let config_page = tree
            .iter()
            .find(|f| f.path == "docs/reference/config.md")
            .expect("config page");
        // The exact source slice for the ## Config section (heading through just
        // before ## Next), verbatim -- sub-heading, fenced code, and blank lines
        // all preserved byte-for-byte.
        let expected = "## Config\n\nSet it up:\n\n### Advanced\n\n```toml\nport = 8080\n```\n\nDone.\n\n";
        // EQUALITY, not `contains`: the page is EXACTLY the source slice, with
        // no bytes added around it (no appended trailing newline, no wrapper).
        assert_eq!(
            config_page.content, expected,
            "page is not the byte-exact verbatim source slice"
        );
    }

    #[test]
    fn a_heading_inside_a_code_fence_does_not_split_a_section() {
        // A `## ` line inside a ``` fence is part of a code EXAMPLE, not a real
        // section -- it must not split the enclosing section or invent a page,
        // and the fence must stay intact in the relocated page.
        let old = "# T\n\n## Usage\n\nExample:\n\n```md\n## Not A Real Section\nstill inside the fence\n```\n\nDone.\n\n## Config\n\nSet up `WIDGET_PORT`.\n";
        let tree = build_docs_tree_from_old_readme(old);
        // Exactly two reference pages (Usage, Config) + the index -> 3 files.
        let ref_pages: Vec<_> = tree.iter().filter(|f| f.path.starts_with("docs/reference/")).collect();
        assert_eq!(ref_pages.len(), 2, "fenced ## must not create a third page: {:?}",
            ref_pages.iter().map(|f| &f.path).collect::<Vec<_>>());
        let usage = tree.iter().find(|f| f.path == "docs/reference/usage.md").expect("usage page");
        // The fenced heading + its fence live verbatim inside the Usage page.
        assert!(usage.content.contains("```md\n## Not A Real Section\nstill inside the fence\n```"),
            "fence not preserved intact: {}", usage.content);
        assert!(tree.iter().any(|f| f.path == "docs/reference/config.md"));
        assert!(!tree.iter().any(|f| f.path == "docs/reference/not-a-real-section.md"));
    }

    // ── Edge: README with no `##` sections -> one whole-document page ────

    #[tokio::test]
    async fn backfill_relocates_a_single_prose_blob_with_no_headings_into_one_page() {
        let root = unique_tmp_dir("no-headings");
        let old_readme =
            "Just a flat README with a `WIDGET_LEGACY_FLAG` reference and no markdown sections at all.";
        std::fs::write(root.join("README.md"), old_readme).unwrap();

        let generator = MockDocGenerator::new("# Widget\n\nA tidy widget factory.\n");
        let store = VersionStore::new();

        let report = backfill_readme(
            &generator,
            &store,
            "TERM",
            ".",
            "backfill-no-headings",
            "backfill against a README with no ## sections",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(report.missing.is_empty(), "{:?}", report.missing);
        assert!(report.placed, "{}", report.summary);
        assert_eq!(report.docs_files_created.len(), 2, "{:?}", report.docs_files_created); // index + overview

        assert!(root.join("docs/reference/overview.md").exists());
        let overview = std::fs::read_to_string(root.join("docs/reference/overview.md")).unwrap();
        assert!(overview.contains("WIDGET_LEGACY_FLAG"), "{overview}");

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

    // ── An oversized LLM landing is TRIMMED, not refused ─────────────────
    //
    // DLAND-RELOC: this replaces the old DLAND-05
    // `backfill_refuses_to_place_on_a_landing_gate_failure` test. That test
    // asserted a landing lint failure REFUSED placement -- but
    // `assemble_final_landing`/`enforce_landing_length` now trim an
    // oversized LLM hero/quick-start down to the concise-landing budget
    // (never dropping the Documentation link), so this scenario should
    // succeed rather than get withheld. The DLAND-03 gates remain enforced
    // as a fail-closed backstop inside `place_docs` regardless (see
    // `assemble_final_landing`'s own doc comment).
    #[tokio::test]
    async fn backfill_trims_an_oversized_llm_landing_instead_of_refusing_it() {
        let root = unique_tmp_dir("gate-failure");
        // No README.md at target_root -- first-doc case, nothing to lose.
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
        assert!(report.gate_failures.is_empty(), "{:?}", report.gate_failures);
        assert!(report.placed, "an oversized LLM landing should be trimmed and placed: {}", report.summary);
        let new_lines = report.new_landing_lines.expect("a landing was generated");
        assert!(new_lines <= LANDING_MAX_LINES, "landing was not trimmed: {new_lines} lines");
        assert!(root.join("README.md").exists());
        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert!(readme.contains("## Documentation"), "the Documentation link must survive trimming");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Link-integrity: every docs/ link in the final landing resolves ───

    #[tokio::test]
    async fn final_landing_docs_links_always_resolve_against_the_docs_tree() {
        let root = unique_tmp_dir("link-integrity");
        let old_readme = "# Widget\n\n## Install\n\nRun `widget_setup()`.\n\n## Usage\n\nCall `widget_run()`.\n";
        std::fs::write(root.join("README.md"), old_readme).unwrap();

        // The mock LLM even tries to link to a `docs/` path of its own
        // invention -- `assemble_final_landing` must strip it (it would
        // dangle) rather than let it leak into the placed landing.
        let generator = MockDocGenerator::new(
            "# Widget\n\nA widget factory.\n\n## Quickstart\n\nSee [Setup](docs/made-up-by-llm.md) first.\n\n\
## Documentation\n\nSee [Fake Docs](docs/also-made-up.md).\n",
        );
        let store = VersionStore::new();

        let report = backfill_readme(
            &generator,
            &store,
            "TERM",
            ".",
            "backfill-link-integrity",
            "backfill whose LLM landing invents dangling docs/ links",
            Some(&readme_config()),
            &BTreeSet::new(),
            "2026-07-18T00:00:00Z",
            &root,
        )
        .await;

        assert!(report.placed, "{}", report.summary);
        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert!(!readme.contains("made-up-by-llm"), "{readme}");
        assert!(!readme.contains("also-made-up"), "{readme}");

        let docs_tree = build_docs_tree_from_old_readme(old_readme);
        assert!(check_landing_links(&readme, &docs_tree).is_ok(), "{readme}");

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
