//! PDF renderer (DOCGEN-06).
//!
//! No PDF-generation crate is present anywhere in this workspace's
//! `Cargo.toml` (checked before writing this file), and the RECONCILIATION
//! CONSTRAINTS explicitly forbid adding a heavyweight dependency that might
//! not build in this sandbox. Per the spec's own guidance for exactly this
//! situation ("if no PDF lib is available in this sandbox, structure the
//! PDF renderer to return a clear 'renderer unavailable' skip like the
//! unavailable-target path, and unit-test that path"), this renderer
//! ALWAYS returns [`RenderedArtifact::skipped`] with a clear note --
//! mirroring the same shape a missing-credential target produces in
//! `render/mod.rs`, so callers handle both uniformly.
//!
//! ## Pagination (EDGE CASE: "PDF of very long content -> paginates,
//! doesn't truncate")
//! The pagination LOGIC this renderer would use once a real PDF backend is
//! wired in is implemented and unit-tested here now ([`paginate`]) rather
//! than deferred wholesale -- it never truncates content, only splits it
//! into page-sized chunks on line boundaries. `render()` reports the page
//! count this content *would* need in its skip note, so the caller can see
//! pagination was considered, not merely hand-waved.

use super::{RenderContext, RenderedArtifact};
use crate::tools::docgen::config::DocTargetType;

/// Rough per-page character budget for the eventual PDF backend. Kept as a
/// named constant so `paginate`'s test coverage documents intent, not a
/// magic number.
const CHARS_PER_PAGE: usize = 3000;

/// Split `content` into page-sized chunks, breaking only on line
/// boundaries (never mid-line, so a line is never truncated) and never
/// dropping content -- concatenating every returned page's lines
/// reproduces `content` exactly. Returns at least one page, even for empty
/// content (a single empty page), so callers never need to special-case
/// "zero pages."
fn paginate(content: &str, chars_per_page: usize) -> Vec<String> {
    let mut pages = Vec::new();
    let mut current = String::new();

    for line in content.lines() {
        if !current.is_empty() && current.len() + line.len() + 1 > chars_per_page {
            pages.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    pages.push(current);
    pages
}

/// This renderer's availability. Always `false` in this build (no PDF crate
/// in the tree) -- a real backend would replace this with an actual
/// capability check, exactly like [`super::notion::NotionClient`]/
/// [`super::blog::BlogClient`]'s validation seam for their targets.
fn is_available() -> bool {
    false
}

pub fn render(ctx: &RenderContext<'_>) -> RenderedArtifact {
    if !is_available() {
        let pages = paginate(ctx.content, CHARS_PER_PAGE);
        return RenderedArtifact::skipped(
            DocTargetType::Pdf,
            "pdf",
            format!(
                "pdf renderer unavailable in this build (no PDF-generation crate in the \
workspace) -- skipping. Pagination is implemented and would produce {} page(s) at ~{CHARS_PER_PAGE} \
chars/page for this content, without truncation, once a real PDF backend is added.",
                pages.len()
            ),
        );
    }

    // Unreachable while `is_available()` is hardcoded `false`; kept so a
    // future real backend has an obvious insertion point without
    // restructuring this function's shape or its callers in `render/mod.rs`.
    RenderedArtifact::skipped(DocTargetType::Pdf, "pdf", "pdf renderer not implemented".to_string())
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

    /// Negative test: PDF is always skipped (no renderer in this sandbox),
    /// with a clear note -- mirrors the missing-credential skip shape.
    #[test]
    fn pdf_target_is_skipped_with_clear_note() {
        let artifact = render(&ctx("# Widget\n\nBody."));
        assert!(!artifact.was_rendered());
        let note = artifact.note.unwrap();
        assert!(note.contains("unavailable"));
    }

    #[test]
    fn paginate_never_truncates_and_reconstructs_exactly() {
        let content = (0..500)
            .map(|i| format!("line {i} of some reasonably long content"))
            .collect::<Vec<_>>()
            .join("\n");
        let pages = paginate(&content, 200);
        assert!(pages.len() > 1, "long content should span multiple pages");
        let reconstructed = pages.join("\n");
        assert_eq!(reconstructed, content, "pagination must never lose or truncate content");
    }

    #[test]
    fn paginate_empty_content_returns_single_empty_page() {
        let pages = paginate("", CHARS_PER_PAGE);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0], "");
    }

    #[test]
    fn paginate_never_splits_within_a_line() {
        let long_line = "x".repeat(50);
        let content = format!("{long_line}\n{long_line}\n{long_line}");
        let pages = paginate(&content, 60);
        for page in &pages {
            for line in page.lines() {
                assert_eq!(line.len(), 50, "no line should be split across a page boundary");
            }
        }
    }
}
