//! README (markdown) renderer (DOCGEN-06).
//!
//! REUSE, not reimplementation: this renderer builds its output entirely
//! from `crate::scribe::vault`'s existing pure note-rendering primitives
//! (`render_note`, `NoteFrontmatter`) -- per the RECONCILIATION
//! CONSTRAINTS, markdown/Obsidian rendering is never duplicated here. Only
//! the PURE (no-I/O) half of `scribe::vault` is used; `write_note_and_push`
//! is never called (see `render/mod.rs`'s write-model-inversion doc
//! comment).
//!
//! ## DOCGEN-13: layered output (progressive disclosure)
//! As of DOCGEN-13 (S95, Plane TERM-164), the README target no longer emits
//! a single flat body -- it delegates to
//! [`crate::tools::docgen::readme_layers::render_layered_readme`], which
//! splits the generated content into hero/quickstart/deep-dive layers,
//! follows the standard-readme section order, adds shields.io badges and a
//! DOCGEN-11 architecture-SVG placeholder slot, and still goes through this
//! same `render_note`/`NoteFrontmatter` pure primitive for its frontmatter
//! block. `render()` here has no access to a project's PRIOR rendered
//! README (this function's signature, shared with every other renderer in
//! this module via `render_all`, carries only the current `RenderContext`),
//! so it always renders as a project's first layered README
//! (`existing_readme: None`); a caller that wants the per-layer
//! deepen-not-regenerate merge against a real prior README calls
//! [`crate::tools::docgen::readme_layers::render_layered_readme`] directly
//! with that prior content, exactly as `render_diataxis_set` is called
//! directly for the parallel Diátaxis split (this module intentionally
//! does not also try to surface four more `RenderedArtifact`s here --
//! `crate::tools::docgen::config::DocTargetType` has no Diátaxis-mode
//! variants, and adding them is out of this item's scope; see
//! `readme_layers`'s own module doc comment).

use super::{RenderContext, RenderedArtifact};
use crate::tools::docgen::readme_layers::render_layered_readme;

pub fn render(ctx: &RenderContext<'_>) -> RenderedArtifact {
    render_layered_readme(ctx, None)
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

    #[test]
    fn renders_valid_markdown_with_frontmatter_and_body() {
        let artifact = render(&ctx("# Widget\n\nThe widget does A."));
        assert!(artifact.was_rendered());
        let content = artifact.content.unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("type: readme"));
        assert!(content.contains("The widget does A."));
    }

    /// DOCGEN-13: the README target now emits the layered structure
    /// (`readme_layers::render_layered_readme`), so its format tag reflects
    /// that -- see `readme_layers.rs` for the layer-splitting/standard-
    /// readme-ordering tests themselves.
    #[test]
    fn format_tag_is_markdown_layered() {
        let artifact = render(&ctx("body"));
        assert_eq!(artifact.format, "markdown-layered");
    }
}
