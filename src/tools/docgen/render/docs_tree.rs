//! DOCGEN-21 (S95 REVISION, Plane TERM-334): the `docs/` tree -- one
//! Diátaxis mode per file, wiki-style multi-page documentation output that
//! `readme_layers::render_layered_readme`'s concise landing links out to
//! instead of inlining.
//!
//! ## Shape (§D2 of the revision spec)
//! `docs/index.md` (nav hub / map) -> `docs/getting-started.md` (TUTORIAL)
//! -> `docs/guides/index.md` + `docs/guides/overview.md` (HOW-TO) ->
//! `docs/reference/index.md` + `docs/reference/{cli,api,configuration}.md`
//! (REFERENCE) -> `docs/architecture.md` (EXPLANATION, full diagram +
//! component/data-flow) -> `_Sidebar.md` (Gitea/GitHub wiki nav). Every
//! generated page carries a breadcrumb back to `docs/index.md` plus
//! cross-links to the sibling category home pages (Diátaxis
//! cross-linking), per the revision spec.
//!
//! ## `guides/<task>.md`: one task page shipped, the shape ready for more
//! The revision spec names `guides/<task>.md` (plural, task-specific
//! pages). [`super::super::readme_layers::render_diataxis_set`] produces
//! ONE How-To body per generation round (not yet split per task), so this
//! item ships exactly one task page (`guides/overview.md`) fed from that
//! body, plus the `guides/index.md` hub that would list further task pages
//! the moment a future item splits How-To content per task -- the nav hub
//! and cross-link shape here never assumes a fixed page count. This
//! mirrors this crate's existing precedent of shipping the tested shape and
//! documenting the deferred finer split rather than half-building it (see
//! `wiki_graph`'s "Starlight site target (deferred)" note).
//!
//! ## Reference sub-split: `api.md` gets the body, `cli.md`/`configuration.md` are stubs
//! `render_diataxis_set` produces ONE Reference body, not three. This item
//! routes it into `reference/api.md` (the closest existing match) and
//! renders `reference/cli.md` / `reference/configuration.md` as explicit
//! "no content yet" stub pages that still carry breadcrumbs/cross-links and
//! point back at `api.md` -- matching this crate's established convention
//! (`readme_layers::build_layered_body`'s `_No quickstart content yet._`)
//! of an explicit placeholder over a silently missing page. A future item
//! that teaches generation to structure Reference content per sub-area can
//! populate these without changing this module's file shape.
//!
//! ## Source: `render_diataxis_set`'s artifacts, never regenerated here
//! This module performs NO generation and NO PII scanning of its own
//! (content arrives here already deepened and swept upstream, matching
//! every other renderer in `render/`). It only consumes the
//! [`super::super::readme_layers::DiataxisArtifact`]s a caller already built
//! via [`super::super::readme_layers::render_diataxis_set`] and routes each
//! mode's body into its file(s) -- reuse, not reimplementation, per the
//! revision spec's explicit instruction.
//!
//! ## One architecture source, reused (not reinvented)
//! [`build_docs_tree`] embeds the exact SAME architecture mermaid source
//! `readme_layers::architecture_slot` puts in the README hero (via
//! `diagram::default_architecture_mermaid_source` +
//! `diagram::mermaid_fence`) into `docs/architecture.md` and the
//! `docs/index.md` hub -- one source of truth for the diagram, per the
//! revision spec's "Reuse ONE `architecture.mmd` in both README hero and
//! `docs/architecture.md`."
//!
//! ## WRITE-MODEL INVERSION (unchanged from the rest of `render/`)
//! Every function in this module is pure: it takes a [`RenderContext`] plus
//! a `&[DiataxisArtifact]` slice and RETURNS `Vec<DocsTreeFile>`. Nothing
//! here writes to a repo, the filesystem, or the knowledge vault, and
//! nothing performs any git/network/subprocess I/O -- placement is entirely
//! the calling harness's decision, exactly like every other renderer here
//! (see `docs_tree_never_touches_the_filesystem` below).

use super::super::diagram::{default_architecture_mermaid_source, mermaid_fence};
use super::super::readme_layers::{strip_frontmatter, DiataxisArtifact, DiataxisMode};
use super::RenderContext;

// ─── Repo-relative paths (must match readme_layers's DOCS_* constants) ──────

const DOCS_INDEX: &str = "docs/index.md";
const GETTING_STARTED: &str = "docs/getting-started.md";
const GUIDES_INDEX: &str = "docs/guides/index.md";
const GUIDES_OVERVIEW: &str = "docs/guides/overview.md";
const REFERENCE_INDEX: &str = "docs/reference/index.md";
const REFERENCE_CLI: &str = "docs/reference/cli.md";
const REFERENCE_API: &str = "docs/reference/api.md";
const REFERENCE_CONFIGURATION: &str = "docs/reference/configuration.md";
const ARCHITECTURE: &str = "docs/architecture.md";
const SIDEBAR: &str = "_Sidebar.md";

const STATIC_DIAGRAM_FALLBACK: &str = "```mermaid\nflowchart LR\n    A[Client] --> B[Core]\n```";

/// One file in the generated `docs/` tree: a repo-relative path (rooted at
/// `docs/`, or the bare `_Sidebar.md`) and its full Markdown content, with
/// breadcrumb and cross-links already spliced in. Deliberately plain data
/// -- see the module doc comment's WRITE-MODEL INVERSION note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocsTreeFile {
    pub path: String,
    pub content: String,
}

/// Breadcrumb back to the docs hub, relative to a page `depth` directory
/// levels below `docs/` (0 for `docs/*.md` itself, 1 for
/// `docs/guides/*.md` / `docs/reference/*.md`).
fn breadcrumb(depth: usize) -> String {
    let up = "../".repeat(depth);
    format!("[\u{2190} Docs Home]({up}index.md)")
}

/// The four category home pages every generated page cross-links to
/// (Diátaxis cross-linking), relative to a page `depth` directory levels
/// below `docs/`.
fn cross_links(depth: usize) -> String {
    let up = "../".repeat(depth);
    format!(
        "**See also:** [Getting Started]({up}getting-started.md) \u{b7} \
[Guides]({up}guides/index.md) \u{b7} [Reference]({up}reference/index.md) \u{b7} \
[Architecture]({up}architecture.md)"
    )
}

/// A standard generated page: title, breadcrumb, the mode's body, then
/// cross-links -- the shape every leaf page in the tree shares.
fn page(title: &str, depth: usize, body: &str) -> String {
    format!(
        "# {title}\n\n{}\n\n---\n\n{body}\n\n---\n\n{}\n",
        breadcrumb(depth),
        cross_links(depth)
    )
}

/// Recover one Diátaxis mode's plain body out of `diataxis` (stripping the
/// `render_note` frontmatter [`super::super::readme_layers::render_diataxis_set`]
/// wrapped it in), or an explicit "no content yet" placeholder when that
/// mode is absent or empty -- never a silently missing page.
fn mode_body(diataxis: &[DiataxisArtifact], mode: DiataxisMode) -> String {
    diataxis
        .iter()
        .find(|a| a.mode == mode)
        .map(|a| strip_frontmatter(&a.content).trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| format!("_No {} content yet._", mode.as_str()))
}

/// The one architecture mermaid source, reused verbatim from
/// `readme_layers::architecture_slot`'s own template + fallback (never a
/// second diagram source) -- see the module doc comment's "One architecture
/// source, reused" note.
fn architecture_diagram(module: &str) -> String {
    default_architecture_mermaid_source(module)
        .ok()
        .and_then(|source| mermaid_fence(&source).ok())
        .unwrap_or_else(|| STATIC_DIAGRAM_FALLBACK.to_string())
}

/// Build the full `docs/` tree (§D2) from `ctx` and the four
/// [`DiataxisArtifact`]s a caller already produced via
/// [`super::super::readme_layers::render_diataxis_set`]. Deterministic --
/// one call produces the whole file set, in the stable order listed in the
/// module doc comment.
pub fn build_docs_tree(ctx: &RenderContext<'_>, diataxis: &[DiataxisArtifact]) -> Vec<DocsTreeFile> {
    let module = ctx.module;
    let tutorial = mode_body(diataxis, DiataxisMode::Tutorial);
    let howto = mode_body(diataxis, DiataxisMode::HowTo);
    let reference = mode_body(diataxis, DiataxisMode::Reference);
    let explanation = mode_body(diataxis, DiataxisMode::Explanation);
    let diagram = architecture_diagram(module);

    vec![
        DocsTreeFile { path: DOCS_INDEX.to_string(), content: index_page(module, &diagram) },
        DocsTreeFile {
            path: GETTING_STARTED.to_string(),
            content: page(&format!("Getting Started \u{2014} {module}"), 0, &tutorial),
        },
        DocsTreeFile { path: GUIDES_INDEX.to_string(), content: guides_index_page(module) },
        DocsTreeFile {
            path: GUIDES_OVERVIEW.to_string(),
            content: page(&format!("Guides \u{2014} {module}"), 1, &howto),
        },
        DocsTreeFile { path: REFERENCE_INDEX.to_string(), content: reference_index_page(module) },
        DocsTreeFile {
            path: REFERENCE_API.to_string(),
            content: page(&format!("API Reference \u{2014} {module}"), 1, &reference),
        },
        DocsTreeFile {
            path: REFERENCE_CLI.to_string(),
            content: page(
                &format!("CLI Reference \u{2014} {module}"),
                1,
                "_No CLI reference content yet -- see [API Reference](api.md) for the generated \
reference body._",
            ),
        },
        DocsTreeFile {
            path: REFERENCE_CONFIGURATION.to_string(),
            content: page(
                &format!("Configuration Reference \u{2014} {module}"),
                1,
                "_No configuration reference content yet -- see [API Reference](api.md) for the \
generated reference body._",
            ),
        },
        DocsTreeFile {
            path: ARCHITECTURE.to_string(),
            content: architecture_page(module, &diagram, &explanation),
        },
        DocsTreeFile { path: SIDEBAR.to_string(), content: sidebar_page(module) },
    ]
}

fn index_page(module: &str, diagram: &str) -> String {
    format!(
        "# {module} Documentation\n\n\
Welcome to the {module} documentation hub. This is the map -- every page below links back \
here, and to its sibling sections, so you're never more than one click from anywhere else \
in the docs.\n\n\
{diagram}\n\n\
## Sections\n\n\
- [Getting Started](getting-started.md) \u{2014} a first working setup, tutorial-style.\n\
- [Guides](guides/index.md) \u{2014} task-oriented how-tos.\n\
- [Reference](reference/index.md) \u{2014} CLI, API, and configuration reference.\n\
- [Architecture](architecture.md) \u{2014} how the system fits together, in depth.\n\n\
[Back to the project README](../README.md)\n"
    )
}

fn guides_index_page(module: &str) -> String {
    format!(
        "# Guides \u{2014} {module}\n\n{}\n\n---\n\n## Available guides\n\n- [Overview](overview.md)\n\n{}\n",
        breadcrumb(1),
        cross_links(1)
    )
}

fn reference_index_page(module: &str) -> String {
    format!(
        "# Reference \u{2014} {module}\n\n{}\n\n---\n\n\
## Reference pages\n\n\
- [CLI](cli.md)\n\
- [API](api.md)\n\
- [Configuration](configuration.md)\n\n\
{}\n",
        breadcrumb(1),
        cross_links(1)
    )
}

fn architecture_page(module: &str, diagram: &str, explanation: &str) -> String {
    format!(
        "# Architecture \u{2014} {module}\n\n{}\n\n---\n\n{diagram}\n\n{explanation}\n\n---\n\n{}\n",
        breadcrumb(0),
        cross_links(0)
    )
}

fn sidebar_page(module: &str) -> String {
    format!(
        "# {module}\n\n\
- [Docs Home](docs/index.md)\n\
- [Getting Started](docs/getting-started.md)\n\
- [Guides](docs/guides/index.md)\n\
- [Reference](docs/reference/index.md)\n\
  - [CLI](docs/reference/cli.md)\n\
  - [API](docs/reference/api.md)\n\
  - [Configuration](docs/reference/configuration.md)\n\
- [Architecture](docs/architecture.md)\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::docgen::readme_layers::render_diataxis_set;

    fn ctx<'a>(content: &'a str) -> RenderContext<'a> {
        RenderContext {
            project: "widget-factory",
            module: "src/widget",
            source_commit: "abc123",
            generated_at: "2026-07-11T00:00:00Z",
            content,
        }
    }

    const SAMPLE_DIATAXIS: &str = "# Widget\n\n\
## Tutorial\n\nFollow along to build your first widget from scratch.\n\n\
## How-To\n\nTo reconfigure the widget, edit its config file.\n\n\
## Reference\n\n`widget build [--flag]` -- builds a widget.\n\n\
## Explanation\n\nThe widget pipeline exists because raw material varies.\n";

    fn sample_artifacts() -> Vec<DiataxisArtifact> {
        render_diataxis_set(&ctx(SAMPLE_DIATAXIS), None)
    }

    // ── Shape: exactly the files the module doc comment promises ────────

    #[test]
    fn build_docs_tree_produces_the_full_expected_file_set() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                DOCS_INDEX,
                GETTING_STARTED,
                GUIDES_INDEX,
                GUIDES_OVERVIEW,
                REFERENCE_INDEX,
                REFERENCE_API,
                REFERENCE_CLI,
                REFERENCE_CONFIGURATION,
                ARCHITECTURE,
                SIDEBAR,
            ]
        );
    }

    // ── Mode routing: each Diátaxis body lands on its own file, isolated ─

    #[test]
    fn tutorial_body_lands_only_on_getting_started() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        let getting_started = find(&files, GETTING_STARTED);
        assert!(getting_started.content.contains("Follow along to build your first widget"));

        // Isolation: the tutorial body must not leak onto sibling pages.
        for other in [GUIDES_OVERVIEW, REFERENCE_API, ARCHITECTURE] {
            assert!(
                !find(&files, other).content.contains("Follow along to build your first widget"),
                "{other} must not contain the tutorial body"
            );
        }
    }

    #[test]
    fn howto_body_lands_on_guides_overview() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        assert!(find(&files, GUIDES_OVERVIEW).content.contains("reconfigure the widget"));
    }

    #[test]
    fn reference_body_lands_on_reference_api_not_cli_or_configuration() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        assert!(find(&files, REFERENCE_API).content.contains("widget build [--flag]"));
        assert!(!find(&files, REFERENCE_CLI).content.contains("widget build [--flag]"));
        assert!(!find(&files, REFERENCE_CONFIGURATION).content.contains("widget build [--flag]"));
        // The stubs are explicit placeholders, not silently blank pages.
        assert!(find(&files, REFERENCE_CLI).content.contains("No CLI reference content yet"));
        assert!(
            find(&files, REFERENCE_CONFIGURATION).content.contains("No configuration reference content yet")
        );
    }

    #[test]
    fn explanation_body_lands_on_architecture_page() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        assert!(find(&files, ARCHITECTURE).content.contains("raw material varies"));
    }

    // ── Absent mode -> explicit placeholder, not a missing/blank page ───

    #[test]
    fn missing_diataxis_mode_renders_an_explicit_placeholder_not_a_blank_page() {
        let bare = "# Widget\n\n## Tutorial\n\nOnly a tutorial section this round.\n";
        let artifacts = render_diataxis_set(&ctx(bare), None);
        let files = build_docs_tree(&ctx(bare), &artifacts);
        assert!(find(&files, GUIDES_OVERVIEW).content.contains("_No how-to content yet._"));
        assert!(find(&files, REFERENCE_API).content.contains("_No reference content yet._"));
        assert!(find(&files, ARCHITECTURE).content.contains("_No explanation content yet._"));
    }

    // ── Breadcrumbs + cross-links present, correct relative depth ───────

    #[test]
    fn top_level_pages_breadcrumb_to_index_md_directly() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        for path in [GETTING_STARTED, ARCHITECTURE] {
            let content = &find(&files, path).content;
            assert!(content.contains("(index.md)"), "{path} breadcrumb must be depth-0: {content}");
        }
    }

    #[test]
    fn nested_pages_breadcrumb_up_one_level_to_docs_index() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        for path in [GUIDES_INDEX, GUIDES_OVERVIEW, REFERENCE_INDEX, REFERENCE_API, REFERENCE_CLI] {
            let content = &find(&files, path).content;
            assert!(content.contains("(../index.md)"), "{path} breadcrumb must be depth-1: {content}");
        }
    }

    #[test]
    fn every_generated_page_carries_cross_links_to_sibling_categories() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        for path in [GETTING_STARTED, GUIDES_OVERVIEW, REFERENCE_API, ARCHITECTURE] {
            let content = &find(&files, path).content;
            assert!(content.contains("See also"), "{path} missing cross-links: {content}");
            assert!(content.contains("Getting Started"));
            assert!(content.contains("Guides"));
            assert!(content.contains("Reference"));
            assert!(content.contains("Architecture"));
        }
    }

    // ── index.md hub + _Sidebar.md link every section ────────────────────

    #[test]
    fn index_page_links_to_every_section_and_carries_the_architecture_diagram() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        let content = &find(&files, DOCS_INDEX).content;
        assert!(content.contains("```mermaid"), "index.md must carry the architecture diagram: {content}");
        assert!(content.contains("getting-started.md"));
        assert!(content.contains("guides/index.md"));
        assert!(content.contains("reference/index.md"));
        assert!(content.contains("architecture.md"));
    }

    #[test]
    fn sidebar_lists_every_page_in_the_tree() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        let content = &find(&files, SIDEBAR).content;
        for path in [
            DOCS_INDEX,
            GETTING_STARTED,
            GUIDES_INDEX,
            REFERENCE_INDEX,
            REFERENCE_CLI,
            REFERENCE_API,
            REFERENCE_CONFIGURATION,
            ARCHITECTURE,
        ] {
            assert!(content.contains(path), "_Sidebar.md missing a link to {path}: {content}");
        }
    }

    // ── One architecture source, reused across index.md + architecture.md ─

    #[test]
    fn index_and_architecture_pages_embed_the_same_diagram_source() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());
        let index_content = &find(&files, DOCS_INDEX).content;
        let arch_content = &find(&files, ARCHITECTURE).content;
        let diagram = architecture_diagram("src/widget");
        assert!(index_content.contains(&diagram));
        assert!(arch_content.contains(&diagram));
    }

    // ── WRITE-MODEL INVERSION: pure, never touches the filesystem ───────

    #[test]
    fn docs_tree_never_touches_the_filesystem() {
        let tmp = std::env::temp_dir().join(format!("docgen-docs-tree-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let before: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(before.is_empty());

        let _ = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &sample_artifacts());

        let after: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(after.is_empty(), "build_docs_tree must never write files -- it only returns artifacts");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ── Negative: empty diataxis slice never panics ──────────────────────

    #[test]
    fn empty_diataxis_slice_still_produces_the_full_file_set_with_placeholders() {
        let files = build_docs_tree(&ctx(SAMPLE_DIATAXIS), &[]);
        assert_eq!(files.len(), 10);
        assert!(find(&files, GETTING_STARTED).content.contains("_No tutorial content yet._"));
    }

    fn find<'a>(files: &'a [DocsTreeFile], path: &str) -> &'a DocsTreeFile {
        files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("no file at {path}"))
    }
}
