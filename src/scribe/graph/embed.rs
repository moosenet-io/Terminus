//! Atlas doc-embedding (KGRAPH-09): fold a project's rendered map into the docs
//! Scribe generates, so the graph informs the visual doc output (the bonus).
//!
//! `embed_map_section` appends an "## Architecture map" section — the SVG plus
//! the confidence legend — to a generated README/wiki body when a graph SVG
//! exists for the project. It is purely additive: with no `project_id`, no
//! stored SVG, or embedding disabled, the document is returned byte-for-byte
//! unchanged (so a project without a graph gets exactly today's output). Reads
//! only the already-rendered SVG file under the KG store; no networking.

use std::fs;
use std::path::Path;

use crate::scribe::vault::slugify;
use crate::scribe::ScribeConfig;

/// Append the architecture-map section for `project_id` to `doc`, if a rendered
/// `<slug>.svg` exists under `cfg.kg_store_dir`. `inline`=true embeds the SVG
/// markup directly (self-contained — for the Obsidian vault note); otherwise a
/// relative image reference is emitted (for a repo README that ships the SVG
/// alongside it). Returns `doc` unchanged when there is nothing to embed.
pub fn embed_map_section(doc: String, project_id: Option<&str>, cfg: &ScribeConfig, inline: bool) -> String {
    let Some(pid) = project_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return doc;
    };
    let slug = slugify(pid);
    let svg_path = Path::new(&cfg.kg_store_dir).join(format!("{slug}.svg"));
    if !svg_path.exists() {
        return doc;
    }

    let mut out = doc;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n## Architecture map\n\n");
    if inline {
        match fs::read_to_string(&svg_path) {
            Ok(svg) => {
                out.push_str(svg.trim_end());
                out.push('\n');
            }
            // Unreadable → fall back to a reference rather than losing the section.
            Err(_) => out.push_str(&format!("![Atlas knowledge graph]({slug}.svg)\n")),
        }
    } else {
        out.push_str(&format!("![Atlas knowledge graph]({slug}.svg)\n"));
    }
    out.push_str(
        "\n_Edge confidence: **solid** = extracted, **dashed** = inferred, **dotted** = ambiguous. \
Node color = module cluster; node size = degree._\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TmpStore {
        dir: std::path::PathBuf,
    }
    impl Drop for TmpStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
    fn store_with_svg(tag: &str, slug: &str, svg: &str) -> (ScribeConfig, TmpStore) {
        let dir = std::env::temp_dir().join(format!("atlas-embed-{}-{}", tag, std::process::id()));
        let _ = fs::create_dir_all(&dir);
        if !svg.is_empty() {
            fs::write(dir.join(format!("{slug}.svg")), svg).unwrap();
        }
        let mut cfg = ScribeConfig::default();
        cfg.kg_store_dir = dir.to_string_lossy().into_owned();
        (cfg, TmpStore { dir })
    }

    #[test]
    fn appends_section_when_svg_exists() {
        let (cfg, _s) = store_with_svg("has", "term", "<svg>…</svg>");
        let out = embed_map_section("# Readme\n\nBody.".to_string(), Some("TERM"), &cfg, false);
        assert!(out.contains("## Architecture map"), "section appended");
        assert!(out.contains("![Atlas knowledge graph](term.svg)"), "relative image ref");
        assert!(out.contains("solid** = extracted"), "legend present");
        assert!(out.starts_with("# Readme\n\nBody."), "original body preserved");
    }

    #[test]
    fn inline_embeds_svg_markup() {
        let (cfg, _s) = store_with_svg("inl", "term", "<svg id=\"m\">X</svg>");
        let out = embed_map_section("body".to_string(), Some("TERM"), &cfg, true);
        assert!(out.contains("<svg id=\"m\">X</svg>"), "svg inlined for the vault");
    }

    #[test]
    fn unchanged_when_no_project_id() {
        let (cfg, _s) = store_with_svg("nop", "term", "<svg/>");
        let doc = "unchanged".to_string();
        assert_eq!(embed_map_section(doc.clone(), None, &cfg, false), doc);
        assert_eq!(embed_map_section(doc.clone(), Some("   "), &cfg, false), doc);
    }

    #[test]
    fn unchanged_when_no_svg_for_project() {
        let (cfg, _s) = store_with_svg("missing", "term", ""); // no svg written
        let doc = "# Readme\nBody.".to_string();
        assert_eq!(embed_map_section(doc.clone(), Some("TERM"), &cfg, false), doc, "byte-identical when no graph");
    }
}
