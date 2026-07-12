//! DOCGEN-11: Auto architecture/flow/sequence diagrams from the merged diff
//! (diagram-as-code -> SVG), S95, Plane TERM-162.
//!
//! After a merged feat, derive an architecture/flow/sequence diagram from
//! the code+diff and render it to SVG for injection into README/wiki
//! artifacts (DOCGEN-06) and versioning (DOCGEN-07). The LLM emits **D2 or
//! Mermaid SOURCE** -- deterministic, diffable, PII-inspectable TEXT --
//! never a binary image straight from the model.
//!
//! ## Two independent sweep points (load-bearing)
//! 1. The feat context fed INTO generation is already a [`super::generate::SweptFeatContext`]
//!    (DOCGEN-02 gate, enforced structurally by that type -- see `generate.rs`).
//! 2. The diagram SOURCE the model EMITS is swept AGAIN, here, before it is
//!    ever handed to a renderer. This second sweep is the one this item
//!    adds: node labels are free text the model writes, and can restate an
//!    internal hostname/IP from its own knowledge even when the swept input
//!    context didn't contain one verbatim (spec: "node labels leak
//!    hostnames"). [`SweptDiagramSource`] mirrors [`super::generate::SweptFeatContext`]'s
//!    structural enforcement: its only public constructor takes a
//!    [`super::pii_gate::PiiGateOutcome`], so there is no code path in this
//!    module through which unswept diagram text could reach a renderer.
//!
//! ## Rendering: d2 (dagre/ELK) only -- never TALA
//! D2's bundled layout engines (dagre, ELK) are MPL-2.0 and browser-free;
//! its proprietary hosted layout engine (TALA) is NEVER invoked here -- this
//! module always passes `--layout dagre` explicitly on every `d2` CLI
//! invocation, so there is no code path that could silently fall back to
//! TALA. Mermaid diagrams are not rendered by this item (Mermaid needs a
//! browser or a self-hosted Kroki instance, neither of which is wired in
//! this build) -- a Mermaid [`SweptDiagramSource`] is always reported as a
//! skip with a clear note; its swept source is still versioned.
//!
//! ## Skip-if-unavailable (mirrors `render/pdf.rs`)
//! Exactly like the PDF renderer (DOCGEN-06), when the `d2` CLI is not on
//! PATH this module returns a clear "renderer unavailable" skip rather than
//! failing the whole pipeline -- the diagram SOURCE (the diffable, versioned
//! artifact) is still produced and versioned either way; only the SVG raster
//! is conditional on the tool being present. [`D2Renderer`] is the
//! injectable seam (mirrors `render/notion.rs`/`render/blog.rs`'s
//! validation-client seams) that makes both the skip path and the
//! present-and-succeeds path deterministically unit-testable without
//! depending on whether `d2` actually happens to be installed on the box
//! running the test suite.
//!
//! ## Versioning (DOCGEN-07 reuse, not reimplementation)
//! Both the swept diagram SOURCE and the rendered SVG (when produced) are
//! stored via [`super::versioning::VersionStore`] -- the exact same
//! append-only, diffable, rollback-able store DOCGEN-07 already ships. This
//! module adds no second version-storage mechanism.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::generate::{DocGenerator, SweptFeatContext};
use super::pii_gate::{sweep_input, PiiGateOutcome};
use super::versioning::{ArtifactKey, ArtifactVersion, VersionStore};
use crate::error::ToolError;

/// Which diagram-as-code language the LLM was asked to (and did) emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagramFormat {
    D2,
    Mermaid,
}

impl DiagramFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiagramFormat::D2 => "d2",
            DiagramFormat::Mermaid => "mermaid",
        }
    }

    /// File extension convention for the source artifact (`.d2` / `.mmd`),
    /// per the spec's storage naming.
    pub fn source_extension(&self) -> &'static str {
        match self {
            DiagramFormat::D2 => "d2",
            DiagramFormat::Mermaid => "mmd",
        }
    }
}

// ---------------------------------------------------------------------------
// SweptDiagramSource -- ordering enforcement (mirrors SweptFeatContext)
// ---------------------------------------------------------------------------

/// A diagram-as-code SOURCE string that has ALREADY passed the DOCGEN-02 PII
/// gate. The inner content is private and reachable only via
/// [`Self::as_str`]; the only public constructor is
/// [`Self::from_gate_outcome`], which requires a
/// [`super::pii_gate::PiiGateOutcome`] -- the value type
/// [`super::pii_gate::sweep_input`] returns. This is the structural
/// enforcement of the second sweep point described in the module doc
/// comment: there is no way to construct a [`SweptDiagramSource`] from a
/// bare, unswept `&str`, so no renderer in this module can ever be handed
/// unswept diagram text.
#[derive(Debug, Clone)]
pub struct SweptDiagramSource {
    content: String,
    format: DiagramFormat,
}

impl SweptDiagramSource {
    /// Build a [`SweptDiagramSource`] from an already-computed PII gate
    /// outcome (`super::pii_gate::sweep_input(raw)?`). This is the ONLY way
    /// to construct one.
    pub fn from_gate_outcome(outcome: &PiiGateOutcome, format: DiagramFormat) -> Self {
        Self { content: outcome.sanitized_content().to_string(), format }
    }

    pub fn as_str(&self) -> &str {
        &self.content
    }

    pub fn format(&self) -> DiagramFormat {
        self.format
    }
}

// ---------------------------------------------------------------------------
// Generation: feat context -> diagram source (Chord seam, reused)
// ---------------------------------------------------------------------------

/// Build the prompt asking the model for diagram-as-code SOURCE ONLY -- no
/// prose, no code fences wrapping the whole response, no binary image. Kept
/// as its own function so prompt shaping is unit-testable independent of a
/// real [`DocGenerator`].
fn build_diagram_prompt(module_path: &str, git_ref: &str, feat_context: &str, format: DiagramFormat) -> String {
    let (lang_name, syntax_hint) = match format {
        DiagramFormat::D2 => (
            "D2",
            "D2 syntax (shapes, arrows like `a -> b: label`, containers with `{ }`)",
        ),
        DiagramFormat::Mermaid => (
            "Mermaid",
            "Mermaid syntax (e.g. `flowchart TD` / `sequenceDiagram` / `graph LR`)",
        ),
    };
    format!(
        "You are generating an architecture/flow/sequence diagram, as diagram-as-code SOURCE \
TEXT, for a change to a Rust codebase.\n\n\
Module path: {module_path}\n\
Git ref this diagram is generated against: {git_ref}\n\n\
What changed (already sanitized for private infrastructure details):\n{feat_context}\n\n\
Write ONLY {lang_name} diagram source describing the architecture/flow/sequence this change \
introduces or affects, using {syntax_hint}. Base every node and edge ONLY on what the change \
above actually shows -- never invent components that aren't evidenced by it. Respond with ONLY \
the {lang_name} source -- no preamble, no meta-commentary, no markdown code fences wrapping the \
response.\n"
    )
}

/// Generate a diagram-as-code SOURCE for `module_path`/`git_ref` from an
/// already-swept feat context, via `generator` (the same [`DocGenerator`]
/// Chord-routing seam DOCGEN-05 uses -- this function never picks a model
/// itself). The model's raw output is swept through [`sweep_input`] AGAIN
/// before being wrapped in a [`SweptDiagramSource`] -- see the module doc
/// comment's "Two independent sweep points" section; this is the load-
/// bearing step this item adds on top of DOCGEN-02/05's existing input
/// sweep.
pub async fn generate_diagram_source(
    generator: &dyn DocGenerator,
    module_path: &str,
    git_ref: &str,
    feat_context: &SweptFeatContext,
    format: DiagramFormat,
) -> Result<SweptDiagramSource, ToolError> {
    let prompt = build_diagram_prompt(module_path, git_ref, feat_context.as_str(), format);
    let raw = generator.generate(&prompt).await?;
    let outcome = sweep_input(&raw)?;
    Ok(SweptDiagramSource::from_gate_outcome(&outcome, format))
}

// ---------------------------------------------------------------------------
// Rendering: swept source -> SVG (d2 CLI, dagre/ELK -- never TALA), or skip
// ---------------------------------------------------------------------------

/// The injectable rendering seam. [`SystemD2Renderer`] is the real
/// implementation (shells out to the `d2` CLI); tests inject a mock so both
/// the "d2 absent -> skip" and "d2 present -> renders" paths are
/// deterministically covered regardless of whether `d2` actually happens to
/// be installed on the box running the test suite (mirrors
/// `render/notion.rs`/`render/blog.rs`'s validation-client seam pattern).
pub trait D2Renderer: Send + Sync {
    /// Cheap, side-effect-free check for whether this renderer can actually
    /// render right now (e.g. the `d2` binary is on PATH).
    fn is_available(&self) -> bool;

    /// Render `source` (already-swept D2 source) to SVG. Only called when
    /// [`Self::is_available`] returned `true`.
    fn render(&self, source: &str) -> Result<String, String>;
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The real renderer: shells out to the `d2` CLI. Every invocation passes
/// `--layout dagre` explicitly -- this is the one and only layout flag this
/// module ever sets, so there is no code path that could invoke the
/// proprietary TALA engine.
pub struct SystemD2Renderer;

impl D2Renderer for SystemD2Renderer {
    fn is_available(&self) -> bool {
        Command::new("d2")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn render(&self, source: &str) -> Result<String, String> {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("docgen-d2-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not create d2 temp dir: {e}"))?;
        let in_path = dir.join("diagram.d2");
        let out_path = dir.join("diagram.svg");
        std::fs::write(&in_path, source).map_err(|e| format!("could not write d2 source: {e}"))?;

        let output = Command::new("d2")
            .arg("--layout")
            .arg("dagre")
            .arg(&in_path)
            .arg(&out_path)
            .output()
            .map_err(|e| format!("failed to spawn d2: {e}"))?;

        let result = if !output.status.success() {
            Err(format!(
                "d2 exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
        } else {
            std::fs::read_to_string(&out_path).map_err(|e| format!("could not read rendered svg: {e}"))
        };
        std::fs::remove_dir_all(&dir).ok();
        result
    }
}

/// One diagram render attempt's outcome. Either real SVG content
/// (`svg.is_some()`, `note.is_none()`), or a skip with a human-readable
/// reason (`svg.is_none()`, `note.is_some()`) -- never both, never neither,
/// mirroring `render::RenderedArtifact`'s shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramRenderOutcome {
    pub svg: Option<String>,
    pub note: Option<String>,
}

impl DiagramRenderOutcome {
    fn rendered(svg: String) -> Self {
        Self { svg: Some(svg), note: None }
    }

    fn skipped(note: impl Into<String>) -> Self {
        Self { svg: None, note: Some(note.into()) }
    }

    pub fn was_rendered(&self) -> bool {
        self.svg.is_some()
    }
}

/// Render `source` to SVG via `renderer`. Mermaid sources are always
/// skipped with a clear note (Mermaid rendering routes through a browser or
/// self-hosted Kroki, neither of which is wired in this build) -- their
/// swept source is still produced by [`generate_diagram_source`] and is
/// still versionable via [`version_diagram`]. D2 sources render through
/// `renderer` when available; otherwise they are skipped with a clear
/// "renderer unavailable" note, exactly mirroring `render/pdf.rs`'s
/// skip-if-unavailable pattern -- the diagram SOURCE remains the guaranteed,
/// diffable artifact regardless of whether the SVG raster could be produced.
pub fn render_diagram_svg(source: &SweptDiagramSource, renderer: &dyn D2Renderer) -> DiagramRenderOutcome {
    if source.format() == DiagramFormat::Mermaid {
        return DiagramRenderOutcome::skipped(
            "mermaid rendering routes via a browser or self-hosted Kroki, neither of which is \
wired in this build -- skipping SVG raster. The swept .mmd SOURCE is still produced and \
versioned; only the SVG is unavailable for this format in this build.",
        );
    }

    if !renderer.is_available() {
        return DiagramRenderOutcome::skipped(
            "d2/kroki renderer unavailable in this build (the d2 CLI was not found on PATH, or \
reported itself unavailable) -- skipping SVG raster. The diagram SOURCE (.d2) is still produced \
and versioned; install the d2 CLI (dagre/ELK layout, MPL-2.0 -- never the proprietary TALA \
engine) to enable SVG rendering.",
        );
    }

    match renderer.render(source.as_str()) {
        Ok(svg) => DiagramRenderOutcome::rendered(svg),
        Err(e) => DiagramRenderOutcome::skipped(format!(
            "d2 CLI was available but failed to render this diagram source -- skipping SVG \
raster (SOURCE is still produced and versioned): {e}"
        )),
    }
}

// ---------------------------------------------------------------------------
// DOCGEN-22: native Mermaid fence embed (revision -- fixes the broken SVG)
// ---------------------------------------------------------------------------
//
// Root cause (operator report, S95 REVISION-multipage-mermaid.md): a
// filter-heavy, dark-background `assets/architecture.svg` embedded via
// `<img src=...>` renders as a black/blank box on GitHub/Gitea (both
// sanitize/rasterize `<img>`-embedded SVG, stripping `<filter>` and any
// dark-mode adaptation) -- and this module's own d2 CLI path silently
// produces nothing when `d2` isn't on PATH, so the embed slot never had a
// real fallback. The fix: the DEFAULT / binary-absent embed is now a fenced
// ```mermaid `flowchart` block -- a plain markdown string, renders natively
// on GitHub (Viewscreen) and Gitea >=1.18 with **no binary**, theme-aware.
// The d2/SVG raster path is kept, but ONLY as the embed when the `d2`
// binary is actually present AND the render succeeded; every other case
// (unavailable, failed, or the source was Mermaid to begin with) resolves
// to a rendering mermaid fence -- never a skipped/broken `<img>`.

/// Gitea's `MERMAID_MAX_SOURCE_CHARACTERS` default -- the fenced source must
/// stay under this or Gitea refuses to render the diagram at all.
pub const MERMAID_MAX_SOURCE_CHARS: usize = 5000;

/// Validate that `source` is renderable Mermaid `flowchart` source per the
/// DOCGEN-22 rules: non-empty, under [`MERMAID_MAX_SOURCE_CHARS`], a
/// `flowchart` (never `architecture-beta`/`C4*` -- those need external
/// icon packs/Kroki and are not safe auto-gen targets), no external icon
/// references (`http(s)://` asset URLs, `fa:`/iconify shorthand, raw
/// `<img`), and any `subgraph`/`end` pairs balanced (an unbalanced pair is
/// invalid Mermaid and Gitea/GitHub simply fail to render the whole block).
pub fn validate_mermaid_flowchart(source: &str) -> Result<(), String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err("mermaid source is empty".to_string());
    }
    if trimmed.len() > MERMAID_MAX_SOURCE_CHARS {
        return Err(format!(
            "mermaid source is {} chars, over Gitea's MERMAID_MAX_SOURCE_CHARACTERS limit of {}",
            trimmed.len(),
            MERMAID_MAX_SOURCE_CHARS
        ));
    }
    let first_line = trimmed.lines().next().unwrap_or("").trim();
    if !first_line.starts_with("flowchart") {
        return Err(format!(
            "mermaid source must open with `flowchart LR`/`flowchart TD`, got: {first_line:?}"
        ));
    }
    if trimmed.contains("architecture-beta") {
        return Err("architecture-beta is not a supported auto-gen diagram type -- use flowchart".to_string());
    }
    for c4 in ["C4Context", "C4Container", "C4Component", "C4Dynamic", "C4Deployment"] {
        if trimmed.contains(c4) {
            return Err(format!("{c4} is not a supported auto-gen diagram type -- use flowchart"));
        }
    }
    if trimmed.contains("http://") || trimmed.contains("https://") || trimmed.contains("<img") {
        return Err("mermaid source must not reference external icon packs/images".to_string());
    }
    let subgraph_count = trimmed
        .lines()
        .filter(|l| l.trim_start().starts_with("subgraph "))
        .count();
    let end_count = trimmed.lines().filter(|l| l.trim() == "end").count();
    if subgraph_count != end_count {
        return Err(format!(
            "unbalanced subgraph/end: {subgraph_count} subgraph(s) vs {end_count} end(s)"
        ));
    }
    // A labeled dotted arrow (`-. "text" .->`) requires a space + quoted
    // label between the two dots -- `-.text.->` (glued, unquoted) is
    // invalid mermaid grammar and fails to render the WHOLE diagram on
    // Gitea/GitHub, even though it passes every check above (found in
    // review, DOCGEN-22).
    static MALFORMED_DOTTED_LABEL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = MALFORMED_DOTTED_LABEL
        .get_or_init(|| regex::Regex::new(r"-\.[A-Za-z0-9_]+\.-{1,2}>").expect("static regex is valid"));
    if let Some(m) = re.find(trimmed) {
        return Err(format!(
            "malformed labeled dotted arrow {:?} -- use `-. \"label\" .->` (space + quoted label), not a glued/unquoted form",
            m.as_str()
        ));
    }
    Ok(())
}

/// Wrap already-swept Mermaid diagram SOURCE (a [`SweptDiagramSource`] with
/// [`DiagramFormat::Mermaid`]) in a fenced ```mermaid code block, after
/// running [`validate_mermaid_flowchart`]. This -- not a raster SVG, not a
/// skipped/broken `<img>` -- is what a README/docs architecture slot
/// embeds by default.
pub fn mermaid_fence(source: &SweptDiagramSource) -> Result<String, String> {
    if source.format() != DiagramFormat::Mermaid {
        return Err(format!(
            "mermaid_fence requires DiagramFormat::Mermaid, got {:?}",
            source.format()
        ));
    }
    validate_mermaid_flowchart(source.as_str())?;
    Ok(format!("```mermaid\n{}\n```", source.as_str().trim()))
}

/// A safe mermaid identifier derived from `label` -- non-alphanumeric
/// characters (`/`, `-`, spaces, ...) become `_` so a project/module name
/// like `src/widget` can be used as a `subgraph` id.
fn mermaid_safe_id(label: &str) -> String {
    let mut id: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if id.is_empty() || !id.chars().next().unwrap().is_ascii_alphabetic() {
        id = format!("m_{id}");
    }
    id
}

/// Build a generic, always-valid default architecture `flowchart` for
/// `module` -- used whenever there is no project-specific diagram source
/// available yet (e.g. the readme_layers architecture slot, which has no
/// per-call access to a generated diagram) or the d2 binary path could not
/// produce a raster. `module` is swept through the same DOCGEN-02 gate
/// ([`sweep_input`]) as any other diagram source before being wrapped in a
/// [`SweptDiagramSource`] -- even this generic template never embeds an
/// unswept label (spec AC: "diagram source PII-swept before embed").
pub fn default_architecture_mermaid_source(module: &str) -> Result<SweptDiagramSource, ToolError> {
    // Sweep the RAW label first, then derive the mermaid-safe id from the
    // already-sanitized text -- not the other way around. Deriving the id
    // from the raw label and sweeping the composed template afterward would
    // let a dotted IPv4/hostname survive undetected: `mermaid_safe_id`
    // flattens `.`/`-` to `_`, so a private address would become an
    // underscore-joined digit run in the id BEFORE the sweep ever runs,
    // which no longer matches the dotted-quad PII pattern -- a real, still
    // human-readable leak that would slip past `scan_for_pii`. Sweeping
    // first closes that (see the diagram module's test coverage for a
    // concrete before/after fixture).
    let label_outcome = sweep_input(module)?;
    let sanitized_label = label_outcome.sanitized_content();
    let id = mermaid_safe_id(sanitized_label);
    let raw = format!(
        "flowchart LR\n    subgraph {id}[\"{sanitized_label}\"]\n        A[Client] --> B[Core]\n        B --> C[Output]\n    end\n"
    );
    // Belt-and-suspenders: sweep the fully composed template too, matching
    // this module's existing "two independent sweep points" posture.
    let outcome = sweep_input(&raw)?;
    Ok(SweptDiagramSource::from_gate_outcome(&outcome, DiagramFormat::Mermaid))
}

/// The final embed markdown for a README/docs architecture slot, deciding
/// between the SVG raster path and the mermaid-fence fallback per the
/// DOCGEN-22 rule: SVG **only** when `source` is D2 AND `render_outcome`
/// actually produced one (the binary was present and verified); every
/// other case -- unavailable, failed, or `source` already being Mermaid --
/// resolves to a rendering mermaid fence, embedded inline as raw `<svg>`
/// markup (never an `<img>` tag, which is exactly the sanitize/rasterize
/// hazard this item fixes) when a raster is available, or the fence
/// otherwise. `fallback_label` seeds the generic default diagram when
/// `source` cannot itself be expressed as mermaid (a D2 source with no
/// raster available).
pub fn architecture_embed_markdown(
    source: &SweptDiagramSource,
    render_outcome: &DiagramRenderOutcome,
    fallback_label: &str,
) -> Result<String, ToolError> {
    if source.format() == DiagramFormat::D2 {
        if let Some(svg) = &render_outcome.svg {
            // Inline raw <svg> markup -- never an <img> tag, so there is no
            // sanitize/rasterize step to strip filters or skip dark-mode.
            return Ok(svg.trim().to_string());
        }
        // d2 unavailable/failed -- fall back to the generic mermaid default,
        // since a D2 source string cannot be reinterpreted as mermaid.
        let default_source = default_architecture_mermaid_source(fallback_label)?;
        return mermaid_fence(&default_source)
            .map_err(|e| ToolError::InvalidArgument(format!("default mermaid fence invalid: {e}")));
    }

    // Already mermaid -- fence it directly (this is the DEFAULT path).
    mermaid_fence(source).map_err(|e| ToolError::InvalidArgument(format!("mermaid fence invalid: {e}")))
}

// ---------------------------------------------------------------------------
// Versioning (DOCGEN-07 reuse)
// ---------------------------------------------------------------------------

/// The result of versioning a diagram's SOURCE (always) and SVG (only when
/// [`DiagramRenderOutcome::was_rendered`]).
#[derive(Debug, Clone)]
pub struct DiagramVersioningResult {
    pub source_version: ArtifactVersion,
    pub svg_version: Option<ArtifactVersion>,
}

/// Diagram-specific [`ArtifactKey`] targets: `diagram-source-{module}-{fmt}`
/// and `diagram-svg-{module}`. A distinct key per module (and, for the
/// source, per format) so multiple diagrams for the same project keep
/// independent histories -- same independence guarantee
/// `versioning.rs`'s `different_targets_have_independent_histories` already
/// covers for `readme`/`wiki`; this module reuses that guarantee by
/// construction rather than re-deriving it.
pub fn diagram_source_key(project: &str, module: &str, format: DiagramFormat) -> ArtifactKey {
    ArtifactKey::new(project, format!("diagram-source-{module}-{}", format.as_str()))
}

pub fn diagram_svg_key(project: &str, module: &str) -> ArtifactKey {
    ArtifactKey::new(project, format!("diagram-svg-{module}"))
}

/// Store `source` (always) and `render_outcome`'s SVG (when produced) as new
/// versions in `store`, via [`super::versioning::VersionStore::store_version`]
/// -- the exact same append-only DOCGEN-07 store every other docgen artifact
/// uses. Never overwrites a prior version (that guarantee is
/// `VersionStore`'s own, unchanged here).
pub fn version_diagram(
    store: &VersionStore,
    project: &str,
    module: &str,
    source: &SweptDiagramSource,
    render_outcome: &DiagramRenderOutcome,
    source_commit: &str,
    timestamp: &str,
) -> DiagramVersioningResult {
    let source_version = store.store_version(
        diagram_source_key(project, module, source.format()),
        source.as_str(),
        source_commit,
        timestamp,
    );

    let svg_version = render_outcome.svg.as_ref().map(|svg| {
        store.store_version(diagram_svg_key(project, module), svg.clone(), source_commit, timestamp)
    });

    DiagramVersioningResult { source_version, svg_version }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    // ── test doubles ─────────────────────────────────────────────────

    struct MockDocGenerator {
        response: String,
        captured_prompt: Mutex<Option<String>>,
    }

    impl MockDocGenerator {
        fn new(response: impl Into<String>) -> Self {
            Self { response: response.into(), captured_prompt: Mutex::new(None) }
        }

        fn captured_prompt(&self) -> String {
            self.captured_prompt.lock().unwrap().clone().expect("generate() was never called")
        }
    }

    #[async_trait]
    impl DocGenerator for MockDocGenerator {
        async fn generate(&self, prompt: &str) -> Result<String, ToolError> {
            *self.captured_prompt.lock().unwrap() = Some(prompt.to_string());
            Ok(self.response.clone())
        }
    }

    struct FailingDocGenerator;

    #[async_trait]
    impl DocGenerator for FailingDocGenerator {
        async fn generate(&self, _prompt: &str) -> Result<String, ToolError> {
            Err(ToolError::Http("backend down".to_string()))
        }
    }

    /// Mock [`D2Renderer`] whose availability and render outcome are set by
    /// the test -- makes both the skip path and the "renders" path
    /// deterministic without depending on whether `d2` is actually on PATH.
    struct MockD2Renderer {
        available: bool,
        result: Result<String, String>,
    }

    impl D2Renderer for MockD2Renderer {
        fn is_available(&self) -> bool {
            self.available
        }
        fn render(&self, _source: &str) -> Result<String, String> {
            self.result.clone()
        }
    }

    fn swept_feat(raw: &str) -> SweptFeatContext {
        let outcome = super::super::pii_gate::sweep_input(raw).expect("fixture should not block");
        SweptFeatContext::from_gate_outcome(&outcome)
    }

    // ── generation: LLM (mock) -> diagram source ────────────────────────

    #[tokio::test]
    async fn generates_d2_source_from_swept_feat_context() {
        let feat = swept_feat("+ added a new worker that talks to the queue");
        let mock = MockDocGenerator::new("a -> b: enqueue\nb -> c: process");
        let source = generate_diagram_source(&mock, "src/worker", "abc123", &feat, DiagramFormat::D2)
            .await
            .unwrap();

        assert_eq!(source.format(), DiagramFormat::D2);
        assert_eq!(source.as_str(), "a -> b: enqueue\nb -> c: process");
        assert!(mock.captured_prompt().contains("D2"));
        assert!(mock.captured_prompt().contains("src/worker"));
    }

    #[tokio::test]
    async fn generates_mermaid_source_when_requested() {
        let feat = swept_feat("+ added a sequence of calls");
        let mock = MockDocGenerator::new("sequenceDiagram\n  A->>B: call");
        let source =
            generate_diagram_source(&mock, "src/x", "abc123", &feat, DiagramFormat::Mermaid).await.unwrap();

        assert_eq!(source.format(), DiagramFormat::Mermaid);
        assert!(mock.captured_prompt().contains("Mermaid"));
    }

    #[tokio::test]
    async fn generator_failure_propagates_as_error() {
        let feat = swept_feat("+ trivial change");
        let result =
            generate_diagram_source(&FailingDocGenerator, "src/x", "m1", &feat, DiagramFormat::D2).await;
        assert!(result.is_err());
    }

    // ── second sweep point: LLM OUTPUT is swept before render ──────────

    /// LOAD-BEARING negative test (spec AC #2): the diagram SOURCE the model
    /// emits is swept for PII BEFORE it can reach a renderer -- a hostname
    /// in a node label is redacted. This is distinct from (and in addition
    /// to) the input-side sweep already enforced by `SweptFeatContext`.
    #[tokio::test]
    async fn hostname_in_node_label_is_redacted_before_reaching_renderer() {
        let feat = swept_feat("+ new worker pulls from the queue");
        // The model "hallucinates"/restates an internal hostname in a node
        // label, something the swept input context alone doesn't prevent --
        // this is exactly the case DOCGEN-11's own AC calls out.
        let mock = MockDocGenerator::new("worker -> host_pvf1: poll\nhost_pvf1 -> \"<internal-ip>\": status"); // pii-test-fixture
        let source = generate_diagram_source(&mock, "src/worker", "abc123", &feat, DiagramFormat::D2)
            .await
            .unwrap();

        assert!(!source.as_str().contains("<internal-ip>")); // pii-test-fixture
        assert!(
            crate::github::pii::scan_for_pii(source.as_str()).is_empty(),
            "diagram source handed to the renderer must be clean per the canonical scanner: {:?}",
            source.as_str()
        );

        // Prove the renderer never sees the raw label either.
        let renderer = MockD2Renderer { available: true, result: Ok("<svg/>".to_string()) };
        let outcome = render_diagram_svg(&source, &renderer);
        assert!(outcome.was_rendered());
    }

    // ── rendering: skip when d2 unavailable ─────────────────────────────

    #[test]
    fn d2_unavailable_skips_svg_with_clear_note_source_still_usable() {
        let feat_outcome = sweep_input("a -> b: ok").unwrap();
        let source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::D2);
        let renderer = MockD2Renderer { available: false, result: Ok("<svg/>".to_string()) };

        let outcome = render_diagram_svg(&source, &renderer);
        assert!(!outcome.was_rendered());
        let note = outcome.note.unwrap();
        assert!(note.contains("unavailable"));
        assert!(note.contains("d2"));
        // The source itself remains fully usable/versionable regardless.
        assert_eq!(source.as_str(), "a -> b: ok");
    }

    #[test]
    fn d2_available_but_cli_fails_still_skips_with_note_not_a_panic() {
        let feat_outcome = sweep_input("a -> b: ok").unwrap();
        let source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::D2);
        let renderer = MockD2Renderer { available: true, result: Err("boom".to_string()) };

        let outcome = render_diagram_svg(&source, &renderer);
        assert!(!outcome.was_rendered());
        assert!(outcome.note.unwrap().contains("boom"));
    }

    // ── rendering: d2-present path renders svg ──────────────────────────

    #[test]
    fn d2_present_path_renders_svg() {
        let feat_outcome = sweep_input("a -> b: ok\nb -> c: done").unwrap();
        let source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::D2);
        let renderer =
            MockD2Renderer { available: true, result: Ok("<svg>rendered diagram</svg>".to_string()) };

        let outcome = render_diagram_svg(&source, &renderer);
        assert!(outcome.was_rendered());
        assert_eq!(outcome.svg.unwrap(), "<svg>rendered diagram</svg>");
        assert!(outcome.note.is_none());
    }

    /// Real-world smoke test against the actual `d2` CLI, when present. On a
    /// box without `d2` installed (e.g. this build's dev sandbox) this just
    /// exercises (and asserts) the skip path for real, rather than being
    /// skipped itself -- so the test always runs and always asserts
    /// something concrete either way.
    #[test]
    fn system_d2_renderer_smoke_test_present_or_absent() {
        let feat_outcome = sweep_input("a -> b: ok").unwrap();
        let source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::D2);
        let renderer = SystemD2Renderer;

        let outcome = render_diagram_svg(&source, &renderer);
        if renderer.is_available() {
            assert!(outcome.was_rendered(), "d2 is on PATH but rendering did not produce SVG");
            assert!(outcome.svg.unwrap().contains("<svg"));
        } else {
            assert!(!outcome.was_rendered());
            assert!(outcome.note.unwrap().contains("unavailable"));
        }
    }

    // ── mermaid: always skipped for SVG, source still fine ──────────────

    #[test]
    fn mermaid_source_always_skips_svg_with_clear_note() {
        let feat_outcome = sweep_input("sequenceDiagram\n  A->>B: call").unwrap();
        let source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::Mermaid);
        // Even a renderer that claims availability must not be invoked for
        // mermaid -- assert via a renderer that would panic/fail if called.
        let renderer = MockD2Renderer { available: true, result: Err("should not be called".to_string()) };

        let outcome = render_diagram_svg(&source, &renderer);
        assert!(!outcome.was_rendered());
        assert!(outcome.note.unwrap().contains("mermaid"));
    }

    // ── versioning: source always, svg only when rendered ───────────────

    #[test]
    fn versions_source_always_and_svg_only_when_rendered() {
        let store = VersionStore::new();
        let feat_outcome = sweep_input("a -> b: ok").unwrap();
        let source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::D2);

        let skipped = DiagramRenderOutcome::skipped("d2 unavailable");
        let result = version_diagram(&store, "terminus", "src/worker", &source, &skipped, "c1", "t0");
        assert_eq!(result.source_version.version, 1);
        assert!(result.svg_version.is_none());

        let rendered = DiagramRenderOutcome::rendered("<svg/>".to_string());
        let result2 = version_diagram(&store, "terminus", "src/worker", &source, &rendered, "c2", "t1");
        // Source key is distinct from svg key, so this is version 2 of the
        // SOURCE history (not clobbered by the earlier skip), and a fresh
        // version 1 of the SVG history (first time SVG was ever produced).
        assert_eq!(result2.source_version.version, 2);
        assert_eq!(result2.svg_version.as_ref().unwrap().version, 1);

        let source_history = store.history(&diagram_source_key("terminus", "src/worker", DiagramFormat::D2));
        assert_eq!(source_history.len(), 2, "prior source version must never be overwritten");

        let svg_history = store.history(&diagram_svg_key("terminus", "src/worker"));
        assert_eq!(svg_history.len(), 1);
    }

    #[test]
    fn different_modules_and_formats_have_independent_diagram_histories() {
        let store = VersionStore::new();
        let d2 = sweep_input("a -> b").unwrap();
        let d2_source = SweptDiagramSource::from_gate_outcome(&d2, DiagramFormat::D2);
        let mmd = sweep_input("graph TD; a --> b").unwrap();
        let mmd_source = SweptDiagramSource::from_gate_outcome(&mmd, DiagramFormat::Mermaid);
        let skipped = DiagramRenderOutcome::skipped("n/a");

        version_diagram(&store, "terminus", "src/a", &d2_source, &skipped, "c1", "t0");
        version_diagram(&store, "terminus", "src/a", &mmd_source, &skipped, "c1", "t0");
        version_diagram(&store, "terminus", "src/b", &d2_source, &skipped, "c1", "t0");

        assert_eq!(
            store.history(&diagram_source_key("terminus", "src/a", DiagramFormat::D2)).len(),
            1
        );
        assert_eq!(
            store.history(&diagram_source_key("terminus", "src/a", DiagramFormat::Mermaid)).len(),
            1
        );
        assert_eq!(
            store.history(&diagram_source_key("terminus", "src/b", DiagramFormat::D2)).len(),
            1
        );
    }

    // ── PII scan hygiene of this module itself ──────────────────────────

    #[test]
    fn diagram_format_extensions_match_spec_naming() {
        assert_eq!(DiagramFormat::D2.source_extension(), "d2");
        assert_eq!(DiagramFormat::Mermaid.source_extension(), "mmd");
    }

    // ── DOCGEN-22: mermaid fence validation + embed ─────────────────────

    fn mermaid(raw: &str) -> SweptDiagramSource {
        let outcome = sweep_input(raw).expect("fixture should not block");
        SweptDiagramSource::from_gate_outcome(&outcome, DiagramFormat::Mermaid)
    }

    #[test]
    fn valid_flowchart_fences_cleanly() {
        let src = mermaid("flowchart LR\n    A[Client] --> B[Core]\n    B --> C[Output]\n");
        let fence = mermaid_fence(&src).unwrap();
        assert!(fence.starts_with("```mermaid\n"));
        assert!(fence.trim_end().ends_with("```"));
        assert!(fence.contains("flowchart LR"));
    }

    #[test]
    fn valid_flowchart_with_balanced_subgraph_passes() {
        let src = mermaid(
            "flowchart TD\n    subgraph core[\"Core\"]\n        A --> B\n    end\n    B --> C\n",
        );
        assert!(mermaid_fence(&src).is_ok());
    }

    #[test]
    fn unbalanced_subgraph_is_rejected() {
        let src = mermaid("flowchart TD\n    subgraph core[\"Core\"]\n        A --> B\n");
        let err = mermaid_fence(&src).unwrap_err();
        assert!(err.contains("unbalanced"), "unexpected error: {err}");
    }

    /// Review finding (DOCGEN-22): a glued/unquoted labeled dotted arrow
    /// (`-.text.->`) passes every other structural check here but is
    /// invalid mermaid grammar and fails to render on Gitea/GitHub. Must be
    /// rejected so this exact class of bug can't silently ship again.
    #[test]
    fn malformed_glued_dotted_arrow_label_is_rejected() {
        let src = mermaid("flowchart LR\n    A -.optional.-> B\n");
        let err = mermaid_fence(&src).unwrap_err();
        assert!(err.contains("malformed labeled dotted arrow"), "unexpected error: {err}");
    }

    /// The correctly-spaced, quoted form is valid and must NOT be rejected.
    #[test]
    fn correctly_formed_dotted_arrow_label_is_accepted() {
        let src = mermaid("flowchart LR\n    A -. \"optional\" .-> B\n");
        assert!(mermaid_fence(&src).is_ok());
    }

    /// A plain (unlabeled) dotted arrow is valid mermaid and must not be
    /// flagged by the labeled-arrow check.
    #[test]
    fn unlabeled_dotted_arrow_is_accepted() {
        let src = mermaid("flowchart LR\n    A -.-> B\n");
        assert!(mermaid_fence(&src).is_ok());
    }

    #[test]
    fn architecture_beta_is_rejected() {
        let src = mermaid("architecture-beta\n    group api(cloud)[API]\n");
        let err = mermaid_fence(&src).unwrap_err();
        assert!(err.contains("architecture-beta"));
    }

    #[test]
    fn c4_diagram_type_is_rejected() {
        let src = mermaid("flowchart LR\n    C4Context\n    Person(a, \"User\")\n");
        // starts with flowchart but embeds a C4 macro -- still rejected.
        let err = mermaid_fence(&src).unwrap_err();
        assert!(err.contains("C4Context"));
    }

    #[test]
    fn external_icon_reference_is_rejected() {
        let src = mermaid("flowchart LR\n    A[Client] --> B[<img src=\"https://example.com/icon.png\">]\n"); // pii-test-fixture
        let err = mermaid_fence(&src).unwrap_err();
        assert!(err.contains("external"));
    }

    #[test]
    fn oversized_mermaid_source_is_rejected() {
        let mut raw = "flowchart LR\n".to_string();
        while raw.len() < MERMAID_MAX_SOURCE_CHARS + 100 {
            raw.push_str("    A --> B\n");
        }
        let src = mermaid(&raw);
        let err = mermaid_fence(&src).unwrap_err();
        assert!(err.contains("MERMAID_MAX_SOURCE_CHARACTERS"));
    }

    #[test]
    fn d2_format_is_rejected_by_mermaid_fence() {
        let feat_outcome = sweep_input("a -> b: ok").unwrap();
        let d2_source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::D2);
        let err = mermaid_fence(&d2_source).unwrap_err();
        assert!(err.contains("DiagramFormat::Mermaid"));
    }

    // ── DOCGEN-22: default template + PII sweep ordering ────────────────

    /// LOAD-BEARING negative test (spec AC #5): even the generic default
    /// diagram template is swept for PII before it can ever be embedded --
    /// a module/project label carrying a hostname is redacted, exactly like
    /// the existing model-output sweep test above but for the readme_layers
    /// no-generation-available default path.
    #[test]
    fn default_template_sweeps_hostname_label_before_embed() {
        let source = default_architecture_mermaid_source("host_pvf1.internal <internal-ip>") // pii-test-fixture
            .expect("generic template should never be fully blocked");
        assert!(!source.as_str().contains("<internal-ip>")); // pii-test-fixture
        // Load-bearing: the id is derived from the SANITIZED label, so the
        // flattened digit form (dots -> underscores) must not leak the
        // address either -- this is the exact bypass a naive
        // sweep-after-flatten implementation would miss.
        assert!(!source.as_str().contains("192_168_0_104")); // pii-test-fixture
        assert!(
            crate::github::pii::scan_for_pii(source.as_str()).is_empty(),
            "default architecture diagram source must be clean per the canonical scanner: {:?}",
            source.as_str()
        );
        // Still valid, renderable mermaid despite the redaction.
        assert!(mermaid_fence(&source).is_ok());
    }

    #[test]
    fn default_template_is_valid_flowchart_for_plain_module_name() {
        let source = default_architecture_mermaid_source("src/widget").unwrap();
        assert_eq!(source.format(), DiagramFormat::Mermaid);
        let fence = mermaid_fence(&source).unwrap();
        assert!(fence.contains("flowchart LR"));
        assert!(fence.contains("src/widget"));
    }

    // ── DOCGEN-22: architecture_embed_markdown decision table ───────────

    #[test]
    fn embed_uses_mermaid_fence_when_source_is_mermaid() {
        let src = mermaid("flowchart LR\n    A --> B\n");
        let no_render = DiagramRenderOutcome::skipped("mermaid rendering not wired");
        let embed = architecture_embed_markdown(&src, &no_render, "fallback-module").unwrap();
        assert!(embed.starts_with("```mermaid\n"));
        assert!(!embed.contains("<img"));
    }

    /// AC #3: "tested skip -> mermaid, not broken img" -- when the source is
    /// D2 and the renderer was unavailable/failed (no svg produced), the
    /// embed falls back to a rendering mermaid fence built from the generic
    /// default template, never a skipped/broken `<img>`.
    #[test]
    fn embed_falls_back_to_mermaid_when_d2_unrendered() {
        let feat_outcome = sweep_input("a -> b: ok").unwrap();
        let d2_source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::D2);
        let skipped = DiagramRenderOutcome::skipped("d2 unavailable");

        let embed = architecture_embed_markdown(&d2_source, &skipped, "src/worker").unwrap();
        assert!(embed.starts_with("```mermaid\n"), "expected a mermaid fence, got: {embed}");
        assert!(!embed.contains("<img"), "must never embed a broken <img> tag");
        assert!(embed.contains("src/worker"));
    }

    /// The d2/SVG raster path is used ONLY when the binary is present AND
    /// verified (i.e. `render_outcome` actually carries an SVG) -- in that
    /// one case the embed is the raw inline SVG (never `<img>`).
    #[test]
    fn embed_uses_inline_svg_when_d2_actually_rendered() {
        let feat_outcome = sweep_input("a -> b: ok").unwrap();
        let d2_source = SweptDiagramSource::from_gate_outcome(&feat_outcome, DiagramFormat::D2);
        let rendered = DiagramRenderOutcome::rendered("<svg>real diagram</svg>".to_string());

        let embed = architecture_embed_markdown(&d2_source, &rendered, "src/worker").unwrap();
        assert_eq!(embed, "<svg>real diagram</svg>");
        assert!(!embed.contains("<img"));
    }

    #[test]
    fn mermaid_embed_never_exceeds_gitea_source_limit() {
        let src = mermaid("flowchart LR\n    A --> B\n");
        let embed = architecture_embed_markdown(&src, &DiagramRenderOutcome::skipped("n/a"), "x").unwrap();
        // The fence wrapper adds constant overhead; the SOURCE itself (what
        // Gitea measures) must be under the limit -- already enforced by
        // mermaid_fence/validate_mermaid_flowchart, asserted again here at
        // the embed-function boundary.
        assert!(embed.len() < MERMAID_MAX_SOURCE_CHARS + 20);
    }

    // ── DOCGEN-22: the LIVE README's architecture embed stays valid ─────

    /// Regression guard for the repo's own `README.md` `## Architecture`
    /// section, fixed as part of this item: extracts the fenced mermaid
    /// block between `## Architecture` and the next `## ` heading and runs
    /// it through the exact same [`validate_mermaid_flowchart`] rules the
    /// engine enforces on generated diagrams, so the live README can never
    /// silently drift back to a broken `<img>` embed or invalid mermaid
    /// without this test catching it.
    #[test]
    fn live_readme_architecture_section_is_a_valid_mermaid_flowchart() {
        let readme = include_str!("../../../README.md");
        let arch_start = readme.find("\n## Architecture\n").expect("README must have an ## Architecture section");
        let after = &readme[arch_start + 1..];
        let section_end = after[1..]
            .find("\n## ")
            .map(|i| i + 1)
            .unwrap_or(after.len());
        let section = &after[..section_end];

        assert!(!section.contains("<img"), "README Architecture section must not embed via <img>: {section}");

        let fence_start = section.find("```mermaid\n").expect("README Architecture section must contain a ```mermaid fence");
        let after_fence = &section[fence_start + "```mermaid\n".len()..];
        let fence_end = after_fence.find("```").expect("unterminated ```mermaid fence in README");
        let mermaid_source = &after_fence[..fence_end];

        validate_mermaid_flowchart(mermaid_source)
            .expect("README's architecture mermaid block must be valid renderable mermaid");
    }
}
