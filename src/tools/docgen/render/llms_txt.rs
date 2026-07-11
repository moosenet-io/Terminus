//! DOCGEN-20: machine-readable AI surface -- `llms.txt` / `llms-full.txt`
//! (S95, Plane TERM-171).
//!
//! Copies Mintlify's machine-readable AI surface: alongside the human-
//! facing render targets ([`super::markdown`], [`super::wiki`], ...) a
//! project's documentation corpus is also emitted as two AI-consumption
//! artifacts:
//!   - **`llms.txt`** -- an INDEX: one line per page, `- [Title](path):
//!     one-liner summary`, so an LLM client can cheaply decide which pages
//!     are worth fetching in full (the Mintlify/`llmstxt.org` convention).
//!   - **`llms-full.txt`** -- the FULL markdown corpus, every page's
//!     complete content concatenated in order, for a client that wants
//!     everything in one shot instead of following the index.
//!
//! ## Scope and shape (why this isn't just another `render/*.rs` target)
//! Every single-target renderer in this module ([`super::markdown`],
//! [`super::wiki`], [`super::pdf`], [`super::obsidian`], [`super::notion`],
//! [`super::blog`]) renders ONE piece of already-generated content,
//! described by a single [`super::RenderContext`]. `llms.txt`/
//! `llms-full.txt` are inherently *whole-corpus* concerns -- the index
//! needs every page's title/path/one-liner at once, and the full corpus is
//! literally every page concatenated -- so, exactly like
//! [`super::wiki_graph`] (nav/backlinks/graph-view), this module operates
//! over a *slice* of pages ([`LlmsPage`]), not a single [`super::RenderContext`],
//! and does not plug into [`super::render_all`] / [`super::config::DocTargetType`]
//! (both are one-target-at-a-time). A future item can wire a whole-corpus
//! pass through [`render_llms_txt`] once a caller assembles the page slice
//! (e.g. from a project's rendered README pages); this item ships the
//! engine + tested logic.
//!
//! ## WRITE-MODEL INVERSION (unchanged from the rest of `render/`)
//! Every function here is pure (no filesystem, network, or subprocess I/O).
//! [`render_llms_txt`] RETURNS both artifacts as plain `String`s -- it never
//! writes a file, commits, or pushes anything. The calling harness (outside
//! this crate) decides where `llms.txt`/`llms-full.txt` land, exactly like
//! every other renderer in this module (see `render/mod.rs`'s
//! WRITE-MODEL-INVERSION doc comment).
//!
//! ## Ordering (load-bearing): swept-before-corpus, no unswept path
//! The corpus this module emits derives ONLY from already-PII-swept
//! content. [`LlmsPage::content`] is typed as
//! `&`[`crate::tools::docgen::generate::SweptFeatContext`] -- the SAME
//! ordering-enforcement type [`super::super::generate::generate_docs`]
//! already requires for a feat's diff/spec/code context (DOCGEN-05), reused
//! here rather than inventing a second wrapper. `SweptFeatContext`'s only
//! public constructor is `from_gate_outcome(&PiiGateOutcome)`, and
//! `PiiGateOutcome` is only produced by [`super::super::pii_gate::sweep_input`]
//! (DOCGEN-02's gate). So there is no way to hand [`render_llms_txt`] a raw,
//! unswept `&str` -- a caller MUST have already run every page's content
//! through the PII gate. See `corpus_only_ever_contains_swept_content_never_raw_pii`
//! below for the negative test asserting this end to end (a raw private-IP
//! literal, present in pre-sweep input, never appears in either emitted
//! artifact -- only the gate's redaction placeholder does).
//!
//! ## Optional follow-up (documented, not built): MCP doc-query / clean-
//! markdown content negotiation
//! Beyond the static `llms.txt`/`llms-full.txt` files, Mintlify-style sites
//! also serve a clean-markdown variant of every page on request (e.g.
//! `GET /page.md` or `Accept: text/markdown` content negotiation on the
//! normal page URL), and some sites expose an MCP server so an agent can
//! query docs directly as tool calls instead of fetching static files. Both
//! are OUT OF SCOPE for this item -- a full MCP doc-query server is a
//! materially larger, standalone surface (its own transport, auth posture,
//! and registration decision: core vs. personal registry, per this crate's
//! S9 single-access-path rule) than "wire two new render targets," and
//! half-building it here would violate the same "note it as a follow-up,
//! don't half-build" posture [`super::wiki_graph`] already applies to its
//! own deferred Starlight-site target. The design sketch, for a future
//! item:
//!   - **Content negotiation**: the calling harness that places rendered
//!     artifacts (this module never does its own placement, per the
//!     WRITE-MODEL INVERSION above) could serve `{page}.md` alongside the
//!     existing `{page}` HTML route, returning exactly the swept markdown
//!     this module already produces per page -- no new rendering needed,
//!     only a routing/placement decision downstream of this engine.
//!   - **MCP doc-query tool**: a `docgen_query_docs` tool (name TBD) taking
//!     a project + free-text query and returning the most relevant already-
//!     rendered page(s) -- would reuse [`LlmsPage`]'s index (title + one-
//!     liner) for cheap relevance ranking before returning a page's full
//!     swept content, mirroring how `llms.txt` already lets a client filter
//!     before fetching `llms-full.txt`. Registration would follow the same
//!     core-registry path every other docgen tool uses (`docgen::register`,
//!     `src/tools/docgen/mod.rs`) -- never a second, ad hoc doc-serving
//!     path per S9.
//!
//! ## REUSE, not reimplementation (RECONCILIATION CONSTRAINTS)
//! No new PII scanning, no new "swept content" wrapper type, no new
//! markdown dialect: this module reuses [`crate::tools::docgen::generate::SweptFeatContext`]
//! for the ordering enforcement above, and emits plain Markdown (the same
//! format [`super::markdown`] already produces) -- `llms.txt`/
//! `llms-full.txt` are, by convention, plain Markdown/text files, not a
//! new templating format.

use crate::tools::docgen::generate::SweptFeatContext;

/// The maximum character length of a one-liner summary in `llms.txt`'s
/// index before truncation with a trailing ellipsis. Matches a conventional
/// single-line index entry width -- long enough to be useful, short enough
/// that the index stays cheap to scan/fetch in full.
const ONE_LINER_MAX_CHARS: usize = 160;

/// One documentation page's facts, as already produced by an earlier render
/// pass (e.g. [`super::markdown::render`]'s output for that page).
/// Deliberately NOT [`super::RenderContext`] -- that type carries a single
/// already-rendered artifact's content plus source-commit/generated-at
/// provenance for ONE render call; a whole-corpus pass only needs these
/// four fields per page, matching [`super::wiki_graph::VaultNoteSummary`]'s
/// same "whole-corpus input shape" convention.
#[derive(Debug, Clone)]
pub struct LlmsPage<'a> {
    /// The page's human-readable title, e.g. a module name or README title.
    pub title: &'a str,
    /// The module/path within the project this page documents.
    pub module: &'a str,
    /// A repo-relative (or site-relative) link target for `llms.txt`'s
    /// index, e.g. `"docs/widget/README.md"`. Informational only -- this
    /// module performs no filesystem I/O and never validates the path
    /// resolves to anything.
    pub path_hint: &'a str,
    /// The page's full content. MUST already be PII-swept -- see this
    /// module's "Ordering (load-bearing)" doc comment above; there is no
    /// way to construct this field from a raw, unswept `&str`.
    pub content: &'a SweptFeatContext,
}

/// The two AI-consumption artifacts this module produces for a project's
/// page corpus. Plain `String`s -- see the WRITE-MODEL INVERSION doc
/// comment above; nothing here is ever written to disk by this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmsTxtArtifacts {
    /// `llms.txt`: the page index with one-liner summaries.
    pub index: String,
    /// `llms-full.txt`: the full corpus, every page concatenated in order.
    pub full: String,
}

/// Render both `llms.txt` (index) and `llms-full.txt` (full corpus) for
/// `project`'s `pages`, in the order given. An empty `pages` slice still
/// produces a stable, well-formed pair of artifacts (a header-only index
/// and an empty-but-titled full corpus) -- never a panic or an empty
/// string (spec-equivalent edge case to [`super::wiki_graph`]'s
/// deterministic-regardless-of-input-order posture).
pub fn render_llms_txt(project: &str, pages: &[LlmsPage<'_>]) -> LlmsTxtArtifacts {
    LlmsTxtArtifacts {
        index: render_index(project, pages),
        full: render_full(project, pages),
    }
}

fn render_index(project: &str, pages: &[LlmsPage<'_>]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {project}\n\n"));
    out.push_str(&format!(
        "> Machine-readable documentation index for {project}. Fetch the full \
corpus at llms-full.txt, or follow an individual page link below.\n\n"
    ));
    out.push_str("## Pages\n\n");
    for page in pages {
        let summary = one_liner(page.content.as_str());
        if summary.is_empty() {
            out.push_str(&format!("- [{}]({})\n", page.title, page.path_hint));
        } else {
            out.push_str(&format!("- [{}]({}): {}\n", page.title, page.path_hint, summary));
        }
    }
    out
}

fn render_full(project: &str, pages: &[LlmsPage<'_>]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {project} -- Full Documentation Corpus\n\n"));
    for (i, page) in pages.iter().enumerate() {
        if i > 0 {
            out.push_str("\n---\n\n");
        }
        out.push_str(&format!("## {} ({})\n\n", page.title, page.module));
        out.push_str(page.content.as_str());
        if !page.content.as_str().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Extract a one-line summary from a page's (already-swept) content: the
/// first non-empty line, with any leading Markdown heading markers (`#`)
/// stripped, truncated to [`ONE_LINER_MAX_CHARS`] with a trailing ellipsis
/// when longer. Returns an empty string for content with no non-empty,
/// non-heading-marker-only line (e.g. blank content) -- callers render an
/// index entry without a trailing summary in that case rather than a
/// misleading blank `": "`.
fn one_liner(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let stripped = trimmed.trim_start_matches('#').trim();
        if stripped.is_empty() {
            continue;
        }
        return truncate_chars(stripped, ONE_LINER_MAX_CHARS);
    }
    String::new()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}\u{2026}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::docgen::pii_gate::sweep_input;

    fn swept(raw: &str) -> SweptFeatContext {
        let outcome = sweep_input(raw).unwrap();
        SweptFeatContext::from_gate_outcome(&outcome)
    }

    // ── llms.txt index ──────────────────────────────────────────────────

    #[test]
    fn index_lists_every_page_with_title_path_and_one_liner() {
        let c1 = swept("# Widget\n\nThe widget does A and B.");
        let c2 = swept("# Gadget\n\nThe gadget does C.");
        let pages = vec![
            LlmsPage { title: "Widget", module: "src/widget", path_hint: "docs/widget/README.md", content: &c1 },
            LlmsPage { title: "Gadget", module: "src/gadget", path_hint: "docs/gadget/README.md", content: &c2 },
        ];
        let out = render_llms_txt("widget-factory", &pages);

        assert!(out.index.starts_with("# widget-factory\n\n"));
        assert!(out.index.contains("## Pages"));
        assert!(out.index.contains("- [Widget](docs/widget/README.md): Widget"));
        assert!(out.index.contains("- [Gadget](docs/gadget/README.md): Gadget"));
    }

    #[test]
    fn index_one_liner_strips_heading_markers_and_uses_first_nonempty_line() {
        let c = swept("\n\n### The Widget Module\n\nBody text here.");
        let pages = vec![LlmsPage { title: "Widget", module: "src/widget", path_hint: "widget.md", content: &c }];
        let out = render_llms_txt("proj", &pages);
        assert!(out.index.contains("The Widget Module"));
        assert!(!out.index.contains("### The Widget Module"));
    }

    #[test]
    fn index_truncates_long_one_liner_with_ellipsis() {
        let long_line = "x".repeat(300);
        let c = swept(&long_line);
        let pages = vec![LlmsPage { title: "Long", module: "m", path_hint: "long.md", content: &c }];
        let out = render_llms_txt("proj", &pages);
        // one truncated summary line present, capped near ONE_LINER_MAX_CHARS
        let summary_line = out.index.lines().find(|l| l.starts_with("- [Long]")).unwrap();
        assert!(summary_line.ends_with('\u{2026}'));
        assert!(summary_line.chars().count() < 300);
    }

    #[test]
    fn index_page_with_blank_content_has_no_trailing_summary() {
        let c = swept("");
        let pages = vec![LlmsPage { title: "Empty", module: "m", path_hint: "empty.md", content: &c }];
        let out = render_llms_txt("proj", &pages);
        assert!(out.index.contains("- [Empty](empty.md)\n"));
        assert!(!out.index.contains("- [Empty](empty.md):"));
    }

    #[test]
    fn empty_pages_slice_produces_stable_header_only_index() {
        let out = render_llms_txt("proj", &[]);
        assert_eq!(out.index, "# proj\n\n> Machine-readable documentation index for proj. Fetch the full corpus at llms-full.txt, or follow an individual page link below.\n\n## Pages\n\n");
    }

    // ── llms-full.txt corpus ────────────────────────────────────────────

    #[test]
    fn full_concatenates_every_page_complete_content_in_order() {
        let c1 = swept("# Widget\n\nThe widget does A and B.");
        let c2 = swept("# Gadget\n\nThe gadget does C.");
        let pages = vec![
            LlmsPage { title: "Widget", module: "src/widget", path_hint: "widget.md", content: &c1 },
            LlmsPage { title: "Gadget", module: "src/gadget", path_hint: "gadget.md", content: &c2 },
        ];
        let out = render_llms_txt("widget-factory", &pages);

        assert!(out.full.starts_with("# widget-factory -- Full Documentation Corpus\n\n"));
        let widget_pos = out.full.find("The widget does A and B.").unwrap();
        let gadget_pos = out.full.find("The gadget does C.").unwrap();
        assert!(widget_pos < gadget_pos, "pages must appear in input order");
        assert!(out.full.contains("## Widget (src/widget)"));
        assert!(out.full.contains("## Gadget (src/gadget)"));
        assert!(out.full.contains("---"), "pages are separated by a rule");
    }

    #[test]
    fn empty_pages_slice_produces_titled_but_empty_full_corpus() {
        let out = render_llms_txt("proj", &[]);
        assert_eq!(out.full, "# proj -- Full Documentation Corpus\n\n");
    }

    // ── Ordering enforcement: swept-only corpus, negative test ─────────

    /// Negative test (spec ACCEPTANCE CRITERIA): the corpus this module
    /// emits derives ONLY from already-PII-swept content -- there is no
    /// unswept path. Feeds raw content containing a private-IP literal
    /// (canonical PII per `crate::github::pii`) through the SAME PII gate
    /// [`SweptFeatContext::from_gate_outcome`] requires, and asserts the
    /// raw literal never appears in either emitted artifact -- only the
    /// gate's `[REDACTED:...]` placeholder does. Because [`LlmsPage::content`]
    /// is typed as `&SweptFeatContext`, and that type's only public
    /// constructor requires a `PiiGateOutcome` from
    /// `crate::tools::docgen::pii_gate::sweep_input`, this is also a
    /// *structural* guarantee (a caller cannot compile a call to
    /// [`render_llms_txt`] with a bare unswept `&str`), verified here at
    /// the value level too.
    #[test]
    fn corpus_only_ever_contains_swept_content_never_raw_pii() {
        // pii-test-fixture: deliberate private-IP literal to exercise the
        // PII gate's redaction path; never leaves this test (see assertions
        // below), same precedented pattern as pii_gate.rs's own tests.
        let raw = "The internal service lives at <internal-ip> and talks to it directly."; // pii-test-fixture
        let outcome = sweep_input(raw).unwrap();
        assert!(
            matches!(outcome, crate::tools::docgen::pii_gate::PiiGateOutcome::Redacted { .. }),
            "fixture must actually trigger the gate's redaction path"
        );
        let c = SweptFeatContext::from_gate_outcome(&outcome);
        let pages = vec![LlmsPage { title: "Internal Service", module: "m", path_hint: "svc.md", content: &c }];

        let out = render_llms_txt("proj", &pages);

        assert!(!out.index.contains("<internal-ip>"), "raw IP must never reach llms.txt"); // pii-test-fixture
        assert!(!out.full.contains("<internal-ip>"), "raw IP must never reach llms-full.txt"); // pii-test-fixture
        assert!(out.full.contains("REDACTED"), "the gate's redaction placeholder must be present instead");
    }

    #[test]
    fn one_liner_returns_empty_for_all_whitespace_content() {
        assert_eq!(one_liner("\n\n   \n\t\n"), "");
    }

    #[test]
    fn one_liner_returns_empty_for_heading_markers_with_no_text() {
        assert_eq!(one_liner("###\n\nreal body"), "real body");
        assert_eq!(one_liner("###"), "");
    }
}
