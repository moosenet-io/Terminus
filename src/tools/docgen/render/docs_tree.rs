//! DOCGEN-21 (S95 REVISION, Plane TERM-334): the `docs/` tree -- one
//! Diátaxis mode per file, wiki-style multi-page documentation output that
//! `readme_layers::render_layered_readme`'s concise landing links out to
//! instead of inlining.
//!
//! ## DGRICH-06 addition: [`build_repo_docs_tree`] (rich, KG-derived tree)
//! Everything above this note describes [`build_docs_tree`], the legacy
//! per-module renderer fed by [`super::super::readme_layers::render_diataxis_set`]'s
//! four fixed `DiataxisArtifact`s -- it is UNCHANGED by DGRICH-06 and keeps
//! its only production caller (`render::render_all`). [`build_repo_docs_tree`]
//! is a new, separate function for the rich repo-level pipeline (design
//! `fable-docgen-redesign.md` §1.2): it takes a
//! [`super::super::generate::RepoDocsOutcome`] (DGRICH-03's Pass 1-3
//! output: identity + N per-subsystem pages + guides) and the
//! [`super::super::repo_facts::RepoFacts`] (DGRICH-01) it was generated
//! from, and emits `reference/<subsystem>.md` per generated page, populated
//! `configuration.md`/`cli.md` (from the facts surface, never stubs), and a
//! `legacy/<slug>.md` passthrough for the DGRICH-08 no-loss backstop --
//! reusing this module's existing breadcrumb/cross-link/`_Sidebar.md`
//! helpers rather than duplicating them. See that function's doc comment
//! for the full shape and the degradation rules.
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

use super::super::diagram::{
    default_architecture_mermaid_source, full_subsystem_architecture_mermaid_source, mermaid_fence,
};
use super::super::generate::RepoDocsOutcome;
use super::super::prompts::RepoIdentity;
use super::super::readme_layers::{strip_frontmatter, DiataxisArtifact, DiataxisMode};
use super::super::repo_facts::{RepoFacts, Subsystem};
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

// ─────────────────────────────────────────────────────────────────────────
// DGRICH-06: per-subsystem docs tree render (S119, `TERM` DGRICH,
// `fable-docgen-redesign.md` §1.2).
//
// `build_repo_docs_tree` is a NEW function, not a signature change to
// `build_docs_tree` above: the legacy per-module path (`build_docs_tree`,
// fed by `render_diataxis_set`'s four `DiataxisArtifact`s) is untouched and
// keeps its only caller (`render::render_all`, DOCGEN-06/DOCGEN-21). The
// rich, repo-level path this item adds consumes a completely different
// shape of input -- a [`RepoDocsOutcome`] (DGRICH-03: identity + N
// per-subsystem pages + guides, assembled over up to ~19 Chord calls) plus
// the [`RepoFacts`] grounding layer (DGRICH-01) it was generated from --
// so a second, explicitly-named function is a clearer fit than overloading
// `build_docs_tree`'s parameters or return shape. DGRICH-07 (the trigger's
// repo-level mode, not yet built) is expected to call this one; the legacy
// function keeps serving projects without a KG/checkout.
//
// Same WRITE-MODEL INVERSION as the rest of this module: pure function,
// `Vec<DocsTreeFile>` out, no filesystem/git/network I/O -- `place_docs`
// remains the sole writer.
// ─────────────────────────────────────────────────────────────────────────

const REFERENCE_CONFIGURATION_RICH: &str = "docs/reference/configuration.md";
const REFERENCE_CLI_RICH: &str = "docs/reference/cli.md";

/// Recover the real first paragraph of a generated markdown page: skip
/// leading blank lines and `#`-heading lines, then join lines up to the
/// next blank line into one paragraph. Used as the per-page description in
/// `docs/index.md`'s nav (design §1.2: "full nav with per-page
/// descriptions") -- never a hand-authored summary, always the page's own
/// real prose, so the description can't drift from what the page actually
/// says. Returns an empty string (never panics) when a page has no body
/// text at all (e.g. only a heading) -- callers substitute an honest
/// fallback in that case.
pub fn first_paragraph(content: &str) -> String {
    let mut lines = content.lines().skip_while(|l| {
        let t = l.trim();
        t.is_empty() || t.starts_with('#')
    });

    let mut para: Vec<&str> = Vec::new();
    for line in &mut lines {
        if line.trim().is_empty() {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        para.push(line.trim());
    }
    para.join(" ")
}

/// Wrap an already-titled page body (a subsystem page, a guide, or
/// getting-started content already carries its own leading `#` heading --
/// see the DGRICH-02 prompts) with the breadcrumb above and cross-links
/// below, WITHOUT imposing a second title -- the sibling of [`page`] for
/// content whose heading is already the model's, not this renderer's.
fn wrap_body(depth: usize, body: &str) -> String {
    format!("{}\n\n---\n\n{}\n\n---\n\n{}\n", breadcrumb(depth), body.trim(), cross_links(depth))
}

fn full_architecture_diagram(facts: &RepoFacts) -> String {
    full_subsystem_architecture_mermaid_source(facts)
        .ok()
        .and_then(|source| mermaid_fence(&source).ok())
        .unwrap_or_else(|| STATIC_DIAGRAM_FALLBACK.to_string())
}

/// One row of `docs/index.md`'s full nav: a relative link (from `docs/`),
/// a human title, and the page's real first paragraph as its description.
struct NavEntry {
    link: String,
    title: String,
    description: String,
}

impl NavEntry {
    fn new(link: impl Into<String>, title: impl Into<String>, body: &str, fallback: &str) -> Self {
        let description = first_paragraph(body);
        let description = if description.is_empty() { fallback.to_string() } else { description };
        Self { link: link.into(), title: title.into(), description }
    }

    fn row(&self) -> String {
        format!("- [{}]({}) \u{2014} {}", self.title, self.link, self.description)
    }
}

/// Build the KG-derived `docs/` tree (design §1.2 / DGRICH-06) from a
/// [`RepoDocsOutcome`] (DGRICH-03's Pass 1-3 output) and the [`RepoFacts`]
/// (DGRICH-01) it was generated from.
///
/// `project` names the repo for page titles/hub text; `legacy_pages` is
/// the DGRICH-08 no-loss backstop -- `(slug, verbatim_content)` pairs that
/// become `docs/legacy/<slug>.md`, linked from `reference/index.md`.
/// Empty is the norm (ideally every old-README section is covered by
/// generation); this item only wires the parameter and the passthrough/
/// link, DGRICH-08 is what populates it.
///
/// Degradation (never a missing page, always an honest one): when
/// `outcome.identity` is `None` (the identity pass never succeeded), the
/// hub one-liner and architecture narrative fall back to plain,
/// non-fabricated text, and `reference/index.md` says explicitly that no
/// subsystem pages were generated this round -- `getting-started.md` and
/// `guides/` still render from whatever `outcome` does carry (they do not
/// depend on `identity` being present in this function; DGRICH-03 already
/// skips Pass 2/3 for a caller when identity fails, so in practice this
/// case emits index/getting-started(placeholder)/guides(placeholder)
/// exactly per the EDGE CASE this item's spec calls out).
pub fn build_repo_docs_tree(
    project: &str,
    facts: &RepoFacts,
    outcome: &RepoDocsOutcome,
    legacy_pages: &[(String, String)],
) -> Vec<DocsTreeFile> {
    let diagram = full_architecture_diagram(facts);

    // Kept subsystems that actually got a generated page, in `facts`'s
    // stable rollup order (not `outcome.subsystem_pages`'s insertion order,
    // which is concurrency-dependent -- see `run_subsystem_pass`).
    let pages_by_name: std::collections::BTreeMap<&str, &str> =
        outcome.subsystem_pages.iter().map(|(n, c)| (n.as_str(), c.as_str())).collect();
    let ordered_subsystems: Vec<&Subsystem> = facts.subsystems.iter().filter(|s| !s.is_misc).collect();

    let mut files = Vec::new();

    // ── guides/*.md (already full `docs/guides/<slug>.md` paths) ────────
    let mut guide_entries: Vec<NavEntry> = Vec::new();
    let mut guide_files: Vec<DocsTreeFile> = Vec::new();
    for (path, content) in &outcome.guides {
        let path_str = path.to_string_lossy().to_string();
        let title = guide_title(&path_str);
        let link = path_str.strip_prefix("docs/").unwrap_or(&path_str).to_string();
        guide_entries.push(NavEntry::new(link, title, content, "(no guide description available)"));
        guide_files.push(DocsTreeFile { path: path_str, content: wrap_body(1, content) });
    }

    // ── reference/<subsystem>.md, in facts rollup order ─────────────────
    let mut reference_entries: Vec<NavEntry> = Vec::new();
    let mut reference_files: Vec<DocsTreeFile> = Vec::new();
    let mut reference_rows: Vec<String> = Vec::new();
    let identity_one_liner = |name: &str| -> Option<String> {
        outcome.identity.as_ref().and_then(|id| {
            id.subsystems.iter().find(|b| b.name == name).map(|b| b.one_liner.clone())
        })
    };
    for subsystem in &ordered_subsystems {
        let path = format!("docs/reference/{}.md", subsystem.name);
        match pages_by_name.get(subsystem.name.as_str()) {
            Some(content) => {
                let content: &str = *content;
                let link = format!("reference/{}.md", subsystem.name);
                reference_entries.push(NavEntry::new(
                    link.clone(),
                    subsystem.name.clone(),
                    content,
                    "(no description available)",
                ));
                reference_files.push(DocsTreeFile { path, content: wrap_body(1, content) });
                let one_liner = identity_one_liner(&subsystem.name)
                    .unwrap_or_else(|| first_paragraph(content));
                reference_rows.push(format!(
                    "| [{}](reference/{}.md) | {} |",
                    subsystem.name, subsystem.name, one_liner
                ));
            }
            None => {
                reference_rows.push(format!(
                    "| {} | _not generated this round -- see the pass ledger_ |",
                    subsystem.name
                ));
            }
        }
    }

    // ── docs/legacy/<slug>.md passthrough (DGRICH-08 backstop) ──────────
    let mut legacy_files: Vec<DocsTreeFile> = Vec::new();
    let mut legacy_links: Vec<String> = Vec::new();
    for (slug, content) in legacy_pages {
        let path = format!("docs/legacy/{slug}.md");
        legacy_files.push(DocsTreeFile { path: path.clone(), content: wrap_body(1, content) });
        legacy_links.push(format!("- [{slug}](legacy/{slug}.md)"));
    }

    // ── docs/getting-started.md ──────────────────────────────────────────
    let getting_started_body = if outcome.getting_started.trim().is_empty() {
        "_Getting-started content is not available for this generation round -- see the pass \
ledger for why._"
            .to_string()
    } else {
        outcome.getting_started.clone()
    };
    let getting_started_entry = NavEntry::new(
        "getting-started.md",
        "Getting Started",
        &getting_started_body,
        "(no getting-started description available)",
    );
    files.push(DocsTreeFile {
        path: GETTING_STARTED.to_string(),
        content: wrap_body(0, &getting_started_body),
    });

    // ── docs/architecture.md ─────────────────────────────────────────────
    let architecture_narrative = match &outcome.identity {
        Some(identity) if !identity.subsystems.is_empty() => identity
            .subsystems
            .iter()
            .map(|s| format!("- **{}** ({}) -- {}", s.name, s.role, s.one_liner))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => "_No per-subsystem narrative available for this generation round._".to_string(),
    };
    files.push(DocsTreeFile {
        path: ARCHITECTURE.to_string(),
        content: rich_architecture_page(project, &diagram, &architecture_narrative),
    });

    // ── docs/guides/index.md + docs/guides/*.md ─────────────────────────
    files.push(DocsTreeFile {
        path: GUIDES_INDEX.to_string(),
        content: rich_guides_index_page(&guide_entries),
    });
    files.extend(guide_files);

    // ── docs/reference/index.md + docs/reference/<subsystem>.md ─────────
    files.push(DocsTreeFile {
        path: REFERENCE_INDEX.to_string(),
        content: rich_reference_index_page(&reference_rows, &legacy_links),
    });
    files.extend(reference_files);

    // ── docs/reference/configuration.md (names only, never values) ──────
    files.push(DocsTreeFile {
        path: REFERENCE_CONFIGURATION_RICH.to_string(),
        content: rich_configuration_page(facts),
    });

    // ── docs/reference/cli.md (real `[[bin]]` targets) ───────────────────
    files.push(DocsTreeFile { path: REFERENCE_CLI_RICH.to_string(), content: rich_cli_page(facts) });

    // ── docs/legacy/<slug>.md passthrough ─────────────────────────────────
    files.extend(legacy_files);

    // ── docs/index.md (hub; built last so it can describe every page
    //    above -- inserted at the front to match this module's stable
    //    "index first" file-set convention) ────────────────────────────
    let hub = rich_index_page(
        project,
        &diagram,
        outcome.identity.as_ref(),
        &getting_started_entry,
        &guide_entries,
        &reference_entries,
    );
    files.insert(0, DocsTreeFile { path: DOCS_INDEX.to_string(), content: hub });

    // ── _Sidebar.md ───────────────────────────────────────────────────────
    let sidebar_paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    files.push(DocsTreeFile { path: SIDEBAR.to_string(), content: rich_sidebar_page(project, &sidebar_paths) });

    files
}

/// Human title for a `docs/guides/<slug>.md` path: the slug with `-`/`_`
/// turned into spaces and title-cased, e.g. `run-assessment` -> "Run
/// Assessment". Best-effort display only -- never used for lookups.
fn guide_title(path: &str) -> String {
    let stem = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md");
    stem.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn rich_index_page(
    project: &str,
    diagram: &str,
    identity: Option<&RepoIdentity>,
    getting_started: &NavEntry,
    guides: &[NavEntry],
    reference: &[NavEntry],
) -> String {
    let one_liner = identity
        .map(|id| id.tagline.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("Documentation for {project}."));

    let mut guide_rows = guides.iter().map(NavEntry::row).collect::<Vec<_>>().join("\n");
    if guide_rows.is_empty() {
        guide_rows = "_No guides generated this round._".to_string();
    }
    let mut reference_rows = reference.iter().map(NavEntry::row).collect::<Vec<_>>().join("\n");
    if reference_rows.is_empty() {
        reference_rows = "_No subsystem reference pages generated this round._".to_string();
    }

    format!(
        "# {project} Documentation\n\n\
{one_liner}\n\n\
{diagram}\n\n\
## Getting Started\n\n\
{}\n\n\
## Guides\n\n\
{guide_rows}\n\n\
## Reference\n\n\
{reference_rows}\n\
- [Configuration](reference/configuration.md)\n\
- [CLI](reference/cli.md)\n\n\
## Architecture\n\n\
- [Architecture](architecture.md) \u{2014} the full derived diagram plus per-subsystem narrative.\n\n\
[Back to the project README](../README.md)\n",
        getting_started.row(),
    )
}

fn rich_guides_index_page(guides: &[NavEntry]) -> String {
    let rows = if guides.is_empty() {
        "_No guides generated this round._".to_string()
    } else {
        guides.iter().map(NavEntry::row).collect::<Vec<_>>().join("\n")
    };
    format!(
        "# Guides\n\n{}\n\n---\n\n## Available guides\n\n{rows}\n\n{}\n",
        breadcrumb(1),
        cross_links(1)
    )
}

fn rich_reference_index_page(subsystem_rows: &[String], legacy_links: &[String]) -> String {
    let table = if subsystem_rows.is_empty() {
        "_No subsystem reference pages generated this round._".to_string()
    } else {
        format!(
            "| Subsystem | What it does |\n|---|---|\n{}",
            subsystem_rows.join("\n")
        )
    };
    let legacy_section = if legacy_links.is_empty() {
        String::new()
    } else {
        format!("\n\n## Legacy (no-loss backstop)\n\n{}\n", legacy_links.join("\n"))
    };
    format!(
        "# Reference\n\n{}\n\n---\n\n\
## Subsystem inventory\n\n\
{table}\n\n\
## Other reference pages\n\n\
- [Configuration](configuration.md)\n\
- [CLI](cli.md)\n\
{legacy_section}\n\
{}\n",
        breadcrumb(1),
        cross_links(1)
    )
}

/// `docs/reference/configuration.md` -- populated from
/// `facts.config_surface.env_var_names`. NAMES ONLY, never values (design
/// §2 source 5 / DGRICH-01 acceptance criterion): this function has no
/// access to any secret value in the first place, so there is nothing to
/// accidentally leak, but the honest-empty case is spelled out too rather
/// than left as a silent stub.
fn rich_configuration_page(facts: &RepoFacts) -> String {
    let body = if facts.config_surface.env_var_names.is_empty() {
        "_This repository's configuration scan found no environment-variable accessors._"
            .to_string()
    } else {
        let rows = facts
            .config_surface
            .env_var_names
            .iter()
            .map(|name| format!("- `{name}`"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "The following configuration keys are read by this repository (names only -- values \
are provided by the repository's configured secret source at runtime, never hardcoded and \
never shown here):\n\n{rows}"
        )
    };
    wrap_body(1, &format!("# Configuration Reference\n\n{body}"))
}

/// `docs/reference/cli.md` -- populated from `facts.entry_points.bin_targets`
/// (real Cargo.toml `[[bin]]` targets), never a stub.
fn rich_cli_page(facts: &RepoFacts) -> String {
    let body = if facts.entry_points.bin_targets.is_empty() {
        "_No `[[bin]]` targets were found in this repository's Cargo manifest(s)._".to_string()
    } else {
        let rows = facts
            .entry_points
            .bin_targets
            .iter()
            .map(|b| format!("- `{}` ({})", b.name, b.path))
            .collect::<Vec<_>>()
            .join("\n");
        format!("The following binaries are built from this repository:\n\n{rows}")
    };
    wrap_body(1, &format!("# CLI Reference\n\n{body}"))
}

fn rich_architecture_page(project: &str, diagram: &str, narrative: &str) -> String {
    format!(
        "# Architecture \u{2014} {project}\n\n{}\n\n---\n\n{diagram}\n\n## Subsystems\n\n{narrative}\n\n---\n\n{}\n",
        breadcrumb(0),
        cross_links(0)
    )
}

fn rich_sidebar_page(project: &str, paths: &[String]) -> String {
    let mut body = format!("# {project}\n\n");
    for path in paths {
        // Every generated path is repo-relative from the checkout root
        // (e.g. `docs/reference/mesh.md`, `_Sidebar.md` itself); render
        // depth-appropriate indentation for readability, matching the
        // legacy `sidebar_page`'s nested-bullet convention.
        let depth = path.matches('/').count();
        let indent = "  ".repeat(depth.saturating_sub(1));
        body.push_str(&format!("{indent}- [{path}]({path})\n"));
    }
    body
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
        // render_diataxis_set itself already placeholders an absent mode
        // ("_No {mode} content yet for {module}._") -- mode_body recovers
        // that text verbatim rather than double-placeholdering it.
        assert!(find(&files, GUIDES_OVERVIEW).content.contains("No how-to content yet"));
        assert!(find(&files, REFERENCE_API).content.contains("No reference content yet"));
        assert!(find(&files, ARCHITECTURE).content.contains("No explanation content yet"));
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

    // ─────────────────────────────────────────────────────────────────
    // DGRICH-06: build_repo_docs_tree (the rich, KG-derived tree)
    // ─────────────────────────────────────────────────────────────────

    use super::super::super::generate::RepoDocsOutcome;
    use super::super::super::prompts::{FeatureRow, GuideTopic, RepoIdentity, SubsystemBrief};
    use super::super::super::repo_facts::{BinTarget, ConfigSurface, EntryPoints, RepoFacts, Subsystem};
    use std::path::PathBuf;

    fn fixture_subsystem(name: &str) -> Subsystem {
        Subsystem { name: name.to_string(), is_misc: false, ..Default::default() }
    }

    fn fixture_facts() -> RepoFacts {
        RepoFacts {
            project_id: "widget-factory".to_string(),
            git_ref: "abc123".to_string(),
            kg_grounded: true,
            subsystems: vec![
                fixture_subsystem("a"),
                fixture_subsystem("b"),
                fixture_subsystem("c"),
            ],
            entry_points: EntryPoints {
                bin_targets: vec![BinTarget {
                    name: "widget_cli".to_string(),
                    path: "src/bin/widget_cli.rs".to_string(),
                }],
                ..Default::default()
            },
            config_surface: ConfigSurface {
                env_var_names: vec!["WIDGET_BIND".to_string(), "WIDGET_LOG_LEVEL".to_string()],
            },
            ..Default::default()
        }
    }

    fn fixture_identity() -> RepoIdentity {
        RepoIdentity {
            tagline: "Widget Factory turns raw material into widgets.".to_string(),
            what_is: "Widget Factory is a small manufacturing pipeline.".to_string(),
            audience: "Widget operators.".to_string(),
            subsystems: vec![
                SubsystemBrief { name: "a".to_string(), one_liner: "Subsystem A cuts material.".to_string(), role: "core".to_string() },
                SubsystemBrief { name: "b".to_string(), one_liner: "Subsystem B assembles parts.".to_string(), role: "core".to_string() },
                SubsystemBrief { name: "c".to_string(), one_liner: "Subsystem C ships widgets.".to_string(), role: "integration".to_string() },
            ],
            feature_rows: vec![FeatureRow {
                feature: "Cutting".to_string(),
                description: "Cuts raw material to size.".to_string(),
                subsystem: "a".to_string(),
            }],
            guide_topics: vec![GuideTopic { title: "Run assessment".to_string(), grounding: "widget_cli".to_string() }],
        }
    }

    fn fixture_outcome_full() -> RepoDocsOutcome {
        RepoDocsOutcome {
            identity: Some(fixture_identity()),
            subsystem_pages: vec![
                ("a".to_string(), "# a\n\nSubsystem A purpose paragraph goes here.\n".to_string()),
                ("b".to_string(), "# b\n\nSubsystem B purpose paragraph goes here.\n".to_string()),
                ("c".to_string(), "# c\n\nSubsystem C purpose paragraph goes here.\n".to_string()),
            ],
            guides: vec![
                (PathBuf::from("docs/guides/run-assessment.md"), "# Run Assessment\n\nFollow these steps to run an assessment.\n".to_string()),
                (PathBuf::from("docs/guides/rotate-config.md"), "# Rotate Config\n\nFollow these steps to rotate configuration.\n".to_string()),
            ],
            getting_started: "# Getting Started\n\nClone the repo and run the CLI.\n".to_string(),
            missing: Vec::new(),
            pass_ledger: Vec::new(),
        }
    }

    #[test]
    fn rich_tree_emits_one_reference_page_per_subsystem_page() {
        let files = build_repo_docs_tree("widget-factory", &fixture_facts(), &fixture_outcome_full(), &[]);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        for expected in [
            "docs/reference/a.md",
            "docs/reference/b.md",
            "docs/reference/c.md",
            "docs/guides/run-assessment.md",
            "docs/guides/rotate-config.md",
        ] {
            assert!(paths.contains(&expected), "missing {expected} in {paths:?}");
        }
    }

    #[test]
    fn rich_tree_populates_configuration_and_cli_pages_not_stubs() {
        let files = build_repo_docs_tree("widget-factory", &fixture_facts(), &fixture_outcome_full(), &[]);
        let config = &find(&files, "docs/reference/configuration.md").content;
        assert!(config.contains("WIDGET_BIND"));
        assert!(config.contains("WIDGET_LOG_LEVEL"));
        assert!(!config.contains("No configuration reference content yet"));

        let cli = &find(&files, "docs/reference/cli.md").content;
        assert!(cli.contains("widget_cli"));
        assert!(cli.contains("src/bin/widget_cli.rs"));
        assert!(!cli.contains("No CLI reference content yet"));
    }

    #[test]
    fn rich_tree_configuration_page_lists_names_only_never_values() {
        // The fixture facts carry only NAMES (RepoFacts::config_surface is
        // names-only by construction -- there is no value field to leak),
        // so this asserts the page surfaces exactly those names and never
        // fabricates or echoes anything value-shaped.
        let files = build_repo_docs_tree("widget-factory", &fixture_facts(), &fixture_outcome_full(), &[]);
        let config = &find(&files, "docs/reference/configuration.md").content;
        assert!(config.contains("`WIDGET_BIND`"));
        assert!(config.contains("`WIDGET_LOG_LEVEL`"));
        assert!(config.contains("never hardcoded"));
    }

    #[test]
    fn rich_tree_index_carries_real_first_paragraph_descriptions() {
        let files = build_repo_docs_tree("widget-factory", &fixture_facts(), &fixture_outcome_full(), &[]);
        let index = &find(&files, "docs/index.md").content;
        assert!(index.contains("Subsystem A purpose paragraph goes here."));
        assert!(index.contains("Follow these steps to run an assessment."));
        assert!(index.contains("Clone the repo and run the CLI."));
        // The hub one-liner comes from the identity tagline, not chrome.
        assert!(index.contains("Widget Factory turns raw material into widgets."));
    }

    #[test]
    fn first_paragraph_skips_heading_and_blank_lines() {
        assert_eq!(
            first_paragraph("# Title\n\nThis is the real first paragraph.\nStill part of it.\n\nSecond paragraph."),
            "This is the real first paragraph. Still part of it."
        );
        assert_eq!(first_paragraph("# Title only\n"), "");
    }

    #[test]
    fn rich_tree_breadcrumbs_and_sidebar_still_generated() {
        let files = build_repo_docs_tree("widget-factory", &fixture_facts(), &fixture_outcome_full(), &[]);
        let a_page = &find(&files, "docs/reference/a.md").content;
        assert!(a_page.contains("(../index.md)"), "reference page breadcrumb must be depth-1: {a_page}");
        assert!(a_page.contains("See also"));

        let sidebar = &find(&files, SIDEBAR).content;
        for path in [
            "docs/index.md",
            "docs/getting-started.md",
            "docs/reference/a.md",
            "docs/reference/b.md",
            "docs/reference/c.md",
            "docs/guides/run-assessment.md",
            "docs/reference/configuration.md",
            "docs/reference/cli.md",
        ] {
            assert!(sidebar.contains(path), "_Sidebar.md missing {path}: {sidebar}");
        }
    }

    #[test]
    fn rich_tree_degraded_zero_reference_pages_still_emits_index_getting_started_guides() {
        let degraded = RepoDocsOutcome {
            identity: None,
            subsystem_pages: Vec::new(),
            guides: vec![(
                PathBuf::from("docs/guides/manual-setup.md"),
                "# Manual Setup\n\nDo it by hand for now.\n".to_string(),
            )],
            getting_started: String::new(),
            missing: vec!["identity".to_string()],
            pass_ledger: Vec::new(),
        };
        let files = build_repo_docs_tree("widget-factory", &fixture_facts(), &degraded, &[]);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();

        assert!(paths.contains(&"docs/index.md"));
        assert!(paths.contains(&"docs/getting-started.md"));
        assert!(paths.contains(&"docs/guides/index.md"));
        assert!(paths.contains(&"docs/guides/manual-setup.md"));
        assert!(!paths.iter().any(|p| p.starts_with("docs/reference/") && !p.contains("index") && !p.contains("configuration") && !p.contains("cli")));

        // Honest degradation, not a silently blank page.
        let getting_started = &find(&files, "docs/getting-started.md").content;
        assert!(getting_started.contains("not available for this generation round"));
        let reference_index = &find(&files, "docs/reference/index.md").content;
        assert!(reference_index.contains("not generated this round") || reference_index.contains("No subsystem reference pages"));
    }

    #[test]
    fn rich_tree_legacy_pages_wired_and_linked_from_reference_index() {
        let legacy = vec![("old-notes".to_string(), "## Old Notes\n\nSome verbatim legacy content.\n".to_string())];
        let files = build_repo_docs_tree("widget-factory", &fixture_facts(), &fixture_outcome_full(), &legacy);
        assert!(find(&files, "docs/legacy/old-notes.md").content.contains("Some verbatim legacy content."));
        let reference_index = &find(&files, "docs/reference/index.md").content;
        assert!(reference_index.contains("legacy/old-notes.md"));
    }

    #[test]
    fn rich_tree_never_touches_the_filesystem() {
        let tmp = std::env::temp_dir().join(format!("docgen-rich-docs-tree-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let before: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(before.is_empty());

        let _ = build_repo_docs_tree("widget-factory", &fixture_facts(), &fixture_outcome_full(), &[]);

        let after: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(after.is_empty(), "build_repo_docs_tree must never write files");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
