//! Obsidian note renderer (DOCGEN-06).
//!
//! REUSE, not reimplementation: like [`super::markdown`], this renderer is
//! built entirely from `crate::scribe::vault`'s pure primitives
//! (`render_note`, `NoteFrontmatter`, `build_wikilink`, `slugify`,
//! `note_path`) -- per the RECONCILIATION CONSTRAINTS. The only difference
//! from the README renderer is the note type and that the artifact also
//! carries the vault-relative path the note *would* land at
//! ([`note_path`]) as metadata for the calling harness -- computing that
//! path is pure (no filesystem access), so surfacing it here is not a
//! placement. This renderer never calls `write_note_and_push` or any other
//! I/O-performing vault function.

use std::path::Path;

use crate::scribe::vault::{note_path, render_note, NoteFrontmatter, NoteType};

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
    // A self-referential related link back to the project's module page, so
    // the note carries at least one real wikilink where applicable --
    // mirrors the convention `crate::scribe::vault::render_note`'s own doc
    // comment describes.
    let related = vec![ctx.project.to_string()];
    let body = render_note(&fm, ctx.content, &related);

    // Computing the intended vault path is pure -- no filesystem touched --
    // so it's safe to include as informational metadata for the harness
    // that will actually place the note.
    let intended_path = note_path(Path::new("."), NoteType::Readme, ctx.project, ctx.module);
    let content = format!(
        "{body}\n<!-- intended vault path (informational only; NOT written by this renderer): {} -->\n",
        intended_path.display()
    );

    RenderedArtifact::rendered(DocTargetType::Obsidian, "obsidian-note", content)
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
    fn renders_valid_obsidian_note_with_wikilink() {
        let artifact = render(&ctx("# Widget\n\nThe widget does A."));
        assert!(artifact.was_rendered());
        let content = artifact.content.unwrap();
        assert!(content.contains("[[widget-factory]]"), "expected a real wikilink: {content}");
        assert!(content.contains("The widget does A."));
    }

    #[test]
    fn intended_path_is_informational_only_not_a_write() {
        let artifact = render(&ctx("body"));
        let content = artifact.content.unwrap();
        assert!(content.contains("intended vault path"));
        assert!(content.contains("NOT written by this renderer"));
    }
}
