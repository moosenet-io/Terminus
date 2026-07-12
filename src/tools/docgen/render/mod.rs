//! DOCGEN-06: multi-format rendering (README / wiki / PDF / Notion /
//! Obsidian / blog), S95, Plane TERM-148.
//!
//! Renders generated content (DOCGEN-05's output) into each format a
//! project's [`super::config::ProjectDocConfig`] declares.
//!
//! ## WRITE-MODEL INVERSION (load-bearing, differs from Scribe)
//! This engine RETURNS rendered artifacts -- bytes/strings plus a target
//! descriptor -- and does **NOT** place them into a repo, a hosting
//! surface, or a knowledge vault, and never commits or pushes anything.
//! The calling harness (outside this crate) is the one that decides where
//! a rendered artifact lands. This is the opposite of `crate::scribe::vault`,
//! whose `write_note_and_push` DOES commit+push to the Obsidian vault --
//! this module deliberately reuses only the PURE, no-I/O pieces of that
//! module (`render_note`, `NoteFrontmatter`, `build_wikilink`, `slugify`,
//! `note_path`) and never calls `write_note_and_push` or any other
//! filesystem/git/network *write*. See
//! `render_all_never_touches_filesystem_or_vault` in this module's tests
//! for the negative test asserting this end to end.
//!
//! The one exception, and it is deliberately read-only: the Notion and
//! blog renderers perform a lightweight credential *validation* call
//! (`NotionClient::validate` / `BlogClient::validate`) before rendering, so
//! a target with a present-but-invalid credential is skipped with a clear
//! note rather than returning a bogus artifact. That call never creates,
//! updates, or publishes anything -- it is a read, exactly like checking a
//! token is accepted, not a placement.
//!
//! ## Config-driven (reuse, not reimplement)
//! Which formats to render, and whether a target is credential-available,
//! is entirely decided by [`super::config::ProjectDocConfig::resolve`]
//! (DOCGEN-01) -- this module calls that existing resolver rather than
//! re-deriving target availability itself. A target `resolve()` reports
//! disabled (missing credential key) is skipped here with the SAME hint
//! text `resolve()` already produced, not a second competing message.
//!
//! ## Sub-renderers
//! - [`markdown`] (README) and [`obsidian`] reuse `crate::scribe::vault`'s
//!   pure note-rendering primitives -- no reimplementation of markdown/
//!   Obsidian rendering (RECONCILIATION CONSTRAINTS).
//! - [`wiki`], [`pdf`], [`notion`], [`blog`] are new renderers added by
//!   this item.

pub mod blog;
pub mod docs_tree;
pub mod llms_txt;
pub mod markdown;
pub mod notion;
pub mod obsidian;
pub mod pdf;
pub mod wiki;
pub mod wiki_graph;

use std::collections::BTreeSet;

use super::config::{DocTargetType, ProjectDocConfig};
use super::readme_layers::render_diataxis_set;
use docs_tree::{build_docs_tree, DocsTreeFile};

/// Everything a renderer needs about the piece of content being rendered.
/// Deliberately plain data (no I/O) -- every renderer in this module is a
/// pure function (or, for notion/blog, a function taking an injected
/// validation-only client seam) over this plus its own format concerns.
#[derive(Debug, Clone)]
pub struct RenderContext<'a> {
    /// The project/repo this content belongs to (used for note titles /
    /// frontmatter `module` field / Notion page titles).
    pub project: &'a str,
    /// The module/path within the project this content documents.
    pub module: &'a str,
    /// The commit/feat this content was generated against (DOCGEN-05's
    /// `GenerationOutcome::Generated::source_commit`; also versioning's
    /// `ArtifactVersion` key material) -- carried into frontmatter/notes so
    /// staleness stays detectable, matching `crate::scribe::vault`'s
    /// existing convention.
    pub source_commit: &'a str,
    /// RFC3339 timestamp string. Like `versioning.rs` and `scribe::vault`,
    /// this module never reads the system clock itself -- the caller
    /// supplies it, keeping every renderer deterministic and fully
    /// unit-testable.
    pub generated_at: &'a str,
    /// The generated content to render (DOCGEN-05 output; already deepened
    /// and PII-swept upstream -- this module performs no PII scanning of
    /// its own, matching `versioning.rs`'s "stores only what's already
    /// swept" posture).
    pub content: &'a str,
}

/// One target's render result. Either real rendered content
/// (`content.is_some()`, `note.is_none()`), or a skip with a human-readable
/// reason (`content.is_none()`, `note.is_some()`) -- never both, never
/// neither (see `RenderedArtifact::rendered`/`skipped` constructors, the
/// only ways to build one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedArtifact {
    pub target_type: DocTargetType,
    /// A short format tag for the artifact, e.g. `"markdown"`,
    /// `"mediawiki"`, `"pdf"`, `"notion-blocks-json"`, `"obsidian-note"`,
    /// `"blog-markdown"`. Present even when skipped (names what format
    /// *would* have been produced).
    pub format: &'static str,
    pub content: Option<String>,
    pub note: Option<String>,
}

impl RenderedArtifact {
    pub fn rendered(target_type: DocTargetType, format: &'static str, content: String) -> Self {
        Self { target_type, format, content: Some(content), note: None }
    }

    pub fn skipped(target_type: DocTargetType, format: &'static str, note: impl Into<String>) -> Self {
        Self { target_type, format, content: None, note: Some(note.into()) }
    }

    pub fn was_rendered(&self) -> bool {
        self.content.is_some()
    }
}

/// The full result of rendering a project's declared targets: one
/// [`RenderedArtifact`] per declared target, in declaration order. Never
/// drops a declared target silently -- every target in the config produces
/// exactly one entry here, rendered or skipped.
///
/// ## `docs_tree` (DOCGEN-21 wiring fix, S95 REVISION, TERM-334)
/// When the `readme` target is declared AND actually renders, `render_all`
/// ALSO builds the full `docs/` wiki-style tree ([`docs_tree::build_docs_tree`])
/// from the SAME generated `content` -- reusing
/// [`super::readme_layers::render_diataxis_set`]'s artifacts, never
/// regenerating -- and returns it here. This is what makes the concise
/// landing README's nav table / nav-link row (which link to
/// `docs/index.md`, `docs/getting-started.md`, `docs/guides/index.md`,
/// `docs/reference/index.md`, `docs/architecture.md`, `_Sidebar.md`)
/// resolve: before this field existed, `build_docs_tree` had no production
/// caller and those links were dead. Empty when the readme target isn't
/// declared or didn't render -- there is no landing page to link out from
/// in that case. Same write-model inversion as everything else in this
/// module: these are RETURNED files, never placed/written here.
#[derive(Debug, Clone, Default)]
pub struct RenderOutcome {
    pub artifacts: Vec<RenderedArtifact>,
    pub docs_tree: Vec<DocsTreeFile>,
}

impl RenderOutcome {
    pub fn rendered(&self) -> impl Iterator<Item = &RenderedArtifact> {
        self.artifacts.iter().filter(|a| a.was_rendered())
    }

    pub fn skipped(&self) -> impl Iterator<Item = &RenderedArtifact> {
        self.artifacts.iter().filter(|a| !a.was_rendered())
    }
}

/// Render every target `config` declares, given the set of credential KEY
/// NAMES currently available (same shape `docgen_status`/`ProjectDocConfig::resolve`
/// already use -- key NAMES only, never values; this function resolves an
/// enabled target's actual credential VALUE itself, via
/// [`resolve_credential`], only at the point a renderer that needs one is
/// about to run).
///
/// `notion_client_override` / `blog_client_override` let a caller (or a
/// test) inject a validation seam instead of the real HTTP-backed default
/// -- see [`notion::NotionClient`]/[`blog::BlogClient`]. When `None` and
/// the target is enabled, a real client is built from the resolved
/// credential value.
pub async fn render_all(
    ctx: &RenderContext<'_>,
    config: &ProjectDocConfig,
    available_credential_keys: &BTreeSet<String>,
    notion_client_override: Option<&dyn notion::NotionClient>,
    blog_client_override: Option<&dyn blog::BlogClient>,
) -> RenderOutcome {
    let resolved = config.resolve(available_credential_keys);
    let mut artifacts = Vec::with_capacity(resolved.len());

    for target in resolved {
        if !target.enabled {
            let note = target
                .hint
                .clone()
                .unwrap_or_else(|| format!("{} target disabled", target.target_type.as_str()));
            artifacts.push(RenderedArtifact::skipped(
                target.target_type,
                format_tag(target.target_type),
                note,
            ));
            continue;
        }

        let artifact = match target.target_type {
            DocTargetType::Readme => markdown::render(ctx),
            DocTargetType::Wiki => wiki::render(ctx),
            DocTargetType::Pdf => pdf::render(ctx),
            DocTargetType::Obsidian => obsidian::render(ctx),
            DocTargetType::Notion => {
                render_notion_target(ctx, notion_client_override).await
            }
            DocTargetType::Blog => render_blog_target(ctx, blog_client_override).await,
        };
        artifacts.push(artifact);
    }

    // DOCGEN-21 wiring fix: the concise landing README (the `readme` target,
    // rendered above via `readme_layers::render_layered_readme`) links out to
    // a docs/ tree instead of inlining -- so whenever that target actually
    // rendered, also build that tree from the SAME generated `content`, via
    // the SAME Diátaxis artifacts a caller would otherwise have to build a
    // second time. Reuses `render_diataxis_set` (never regenerates), matching
    // `docs_tree`'s own module doc comment ("Source: render_diataxis_set's
    // artifacts, never regenerated here"). No readme target declared/rendered
    // -> no landing page to link out from -> docs_tree stays empty.
    let docs_tree = if artifacts
        .iter()
        .any(|a| a.target_type == DocTargetType::Readme && a.was_rendered())
    {
        let diataxis = render_diataxis_set(ctx, None);
        build_docs_tree(ctx, &diataxis)
    } else {
        Vec::new()
    };

    RenderOutcome { artifacts, docs_tree }
}

fn format_tag(target_type: DocTargetType) -> &'static str {
    match target_type {
        DocTargetType::Readme => "markdown",
        DocTargetType::Wiki => "mediawiki",
        DocTargetType::Pdf => "pdf",
        DocTargetType::Notion => "notion-blocks-json",
        DocTargetType::Obsidian => "obsidian-note",
        DocTargetType::Blog => "blog-markdown",
    }
}

async fn render_notion_target(
    ctx: &RenderContext<'_>,
    override_client: Option<&dyn notion::NotionClient>,
) -> RenderedArtifact {
    match override_client {
        Some(client) => notion::render(ctx, client).await,
        None => match resolve_credential(DocTargetType::Notion.credential_key().unwrap_or_default()) {
            Some(token) if !token.trim().is_empty() => {
                let client = notion::HttpNotionClient::new(token);
                notion::render(ctx, &client).await
            }
            _ => RenderedArtifact::skipped(
                DocTargetType::Notion,
                format_tag(DocTargetType::Notion),
                "notion target enabled by config but its credential value was empty/unset in the \
runtime secret store -- skipping"
                    .to_string(),
            ),
        },
    }
}

async fn render_blog_target(
    ctx: &RenderContext<'_>,
    override_client: Option<&dyn blog::BlogClient>,
) -> RenderedArtifact {
    match override_client {
        Some(client) => blog::render(ctx, client).await,
        None => match resolve_credential(DocTargetType::Blog.credential_key().unwrap_or_default()) {
            Some(token) if !token.trim().is_empty() => {
                let client = blog::HttpBlogClient::new(token);
                blog::render(ctx, &client).await
            }
            _ => RenderedArtifact::skipped(
                DocTargetType::Blog,
                format_tag(DocTargetType::Blog),
                "blog target enabled by config but its credential value was empty/unset in the \
runtime secret store -- skipping"
                    .to_string(),
            ),
        },
    }
}

/// Resolve a named credential's VALUE from the runtime secret store. This
/// crate's SecretManager/vault materializes secrets into the process
/// environment at startup (`crate::secrets_bootstrap`) -- reading by a KEY
/// NAME passed in as a variable (never a literal `"...TOKEN"` string
/// inline at the call site) is the sanctioned resolution path used
/// throughout this module, mirroring `super::config::DocTargetType::credential_key`'s
/// existing structural pattern of naming keys without ever embedding a
/// literal secret. This is the ONLY place in the render module that reads
/// an environment variable.
fn resolve_credential(key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    std::env::var(key).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn ctx<'a>(content: &'a str) -> RenderContext<'a> {
        RenderContext {
            project: "widget-factory",
            module: "src/widget",
            source_commit: "abc123",
            generated_at: "2026-07-11T00:00:00Z",
            content,
        }
    }

    // ── Config-driven: renders exactly the declared targets ────────────

    #[tokio::test]
    async fn renders_each_declared_format_from_generated_content() {
        let raw = json!({
            "targets": [
                {"type": "readme"},
                {"type": "wiki"},
                {"type": "pdf"},
                {"type": "obsidian"},
            ]
        });
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let c = ctx("# Widget\n\nThe widget does A.");
        // `obsidian` is credential-FREE (DGFIX-02, TERM-200): rendering an
        // Obsidian note is pure and needs no token, so it renders here with
        // no credential keys supplied at all -- unlike `notion`/`blog`,
        // which stay genuinely credential-gated (see
        // `missing_notion_credential_skips_notion_others_still_render`).
        let available = BTreeSet::new();
        let out = render_all(&c, &cfg, &available, None, None).await;

        assert_eq!(out.artifacts.len(), 4);
        // readme, wiki, obsidian all render locally with no credential
        // needed; pdf is always unavailable in this sandbox (see pdf.rs).
        assert!(out.artifacts[0].was_rendered(), "readme should render");
        assert!(out.artifacts[1].was_rendered(), "wiki should render");
        assert!(!out.artifacts[2].was_rendered(), "pdf renderer is unavailable");
        assert!(out.artifacts[3].was_rendered(), "obsidian should render unconditionally");
    }

    // ── WRITE-MODEL INVERSION: no placement, ever ───────────────────────

    /// Negative test (spec TEST PLAN / ACCEPTANCE CRITERIA): the render
    /// path returns artifacts and never writes to a repo, the filesystem,
    /// or the vault. Run inside a temp dir standing in for "wherever a
    /// careless implementation might have written a file" -- asserts the
    /// directory is still empty after rendering every format, including
    /// obsidian (which reuses `scribe::vault`'s PURE note-rendering
    /// functions but never its I/O-performing `write_note_and_push`).
    #[tokio::test]
    async fn render_all_never_touches_filesystem_or_vault() {
        let tmp = std::env::temp_dir().join(format!(
            "docgen-render-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let before: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(before.is_empty());

        let raw = json!({
            "targets": [
                {"type": "readme"}, {"type": "wiki"}, {"type": "pdf"},
                {"type": "obsidian"}, {"type": "notion"}, {"type": "blog"}
            ]
        });
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let c = ctx("# Widget\n\nThe widget does A.");
        let mut available = BTreeSet::new();
        available.insert("NOTION_TOKEN".to_string());
        available.insert("DOCGEN_BLOG_API_TOKEN".to_string());

        let notion_mock = notion::tests_support::AlwaysOkNotionClient;
        let blog_mock = blog::tests_support::AlwaysOkBlogClient;
        let out = render_all(
            &c,
            &cfg,
            &available,
            Some(&notion_mock),
            Some(&blog_mock),
        )
        .await;
        assert_eq!(out.artifacts.len(), 6);

        let after: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(after.is_empty(), "render_all must never write files, including into a would-be vault dir");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ── Unavailable target skipped, others render ───────────────────────

    #[tokio::test]
    async fn missing_notion_credential_skips_notion_others_still_render() {
        let raw = json!({"targets": [{"type": "readme"}, {"type": "notion"}]});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let c = ctx("# Widget\n\nThe widget does A.");
        let out = render_all(&c, &cfg, &BTreeSet::new(), None, None).await;

        assert_eq!(out.artifacts.len(), 2);
        assert!(out.artifacts[0].was_rendered(), "readme unaffected by notion's missing credential");
        assert!(!out.artifacts[1].was_rendered());
        assert!(out.artifacts[1].note.as_ref().unwrap().contains("NOTION_TOKEN"));
    }

    #[tokio::test]
    async fn notion_api_validation_failure_skips_notion_others_still_succeed() {
        let raw = json!({"targets": [{"type": "readme"}, {"type": "notion"}]});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let c = ctx("# Widget\n\nThe widget does A.");
        let mut available = BTreeSet::new();
        available.insert("NOTION_TOKEN".to_string());

        let failing = notion::tests_support::AlwaysFailNotionClient;
        let out = render_all(&c, &cfg, &available, Some(&failing), None).await;

        assert_eq!(out.artifacts.len(), 2);
        assert!(out.artifacts[0].was_rendered());
        assert!(!out.artifacts[1].was_rendered());
        assert!(out.artifacts[1].note.as_ref().unwrap().contains("notion"));
    }

    #[tokio::test]
    async fn blog_api_validation_failure_skips_blog_others_still_succeed() {
        let raw = json!({"targets": [{"type": "readme"}, {"type": "blog"}]});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let c = ctx("# Widget\n\nThe widget does A.");
        let mut available = BTreeSet::new();
        available.insert("DOCGEN_BLOG_API_TOKEN".to_string());

        let failing = blog::tests_support::AlwaysFailBlogClient;
        let out = render_all(&c, &cfg, &available, None, Some(&failing)).await;

        assert_eq!(out.artifacts.len(), 2);
        assert!(out.artifacts[0].was_rendered());
        assert!(!out.artifacts[1].was_rendered());
    }

    // ── Secret access hygiene ───────────────────────────────────────────

    #[test]
    fn resolve_credential_returns_none_for_empty_key() {
        assert_eq!(resolve_credential(""), None);
    }

    // ── DOCGEN-21 wiring fix (TERM-334): docs/ tree is emitted alongside ─
    // ── the landing README, and its nav links all resolve ───────────────

    const SAMPLE_DIATAXIS_CONTENT: &str = "# Widget\n\n\
A widget factory that builds widgets fast and safely.\n\n\
## Tutorial\n\nFollow along to build your first widget from scratch.\n\n\
## How-To\n\nTo reconfigure the widget, edit its config file.\n\n\
## Reference\n\n`widget build [--flag]` -- builds a widget.\n\n\
## Explanation\n\nThe widget pipeline exists because raw material varies.\n";

    /// The bug this wiring fix closes: before it, `build_docs_tree` had NO
    /// production caller, so `render_all` (and `run_docgen_trigger` above
    /// it) never emitted the `docs/` tree the concise landing README's nav
    /// table and nav-link row point at -- every one of those links was
    /// dead. This asserts the full production path (`render_all` with a
    /// `readme`-declaring config) now emits every page the module doc
    /// comment promises.
    #[tokio::test]
    async fn render_all_emits_the_docs_tree_alongside_the_readme() {
        let raw = json!({"targets": [{"type": "readme"}]});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let c = ctx(SAMPLE_DIATAXIS_CONTENT);
        let out = render_all(&c, &cfg, &BTreeSet::new(), None, None).await;

        assert!(out.artifacts[0].was_rendered(), "readme target must render");

        let paths: Vec<&str> = out.docs_tree.iter().map(|f| f.path.as_str()).collect();
        for expected in [
            "docs/index.md",
            "docs/getting-started.md",
            "docs/guides/index.md",
            "docs/reference/index.md",
            "docs/architecture.md",
            "_Sidebar.md",
        ] {
            assert!(paths.contains(&expected), "docs_tree missing {expected}: {paths:?}");
        }
    }

    /// No `readme` target declared/rendered -> no docs_tree. There is no
    /// landing page to link out from, so nothing should be emitted.
    #[tokio::test]
    async fn render_all_omits_docs_tree_when_readme_target_not_declared() {
        let raw = json!({"targets": [{"type": "wiki"}]});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let c = ctx(SAMPLE_DIATAXIS_CONTENT);
        let out = render_all(&c, &cfg, &BTreeSet::new(), None, None).await;

        assert!(out.docs_tree.is_empty());
    }

    /// End-to-end: every link in the landing README's `## Documentation`
    /// nav table and its nav-link row resolves to a path `render_all`
    /// actually emitted in `docs_tree` (or is a known top-level file the
    /// docs engine deliberately does not generate, e.g. CHANGELOG.md/
    /// LICENSE -- those are repo-level files outside this engine's scope).
    /// No dead links.
    #[tokio::test]
    async fn landing_readme_nav_links_all_resolve_to_emitted_docs_tree_files() {
        let raw = json!({"targets": [{"type": "readme"}]});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let c = ctx(SAMPLE_DIATAXIS_CONTENT);
        let out = render_all(&c, &cfg, &BTreeSet::new(), None, None).await;

        let readme_content = out.artifacts[0].content.as_ref().expect("readme rendered");
        let emitted: std::collections::BTreeSet<&str> =
            out.docs_tree.iter().map(|f| f.path.as_str()).collect();

        // Repo-level files the doc engine intentionally does not generate
        // (CHANGELOG.md is versioned separately by DOCGEN-07/changelog.rs;
        // LICENSE is a legal file, not a doc-engine artifact).
        let out_of_scope = ["CHANGELOG.md", "LICENSE"];

        // Every `docs/...`-relative link target referenced by the landing
        // (nav-link row + `## Documentation` table) must be an emitted file.
        let mut checked_any = false;
        for line in readme_content.lines() {
            for cap in extract_markdown_link_targets(line) {
                if out_of_scope.contains(&cap) {
                    continue;
                }
                if cap.starts_with("docs/") || cap == "_Sidebar.md" {
                    checked_any = true;
                    assert!(
                        emitted.contains(cap),
                        "landing README links to {cap}, which render_all never emitted \
(dead link) -- docs_tree = {emitted:?}"
                    );
                }
            }
        }
        assert!(checked_any, "expected the landing README to contain at least one docs/ link to check");
    }

    /// Tiny markdown `[text](target)` link extractor for the dead-link test
    /// above -- no crate dependency needed for this narrow, test-only use.
    fn extract_markdown_link_targets(line: &str) -> Vec<&str> {
        let mut targets = Vec::new();
        let mut rest = line;
        while let Some(open_paren) = rest.find("](") {
            let after = &rest[open_paren + 2..];
            if let Some(close_paren) = after.find(')') {
                targets.push(&after[..close_paren]);
                rest = &after[close_paren + 1..];
            } else {
                break;
            }
        }
        targets
    }
}
