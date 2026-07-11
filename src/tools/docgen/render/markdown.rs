//! README (markdown) renderer (DOCGEN-06).
//!
//! REUSE, not reimplementation: this renderer builds its output entirely
//! from `crate::scribe::vault`'s existing pure note-rendering primitives
//! (`render_note`, `NoteFrontmatter`) -- per the RECONCILIATION
//! CONSTRAINTS, markdown/Obsidian rendering is never duplicated here. Only
//! the PURE (no-I/O) half of `scribe::vault` is used; `write_note_and_push`
//! is never called (see `render/mod.rs`'s write-model-inversion doc
//! comment).

use crate::scribe::vault::{render_note, NoteFrontmatter, NoteType};

use super::{RenderContext, RenderedArtifact};
use crate::tools::docgen::config::DocTargetType;

pub fn render(ctx: &RenderContext<'_>) -> RenderedArtifact {
    let fm = NoteFrontmatter {
        title: ctx.module.to_string(),
        module: ctx.project.to_string(),
        generated_at: ctx.generated_at.to_string(),
        source_commit: ctx.source_commit.to_string(),
        note_type: NoteType::Readme,
    };
    let content = render_note(&fm, ctx.content, &[]);
    RenderedArtifact::rendered(DocTargetType::Readme, "markdown", content)
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

    #[test]
    fn format_tag_is_markdown() {
        let artifact = render(&ctx("body"));
        assert_eq!(artifact.format, "markdown");
    }
}
