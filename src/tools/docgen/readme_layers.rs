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

use crate::scribe::vault::{render_note, NoteFrontmatter, NoteType};

use super::config::DocTargetType;
use super::diagram::default_architecture_mermaid_source;
use super::render::{RenderContext, RenderedArtifact};

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
        quickstart: extract_section_any(content, &["Quickstart", "Usage"]).unwrap_or_default(),
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

/// shields.io badge row. Generic (build/version/license/docs) rather than
/// project-specific CI wiring -- this module has no access to a project's
/// actual CI status, so it renders informational badges naming the project,
/// not a live-status integration (that would be a separate, later item).
fn shields_badges(project: &str) -> String {
    let encoded = project.replace(' ', "%20");
    format!(
        "![build](https://img.shields.io/badge/build-passing-brightgreen) \
![version](https://img.shields.io/badge/version-auto-blue) \
![license](https://img.shields.io/badge/license-MIT-lightgrey) \
![docs](https://img.shields.io/badge/docs-{encoded}-informational)"
    )
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

/// The landing README's target maximum length, in lines (spec §D1: "~130-
/// 180 lines"). A concrete, testable ceiling rather than "keep it concise"
/// left unchecked.
pub const LANDING_MAX_LINES: usize = 180;

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
ceiling (spec §D1) -- move deep content into the docs/ tree instead of inlining it"
        ))
    } else {
        Ok(())
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

/// Turn the hero/background prose into 4-7 short benefit bullets for
/// `## Why {project}` (spec §D1: "BENEFIT bullets, not capabilities"). A
/// light heuristic (sentence splitting), not a re-generation -- this module
/// has no model access (see the file doc comment's "Templating choice"), so
/// a generator that already writes benefit-shaped prose renders well here,
/// and a bare/near-empty hero gets an explicit placeholder rather than a
/// fabricated bullet, matching this file's existing "no content yet"
/// convention.
fn why_bullets(background: &str, module: &str) -> String {
    let sentences: Vec<String> = background
        .split(|c| c == '.' || c == '\n')
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() > 3 && !s.starts_with('#'))
        .take(7)
        .map(|s| format!("- {}.", s.trim_end_matches('.')))
        .collect();
    if sentences.len() < 2 {
        format!(
            "- _No benefit summary generated yet for {module} -- see \
[Architecture at a glance](#architecture-at-a-glance) below._"
        )
    } else {
        sentences.join("\n")
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

/// The `## Architecture at a glance` body: a short (3-4 sentence) blurb
/// plus a link to the full `docs/architecture.md` page -- never the full
/// diagram breakdown itself (that lives only in the docs/ tree and the
/// hero's diagram slot).
fn architecture_glance(background: &str, module: &str) -> String {
    let blurb = background
        .split('.')
        .map(str::trim)
        .find(|s| !s.is_empty() && !s.starts_with('#'))
        .map(|s| format!("{s}."))
        .unwrap_or_else(|| format!("{module} is documented in depth on the architecture page."));
    format!(
        "{blurb} See [Architecture]({DOCS_ARCHITECTURE_PATH}) for the full component and \
data-flow breakdown."
    )
}

/// Build the CONCISE landing README body (everything `render_note` wraps in
/// frontmatter), per the revision spec's §D1 fixed section order: hero
/// (centered name + tagline + badges + nav-link row) -> `---` ->
/// architecture mermaid -> `## Why {project}` -> `## Quick Start` ->
/// `## Documentation` (nav table) -> `## Architecture at a glance` ->
/// `## Contributing` -> `## License`. This STOPS concatenating every
/// layer/mode into one file (the pre-revision behavior) -- the deep-dive
/// layer and the Diátaxis tutorial/how-to/reference/explanation bodies are
/// NEVER inlined here; they live in `render::docs_tree`'s docs/ pages,
/// which this landing only links to. See [`check_landing_length`] for the
/// paired ≤180-line lint.
fn build_layered_body(ctx: &RenderContext<'_>, layers: &ParsedLayers) -> String {
    let (mut one_liner, background) = split_hero(&layers.hero);
    if one_liner.is_empty() {
        one_liner = format!("{} -- documentation generated by the docgen engine.", ctx.module);
    }
    let badges = shields_badges(ctx.project);

    let mut out = String::new();
    // Hero: centered name + tagline + badges + nav-link row.
    out.push_str(&format!("<h1 align=\"center\">{}</h1>\n\n", ctx.module));
    out.push_str(&format!("<p align=\"center\"><em>{one_liner}</em></p>\n\n"));
    out.push_str(&format!("<p align=\"center\">\n\n{badges}\n\n</p>\n\n"));
    out.push_str(&format!("<p align=\"center\">{}</p>\n\n", nav_link_row()));
    out.push_str("---\n\n");
    // Architecture mermaid (DOCGEN-22's slot -- reused, never reinvented).
    out.push_str(&architecture_slot(ctx.module));
    out.push_str("\n\n");
    out.push_str(&format!("## Why {}\n\n", ctx.project));
    out.push_str(&why_bullets(&background, ctx.module));
    out.push_str("\n\n## Quick Start\n\n");
    out.push_str(&quick_start_section(&layers.quickstart));
    out.push_str("\n\n## Documentation\n\n");
    out.push_str(&documentation_nav_table());
    out.push_str("\n## Architecture at a glance\n\n");
    out.push_str(&architecture_glance(&background, ctx.module));
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

        let hero = content.find("Widget").expect("module name in hero");
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

    // ── shields.io badges present ────────────────────────────────────────

    #[test]
    fn layered_readme_includes_shields_io_badges() {
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        let content = artifact.content.unwrap();
        assert!(content.contains("https://img.shields.io/badge/"));
        assert!(content.contains("build-passing"));
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
}
