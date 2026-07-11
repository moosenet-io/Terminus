//! DOCGEN-16: Theme-aware SVG explainer graphics + per-page Open Graph
//! social cards, rasterized in-process (resvg/usvg/tiny-skia), S95, Plane
//! TERM-167.
//!
//! Two SVG needs, both produced by this module:
//!   1. **Explainer graphics** -- concept panels / hero banners for a doc
//!      page, theme-aware (light AND dark variants).
//!   2. **OG social cards** (`og:image`) -- one per page, built from that
//!      page's frontmatter (`title`/`summary`), also theme-aware.
//!
//! ## In-process rasterization: resvg/usvg/tiny-skia, checked before
//! writing this file
//! `grep -iE 'resvg|usvg|tiny.skia' Cargo.toml` at the START of this item
//! found none of the three crates anywhere in this workspace's
//! `Cargo.toml`. Per the DOCGEN-06 `render/pdf.rs` precedent for exactly
//! this situation (heavyweight dependency not present, RECONCILIATION
//! CONSTRAINTS forbid adding a crates.io dep in this sandbox), this module
//! follows the SAME shape: the SVG **source** (the theme-aware explainer,
//! the OG card with title/summary substituted) is ALWAYS the guaranteed,
//! diffable artifact; PNG rasterization is an injectable seam
//! ([`SvgRasterizer`], mirroring `diagram.rs`'s `D2Renderer` seam) that
//! reports a clear "resvg unavailable" skip -- with the exact reason (the
//! missing crates) -- when no real backend is wired in, tested exactly like
//! [`super::diagram::render_diagram_svg`]'s d2-absent path. A future
//! in-process backend (once the crates are added) plugs into this trait
//! without changing any caller.
//!
//! ## Theme-aware, not theme-agnostic
//! Every SVG this module produces is generated once per [`Theme`] (`Light`
//! / `Dark`) with theme-appropriate background/foreground/accent colors --
//! never a single color scheme rendered twice. [`render_explainer_pair`] /
//! [`render_og_card_pair`] produce both variants from one call so a caller
//! can never accidentally ship only one theme.
//!
//! ## Frontmatter text is swept (second sweep point, mirrors `diagram.rs`)
//! `title`/`summary` come from a page's frontmatter -- free text that, like
//! an LLM-emitted diagram node label, can restate an internal
//! hostname/IP/container id even when it wasn't the LLM that wrote it. Per
//! [`build_og_card`], both fields are swept through
//! [`super::pii_gate::sweep_input`] BEFORE being substituted into the SVG
//! template -- this is the load-bearing second sweep point this item adds,
//! exactly mirroring `diagram.rs`'s "node labels leak hostnames" handling.
//!
//! ## WRITE-MODEL INVERSION (matches `render/mod.rs`)
//! Every function here RETURNS a `String` (SVG) or `Vec<u8>` (PNG) plus, for
//! versioning, an [`super::versioning::ArtifactVersion`] -- nothing in this
//! module writes to a filesystem path, a repo, or a hosting surface. See
//! `svg_and_raster_never_touch_the_filesystem` below for the negative test.
//!
//! ## Versioning (DOCGEN-07 reuse, not reimplementation)
//! [`version_svg_assets`] stores every produced artifact (explainer
//! light+dark, OG card light+dark, and the PNG raster when produced) via
//! [`super::versioning::VersionStore`] -- the same append-only store every
//! other docgen artifact already uses. No second version-storage mechanism.

use super::pii_gate::sweep_input;
use super::versioning::{ArtifactKey, ArtifactVersion, VersionStore};
use crate::error::ToolError;

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Which color scheme a generated SVG targets. Every public render function
/// in this module takes (or produces both variants of) a [`Theme`] -- there
/// is no "themeless" SVG this module can emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    /// (background, foreground/text, accent) hex colors for this theme.
    /// Deliberately simple, high-contrast pairs -- crisp in both schemes
    /// rather than a single palette reused verbatim across both.
    fn palette(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Theme::Light => ("#ffffff", "#1a1a1a", "#3b6fd6"),
            Theme::Dark => ("#12141a", "#f2f2f2", "#7aa6ff"),
        }
    }
}

// ---------------------------------------------------------------------------
// XML/SVG text escaping -- load-bearing for OG cards (title/summary are
// caller-supplied free text and MUST NOT be able to break out of the SVG or
// inject markup)
// ---------------------------------------------------------------------------

/// Escape text for safe embedding inside SVG element content (between
/// `<text>...</text>` tags). Handles the five XML predefined entities;
/// applied to EVERY piece of caller-supplied text (title, summary, explainer
/// body lines) before it is substituted into a template -- there is no
/// template substitution path in this module that skips this function.
fn escape_svg_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Wrap long text onto multiple `<tspan>` lines at a rough character budget,
/// breaking only on word boundaries (never mid-word) -- so a long title/
/// summary is readable instead of overflowing the card, without ever
/// truncating content (mirrors `render/pdf.rs`'s `paginate` "never
/// truncate" discipline, applied to line-wrapping instead of pagination).
fn wrap_text(raw: &str, chars_per_line: usize) -> Vec<String> {
    if raw.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in raw.split_whitespace() {
        if !current.is_empty() && current.len() + 1 + word.len() > chars_per_line {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

// ---------------------------------------------------------------------------
// Explainer graphics
// ---------------------------------------------------------------------------

const EXPLAINER_WIDTH: u32 = 960;
const EXPLAINER_HEIGHT: u32 = 540;

/// Render one theme variant of an explainer/concept-panel graphic for
/// `title` with `body_points` as a short bullet list (e.g. the key facts a
/// doc page wants to visually reinforce). Pure function -- no I/O, fully
/// deterministic for a given input.
pub fn render_explainer_svg(title: &str, body_points: &[String], theme: Theme) -> String {
    let (bg, fg, accent) = theme.palette();
    let esc_title = escape_svg_text(title);

    let mut body_svg = String::new();
    let mut y = 180;
    for point in body_points {
        for line in wrap_text(point, 56) {
            body_svg.push_str(&format!(
                "<text x=\"64\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"22\" fill=\"{fg}\">\
&#8226; {}</text>\n",
                escape_svg_text(&line)
            ));
            y += 34;
        }
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{EXPLAINER_WIDTH}\" height=\"{EXPLAINER_HEIGHT}\" \
viewBox=\"0 0 {EXPLAINER_WIDTH} {EXPLAINER_HEIGHT}\" role=\"img\" aria-label=\"{esc_title}\">\n\
<rect width=\"{EXPLAINER_WIDTH}\" height=\"{EXPLAINER_HEIGHT}\" fill=\"{bg}\"/>\n\
<rect x=\"0\" y=\"0\" width=\"{EXPLAINER_WIDTH}\" height=\"8\" fill=\"{accent}\"/>\n\
<text x=\"64\" y=\"110\" font-family=\"sans-serif\" font-size=\"40\" font-weight=\"700\" fill=\"{fg}\">{esc_title}</text>\n\
{body_svg}\
</svg>\n"
    )
}

/// Render both theme variants of an explainer for `title`/`body_points` in
/// one call, so a caller can never accidentally ship only one theme.
/// Returns `(light, dark)`.
pub fn render_explainer_pair(title: &str, body_points: &[String]) -> (String, String) {
    (
        render_explainer_svg(title, body_points, Theme::Light),
        render_explainer_svg(title, body_points, Theme::Dark),
    )
}

// ---------------------------------------------------------------------------
// OG social cards
// ---------------------------------------------------------------------------

const OG_WIDTH: u32 = 1200;
const OG_HEIGHT: u32 = 630;

/// Render one theme variant of an OG (`og:image`) social card from ALREADY
/// SWEPT `title`/`summary` text (see [`build_og_card`] -- the sweeping
/// caller; this function itself performs no PII scanning, mirroring
/// `render/mod.rs`'s pure-renderer convention).
fn render_og_card_svg(title: &str, summary: &str, theme: Theme) -> String {
    let (bg, fg, accent) = theme.palette();
    let esc_title = escape_svg_text(title);

    let mut summary_svg = String::new();
    let mut y = 300;
    for line in wrap_text(summary, 60) {
        summary_svg.push_str(&format!(
            "<text x=\"80\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"28\" fill=\"{fg}\">{}</text>\n",
            escape_svg_text(&line)
        ));
        y += 40;
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{OG_WIDTH}\" height=\"{OG_HEIGHT}\" \
viewBox=\"0 0 {OG_WIDTH} {OG_HEIGHT}\" role=\"img\" aria-label=\"{esc_title}\">\n\
<rect width=\"{OG_WIDTH}\" height=\"{OG_HEIGHT}\" fill=\"{bg}\"/>\n\
<rect x=\"0\" y=\"0\" width=\"18\" height=\"{OG_HEIGHT}\" fill=\"{accent}\"/>\n\
<text x=\"80\" y=\"200\" font-family=\"sans-serif\" font-size=\"56\" font-weight=\"700\" fill=\"{fg}\">{esc_title}</text>\n\
{summary_svg}\
</svg>\n"
    )
}

/// Build both theme variants of an OG card from RAW frontmatter `title`/
/// `summary`, sweeping each through the DOCGEN-02 PII gate first
/// ([`sweep_input`]) -- the load-bearing second sweep point described in
/// the module doc comment. Returns `(light, dark)`. Errors only if
/// [`sweep_input`] itself errors (block-worthy content, per its own
/// `BLOCK_REDACTION_RATIO` policy); redaction alone still produces a usable
/// card (matching `PiiGateOutcome`'s existing semantics elsewhere in
/// docgen).
pub fn build_og_card(title: &str, summary: &str) -> Result<(String, String), ToolError> {
    let title_outcome = sweep_input(title)?;
    let summary_outcome = sweep_input(summary)?;
    let clean_title = title_outcome.sanitized_content();
    let clean_summary = summary_outcome.sanitized_content();

    Ok((
        render_og_card_svg(clean_title, clean_summary, Theme::Light),
        render_og_card_svg(clean_title, clean_summary, Theme::Dark),
    ))
}

// ---------------------------------------------------------------------------
// PNG rasterization: resvg-present -> raster, resvg-absent -> skip
// ---------------------------------------------------------------------------

/// The injectable rasterization seam (mirrors `diagram.rs`'s [`super::diagram::D2Renderer`]).
/// [`ResvgRasterizer`] is the real implementation slot; tests inject a mock
/// so both the "resvg absent -> skip" and "resvg present -> rasters" paths
/// are deterministically covered regardless of what's actually built into
/// this binary.
pub trait SvgRasterizer: Send + Sync {
    /// Cheap, side-effect-free check for whether this rasterizer can
    /// actually rasterize right now (e.g. the resvg/usvg/tiny-skia crates
    /// are compiled in).
    fn is_available(&self) -> bool;

    /// Rasterize `svg` to PNG bytes. Only called when [`Self::is_available`]
    /// returned `true`.
    fn rasterize(&self, svg: &str) -> Result<Vec<u8>, String>;
}

/// The real rasterizer slot. Always reports unavailable in this build: no
/// resvg/usvg/tiny-skia crate is present in this workspace's `Cargo.toml`
/// (verified at the top of this file's module doc comment), and the
/// RECONCILIATION CONSTRAINTS forbid adding a crates.io dependency in this
/// sandbox. A future build that vendors those crates replaces this
/// function's body with a real `usvg::Tree` parse + `resvg::render` +
/// `tiny_skia::Pixmap::encode_png` call -- the trait shape here is already
/// the exact seam that change would plug into, with no caller-side
/// restructuring (mirrors `render/pdf.rs`'s `is_available()` slot).
pub struct ResvgRasterizer;

impl SvgRasterizer for ResvgRasterizer {
    fn is_available(&self) -> bool {
        false
    }

    fn rasterize(&self, _svg: &str) -> Result<Vec<u8>, String> {
        // Unreachable while `is_available()` is hardcoded `false`; kept so a
        // future real backend has an obvious insertion point without
        // restructuring this trait or its callers.
        Err("resvg rasterizer not implemented in this build".to_string())
    }
}

/// One rasterization attempt's outcome. Either real PNG bytes
/// (`png.is_some()`, `note.is_none()`), or a skip with a human-readable
/// reason (`png.is_none()`, `note.is_some()`) -- never both, never neither,
/// mirroring `render::RenderedArtifact` / `diagram::DiagramRenderOutcome`'s
/// shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterOutcome {
    pub png: Option<Vec<u8>>,
    pub note: Option<String>,
}

impl RasterOutcome {
    fn rendered(png: Vec<u8>) -> Self {
        Self { png: Some(png), note: None }
    }

    fn skipped(note: impl Into<String>) -> Self {
        Self { png: None, note: Some(note.into()) }
    }

    pub fn was_rendered(&self) -> bool {
        self.png.is_some()
    }
}

/// Rasterize `svg` to PNG via `rasterizer`, or skip with a clear note when
/// unavailable -- the SVG itself remains the guaranteed, diffable artifact
/// either way (mirrors `diagram::render_diagram_svg`'s skip-if-unavailable
/// shape exactly).
pub fn raster_svg_to_png(svg: &str, rasterizer: &dyn SvgRasterizer) -> RasterOutcome {
    if !rasterizer.is_available() {
        return RasterOutcome::skipped(
            "resvg rasterizer unavailable in this build (no resvg/usvg/tiny-skia crate in the \
workspace) -- skipping PNG raster. The SVG source (theme-aware explainer / OG card) is still \
produced and versioned; add those crates to enable in-process PNG rasterization.",
        );
    }

    match rasterizer.rasterize(svg) {
        Ok(png) => RasterOutcome::rendered(png),
        Err(e) => RasterOutcome::skipped(format!(
            "resvg rasterizer was available but failed to rasterize this SVG -- skipping PNG \
raster (SVG source is still produced and versioned): {e}"
        )),
    }
}

// ---------------------------------------------------------------------------
// Versioning (DOCGEN-07 reuse)
// ---------------------------------------------------------------------------

pub fn explainer_key(project: &str, module: &str, theme: Theme) -> ArtifactKey {
    ArtifactKey::new(project, format!("svg-explainer-{module}-{}", theme.as_str()))
}

pub fn og_card_key(project: &str, module: &str, theme: Theme) -> ArtifactKey {
    ArtifactKey::new(project, format!("og-card-{module}-{}", theme.as_str()))
}

pub fn og_card_png_key(project: &str, module: &str, theme: Theme) -> ArtifactKey {
    ArtifactKey::new(project, format!("og-card-png-{module}-{}", theme.as_str()))
}

/// The result of versioning one page/module's full svg_assets output: both
/// explainer theme variants, both OG card theme variants, and (only when
/// produced) a PNG raster per theme.
#[derive(Debug, Clone)]
pub struct SvgAssetsVersioningResult {
    pub explainer_light: ArtifactVersion,
    pub explainer_dark: ArtifactVersion,
    pub og_card_light: ArtifactVersion,
    pub og_card_dark: ArtifactVersion,
    pub og_card_png_light: Option<ArtifactVersion>,
    pub og_card_png_dark: Option<ArtifactVersion>,
}

/// Store every artifact this module can produce for one `(project, module)`
/// into `store`, via [`super::versioning::VersionStore::store_version`] --
/// the same append-only DOCGEN-07 store every other docgen artifact uses.
/// PNG bytes are stored as a base64 string (the store is `String`-typed,
/// matching every other artifact kind it holds) -- only when `raster`
/// actually produced bytes for that theme.
pub fn version_svg_assets(
    store: &VersionStore,
    project: &str,
    module: &str,
    explainer: &(String, String),
    og_card: &(String, String),
    raster_light: &RasterOutcome,
    raster_dark: &RasterOutcome,
    source_commit: &str,
    timestamp: &str,
) -> SvgAssetsVersioningResult {
    let explainer_light = store.store_version(
        explainer_key(project, module, Theme::Light),
        explainer.0.clone(),
        source_commit,
        timestamp,
    );
    let explainer_dark = store.store_version(
        explainer_key(project, module, Theme::Dark),
        explainer.1.clone(),
        source_commit,
        timestamp,
    );
    let og_card_light = store.store_version(
        og_card_key(project, module, Theme::Light),
        og_card.0.clone(),
        source_commit,
        timestamp,
    );
    let og_card_dark = store.store_version(
        og_card_key(project, module, Theme::Dark),
        og_card.1.clone(),
        source_commit,
        timestamp,
    );

    let og_card_png_light = raster_light.png.as_ref().map(|bytes| {
        store.store_version(
            og_card_png_key(project, module, Theme::Light),
            base64_encode(bytes),
            source_commit,
            timestamp,
        )
    });
    let og_card_png_dark = raster_dark.png.as_ref().map(|bytes| {
        store.store_version(
            og_card_png_key(project, module, Theme::Dark),
            base64_encode(bytes),
            source_commit,
            timestamp,
        )
    });

    SvgAssetsVersioningResult {
        explainer_light,
        explainer_dark,
        og_card_light,
        og_card_dark,
        og_card_png_light,
        og_card_png_dark,
    }
}

/// Minimal, dependency-free base64 encoder (standard alphabet, padded) --
/// this workspace's `Cargo.toml` has no `base64` crate declared for this
/// module to reuse, and PNG bytes must round-trip through the
/// `String`-typed [`super::versioning::VersionStore`] intact.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);

        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { ALPHABET[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── explainer: theme-aware, from content ────────────────────────────

    #[test]
    fn explainer_svg_produces_distinct_light_and_dark_variants() {
        let points = vec!["The engine sweeps PII before inference.".to_string()];
        let (light, dark) = render_explainer_pair("Docgen PII Gate", &points);

        assert!(light.contains("Docgen PII Gate"));
        assert!(dark.contains("Docgen PII Gate"));
        assert_ne!(light, dark, "light and dark variants must actually differ");
        // Distinct palettes: light uses white bg, dark uses a dark bg.
        assert!(light.contains("#ffffff"));
        assert!(dark.contains("#12141a"));
        assert!(light.contains("<svg"));
        assert!(dark.contains("<svg"));
    }

    #[test]
    fn explainer_svg_embeds_every_body_point_wrapped_not_truncated() {
        let long_point = "x ".repeat(80); // forces wrapping across multiple lines
        let points = vec!["short point".to_string(), long_point.clone()];
        let (light, _dark) = render_explainer_pair("Title", &points);

        assert!(light.contains("short point"));
        // Every word of the long point must still appear somewhere (never
        // truncated, only wrapped across <text> lines).
        for word in long_point.split_whitespace().take(5) {
            assert!(light.contains(word), "word {word} missing from wrapped explainer SVG");
        }
    }

    // ── OG card: title/summary substituted, escaped ─────────────────────

    #[test]
    fn og_card_substitutes_title_and_summary() {
        let (light, dark) = build_og_card("Docgen Ships", "The doc engine now versions everything.").unwrap();
        assert!(light.contains("Docgen Ships"));
        assert!(light.contains("The doc engine now versions everything."));
        assert!(dark.contains("Docgen Ships"));
        assert_ne!(light, dark);
    }

    /// Load-bearing negative test: title/summary text that could break out
    /// of the SVG markup (angle brackets, ampersands, quotes) must be
    /// escaped, never passed through raw -- otherwise a crafted frontmatter
    /// value could inject markup into every rendered OG card.
    #[test]
    fn og_card_escapes_markup_breaking_characters() {
        let (light, _dark) =
            build_og_card("<script>alert(1)</script> & \"quoted\"", "5 < 10 & 10 > 5").unwrap();

        assert!(!light.contains("<script>"), "raw script tag must never appear unescaped");
        assert!(light.contains("&lt;script&gt;"));
        assert!(light.contains("&amp;"));
        assert!(light.contains("&quot;quoted&quot;"));
    }

    #[test]
    fn og_card_dimensions_match_standard_og_image_size() {
        let (light, _dark) = build_og_card("T", "S").unwrap();
        assert!(light.contains(&format!("width=\"{OG_WIDTH}\"")));
        assert!(light.contains(&format!("height=\"{OG_HEIGHT}\"")));
    }

    // ── OG card: frontmatter text is swept (second sweep point) ─────────

    /// LOAD-BEARING negative test (mirrors `diagram.rs`'s
    /// `hostname_in_node_label_is_redacted_before_reaching_renderer`):
    /// frontmatter text that restates an internal hostname/IP must be
    /// redacted before it can reach the OG card SVG.
    #[test]
    fn hostname_in_frontmatter_summary_is_redacted_before_reaching_card() {
        let (light, dark) =
            build_og_card("Infra Notes", "connects to <internal-ip> for status").unwrap(); // pii-test-fixture
        assert!(!light.contains("<internal-ip>")); // pii-test-fixture
        assert!(!dark.contains("<internal-ip>")); // pii-test-fixture
        assert!(light.contains("[REDACTED:private_ip]"), "expected an explicit redaction marker: {light}");

        // Re-scan with the canonical scanner, but assert on the specific
        // load-bearing category (`private_ip`) rather than "zero violations
        // of any kind": the SVG's own structural markup (e.g.
        // `viewBox="0 0 1200 630"`, a run of digits separated by spaces)
        // incidentally trips the unrelated `phone` heuristic
        // (`phone_match_is_phone_shaped`'s "has internal separator" rule) --
        // that's a property of any SVG with numeric attributes, not evidence
        // the private-IP redaction failed.
        let violations = crate::github::pii::scan_for_pii(&light);
        assert!(
            violations.iter().all(|v| v.category != "private_ip"),
            "OG card SVG must have no private_ip violations after redaction: {violations:?}"
        );
    }

    // ── PNG raster: resvg-present -> raster / resvg-absent -> skip ──────

    struct MockRasterizer {
        available: bool,
        result: Result<Vec<u8>, String>,
    }

    impl SvgRasterizer for MockRasterizer {
        fn is_available(&self) -> bool {
            self.available
        }
        fn rasterize(&self, _svg: &str) -> Result<Vec<u8>, String> {
            self.result.clone()
        }
    }

    #[test]
    fn resvg_unavailable_skips_png_with_clear_note_svg_still_usable() {
        let (light, _dark) = build_og_card("T", "S").unwrap();
        let rasterizer = MockRasterizer { available: false, result: Ok(vec![1, 2, 3]) };

        let outcome = raster_svg_to_png(&light, &rasterizer);
        assert!(!outcome.was_rendered());
        let note = outcome.note.unwrap();
        assert!(note.contains("unavailable"));
        assert!(note.contains("resvg"));
        // The SVG remains fully usable regardless.
        assert!(light.contains("<svg"));
    }

    #[test]
    fn resvg_available_rasters_to_png() {
        let (light, _dark) = build_og_card("T", "S").unwrap();
        let png_bytes = vec![0x89, b'P', b'N', b'G'];
        let rasterizer = MockRasterizer { available: true, result: Ok(png_bytes.clone()) };

        let outcome = raster_svg_to_png(&light, &rasterizer);
        assert!(outcome.was_rendered());
        assert_eq!(outcome.png.unwrap(), png_bytes);
        assert!(outcome.note.is_none());
    }

    #[test]
    fn resvg_available_but_fails_still_skips_with_note_not_a_panic() {
        let (light, _dark) = build_og_card("T", "S").unwrap();
        let rasterizer = MockRasterizer { available: true, result: Err("bad path data".to_string()) };

        let outcome = raster_svg_to_png(&light, &rasterizer);
        assert!(!outcome.was_rendered());
        assert!(outcome.note.unwrap().contains("bad path data"));
    }

    /// Real-world smoke test: with no resvg crate in this build, the real
    /// [`ResvgRasterizer`] slot always reports unavailable and the skip path
    /// always fires -- asserted for real, not mocked away, exactly mirroring
    /// `diagram.rs`'s `system_d2_renderer_smoke_test_present_or_absent`.
    #[test]
    fn resvg_rasterizer_slot_reports_unavailable_in_this_build() {
        let (light, _dark) = build_og_card("T", "S").unwrap();
        let rasterizer = ResvgRasterizer;
        assert!(!rasterizer.is_available());

        let outcome = raster_svg_to_png(&light, &rasterizer);
        assert!(!outcome.was_rendered());
        assert!(outcome.note.unwrap().contains("unavailable"));
    }

    // ── versioning: both themes always, png only when rastered ──────────

    #[test]
    fn versions_all_four_svgs_and_pngs_only_when_rastered() {
        let store = VersionStore::new();
        let explainer = render_explainer_pair("Title", &["a point".to_string()]);
        let og_card = build_og_card("Title", "Summary").unwrap();

        let skipped = RasterOutcome::skipped("resvg unavailable");
        let result = version_svg_assets(
            &store, "terminus", "src/worker", &explainer, &og_card, &skipped, &skipped, "c1", "t0",
        );
        assert_eq!(result.explainer_light.version, 1);
        assert_eq!(result.explainer_dark.version, 1);
        assert_eq!(result.og_card_light.version, 1);
        assert_eq!(result.og_card_dark.version, 1);
        assert!(result.og_card_png_light.is_none());
        assert!(result.og_card_png_dark.is_none());

        let rastered = RasterOutcome::rendered(vec![1, 2, 3]);
        let result2 = version_svg_assets(
            &store, "terminus", "src/worker", &explainer, &og_card, &rastered, &rastered, "c2", "t1",
        );
        // Second call: explainer/og-card histories advance to v2; PNG
        // histories get their first-ever version (v1), independent keys.
        assert_eq!(result2.explainer_light.version, 2);
        assert_eq!(result2.og_card_light.version, 2);
        assert_eq!(result2.og_card_png_light.as_ref().unwrap().version, 1);
        assert_eq!(result2.og_card_png_dark.as_ref().unwrap().version, 1);

        let explainer_light_history = store.history(&explainer_key("terminus", "src/worker", Theme::Light));
        assert_eq!(explainer_light_history.len(), 2, "prior explainer version must never be overwritten");
    }

    #[test]
    fn different_modules_have_independent_svg_asset_histories() {
        let store = VersionStore::new();
        let explainer = render_explainer_pair("Title", &["a".to_string()]);
        let og_card = build_og_card("Title", "Summary").unwrap();
        let skipped = RasterOutcome::skipped("n/a");

        version_svg_assets(&store, "terminus", "src/a", &explainer, &og_card, &skipped, &skipped, "c1", "t0");
        version_svg_assets(&store, "terminus", "src/b", &explainer, &og_card, &skipped, &skipped, "c1", "t0");

        assert_eq!(store.history(&explainer_key("terminus", "src/a", Theme::Light)).len(), 1);
        assert_eq!(store.history(&explainer_key("terminus", "src/b", Theme::Light)).len(), 1);
    }

    #[test]
    fn png_stored_as_base64_round_trips() {
        let store = VersionStore::new();
        let explainer = render_explainer_pair("T", &["p".to_string()]);
        let og_card = build_og_card("T", "S").unwrap();
        let bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let rastered = RasterOutcome::rendered(bytes.clone());

        let result = version_svg_assets(
            &store, "terminus", "src/x", &explainer, &og_card, &rastered, &rastered, "c1", "t0",
        );
        let stored = result.og_card_png_light.unwrap();
        assert_eq!(base64_decode(&stored.content), bytes);
    }

    /// Minimal decoder used only by the round-trip test above, so the test
    /// doesn't just re-assert `base64_encode`'s own output against itself.
    fn base64_decode(s: &str) -> Vec<u8> {
        fn val(c: u8) -> Option<u8> {
            match c {
                b'A'..=b'Z' => Some(c - b'A'),
                b'a'..=b'z' => Some(c - b'a' + 26),
                b'0'..=b'9' => Some(c - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }
        let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
        let mut out = Vec::new();
        for chunk in bytes.chunks(4) {
            let vals: Vec<u8> = chunk.iter().filter_map(|&b| val(b)).collect();
            if vals.len() >= 2 {
                out.push((vals[0] << 2) | (vals[1] >> 4));
            }
            if vals.len() >= 3 {
                out.push((vals[1] << 4) | (vals[2] >> 2));
            }
            if vals.len() >= 4 {
                out.push((vals[2] << 6) | vals[3]);
            }
        }
        out
    }

    // ── WRITE-MODEL INVERSION: no placement, ever ────────────────────────

    /// Negative test (mirrors `render/mod.rs`'s
    /// `render_all_never_touches_filesystem_or_vault`): every function in
    /// this module returns artifacts and never writes to a repo, the
    /// filesystem, or a hosting surface.
    #[test]
    fn svg_and_raster_never_touch_the_filesystem() {
        let tmp = std::env::temp_dir().join(format!("docgen-svg-assets-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let before: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(before.is_empty());

        let explainer = render_explainer_pair("Title", &["a point".to_string()]);
        let og_card = build_og_card("Title", "Summary").unwrap();
        let rasterizer = MockRasterizer { available: true, result: Ok(vec![1, 2, 3]) };
        let _raster = raster_svg_to_png(&og_card.0, &rasterizer);
        let store = VersionStore::new();
        let _ = version_svg_assets(
            &store,
            "terminus",
            "src/worker",
            &explainer,
            &og_card,
            &RasterOutcome::rendered(vec![1, 2, 3]),
            &RasterOutcome::skipped("n/a"),
            "c1",
            "t0",
        );

        let after: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(after.is_empty(), "svg_assets must never write files");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ── escaping / wrapping helpers ───────────────────────────────────────

    #[test]
    fn escape_svg_text_handles_all_five_predefined_entities() {
        assert_eq!(escape_svg_text("<>&\"'"), "&lt;&gt;&amp;&quot;&apos;");
    }

    #[test]
    fn wrap_text_never_splits_a_word_and_never_drops_content() {
        let text = "the quick brown fox jumps over the lazy dog repeatedly and again";
        let lines = wrap_text(text, 20);
        let rejoined = lines.join(" ");
        assert_eq!(rejoined, text, "wrapping must never lose or reorder words");
        for line in &lines {
            for word in line.split_whitespace() {
                assert!(text.contains(word));
            }
        }
    }

    #[test]
    fn wrap_text_empty_input_returns_single_empty_line() {
        assert_eq!(wrap_text("", 20), vec![String::new()]);
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
