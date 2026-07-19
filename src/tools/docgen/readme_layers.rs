//! DOCGEN-13: multi-layered Diátaxis README + template set (progressive
//! disclosure), S95, Plane TERM-164.
//!
//! ## DOCGEN-21 revision (S95 REVISION, Plane TERM-334, 2026-07-12)
//! Operator feedback: the original DOCGEN-13 landing concatenated hero +
//! quickstart + deep-dive + all four Diátaxis modes into ONE giant
//! infinite-scroll README. [`build_layered_body`] is REWRITTEN to emit a
//! CONCISE (~130-180 line, lint-checked via [`check_landing_length`])
//! marketing-grade landing page in the fixed section order from the
//! revision spec's §D1, that LINKS OUT to a `docs/` tree
//! (`render::docs_tree`, built from this module's own
//! [`render_diataxis_set`] output) instead of inlining anything deep. See
//! the "DOCGEN-21" doc comment directly above [`build_layered_body`] for
//! the exact section order and rationale.
//!
//! ## What this item adds
//! Builds ON the merged render layer (DOCGEN-06, `render/markdown.rs`) --
//! it does not replace it. The README target still goes through
//! `crate::scribe::vault::render_note`/`NoteFrontmatter` for its frontmatter
//! block and body assembly (REUSE, not reimplementation, per the
//! RECONCILIATION CONSTRAINTS the render module's doc comment already
//! establishes) -- this module only decides WHAT goes into that body: a
//! layered, progressive-disclosure structure (hero -> quickstart ->
//! deep-dive) following the standard-readme section order, plus a parallel
//! four-way Diátaxis split (tutorial / how-to / reference / explanation)
//! for wider docs, each tagged with its mode in YAML frontmatter.
//!
//! ## Templating choice: no new heavyweight dependency
//! `grep -i tera Cargo.toml` at the start of this item found no existing
//! Tera dependency in this crate, and the build sandbox this item was
//! implemented in cannot add a new crates.io dependency. Per this item's
//! own instructions ("do NOT block on adding Tera"), this module uses a
//! lightweight built-in string-template approach instead: plain Rust
//! functions building `String`s via `push_str`/`format!`, the same style
//! already used throughout `render/markdown.rs`, `render/wiki.rs`, and
//! `scribe::vault::render_note` -- no template engine, no new dependency.
//! If Tera lands in this crate later, [`build_layered_body`] and
//! [`render_diataxis_set`] are the two functions to convert to templates;
//! their string construction is centralized here for exactly that reason.
//!
//! ## Progressive disclosure: hero -> quickstart -> deep-dive
//! [`parse_layers`] splits a single generated content blob (DOCGEN-05's
//! output, already deepened and PII-swept upstream -- this module performs
//! no PII scanning of its own, matching `render/mod.rs`'s existing posture)
//! into three layers using markdown `## Quickstart` / `## Deep Dive`
//! headings as section markers (case-insensitive): everything before the
//! first `## ` heading is the **hero** layer (title + one-liner +
//! background), the `## Quickstart` section is the **quickstart** layer,
//! and the `## Deep Dive` section is the **deep-dive** layer. A generator
//! that doesn't yet structure its output this way simply produces an empty
//! quickstart/deep-dive layer, which [`build_layered_body`] renders as an
//! explicit "no content yet" placeholder rather than silently omitting the
//! section (so the standard-readme section order is always present, even
//! before a project has real content for every layer).
//!
//! ## Deepen-not-regenerate, per layer (reuses DOCGEN-05's semantics)
//! `generate.rs` (DOCGEN-05) already guarantees deepen-not-regenerate at the
//! *generation* stage (existing docs are always in the prompt so a real
//! model revises rather than starts blank). This module extends the same
//! posture to the *render* stage, per layer: [`deepen_layers`] takes the
//! previous rendered README's layers (parsed back out of its own frontmatter
//! body via [`parse_layers`]) and the newly generated layers, and for any
//! layer where the new generation is empty/whitespace-only, keeps the prior
//! layer's content instead of blanking it out. A layer that DID get new
//! content from this round is fully replaced (deepened), matching
//! `generate.rs`'s "revise/extend, not append forever" model -- this is not
//! a naive concatenation of old+new. See
//! `deepen_layers_preserves_untouched_layer_but_replaces_updated_layer`
//! below for the before/after fixture.
//!
//! ## Diátaxis mode tagging (consistent with `NoteFrontmatter`)
//! [`render_diataxis_set`] produces one artifact per [`DiataxisMode`], each
//! built via the SAME `render_note`/`NoteFrontmatter` pure primitive the
//! README layer uses (title/module/generated_at/source_commit/type --
//! `NoteType::Wiki`, the closest existing `NoteType` for a wider doc
//! artifact; DOCGEN-13 does not add a new `NoteType` variant, since that
//! enum's four members are shared, load-bearing structure other docgen
//! items and Scribe's own vault layout already depend on). A `diataxis:
//! "<mode>"` field is then spliced into that exact frontmatter block by
//! [`tag_diataxis_frontmatter`], immediately before the closing `---` --
//! this keeps every existing frontmatter field/escaping rule from
//! `scribe::vault::render_note` untouched and adds exactly one new,
//! consistently-quoted field per artifact.
//!
//! ## Write-model inversion (same rule as `render/mod.rs`)
//! Every function in this module is pure: it takes context/existing content
//! as plain arguments and RETURNS a `String`/[`RenderedArtifact`]/
//! [`DiataxisArtifact`]. Nothing here writes to a repo, the filesystem, the
//! knowledge vault, or performs any git/network operation -- placement is
//! entirely the calling harness's decision, exactly like every other
//! renderer in `render/`.

use std::collections::HashSet;

use crate::scribe::vault::{render_note, NoteFrontmatter, NoteType};

use super::config::DocTargetType;
use super::diagram::{default_architecture_mermaid_source, mermaid_fence, subsystem_architecture_mermaid_source};
use super::prompts::RepoIdentity;
use super::render::docs_tree::DocsTreeFile;
use super::render::{RenderContext, RenderedArtifact};
use super::repo_facts::RepoFacts;

/// Build the architecture slot's embed markdown for the hero layer.
/// DOCGEN-22 (revision) replaces the original DOCGEN-11 HTML-comment
/// placeholder (which rendered invisibly, since nothing ever replaced it --
/// the module's own d2 raster path shells to a binary that isn't installed
/// by default) with a REAL, rendering fenced ```mermaid `flowchart` block.
/// This function has no per-call access to a project-specific generated
/// diagram (`RenderContext` carries no diagram field -- that stays
/// DOCGEN-11's/DOCGEN-22's concern), so it always renders the generic,
/// always-valid default template
/// ([`default_architecture_mermaid_source`]) -- itself swept through the
/// DOCGEN-02 PII gate before ever reaching this string, so a module label
/// carrying an internal hostname never leaks into the README. Falls back to
/// a minimal static fence in the (practically unreachable) case the sweep
/// fully blocks the label -- this function must never panic, never emit an
/// HTML comment, and never emit a broken `<img>`.
///
/// `pub(crate)`: DOCGEN-21 (S95 REVISION, TERM-334) reuses this SAME slot
/// (never reinventing a second diagram embed) for `docs/architecture.md`'s
/// full diagram in `render::docs_tree` -- one architecture mermaid source,
/// embedded in both the README hero and the deep architecture page.
pub(crate) fn architecture_slot(module: &str) -> String {
    const STATIC_FALLBACK: &str = "```mermaid\nflowchart LR\n    A[Client] --> B[Core]\n```";
    match default_architecture_mermaid_source(module) {
        Ok(source) => super::diagram::mermaid_fence(&source).unwrap_or_else(|_| STATIC_FALLBACK.to_string()),
        Err(_) => STATIC_FALLBACK.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Diátaxis mode
// ---------------------------------------------------------------------------

/// The four Diátaxis documentation modes: learning-oriented (tutorial),
/// task-oriented (how-to), information-oriented (reference), and
/// understanding-oriented (explanation). See
/// <https://diataxis.fr/> for the framework this names come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiataxisMode {
    Tutorial,
    HowTo,
    Reference,
    Explanation,
}

impl DiataxisMode {
    /// All four modes, in the framework's canonical order.
    pub const ALL: [DiataxisMode; 4] = [
        DiataxisMode::Tutorial,
        DiataxisMode::HowTo,
        DiataxisMode::Reference,
        DiataxisMode::Explanation,
    ];

    /// The exact YAML frontmatter value this mode is tagged with, e.g.
    /// `"how-to"` -- also used as the artifact's short slug.
    pub fn as_str(self) -> &'static str {
        match self {
            DiataxisMode::Tutorial => "tutorial",
            DiataxisMode::HowTo => "how-to",
            DiataxisMode::Reference => "reference",
            DiataxisMode::Explanation => "explanation",
        }
    }

    /// The `## <Heading>` name [`extract_section`] looks for in the
    /// generated content blob to find this mode's material.
    fn section_heading(self) -> &'static str {
        match self {
            DiataxisMode::Tutorial => "Tutorial",
            DiataxisMode::HowTo => "How-To",
            DiataxisMode::Reference => "Reference",
            DiataxisMode::Explanation => "Explanation",
        }
    }
}

// ---------------------------------------------------------------------------
// Section parsing (pure, no I/O)
// ---------------------------------------------------------------------------

/// Everything before the first `## ` heading in `content`, trimmed. This is
/// the hero layer's raw material (title line(s) plus lead-in prose).
fn preamble(content: &str) -> String {
    let mut out = String::new();
    for line in content.lines() {
        if line.trim_start().starts_with("## ") {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Find a markdown `## <header_name>` section (case-insensitive exact match
/// on the heading text) and return its body -- everything up to the next
/// `## ` heading or end of content -- trimmed. Returns `None` when the
/// section is absent or present-but-empty, so callers can distinguish "no
/// content for this layer" from "empty string content" identically.
fn extract_section(content: &str, header_name: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut start = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if let Some(title) = trimmed.strip_prefix("## ") {
            if title.trim().eq_ignore_ascii_case(header_name) {
                start = Some(i + 1);
                break;
            }
        }
    }
    let start = start?;
    let mut end = lines.len();
    for (offset, line) in lines[start..].iter().enumerate() {
        if line.trim_start().starts_with("## ") {
            end = start + offset;
            break;
        }
    }
    let section = lines[start..end].join("\n");
    let trimmed = section.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Strip a `render_note`-produced frontmatter block (`---\n...\n---\n\n`)
/// off the front of `rendered`, returning just the body. Used to recover a
/// PRIOR layer's plain content before re-parsing it with [`parse_layers`] /
/// [`extract_section`] for the deepen-per-layer merge.
///
/// `pub(crate)`: DOCGEN-21 (S95 REVISION, TERM-334) reuses this to recover
/// the plain body out of a [`DiataxisArtifact`]'s `render_note`-framed
/// content in `render::docs_tree`, rather than a second copy of the same
/// frontmatter-stripping logic.
pub(crate) fn strip_frontmatter(rendered: &str) -> String {
    const CLOSE: &str = "\n---\n\n";
    if rendered.starts_with("---\n") {
        if let Some(pos) = rendered.find(CLOSE) {
            return rendered[pos + CLOSE.len()..].to_string();
        }
    }
    rendered.to_string()
}

// ---------------------------------------------------------------------------
// README layers: hero / quickstart / deep-dive
// ---------------------------------------------------------------------------

/// The three progressive-disclosure layers a layered README is built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedLayers {
    pub hero: String,
    pub quickstart: String,
    pub deep_dive: String,
}

/// Split a single generated content blob into [`ParsedLayers`]. See the
/// module doc comment's "Progressive disclosure" section for the exact
/// splitting rule.
pub fn parse_layers(content: &str) -> ParsedLayers {
    ParsedLayers {
        hero: preamble(content),
        // Accept both the GENERATED heading names (Quickstart/Deep Dive) and
        // the RENDERED ones (Usage/API, from build_layered_body) so a prior
        // *rendered* README round-trips back into layers for the deepen merge.
        // Without this, a round that OMITS a layer would drop the prior
        // content -- it lives under the rendered heading, not the generated
        // one (regression fixed here: preserves_prior_quickstart_when_round_omits_it).
        // DOCGEN-21: the landing renders "## Quick Start" (not "## Usage"
        // -- that heading name retired with the old giant-file layout), so
        // the round-trip recognizer must accept it too or a previously-
        // rendered landing's quickstart content would silently vanish on
        // the next deepen pass.
        quickstart: extract_section_any(content, &["Quickstart", "Quick Start", "Usage"]).unwrap_or_default(),
        deep_dive: extract_section_any(content, &["Deep Dive", "API"]).unwrap_or_default(),
    }
}

/// Try [`extract_section`] for each candidate heading in order, returning the
/// first match. One parser thus handles both generated content (Quickstart/
/// Deep Dive headings) and a previously-rendered README (Usage/API headings).
fn extract_section_any(content: &str, header_names: &[&str]) -> Option<String> {
    header_names.iter().find_map(|name| extract_section(content, name))
}

/// Preserve a layer's PRIOR content when this round's generation didn't
/// produce anything new for it; otherwise use the new content. Never a
/// concatenation of old+new -- a layer that DID change is fully replaced
/// (deepened), matching `generate.rs`'s deepen semantics at the generation
/// stage.
fn pick_layer(new: &str, existing: &str) -> String {
    if new.trim().is_empty() {
        existing.to_string()
    } else {
        new.to_string()
    }
}

/// Merge previously-rendered layers with newly-generated ones, per layer.
/// `existing` is `None` for a project's first-ever layered README (spec
/// EDGE CASE parity with `generate.rs`'s first-doc case: nothing to
/// preserve, so `new` passes through untouched).
pub fn deepen_layers(existing: Option<&ParsedLayers>, new: &ParsedLayers) -> ParsedLayers {
    let existing = match existing {
        Some(e) => e,
        None => return new.clone(),
    };
    ParsedLayers {
        hero: pick_layer(&new.hero, &existing.hero),
        quickstart: pick_layer(&new.quickstart, &existing.quickstart),
        deep_dive: pick_layer(&new.deep_dive, &existing.deep_dive),
    }
}

/// Split the hero layer into a one-liner (the first non-empty, non-heading
/// line) and the full hero text (used to derive the "Why" bullets and the
/// architecture-at-a-glance blurb).
fn split_hero(hero: &str) -> (String, String) {
    let mut one_liner = String::new();
    for line in hero.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        one_liner = trimmed.to_string();
        break;
    }
    (one_liner, hero.trim().to_string())
}

// ---------------------------------------------------------------------------
// DOCGEN-21 (S95 REVISION, TERM-334): concise landing README
// ---------------------------------------------------------------------------
//
// Operator feedback (2026-07-11): the pre-revision engine concatenated
// EVERY layer/mode into one giant infinite-scroll README (Terminus's own
// README hit 1469 lines). This section replaces that with the fixed
// ~130-180 line landing template from the revision spec's §D1: hero -> ---
// -> architecture mermaid -> Why -> Quick Start -> Documentation (nav
// table) -> Architecture at a glance -> Contributing -> License.
// Everything deeper LINKS OUT to `render::docs_tree`'s docs/ pages --
// never inlined here again. The exact link targets are named constants so
// this table and `render::docs_tree`'s real emitted paths can never
// silently drift apart (see `documentation_nav_table_links_to_the_real_docs_tree_paths`).

/// Repo-relative path constants for the docs/ tree this landing links out
/// to (produced by `render::docs_tree::build_docs_tree`). Centralized here,
/// not scattered string literals, so the landing's links and the real
/// generated tree can never point at different paths.
pub const DOCS_INDEX_PATH: &str = "docs/index.md";
pub const DOCS_GETTING_STARTED_PATH: &str = "docs/getting-started.md";
pub const DOCS_GUIDES_INDEX_PATH: &str = "docs/guides/index.md";
pub const DOCS_REFERENCE_INDEX_PATH: &str = "docs/reference/index.md";
pub const DOCS_ARCHITECTURE_PATH: &str = "docs/architecture.md";
pub const CHANGELOG_PATH: &str = "CHANGELOG.md";
pub const LICENSE_PATH: &str = "LICENSE";

/// The landing README's target maximum length, in lines. DGRICH-05 (S119,
/// `fable-docgen-redesign.md` §1.1) raises this from 180 to 300: the
/// rich, KG-grounded 8-section landing (real "What is"/architecture/
/// subsystem-feature-table/documentation-index/at-a-glance content, see
/// [`build_landing_body`]) genuinely needs 150-250 lines of real content,
/// and 180 was clipping it. A concrete, testable ceiling rather than "keep
/// it concise" left unchecked.
pub const LANDING_MAX_LINES: usize = 300;

/// DGRICH-05: the paired FLOOR to [`LANDING_MAX_LINES`] -- a landing must
/// have at least this many non-blank, non-chrome lines (see
/// [`check_landing_substance`]) or it is exactly as much a gate failure as
/// one over the ceiling. This is what makes the pre-DGRICH-05 bare
/// ~50-61-line all-chrome landing (`fable-docgen-redesign.md` §0) a
/// structural impossibility rather than merely a discouraged outcome.
pub const LANDING_MIN_SUBSTANTIVE_LINES: usize = 80;

/// Lint: how many lines does a rendered landing README (frontmatter
/// included) have?
pub fn landing_line_count(content: &str) -> usize {
    content.lines().count()
}

/// Lint: is `content` at or under [`LANDING_MAX_LINES`]? A dedicated,
/// callable check (not just a test assertion) so a future pipeline gate can
/// reuse it, matching this crate's existing `quality::lint_prose` posture
/// of exposing lints as plain functions.
pub fn check_landing_length(content: &str) -> Result<(), String> {
    let n = landing_line_count(content);
    if n > LANDING_MAX_LINES {
        Err(format!(
            "landing README is {n} lines, exceeds the {LANDING_MAX_LINES}-line concise-landing \
ceiling (fable-docgen-redesign.md §1.1) -- move deep content into the docs/ tree instead of \
inlining it"
        ))
    } else {
        Ok(())
    }
}

/// True for a line [`check_landing_substance`] does NOT count towards the
/// substance floor: blank lines, horizontal rules, and the hero's
/// single-line HTML chrome (`<h1 align=...>`/`<p align=...>` and their
/// closing tags). Everything else -- prose, headings, table rows, list
/// items, mermaid fences, links -- counts as real content.
fn is_substantive_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed == "---" {
        return false;
    }
    if trimmed.starts_with("<h1") || trimmed.starts_with("</h1") {
        return false;
    }
    if trimmed.starts_with("<p align") || trimmed == "</p>" {
        return false;
    }
    true
}

/// How many non-blank, non-chrome lines a rendered landing has -- the
/// counterpart to [`landing_line_count`] for the [`LANDING_MIN_SUBSTANTIVE_LINES`]
/// floor.
pub fn substantive_line_count(content: &str) -> usize {
    content.lines().filter(|l| is_substantive_line(l)).count()
}

/// Lint: does `content` clear [`LANDING_MIN_SUBSTANTIVE_LINES`]? Fails
/// exactly like [`check_landing_length`] fails above the cap -- a landing
/// that is all chrome and no real content is a gate failure, not a warning.
pub fn check_landing_substance(content: &str) -> Result<(), String> {
    let n = substantive_line_count(content);
    if n < LANDING_MIN_SUBSTANTIVE_LINES {
        Err(format!(
            "landing README has only {n} substantive lines, below the \
{LANDING_MIN_SUBSTANTIVE_LINES}-line substance floor (fable-docgen-redesign.md §1.1) -- this \
looks like all chrome and no real content"
        ))
    } else {
        Ok(())
    }
}

/// Extract every markdown inline-link target (`[text](target)`) from
/// `content`, in order of appearance, including duplicates. A tiny hand-rolled
/// scanner rather than a regex dependency (matching this crate's existing
/// posture of avoiding new heavyweight deps for simple string work, per this
/// file's "Templating choice" doc comment) -- link targets in generated
/// landings are always plain paths/URLs with no embedded parentheses, so a
/// naive "find the next `](`, then the matching `)`" scan is sufficient.
fn extract_link_targets(content: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = content[search_from..].find("](") {
        let start = search_from + rel + 2;
        match content[start..].find(')') {
            Some(rel_end) => {
                let end = start + rel_end;
                targets.push(content[start..end].to_string());
                search_from = end + 1;
            }
            None => break,
        }
    }
    targets
}

/// Lint: does every LOCAL `docs/`-rooted link target in `landing` resolve to
/// a file that was actually generated in `docs_tree`? External `http(s)://`
/// links and pure `#anchor` links are ignored entirely (they are not this
/// module's docs/ tree to validate). A link target may carry a `#fragment`
/// suffix (e.g. `docs/index.md#section`) -- only the path portion before the
/// `#` is checked against `docs_tree`.
///
/// Pure and reusable, matching this file's existing lint posture
/// ([`check_landing_length`]): returns `Ok(())` when every local doc link
/// resolves, or `Err(dangling_targets)` (deduplicated, in first-seen order)
/// naming each target that has no matching [`DocsTreeFile::path`].
/// The docs-tree root prefix (e.g. `"docs/"`), DERIVED from the shared
/// `DOCS_*_PATH` constants rather than hardcoded, so if the sub-page tree is
/// ever rooted elsewhere the link gate follows automatically and never silently
/// stops validating local doc links. All `DOCS_*_PATH` constants share this
/// first path component (asserted by a unit test).
pub fn docs_root_prefix() -> &'static str {
    match DOCS_INDEX_PATH.find('/') {
        Some(i) => &DOCS_INDEX_PATH[..=i],
        None => DOCS_INDEX_PATH,
    }
}

pub fn check_landing_links(landing: &str, docs_tree: &[DocsTreeFile]) -> Result<(), Vec<String>> {
    let known: HashSet<&str> = docs_tree.iter().map(|f| f.path.as_str()).collect();
    let root = docs_root_prefix();
    let mut dangling = Vec::new();
    let mut seen = HashSet::new();
    for target in extract_link_targets(landing) {
        let target = target.trim();
        if target.is_empty()
            || target.starts_with('#')
            || target.starts_with("http://")
            || target.starts_with("https://")
        {
            continue;
        }
        let path_part = target.split('#').next().unwrap_or(target);
        if path_part.starts_with(root) && !known.contains(path_part) {
            if seen.insert(target.to_string()) {
                dangling.push(target.to_string());
            }
        }
    }
    if dangling.is_empty() {
        Ok(())
    } else {
        Err(dangling)
    }
}

/// The fixed nav-link row directly under the hero (spec §D1: "nav-link row
/// `Docs · Quickstart · Reference · Architecture · Changelog`").
fn nav_link_row() -> String {
    format!(
        "[Docs]({DOCS_INDEX_PATH}) · [Quickstart]({DOCS_GETTING_STARTED_PATH}) · \
[Reference]({DOCS_REFERENCE_INDEX_PATH}) · [Architecture]({DOCS_ARCHITECTURE_PATH}) · \
[Changelog]({CHANGELOG_PATH})"
    )
}

/// The `## Documentation` nav table: every sub-page the landing links out
/// to instead of inlining.
fn documentation_nav_table() -> String {
    format!(
        "| Section | What's there |\n\
|---|---|\n\
| [Getting Started]({DOCS_GETTING_STARTED_PATH}) | A first working setup, tutorial-style. |\n\
| [Guides]({DOCS_GUIDES_INDEX_PATH}) | Task-oriented how-tos. |\n\
| [Reference]({DOCS_REFERENCE_INDEX_PATH}) | CLI, API, and configuration reference. |\n\
| [Architecture]({DOCS_ARCHITECTURE_PATH}) | How the system fits together, in depth. |\n\
| [Changelog]({CHANGELOG_PATH}) | What changed, release by release. |\n"
    )
}

// ---------------------------------------------------------------------------
// DGRICH-05 (S119, `fable-docgen-redesign.md` §1.1/§4): the rich, repo-level
// landing assembly -- deterministic, assembled from `RepoIdentity` (Pass 1),
// `RepoFacts` (Pass 0), and the ACTUAL emitted docs tree (DGRICH-06's
// `build_docs_tree` output), never a per-module `RenderContext`/content
// blob. This is additive to, not a replacement of, the legacy per-module
// `build_layered_body`/`render_layered_readme` path above (see that
// function's own doc comment and this module's "Reuse plan" note) -- the
// repo-level trigger path (DGRICH-07) wires `build_landing_body` in once it
// lands; until then this is a standalone, independently-tested assembly
// function.
// ---------------------------------------------------------------------------

/// A repo-relative reference-page path for `subsystem`, matching the exact
/// path DGRICH-06's `build_docs_tree` emits one page at per kept subsystem
/// (`docs/reference/<subsystem>.md`) -- centralized here (not a scattered
/// string literal) so the landing's feature-table links and the real
/// generated tree can never point at different paths.
fn subsystem_reference_path(subsystem: &str) -> String {
    format!("docs/reference/{subsystem}.md")
}

/// `n` formatted as a short "11.9k"-style count for `n >= 1000`, or the
/// plain integer otherwise -- used by [`fact_row`] so a KG node count reads
/// like "11.9k KG nodes" rather than "11905 KG nodes".
fn format_count_k(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// The first `n` characters of `git_ref` -- a short-sha-style truncation for
/// the fact row's "analyzed <sha>" clause. `git_ref` is always an
/// ASCII commit hash/ref name in practice, so a byte-index slice is safe;
/// falls back to the whole string when it is already shorter than `n`.
fn short_git_ref(git_ref: &str) -> &str {
    let n = git_ref.len().min(7);
    &git_ref[..n]
}

/// A deterministic, non-fabricated "primary language" label for the fact
/// row. `RepoFacts` carries no explicit language field, so this is a small,
/// honest heuristic over facts it DOES carry: a Cargo.toml description or
/// any real `[[bin]]` target means Rust; a subsystem name carrying the
/// `<pkg>::src` TS-tree shape (`repo_facts::subsystem_prefix`'s own
/// TS-tree rule) means TypeScript; otherwise a neutral "Code" rather than
/// guessing.
fn primary_language(facts: &RepoFacts) -> &'static str {
    if facts.prose_anchors.crate_description.is_some() || !facts.entry_points.bin_targets.is_empty() {
        "Rust"
    } else if facts.subsystems.iter().any(|s| s.name.ends_with("::src")) {
        "TypeScript"
    } else {
        "Code"
    }
}

/// DGRICH-05: the fact row that REPLACES the fake shields.io badge row
/// (`fable-docgen-redesign.md` §1.1 row 1, §4) -- one plain, theme-safe
/// line, e.g. `Rust · 410 modules · 53 MCP tools · 11.9k KG nodes ·
/// analyzed a1b2c3d`. Every number is computed directly from `facts`; when
/// `facts.kg_grounded` is `false`, the KG-derived clauses (module count, KG
/// node count) are OMITTED rather than fabricated -- the registered-tool
/// count is kept regardless, since it comes from a checkout scan
/// (`registry.rs`'s `.register(` call-site count), not the graph.
pub fn fact_row(facts: &RepoFacts) -> String {
    let mut parts: Vec<String> = vec![primary_language(facts).to_string()];

    if facts.kg_grounded {
        if let Some(&modules) = facts.scale.by_kind.get("module") {
            parts.push(format!("{modules} modules"));
        }
    }

    if let Some(tools) = facts.entry_points.registered_tool_count {
        parts.push(format!("{tools} MCP tools"));
    }

    if facts.kg_grounded && facts.scale.node_count > 0 {
        parts.push(format!("{} KG nodes", format_count_k(facts.scale.node_count)));
    }

    parts.push(format!("analyzed {}", short_git_ref(&facts.git_ref)));

    parts.join(" \u{b7} ")
}

/// The `## Architecture` embed for the rich landing: the real derived
/// diagram (DGRICH-04's [`subsystem_architecture_mermaid_source`]), which
/// itself already falls back to the generic default when `facts` has fewer
/// than two real subsystems (the `kg_grounded: false` case included --
/// see that function's own doc comment). Never panics: a hard `Err` from
/// either the derivation or the mermaid-fence validation falls back to the
/// same minimal static fence [`architecture_slot`] uses.
fn architecture_section(facts: &RepoFacts) -> String {
    const STATIC_FALLBACK: &str = "```mermaid\nflowchart LR\n    A[Client] --> B[Core]\n```";
    match subsystem_architecture_mermaid_source(facts) {
        Ok(source) => mermaid_fence(&source).unwrap_or_else(|_| STATIC_FALLBACK.to_string()),
        Err(_) => STATIC_FALLBACK.to_string(),
    }
}

/// The `## Subsystems and Features` table: one row per `identity`
/// feature, each linking to its subsystem's real emitted reference page
/// ([`subsystem_reference_path`]) -- row count tracks the repository's
/// actual feature inventory (5-12 per the Pass 1 prompt), never a fixed
/// count.
fn feature_table(identity: &RepoIdentity) -> String {
    if identity.feature_rows.is_empty() {
        return "_No feature inventory generated yet._".to_string();
    }
    let mut out = String::from("| Feature | Description | Subsystem |\n|---|---|---|\n");
    for row in &identity.feature_rows {
        out.push_str(&format!(
            "| {} | {} | [{}]({}) |\n",
            row.feature,
            row.description,
            row.subsystem,
            subsystem_reference_path(&row.subsystem)
        ));
    }
    out
}

/// The first real descriptive line of a generated docs-tree page's content
/// -- skips blank lines, `#`/`##` headings, the standard breadcrumb/
/// cross-link lines (`render::docs_tree`'s own `breadcrumb`/`cross_links`
/// shape), horizontal rules, and code-fence delimiters. Used by
/// [`documentation_index_table`] so the `## Documentation` index's
/// one-liners are the page's REAL first paragraph, never a fixed
/// description string.
fn first_paragraph(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" {
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("```") {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.contains("Docs Home") {
            continue;
        }
        if trimmed.starts_with("**See also:**") {
            continue;
        }
        return trimmed.to_string();
    }
    "_No description available._".to_string()
}

/// A docs-tree page's display title: its own `# Heading` line if present,
/// else the file's stem (e.g. `docs/reference/mesh.md` -> `mesh`) -- never
/// a fabricated label.
fn page_title(file: &DocsTreeFile) -> String {
    for line in file.content.lines() {
        if let Some(title) = line.trim().strip_prefix("# ") {
            return title.trim().to_string();
        }
    }
    std::path::Path::new(&file.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&file.path)
        .to_string()
}

/// DGRICH-05: the `## Documentation` index for the rich landing, generated
/// from the ACTUAL emitted `docs_tree` -- replaces the fixed 5-row
/// [`documentation_nav_table`] the legacy per-module landing still uses.
/// Row count and content both track what was really generated; a
/// zero-page tree (e.g. a fully degraded generation round) renders an
/// honest placeholder rather than an empty/broken table.
fn documentation_index_table(docs_tree: &[DocsTreeFile]) -> String {
    let pages: Vec<&DocsTreeFile> = docs_tree.iter().filter(|f| f.path != "_Sidebar.md").collect();
    if pages.is_empty() {
        return "_No documentation pages have been generated yet._".to_string();
    }
    let mut out = String::from("| Page | What's there |\n|---|---|\n");
    for file in &pages {
        out.push_str(&format!("| [{}]({}) | {} |\n", page_title(file), file.path, first_paragraph(&file.content)));
    }
    out
}

/// The `## At a Glance` section: computed counts only, straight from
/// `facts` -- functions/structs/traits/modules (from `scale.by_kind`),
/// workspace members, and real `[[bin]]` binaries. When `facts.kg_grounded`
/// is `false` the code-scale counts are honestly reported as unavailable
/// rather than fabricated; the checkout-derived facts (workspace members,
/// binaries) are still shown either way.
fn at_a_glance_section(facts: &RepoFacts) -> String {
    const GLANCE_KINDS: [&str; 4] = ["function", "struct", "trait", "module"];

    let mut lines: Vec<String> = Vec::new();
    if facts.kg_grounded {
        lines.push(format!(
            "- **Nodes / edges:** {} / {}",
            facts.scale.node_count, facts.scale.edge_count
        ));
        for kind in GLANCE_KINDS {
            if let Some(count) = facts.scale.by_kind.get(kind) {
                lines.push(format!("- **{kind}s:** {count}"));
            }
        }
        if !facts.scale.hotspots.is_empty() {
            let names: Vec<String> =
                facts.scale.hotspots.iter().take(5).map(|s| format!("`{}`", s.id)).collect();
            lines.push(format!("- **Top hotspots:** {}", names.join(", ")));
        }
    } else {
        lines.push("- _Not yet KG-grounded -- code-scale counts are unavailable this round._".to_string());
    }

    if !facts.entry_points.workspace_members.is_empty() {
        lines.push(format!("- **Workspace members:** {}", facts.entry_points.workspace_members.join(", ")));
    }
    if !facts.entry_points.bin_targets.is_empty() {
        let names: Vec<&str> = facts.entry_points.bin_targets.iter().map(|b| b.name.as_str()).collect();
        lines.push(format!("- **Binaries:** {}", names.join(", ")));
    }
    lines.join("\n")
}

/// Build the rich, repo-level landing README body (DGRICH-05,
/// `fable-docgen-redesign.md` §1.1's 8-section skeleton), deterministically
/// assembled from `identity` (Pass 1's [`RepoIdentity`]), `facts` (Pass 0's
/// [`RepoFacts`] grounding), and `docs_tree` (the ACTUAL emitted docs
/// tree -- DGRICH-06's `build_docs_tree` output). Every value is
/// repo-derived by construction: there is no hardcoded chrome here for a
/// latch or a generic diagram to hide behind. Section order, verbatim from
/// the design table:
///
/// 1. Hero (`<h1>` + tagline + [`fact_row`])
/// 2. What is `<name>` (`identity.what_is`)
/// 3. Architecture ([`architecture_section`] -- real derived diagram)
/// 4. Subsystems and Features ([`feature_table`] -- linked to real pages)
/// 5. Quick Start (points at `docs/getting-started.md`, never inlined here)
/// 6. Documentation index ([`documentation_index_table`] -- from the real tree)
/// 7. At a Glance ([`at_a_glance_section`] -- computed counts only)
/// 8. Contributing + License (static short pointers)
///
/// This is additive: it does not replace [`build_layered_body`]/
/// [`render_layered_readme`] (the legacy per-module path other renderers in
/// this crate still call) -- see this function's home section's doc
/// comment. The paired [`check_landing_length`]/[`check_landing_substance`]
/// gates apply to its output exactly as they do to the legacy landing.
pub fn build_landing_body(identity: &RepoIdentity, facts: &RepoFacts, docs_tree: &[DocsTreeFile]) -> String {
    let mut out = String::new();

    // 1. Hero: centered name + tagline + fact row.
    out.push_str(&format!("<h1 align=\"center\">{}</h1>\n\n", facts.project_id));
    out.push_str(&format!("<p align=\"center\"><em>{}</em></p>\n\n", identity.tagline));
    out.push_str(&format!("<p align=\"center\">{}</p>\n\n", fact_row(facts)));
    out.push_str("---\n\n");

    // 2. What is <name>.
    out.push_str(&format!("## What is {}\n\n", facts.project_id));
    out.push_str(identity.what_is.trim());
    out.push_str("\n\n");

    // 3. Architecture -- real derived diagram.
    out.push_str("## Architecture\n\n");
    out.push_str(&architecture_section(facts));
    out.push_str("\n\n");

    // 4. Subsystems / Features table -- every row links to its emitted page.
    out.push_str("## Subsystems and Features\n\n");
    out.push_str(&feature_table(identity));
    out.push_str("\n\n");

    // 5. Quick Start -- points at the real getting-started page; never
    // inlines steps here (those belong to docs/getting-started.md, Pass
    // 3's output).
    out.push_str("## Quick Start\n\n");
    out.push_str(&format!(
        "See [Getting Started]({DOCS_GETTING_STARTED_PATH}) for a first working setup, \
tutorial-style.\n"
    ));
    out.push('\n');

    // 6. Documentation index -- generated from the ACTUAL emitted tree.
    out.push_str("## Documentation\n\n");
    out.push_str(&documentation_index_table(docs_tree));
    out.push('\n');

    // 7. At a glance -- computed counts only, never invented.
    out.push_str("## At a Glance\n\n");
    out.push_str(&at_a_glance_section(facts));
    out.push_str("\n\n");

    // 8. Contributing + License.
    out.push_str("## Contributing\n\nSee the project's build pipeline docs for the contribution process.\n\n");
    out.push_str(&format!("## License\n\nSee [LICENSE]({LICENSE_PATH}).\n"));

    out
}

/// The `## Why {project}` body for the LEGACY per-module landing
/// ([`build_layered_body`]): the hero/background prose as-is, or an
/// explicit placeholder for a bare/near-empty hero. DGRICH-05 (S119)
/// deletes this file's prior `why_bullets` -- a sentence-splitting
/// heuristic that fabricated "benefit bullets" out of whatever prose
/// happened to be in the hero layer (garbage in, template out, per
/// `fable-docgen-redesign.md` §0 item 2). This replacement renders the real
/// prose directly rather than re-shaping it into invented bullet points.
fn why_section(background: &str, module: &str) -> String {
    let trimmed = background.trim();
    if trimmed.is_empty() {
        format!(
            "_No background generated yet for {module} -- see \
[Architecture at a glance](#architecture-at-a-glance) below._"
        )
    } else {
        trimmed.to_string()
    }
}

/// The `## Quick Start` body: the quickstart layer as-is (already
/// step/command-shaped from generation, per the file doc comment's
/// "Progressive disclosure" section), or an explicit placeholder pointing
/// at the full tutorial when this round produced none.
fn quick_start_section(quickstart: &str) -> String {
    if quickstart.trim().is_empty() {
        format!(
            "_No quickstart content generated yet -- see [Getting Started]\
({DOCS_GETTING_STARTED_PATH}) for the full tutorial._"
        )
    } else {
        quickstart.trim().to_string()
    }
}

/// The `## Architecture at a glance` body for the LEGACY per-module
/// landing: a link to the full `docs/architecture.md` page -- never the
/// full diagram breakdown itself (that lives only in the docs/ tree and the
/// hero's diagram slot). DGRICH-05 deletes this file's prior
/// `architecture_glance`, which mined the hero's first sentence for a
/// "blurb" -- the same fabricate-a-summary-from-whatever-prose-is-lying-
/// around pattern `why_bullets` had; this module has no per-call access to
/// real architecture facts for the legacy per-module path (no `RepoFacts`
/// is threaded through it), so it links out honestly instead of guessing.
fn architecture_glance_section(module: &str) -> String {
    format!(
        "See [Architecture]({DOCS_ARCHITECTURE_PATH}) for {module}'s full component and \
data-flow breakdown."
    )
}

/// Build the CONCISE landing README body (everything `render_note` wraps in
/// frontmatter) for the LEGACY per-module path, per the revision spec's §D1
/// fixed section order: hero (centered name + tagline + nav-link row) ->
/// `---` -> architecture mermaid -> `## Why {project}` -> `## Quick Start`
/// -> `## Documentation` (nav table) -> `## Architecture at a glance` ->
/// `## Contributing` -> `## License`. This STOPS concatenating every
/// layer/mode into one file (the pre-revision behavior) -- the deep-dive
/// layer and the Diátaxis tutorial/how-to/reference/explanation bodies are
/// NEVER inlined here; they live in `render::docs_tree`'s docs/ pages,
/// which this landing only links to. See [`check_landing_length`] for the
/// paired line-count lint.
///
/// ## DGRICH-05 (S119): the fake shields.io badge row is GONE
/// The pre-DGRICH-05 version of this function rendered an identical
/// `build-passing / version-auto / license-MIT` badge row on every repo --
/// the code comment for the deleted `shields_badges` helper even admitted
/// it had no access to a project's actual CI status
/// (`fable-docgen-redesign.md` §0 item 2, §4). It is deleted, not replaced:
/// the hero now carries only real, repo-derived text (tagline + nav row).
/// This function is NOT the rich repo-level landing DGRICH-05 adds
/// ([`build_landing_body`]) -- it remains the legacy per-module assembly
/// (see the module doc comment's "Reuse plan"), just with the fabricated
/// chrome removed.
fn build_layered_body(ctx: &RenderContext<'_>, layers: &ParsedLayers) -> String {
    let (mut one_liner, background) = split_hero(&layers.hero);
    if one_liner.is_empty() {
        one_liner = format!("{} -- documentation generated by the docgen engine.", ctx.module);
    }

    let mut out = String::new();
    // Hero: centered name + tagline + nav-link row -- no badge row.
    out.push_str(&format!("<h1 align=\"center\">{}</h1>\n\n", ctx.module));
    out.push_str(&format!("<p align=\"center\"><em>{one_liner}</em></p>\n\n"));
    out.push_str(&format!("<p align=\"center\">{}</p>\n\n", nav_link_row()));
    out.push_str("---\n\n");
    // Architecture mermaid (DOCGEN-22's slot -- reused, never reinvented).
    out.push_str(&architecture_slot(ctx.module));
    out.push_str("\n\n");
    out.push_str(&format!("## Why {}\n\n", ctx.project));
    out.push_str(&why_section(&background, ctx.module));
    out.push_str("\n\n## Quick Start\n\n");
    out.push_str(&quick_start_section(&layers.quickstart));
    out.push_str("\n\n## Documentation\n\n");
    out.push_str(&documentation_nav_table());
    out.push_str("\n## Architecture at a glance\n\n");
    out.push_str(&architecture_glance_section(ctx.module));
    out.push_str("\n\n## Contributing\n\nSee the project's build pipeline docs for the contribution process.\n\n");
    out.push_str(&format!("## License\n\nSee [LICENSE]({LICENSE_PATH}).\n"));
    out
}

/// Render the layered README artifact. `existing_readme` is the PRIOR
/// rendered README (as `render_note` produced it, frontmatter included) if
/// one exists, for the per-layer deepen merge -- `None` for a project's
/// first-ever README.
pub fn render_layered_readme(ctx: &RenderContext<'_>, existing_readme: Option<&str>) -> RenderedArtifact {
    let new_layers = parse_layers(ctx.content);
    let existing_layers = existing_readme.map(|prior| parse_layers(&strip_frontmatter(prior)));
    let layers = deepen_layers(existing_layers.as_ref(), &new_layers);
    let body = build_layered_body(ctx, &layers);

    let fm = NoteFrontmatter {
        title: ctx.module.to_string(),
        module: ctx.project.to_string(),
        generated_at: ctx.generated_at.to_string(),
        source_commit: ctx.source_commit.to_string(),
        note_type: NoteType::Readme,
    };
    let content = render_note(&fm, &body, &[]);
    RenderedArtifact::rendered(DocTargetType::Readme, "markdown-layered", content)
}

// ---------------------------------------------------------------------------
// Diátaxis four-way split
// ---------------------------------------------------------------------------

/// One rendered Diátaxis-mode artifact: full `render_note`-produced content
/// (frontmatter, including the spliced-in `diataxis` field, plus body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiataxisArtifact {
    pub mode: DiataxisMode,
    pub content: String,
}

/// Splice a `diataxis: "<mode>"` field into a `render_note`-produced
/// frontmatter block, immediately before the closing `---`. Leaves every
/// other field/escaping rule from `scribe::vault::render_note` untouched --
/// this never re-implements YAML quoting, it only inserts one more
/// already-safe (mode names are a closed, hardcoded set, never user input)
/// line into the existing block.
fn tag_diataxis_frontmatter(rendered: &str, mode: DiataxisMode) -> String {
    const CLOSE: &str = "\n---\n\n";
    if let Some(pos) = rendered.find(CLOSE) {
        let (head, tail) = rendered.split_at(pos);
        format!("{head}\ndiataxis: \"{}\"{tail}", mode.as_str())
    } else {
        rendered.to_string()
    }
}

/// Render all four Diátaxis-mode artifacts from a single generated content
/// blob, splitting on `## Tutorial` / `## How-To` / `## Reference` /
/// `## Explanation` headings (case-insensitive) the same way
/// [`parse_layers`] splits the README layers. `existing` carries the PRIOR
/// round's four artifacts (if any) for the same per-mode deepen-not-
/// regenerate preservation [`deepen_layers`] applies to the README: a mode
/// with no new section this round keeps its prior body rather than being
/// blanked or dropped.
pub fn render_diataxis_set(
    ctx: &RenderContext<'_>,
    existing: Option<&[DiataxisArtifact]>,
) -> Vec<DiataxisArtifact> {
    DiataxisMode::ALL
        .iter()
        .map(|&mode| {
            let mut body = extract_section(ctx.content, mode.section_heading()).unwrap_or_default();
            if body.trim().is_empty() {
                if let Some(prior) = existing.and_then(|set| set.iter().find(|a| a.mode == mode)) {
                    let prior_body = strip_frontmatter(&prior.content);
                    if !prior_body.trim().is_empty() {
                        body = prior_body.trim().to_string();
                    }
                }
            }
            if body.trim().is_empty() {
                body = format!("_No {} content yet for {}._", mode.as_str(), ctx.module);
            }

            let fm = NoteFrontmatter {
                title: format!("{} ({})", ctx.module, mode.as_str()),
                module: ctx.project.to_string(),
                generated_at: ctx.generated_at.to_string(),
                source_commit: ctx.source_commit.to_string(),
                note_type: NoteType::Wiki,
            };
            let rendered = render_note(&fm, &body, &[]);
            let content = tag_diataxis_frontmatter(&rendered, mode);
            DiataxisArtifact { mode, content }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(content: &'a str) -> RenderContext<'a> {
        RenderContext {
            project: "widget-factory",
            module: "src/widget",
            source_commit: "abc123",
            generated_at: "2026-07-11T00:00:00Z",
            content,
        }
    }

    const SAMPLE: &str = "# Widget\n\nThe widget turns raw material into widgets.\n\n\
## Quickstart\n\nRun `widget build` to produce your first widget.\n\n\
## Deep Dive\n\nThe widget pipeline has three stages: intake, shape, finish.\n";

    // ── Layered output: hero / quickstart / deep-dive from sample content ──

    #[test]
    fn parse_layers_splits_sample_content_into_three_layers() {
        let layers = parse_layers(SAMPLE);
        assert!(layers.hero.contains("The widget turns raw material"));
        assert!(layers.quickstart.contains("widget build"));
        assert!(layers.deep_dive.contains("three stages"));
    }

    // ── DOCGEN-21 (S95 REVISION, TERM-334): concise landing, fixed order ──

    #[test]
    fn render_layered_readme_follows_the_fixed_landing_section_order() {
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        assert!(artifact.was_rendered());
        let content = artifact.content.unwrap();

        assert!(content.contains("widget build"), "quickstart layer must reach Quick Start");

        let hero = content.find("src/widget").expect("module name in hero"); // ctx() uses module "src/widget"
        let divider = content.find("\n---\n").expect("hero/body divider");
        let mermaid = content.find("```mermaid").expect("architecture mermaid fence");
        let why = content.find("## Why").expect("Why section");
        let quickstart = content.find("## Quick Start").expect("Quick Start section");
        let docs = content.find("## Documentation").expect("Documentation nav table");
        let arch = content.find("## Architecture at a glance").expect("Architecture at a glance");
        let contributing = content.find("## Contributing").expect("Contributing section");
        let license = content.find("## License").expect("License section");

        assert!(hero < divider, "hero must precede the --- divider");
        assert!(divider < mermaid, "divider must precede the architecture diagram");
        assert!(mermaid < why, "architecture diagram must precede Why");
        assert!(why < quickstart, "Why must precede Quick Start");
        assert!(quickstart < docs, "Quick Start must precede Documentation");
        assert!(docs < arch, "Documentation must precede Architecture at a glance");
        assert!(arch < contributing, "Architecture at a glance must precede Contributing");
        assert!(contributing < license, "Contributing must precede License");
    }

    #[test]
    fn missing_layers_render_explicit_placeholders_not_silently_dropped() {
        let artifact = render_layered_readme(&ctx("# Bare\n\nJust a title, nothing else."), None);
        let content = artifact.content.unwrap();
        // Sections still present even when a layer has no content yet --
        // structure never silently collapses.
        assert!(content.contains("## Quick Start"));
        assert!(content.contains("## Why"));
        assert!(
            content.contains("No quickstart content generated yet"),
            "empty quickstart layer must render an explicit placeholder, not vanish: {content}"
        );
    }

    // ── DOCGEN-21: landing NEVER inlines deep content -- links out instead ──

    #[test]
    fn landing_readme_never_inlines_deep_dive_or_diataxis_bodies() {
        let content_with_deep_dive =
            "# Widget\n\nIntro sentence.\n\n## Deep Dive\n\nDEEPDIVEMARKER content.\n";
        let artifact = render_layered_readme(&ctx(content_with_deep_dive), None);
        let content = artifact.content.unwrap();
        assert!(
            !content.contains("DEEPDIVEMARKER"),
            "landing must link out to docs/, never inline the deep-dive body: {content}"
        );
        // The old single-giant-file section headings must be gone entirely.
        assert!(!content.contains("## API"));
        assert!(!content.contains("## Background"));
        assert!(!content.contains("## Install"));
        assert!(!content.contains("## Usage"));
        assert!(!content.contains("## Table of Contents"));
    }

    #[test]
    fn documentation_nav_table_links_to_the_real_docs_tree_paths() {
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        let content = artifact.content.unwrap();
        assert!(content.contains(DOCS_GETTING_STARTED_PATH));
        assert!(content.contains(DOCS_GUIDES_INDEX_PATH));
        assert!(content.contains(DOCS_REFERENCE_INDEX_PATH));
        assert!(content.contains(DOCS_ARCHITECTURE_PATH));
        // Nav-link row also points at the docs hub + reference + architecture.
        assert!(content.contains(DOCS_INDEX_PATH));
    }

    // ── DOCGEN-21: ≤180-line concise-landing lint ────────────────────────

    #[test]
    fn landing_readme_stays_within_the_concise_line_ceiling() {
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        let content = artifact.content.unwrap();
        assert!(
            check_landing_length(&content).is_ok(),
            "landing README exceeded {LANDING_MAX_LINES} lines: {} lines",
            landing_line_count(&content)
        );
    }

    #[test]
    fn check_landing_length_flags_an_oversized_landing() {
        let oversized = "line\n".repeat(LANDING_MAX_LINES + 1);
        let err = check_landing_length(&oversized).unwrap_err();
        assert!(err.contains("exceeds"));
    }

    #[test]
    fn check_landing_length_accepts_a_landing_at_exactly_the_ceiling() {
        let exact = "line\n".repeat(LANDING_MAX_LINES);
        assert!(check_landing_length(&exact).is_ok());
    }

    // ── DLAND-03: link-resolution lint ───────────────────────────────────

    fn sample_docs_tree_files() -> Vec<DocsTreeFile> {
        vec![
            DocsTreeFile { path: DOCS_INDEX_PATH.to_string(), content: String::new() },
            DocsTreeFile { path: DOCS_GETTING_STARTED_PATH.to_string(), content: String::new() },
            DocsTreeFile { path: DOCS_REFERENCE_INDEX_PATH.to_string(), content: String::new() },
        ]
    }

    #[test]
    fn docs_root_prefix_is_derived_and_matches_every_docs_path_constant() {
        // The link gate keys off this prefix; it must stay in lockstep with the
        // actual DOCS_*_PATH constants (derived, never hardcoded), or the gate
        // could silently stop validating real doc links.
        let root = docs_root_prefix();
        assert!(root.ends_with('/'), "docs root prefix should end with '/': {root:?}");
        for p in [
            DOCS_INDEX_PATH,
            DOCS_GETTING_STARTED_PATH,
            DOCS_GUIDES_INDEX_PATH,
            DOCS_REFERENCE_INDEX_PATH,
            DOCS_ARCHITECTURE_PATH,
        ] {
            assert!(p.starts_with(root), "{p} does not start with derived docs root {root:?}");
        }
    }

    #[test]
    fn check_landing_links_passes_when_every_local_doc_link_resolves() {
        let landing = format!(
            "[Docs]({DOCS_INDEX_PATH}) and [Quickstart]({DOCS_GETTING_STARTED_PATH})"
        );
        assert!(check_landing_links(&landing, &sample_docs_tree_files()).is_ok());
    }

    #[test]
    fn check_landing_links_flags_a_dangling_local_doc_link() {
        let landing = "See [Missing](docs/missing.md) for details.";
        let err = check_landing_links(landing, &sample_docs_tree_files()).unwrap_err();
        assert_eq!(err, vec!["docs/missing.md".to_string()]);
    }

    #[test]
    fn check_landing_links_ignores_external_and_anchor_only_links() {
        let landing = "[External](https://example.com/docs/foo.md) and [Anchor](#section) and \
[Secure](http://example.com/docs/bar.md)";
        assert!(check_landing_links(landing, &sample_docs_tree_files()).is_ok());
    }

    #[test]
    fn check_landing_links_ignores_non_docs_local_links_like_changelog_and_license() {
        let landing = format!("[Changelog]({CHANGELOG_PATH}) and [License]({LICENSE_PATH})");
        assert!(check_landing_links(&landing, &sample_docs_tree_files()).is_ok());
    }

    #[test]
    fn check_landing_links_strips_anchor_fragment_before_checking_the_path() {
        let landing = format!("[Section]({DOCS_INDEX_PATH}#some-section)");
        assert!(check_landing_links(&landing, &sample_docs_tree_files()).is_ok());
    }

    #[test]
    fn check_landing_links_dedupes_repeated_dangling_targets() {
        let landing = "[A](docs/missing.md) and again [B](docs/missing.md)";
        let err = check_landing_links(landing, &sample_docs_tree_files()).unwrap_err();
        assert_eq!(err, vec!["docs/missing.md".to_string()]);
    }

    #[test]
    fn check_landing_links_passes_trivially_on_a_landing_with_no_links() {
        assert!(check_landing_links("Just plain text, no links here.", &[]).is_ok());
    }

    #[test]
    fn a_full_rendered_landing_passes_the_link_gate_against_its_matching_docs_tree() {
        use super::super::render::docs_tree::build_docs_tree;
        let artifacts = render_diataxis_set(&ctx(SAMPLE), None);
        let tree = build_docs_tree(&ctx(SAMPLE), &artifacts);
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        let content = artifact.content.unwrap();
        assert!(
            check_landing_links(&content, &tree).is_ok(),
            "a real rendered landing must resolve against its own matching docs tree"
        );
    }

    // ── DGRICH-05: the fake shields.io badge row is GONE ─────────────────

    #[test]
    fn layered_readme_never_emits_a_shields_io_badge_row() {
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        let content = artifact.content.unwrap();
        assert!(
            !content.contains("shields.io"),
            "the fake build/version/license/docs badge row must be deleted, not just hidden: {content}"
        );
        assert!(!content.contains("build-passing"));
    }

    // ── DOCGEN-22: architecture slot is a real rendering mermaid block ──

    #[test]
    fn layered_readme_architecture_slot_is_a_rendering_mermaid_fence_not_a_placeholder() {
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        let content = artifact.content.unwrap();
        assert!(content.contains("```mermaid\n"), "expected a fenced mermaid block: {content}");
        assert!(content.contains("flowchart"), "expected a flowchart diagram type: {content}");
        // Never the old invisible HTML-comment placeholder.
        assert!(!content.contains("<!--"), "must not fall back to an HTML-comment placeholder: {content}");
        // Never a broken/sanitized <img> embed -- the whole point of this revision.
        assert!(!content.contains("<img"), "must never embed via <img>: {content}");
    }

    #[test]
    fn layered_readme_architecture_slot_carries_the_module_name() {
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        let content = artifact.content.unwrap();
        // ctx() uses module "src/widget" -- the default template labels the
        // diagram with the module it's for.
        assert!(content.contains("src/widget"), "expected the module name in the diagram: {content}");
    }

    // ── Deepen-not-regenerate, per layer (before/after fixture) ─────────

    #[test]
    fn deepen_layers_preserves_untouched_layer_but_replaces_updated_layer() {
        let existing = ParsedLayers {
            hero: "Old hero text.".to_string(),
            quickstart: "Old quickstart: run `widget init`.".to_string(),
            deep_dive: "Old deep dive material.".to_string(),
        };
        // This round's generation only produced a new deep-dive section --
        // hero/quickstart came back empty (generator didn't touch them).
        let new_round = ParsedLayers {
            hero: String::new(),
            quickstart: String::new(),
            deep_dive: "New deep dive material covering the v2 pipeline.".to_string(),
        };

        let merged = deepen_layers(Some(&existing), &new_round);

        // Untouched layers preserved verbatim.
        assert_eq!(merged.hero, "Old hero text.");
        assert_eq!(merged.quickstart, "Old quickstart: run `widget init`.");
        // Updated layer fully replaced (deepened), not concatenated with the old.
        assert_eq!(merged.deep_dive, "New deep dive material covering the v2 pipeline.");
        assert!(!merged.deep_dive.contains("Old deep dive material"));
    }

    #[test]
    fn deepen_layers_with_no_existing_passes_new_layers_through_first_doc_case() {
        let new_round = ParsedLayers {
            hero: "Brand new hero.".to_string(),
            quickstart: "Brand new quickstart.".to_string(),
            deep_dive: "Brand new deep dive.".to_string(),
        };
        let merged = deepen_layers(None, &new_round);
        assert_eq!(merged, new_round);
    }

    #[test]
    fn render_layered_readme_end_to_end_preserves_prior_quickstart_when_round_omits_it() {
        let first = render_layered_readme(&ctx(SAMPLE), None);
        let first_content = first.content.unwrap();
        assert!(first_content.contains("widget build"));

        // Second round: only the deep-dive changed; quickstart section is
        // absent from this round's generated content entirely.
        let second_round_content =
            "# Widget\n\nThe widget turns raw material into widgets.\n\n\
## Deep Dive\n\nNow with a fourth stage: package.\n";
        let second = render_layered_readme(&ctx(second_round_content), Some(&first_content));
        let second_content = second.content.unwrap();

        assert!(second_content.contains("widget build"), "prior quickstart must be preserved on the landing");
        // DOCGEN-21: the landing never inlines deep-dive content at all
        // (regardless of round/deepen state) -- it links to docs/ instead.
        assert!(
            !second_content.contains("fourth stage: package"),
            "deep-dive content must never be inlined on the landing, even a freshly-deepened one"
        );
    }

    // ── Diátaxis mode tagging ────────────────────────────────────────────

    #[test]
    fn render_diataxis_set_produces_all_four_modes_tagged_in_frontmatter() {
        let content = "# Widget\n\n\
## Tutorial\n\nFollow along to build your first widget from scratch.\n\n\
## How-To\n\nTo reconfigure the widget, edit its config file.\n\n\
## Reference\n\n`widget build [--flag]` -- builds a widget.\n\n\
## Explanation\n\nThe widget pipeline exists because raw material varies.\n";
        let artifacts = render_diataxis_set(&ctx(content), None);
        assert_eq!(artifacts.len(), 4);

        let expect = [
            (DiataxisMode::Tutorial, "tutorial", "Follow along"),
            (DiataxisMode::HowTo, "how-to", "reconfigure the widget"),
            (DiataxisMode::Reference, "reference", "widget build"),
            (DiataxisMode::Explanation, "explanation", "raw material varies"),
        ];
        for (mode, tag, needle) in expect {
            let artifact = artifacts.iter().find(|a| a.mode == mode).unwrap();
            assert!(
                artifact.content.contains(&format!("diataxis: \"{tag}\"")),
                "missing diataxis: \"{tag}\" tag for {tag}"
            );
            assert!(artifact.content.contains(needle), "missing expected body content for {tag}");
            // Frontmatter still well-formed (existing fields untouched).
            assert!(artifact.content.starts_with("---\n"));
            assert!(artifact.content.contains("type: wiki"));
        }
    }

    #[test]
    fn render_diataxis_set_placeholders_absent_modes() {
        let content = "# Widget\n\n## Tutorial\n\nOnly a tutorial section exists this round.\n";
        let artifacts = render_diataxis_set(&ctx(content), None);
        let reference = artifacts.iter().find(|a| a.mode == DiataxisMode::Reference).unwrap();
        assert!(reference.content.contains("_No reference content yet for src/widget._"));
    }

    #[test]
    fn render_diataxis_set_deepens_per_mode_preserving_untouched_modes() {
        let first_content = "# Widget\n\n\
## Tutorial\n\nOriginal tutorial content.\n\n\
## Reference\n\nOriginal reference content.\n";
        let first = render_diataxis_set(&ctx(first_content), None);

        // Second round only updates Reference; Tutorial section is absent.
        let second_content = "# Widget\n\n## Reference\n\nUpdated reference content, v2.\n";
        let second = render_diataxis_set(&ctx(second_content), Some(&first));

        let tutorial = second.iter().find(|a| a.mode == DiataxisMode::Tutorial).unwrap();
        let reference = second.iter().find(|a| a.mode == DiataxisMode::Reference).unwrap();
        assert!(tutorial.content.contains("Original tutorial content."), "untouched mode must be preserved");
        assert!(reference.content.contains("Updated reference content, v2."), "updated mode must be replaced");
        assert!(!reference.content.contains("Original reference content"));
    }

    // ── Write-model inversion: pure functions, no placement ─────────────

    #[test]
    fn render_layered_readme_and_diataxis_set_never_touch_the_filesystem() {
        let tmp = std::env::temp_dir().join(format!("docgen-readme-layers-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let before: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(before.is_empty());

        let _ = render_layered_readme(&ctx(SAMPLE), None);
        let _ = render_diataxis_set(&ctx(SAMPLE), None);

        let after: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(after.is_empty(), "readme_layers must never write files -- it only returns artifacts");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ── Negative: unknown/garbage content never panics ───────────────────

    #[test]
    fn parse_layers_on_empty_content_returns_empty_layers_not_a_panic() {
        let layers = parse_layers("");
        assert_eq!(layers, ParsedLayers::default());
    }

    #[test]
    fn extract_section_is_case_insensitive_on_heading_text() {
        let content = "intro\n\n## quickSTART\n\nCase-insensitive body.\n";
        let section = extract_section(content, "Quickstart");
        assert_eq!(section.as_deref(), Some("Case-insensitive body."));
    }

    // ─────────────────────────────────────────────────────────────────────
    // DGRICH-05: fact_row / build_landing_body / check_landing_substance
    // ─────────────────────────────────────────────────────────────────────

    use super::super::prompts::{FeatureRow, GuideTopic, SubsystemBrief};
    use super::super::repo_facts::{
        BinTarget, ConfigSurface, EntryPoints, ProseAnchors, RepoScale, Subsystem, SubsystemEdge,
        SubsystemGraph, SymbolRef,
    };
    use std::collections::BTreeMap;

    fn sample_identity() -> RepoIdentity {
        RepoIdentity {
            tagline: "The Lumina Constellation's tool hub, model intake, and code intelligence engines.".to_string(),
            what_is: "Terminus is the tool plane of the Lumina Constellation fleet.\n\n\
It runs intake, forge, and mesh behind one gateway.".to_string(),
            audience: "Fleet operators and agents that need tool access.".to_string(),
            subsystems: vec![
                SubsystemBrief {
                    name: "intake".to_string(),
                    one_liner: "Model discovery and profiling.".to_string(),
                    role: "core".to_string(),
                },
                SubsystemBrief {
                    name: "forge".to_string(),
                    one_liner: "Gitea/GitHub mirror integration.".to_string(),
                    role: "integration".to_string(),
                },
            ],
            feature_rows: vec![
                FeatureRow {
                    feature: "Tool registry".to_string(),
                    description: "Dispatches MCP tool calls.".to_string(),
                    subsystem: "intake".to_string(),
                },
                FeatureRow {
                    feature: "PR replay".to_string(),
                    description: "Replays merged PRs to mirrors.".to_string(),
                    subsystem: "forge".to_string(),
                },
            ],
            guide_topics: vec![GuideTopic {
                title: "Run a fleet assessment".to_string(),
                grounding: "intake::assessment::run".to_string(),
            }],
        }
    }

    fn grounded_facts() -> RepoFacts {
        let mut by_kind = BTreeMap::new();
        by_kind.insert("module".to_string(), 410);
        by_kind.insert("function".to_string(), 10064);
        by_kind.insert("struct".to_string(), 1108);
        by_kind.insert("trait".to_string(), 161);

        RepoFacts {
            project_id: "TERM".to_string(),
            git_ref: "a1b2c3d4e5f6".to_string(),
            kg_grounded: true,
            scale: RepoScale {
                node_count: 11905,
                edge_count: 27107,
                by_kind,
                hotspots: vec![SymbolRef {
                    id: "crate::mesh::principal::PrincipalResolver::map".to_string(),
                    kind: "function",
                    path: "src/mesh/principal.rs".to_string(),
                    rank: 0.9,
                }],
            },
            subsystems: vec![
                Subsystem {
                    name: "intake".to_string(),
                    source_dir: "src/intake".to_string(),
                    node_count: 2059,
                    kind_breakdown: BTreeMap::new(),
                    top_symbols: vec![],
                    aggregate_rank: 1.0,
                    is_misc: false,
                },
                Subsystem {
                    name: "forge".to_string(),
                    source_dir: "src/forge".to_string(),
                    node_count: 836,
                    kind_breakdown: BTreeMap::new(),
                    top_symbols: vec![],
                    aggregate_rank: 1.0,
                    is_misc: false,
                },
            ],
            edge_matrix: SubsystemGraph {
                edges: vec![SubsystemEdge { from: "intake".to_string(), to: "forge".to_string(), weight: 12 }],
            },
            entry_points: EntryPoints {
                bin_targets: vec![BinTarget {
                    name: "terminus_primary".to_string(),
                    path: "src/bin/terminus_primary.rs".to_string(),
                }],
                workspace_members: vec!["crates/lumina-core".to_string()],
                entrypoint_symbols: vec!["registry::register_all".to_string()],
                registered_tool_count: Some(53),
            },
            config_surface: ConfigSurface::default(),
            prose_anchors: ProseAnchors {
                crate_description: Some("MCP tool hub".to_string()),
                crate_root_docs: vec![],
                subsystem_docs: BTreeMap::new(),
            },
            old_readme_sections: vec![],
        }
    }

    fn ungrounded_facts() -> RepoFacts {
        RepoFacts {
            project_id: "GHOST".to_string(),
            git_ref: "deadbeefcafe".to_string(),
            kg_grounded: false,
            entry_points: EntryPoints { registered_tool_count: Some(5), ..Default::default() },
            ..Default::default()
        }
    }

    fn sample_docs_tree() -> Vec<DocsTreeFile> {
        vec![
            DocsTreeFile {
                path: "docs/reference/intake.md".to_string(),
                content: "# intake\n\n[\u{2190} Docs Home](../index.md)\n\n---\n\n\
Model discovery and profiling engine for the fleet.\n\nMore detail follows.\n".to_string(),
            },
            DocsTreeFile {
                path: "docs/reference/forge.md".to_string(),
                content: "# forge\n\nGitea/GitHub mirror integration layer.\n".to_string(),
            },
            DocsTreeFile {
                path: "docs/getting-started.md".to_string(),
                content: "# Getting Started\n\nClone the repo and build it.\n".to_string(),
            },
            DocsTreeFile {
                path: "_Sidebar.md".to_string(),
                content: "# TERM\n\n- [Docs Home](docs/index.md)\n".to_string(),
            },
        ]
    }

    // ── fact_row ──────────────────────────────────────────────────────────

    #[test]
    fn fact_row_computes_counts_and_omits_kg_derived_numbers_when_ungrounded() {
        let grounded = fact_row(&grounded_facts());
        assert!(grounded.contains("Rust"), "{grounded}");
        assert!(grounded.contains("410 modules"), "{grounded}");
        assert!(grounded.contains("53 MCP tools"), "{grounded}");
        assert!(grounded.contains("11.9k KG nodes"), "{grounded}");
        assert!(grounded.contains("analyzed a1b2c3d"), "{grounded}");

        let ungrounded = fact_row(&ungrounded_facts());
        assert!(!ungrounded.contains("modules"), "KG-derived module count must be omitted: {ungrounded}");
        assert!(!ungrounded.contains("KG nodes"), "KG node count must be omitted: {ungrounded}");
        assert!(
            ungrounded.contains("5 MCP tools"),
            "checkout-derived tool count must survive ungrounded degradation: {ungrounded}"
        );
        assert!(ungrounded.contains("analyzed deadbee"));
    }

    // ── build_landing_body ───────────────────────────────────────────────

    #[test]
    fn build_landing_body_emits_all_eight_sections_and_links_feature_rows_to_real_pages() {
        let identity = sample_identity();
        let facts = grounded_facts();
        let tree = sample_docs_tree();
        let body = build_landing_body(&identity, &facts, &tree);

        for heading in [
            "## What is",
            "## Architecture",
            "## Subsystems and Features",
            "## Quick Start",
            "## Documentation",
            "## At a Glance",
            "## Contributing",
            "## License",
        ] {
            assert!(body.contains(heading), "missing section {heading}: {body}");
        }
        assert!(body.contains("<h1"), "centered hero must be present: {body}");
        assert!(body.contains(&identity.tagline), "hero must carry the real tagline: {body}");

        // Every feature row links to a subsystem reference page that was
        // actually emitted in the docs tree.
        assert!(body.contains("docs/reference/intake.md"));
        assert!(body.contains("docs/reference/forge.md"));

        // The documentation index's rows match the emitted tree's REAL
        // first-paragraph content, not a fixed description.
        assert!(body.contains("Model discovery and profiling engine for the fleet."));
        assert!(body.contains("Clone the repo and build it."));
        // The sidebar file is nav chrome, not a documentation page itself.
        assert!(!body.contains("| [TERM]"));
    }

    #[test]
    fn build_landing_body_omits_kg_counts_and_still_renders_when_ungrounded() {
        let identity = sample_identity();
        let facts = ungrounded_facts();
        let body = build_landing_body(&identity, &facts, &[]);
        assert!(body.contains("Not yet KG-grounded"), "{body}");
        assert!(body.contains("_No documentation pages have been generated yet._"), "{body}");
        // Never silently fabricate a diagram either -- falls back to the
        // generic default, which downstream lints (is_generic_placeholder)
        // are responsible for flagging.
        assert!(body.contains("```mermaid"));
    }

    // ── check_landing_substance ──────────────────────────────────────────

    #[test]
    fn check_landing_substance_fails_a_landing_below_the_floor() {
        let sparse = "<h1 align=\"center\">X</h1>\n\n<p align=\"center\">tag</p>\n\n---\n\nOne real line.\n";
        let err = check_landing_substance(sparse).unwrap_err();
        assert!(err.contains("substance floor"), "{err}");
    }

    #[test]
    fn check_landing_substance_passes_a_landing_around_the_target_length() {
        let reasonable = "Real content line describing the repository.\n".repeat(180);
        assert!(check_landing_substance(&reasonable).is_ok());
        assert!(check_landing_length(&reasonable).is_ok());
    }

    #[test]
    fn check_landing_length_and_substance_are_independent_gates() {
        let oversized = "Real content line.\n".repeat(LANDING_MAX_LINES + 1);
        assert!(check_landing_length(&oversized).is_err());
        // An oversized-but-substantive landing still clears the FLOOR --
        // the ceiling and floor are two independent checks, both required.
        assert!(check_landing_substance(&oversized).is_ok());
    }

    #[test]
    fn hero_html_and_horizontal_rules_never_count_towards_substance() {
        let mut content = String::new();
        content.push_str("<h1 align=\"center\">X</h1>\n\n");
        content.push_str("<p align=\"center\">tag</p>\n\n");
        content.push_str("---\n\n");
        for _ in 0..5 {
            content.push_str("Real line.\n");
        }
        assert_eq!(substantive_line_count(&content), 5);
    }
}
