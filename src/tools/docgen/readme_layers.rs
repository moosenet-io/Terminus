//! DOCGEN-13: multi-layered Diátaxis README + template set (progressive
//! disclosure), S95, Plane TERM-164.
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
fn architecture_slot(module: &str) -> String {
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
fn strip_frontmatter(rendered: &str) -> String {
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
/// line) and the full hero text (used as the Background section body).
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

/// Build the layered README body (everything `render_note` wraps in
/// frontmatter): badges -> one-liner -> architecture mermaid diagram ->
/// table of contents -> standard-readme-ordered sections (Background /
/// Install / Usage / API / Contributing / License), with Usage sourced from
/// the quickstart layer and API sourced from the deep-dive layer -- the
/// progressive-disclosure mapping this item exists to add.
fn build_layered_body(ctx: &RenderContext<'_>, layers: &ParsedLayers) -> String {
    let (mut one_liner, background) = split_hero(&layers.hero);
    if one_liner.is_empty() {
        one_liner = format!("{} -- documentation generated by the docgen engine.", ctx.module);
    }
    let badges = shields_badges(ctx.project);

    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", ctx.module));
    out.push_str(&badges);
    out.push_str("\n\n> ");
    out.push_str(&one_liner);
    out.push_str("\n\n");
    out.push_str(&architecture_slot(ctx.module));
    out.push_str(
        "\n\n## Table of Contents\n\n\
- [Background](#background)\n\
- [Install](#install)\n\
- [Usage](#usage)\n\
- [API](#api)\n\
- [Contributing](#contributing)\n\
- [License](#license)\n\n",
    );
    out.push_str("## Background\n\n");
    out.push_str(if background.is_empty() { "_No background content yet._" } else { &background });
    out.push_str("\n\n## Install\n\nSee Quickstart below for the fastest path to a working setup.\n\n");
    out.push_str("## Usage\n\n");
    out.push_str(if layers.quickstart.trim().is_empty() {
        "_No quickstart content yet._"
    } else {
        layers.quickstart.trim()
    });
    out.push_str("\n\n## API\n\n");
    out.push_str(if layers.deep_dive.trim().is_empty() {
        "_No deep-dive content yet._"
    } else {
        layers.deep_dive.trim()
    });
    out.push_str(
        "\n\n## Contributing\n\nSee the project's build pipeline docs for the contribution process.\n\n\
## License\n\nSee LICENSE.\n",
    );
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

    #[test]
    fn render_layered_readme_produces_all_three_layers_in_standard_readme_order() {
        let artifact = render_layered_readme(&ctx(SAMPLE), None);
        assert!(artifact.was_rendered());
        let content = artifact.content.unwrap();

        assert!(content.contains("The widget turns raw material"), "hero/background missing");
        assert!(content.contains("widget build"), "quickstart missing");
        assert!(content.contains("three stages"), "deep-dive missing");

        // Standard-readme section order: Background before Usage before API.
        let bg = content.find("## Background").expect("Background section");
        let usage = content.find("## Usage").expect("Usage section");
        let api = content.find("## API").expect("API section");
        assert!(bg < usage, "Background must precede Usage");
        assert!(usage < api, "Usage must precede API");
    }

    #[test]
    fn missing_layers_render_explicit_placeholders_not_silently_dropped() {
        let artifact = render_layered_readme(&ctx("# Bare\n\nJust a title, nothing else."), None);
        let content = artifact.content.unwrap();
        assert!(content.contains("_No quickstart content yet._"));
        assert!(content.contains("_No deep-dive content yet._"));
        // Sections still present even when empty -- order/structure never
        // silently collapses because a layer has no content yet.
        assert!(content.contains("## Usage"));
        assert!(content.contains("## API"));
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

        // Second round: only the deep-dive changed; quickstart section is
        // absent from this round's generated content entirely.
        let second_round_content =
            "# Widget\n\nThe widget turns raw material into widgets.\n\n\
## Deep Dive\n\nNow with a fourth stage: package.\n";
        let second = render_layered_readme(&ctx(second_round_content), Some(&first_content));
        let second_content = second.content.unwrap();

        assert!(second_content.contains("widget build"), "prior quickstart must be preserved");
        assert!(second_content.contains("fourth stage: package"), "new deep-dive must land");
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
