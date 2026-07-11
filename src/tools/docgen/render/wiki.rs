//! Wiki-markup renderer (DOCGEN-06, ADDED -- not a `scribe::vault` reuse).
//!
//! Produces a generic wiki-page rendering of the generated content: a page
//! title header plus the body, with any existing Markdown ATX headings
//! (`# `/`## `/...) converted to MediaWiki-style `=`-delimited headings, the
//! most broadly compatible wiki markup dialect (Gitea/Forgejo wikis, plain
//! MediaWiki, and most self-hosted wiki engines all render `== Heading ==`
//! correctly, whereas plenty of wiki engines do NOT understand ATX `#`).
//! Also reuses [`crate::scribe::vault::build_wikilink`] for a trailing
//! "See also" section back to the owning project, matching the wikilink
//! convention `crate::scribe::vault` already established -- consistent
//! cross-referencing style across every note-shaped target this engine
//! produces.

use crate::scribe::vault::build_wikilink;

use super::{RenderContext, RenderedArtifact};
use crate::tools::docgen::config::DocTargetType;

/// Convert Markdown ATX headings (`#`..`######`) to MediaWiki `=`-delimited
/// headings. Non-heading lines pass through unchanged.
fn markdown_headings_to_wiki(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim_start();
        let level = trimmed.chars().take_while(|&c| c == '#').count();
        if level > 0 && trimmed.as_bytes().get(level) == Some(&b' ') {
            let level = level.min(6);
            let text = trimmed[level..].trim();
            let eq = "=".repeat(level);
            out.push_str(&format!("{eq} {text} {eq}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

pub fn render(ctx: &RenderContext<'_>) -> RenderedArtifact {
    let mut out = String::new();
    out.push_str(&format!("== {} ==\n\n", ctx.module));
    out.push_str(&markdown_headings_to_wiki(ctx.content));
    out.push_str("\n== See also ==\n\n");
    out.push_str(&format!("* {}\n", build_wikilink(ctx.project)));
    out.push_str(&format!(
        "\n<!-- generated {} from {} -->\n",
        ctx.generated_at, ctx.source_commit
    ));

    RenderedArtifact::rendered(DocTargetType::Wiki, "mediawiki", out)
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
    fn renders_valid_wiki_markup_with_converted_headings() {
        let artifact = render(&ctx("# Widget\n\nThe widget does A.\n\n## Usage\n\nCall it."));
        assert!(artifact.was_rendered());
        let content = artifact.content.unwrap();
        assert!(content.contains("= Widget ="));
        assert!(content.contains("== Usage =="));
        assert!(content.contains("The widget does A."));
    }

    #[test]
    fn includes_see_also_wikilink_to_project() {
        let artifact = render(&ctx("body"));
        let content = artifact.content.unwrap();
        assert!(content.contains("[[widget-factory]]"));
    }

    #[test]
    fn non_heading_hash_lines_pass_through_unchanged() {
        // e.g. a shell comment or a literal "#" not followed by a space
        // must not be mistaken for a heading.
        let artifact = render(&ctx("#!/bin/sh\necho hi"));
        let content = artifact.content.unwrap();
        assert!(content.contains("#!/bin/sh"));
    }
}
