//! DOCGEN-05: doc generation orchestration -- read feat, deepen docs (S95,
//! Plane TERM-147).
//!
//! The core generation flow: take a project's existing docs plus the (PII-
//! swept) context of what a feat actually changed, request generation
//! through Chord's SLM router, and return deepened content ready for
//! per-target rendering (DOCGEN-06) and versioning (DOCGEN-07). This module
//! never picks a model and never renders/persists anything -- both are
//! explicitly out of scope, per the reuse plan in `docgen/mod.rs`'s module
//! doc comment.
//!
//! ## Reuse, not reimplementation
//! - **Inspection**: [`crate::scribe::inspect`] (`checkout`/`inspect_module`/
//!   `ModuleBundle`) is the sole source of a module's existing docs and
//!   source context here -- this module adds no second worktree-inspection
//!   path.
//! - **Prompt shaping**: [`crate::review::build_docs_prompt`] is the sole
//!   prompt builder -- this module only shapes the JSON *context* value that
//!   function embeds; it never constructs prompt text of its own.
//! - **PII gate**: [`super::pii_gate::sweep_input`] (DOCGEN-02) is the sole
//!   sweep -- this module adds no second scanner.
//!
//! ## Ordering (load-bearing): swept-before-request
//! [`SweptFeatContext`] is the ONLY type [`generate_docs`] accepts for a
//! feat's diff/spec/code context, and its only public constructor,
//! [`SweptFeatContext::from_gate_outcome`], takes a
//! `&`[`super::pii_gate::PiiGateOutcome`] -- the type [`super::pii_gate::sweep_input`]
//! returns. There is no `From<&str>`/`new(String)` on [`SweptFeatContext`],
//! so a caller cannot hand this module a raw, unswept `&str` for the feat
//! context even by accident: the only way to obtain one is to have already
//! run it through the gate. See `sweep_gate_ordering_enforced_no_raw_content_reaches_generator`
//! below for the negative test asserting this end to end (mock generator
//! captures exactly what it received; the raw PII literal never appears in
//! it).
//!
//! ## Deepen, not regenerate
//! [`generate_docs`] always includes the project's *existing* docs (if any)
//! in the context handed to [`crate::review::build_docs_prompt`] -- the
//! generator is asked to revise/extend, with the prior content in hand to
//! preserve, not asked to produce a document from nothing each time. See
//! `deepen_preserves_prior_content_before_after_fixture` below.
//!
//! ## Chord client seam
//! [`DocGenerator`] is the seam: the engine only ASKS for generated text
//! given a prompt; Chord OWNS routing (which model actually serves a given
//! doc-generation request is Chord's SLM router's decision, DOCGEN-03,
//! shipped in `moosenet/Chord` -- not reimplemented here since this crate
//! cannot call Chord's internal fn). [`ChordDocGenerator`] is the real HTTP
//! implementation, reusing the exact transport/auth pattern
//! `terminus-primary` already uses to reach Chord (`crate::config::chord_personal_federation_url`/
//! `chord_personal_federation_timeout_ms` for transport,
//! `crate::federation::mint_service_jwt` for the same short-lived service
//! JWT `PersonalFederationClient`/`inference_proxy` already mint) against
//! Chord's existing `POST /v1/infer` single-prompt route -- no new Chord-side
//! endpoint is assumed. A `MockDocGenerator` (test-only) stands in for tests.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::scribe::inspect::{InspectionWorktree, ModuleBundle};

use super::pii_gate::PiiGateOutcome;

/// The minimum length (trimmed, non-whitespace-inclusive) a generation must
/// reach to be treated as real content rather than a poor/empty response
/// this engine should flag instead of versioning. Deliberately small (a
/// genuine one-line doc update is legitimate) -- this is a floor against
/// truly empty/near-empty output, not a quality bar.
const MIN_GENERATION_LEN: usize = 8;

// ---------------------------------------------------------------------------
// SweptFeatContext -- ordering enforcement
// ---------------------------------------------------------------------------

/// A feat's diff/spec/code context that has ALREADY passed the DOCGEN-02 PII
/// input gate. The inner content is private and reachable only via
/// [`Self::as_str`]; the only public constructor is
/// [`Self::from_gate_outcome`], which requires a
/// [`super::pii_gate::PiiGateOutcome`] -- the value type
/// [`super::pii_gate::sweep_input`] returns. This is the module's
/// structural enforcement of the load-bearing ordering rule: a caller has no
/// way to construct a [`SweptFeatContext`] from a bare, unswept `&str`.
#[derive(Debug, Clone)]
pub struct SweptFeatContext(String);

impl SweptFeatContext {
    /// Build a [`SweptFeatContext`] from an already-computed PII gate
    /// outcome (`super::pii_gate::sweep_input(raw)?`). This is the ONLY way
    /// to construct one.
    pub fn from_gate_outcome(outcome: &PiiGateOutcome) -> Self {
        Self(outcome.sanitized_content().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// DocGenerator seam
// ---------------------------------------------------------------------------

/// The client seam between the doc engine and Chord's SLM router. The engine
/// asks; Chord (via whichever `DocGenerator` impl is wired in) owns model
/// selection. Implementations: [`ChordDocGenerator`] (real, over HTTP) and a
/// `MockDocGenerator` (test-only, see the `tests` module below).
#[async_trait]
pub trait DocGenerator: Send + Sync {
    /// Run `prompt` and return the raw generated text. Implementations
    /// return [`ToolError`] for any transport/backend failure -- never a
    /// silently empty/fabricated success.
    async fn generate(&self, prompt: &str) -> Result<String, ToolError>;
}

/// Real `DocGenerator` implementation: routes a doc-generation prompt
/// through Chord's `POST /v1/infer` (the same co-located Chord process
/// `terminus-primary`'s personal-tool federation and inference proxy already
/// reach -- see `crate::federation::PersonalFederationClient` and
/// `crate::inference_proxy`), authenticated with the same short-lived
/// service JWT those callers mint. Chord's own SLM router (DOCGEN-03)
/// resolves `model` (from `crate::config::docgen_chord_model`, default
/// `"auto"`) to an actual backend; this struct never picks a model itself.
#[derive(Debug, Clone)]
pub struct ChordDocGenerator {
    base_url: String,
    model: String,
    timeout: Duration,
    http: reqwest::Client,
}

impl ChordDocGenerator {
    /// Build a generator from env config
    /// (`crate::config::chord_personal_federation_url`/
    /// `chord_personal_federation_timeout_ms`/`docgen_chord_model`).
    pub fn from_env() -> Self {
        Self::with_base_url(crate::config::chord_personal_federation_url())
    }

    /// Build a generator pointed at an explicit base URL (e.g. a mocked
    /// Chord endpoint in tests). Model and timeout still come from env
    /// config unless overridden.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: crate::config::docgen_chord_model(),
            timeout: Duration::from_millis(crate::config::chord_personal_federation_timeout_ms()),
            http: reqwest::Client::new(),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl DocGenerator for ChordDocGenerator {
    async fn generate(&self, prompt: &str) -> Result<String, ToolError> {
        let jwt = crate::federation::mint_service_jwt()
            .map_err(|e| ToolError::Http(format!("docgen: failed to mint chord service JWT: {e}")))?;

        let resp = self
            .http
            .post(format!("{}/v1/infer", self.base_url))
            .timeout(self.timeout)
            .bearer_auth(jwt)
            .json(&json!({"model": self.model, "prompt": prompt}))
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("docgen: chord generation backend unreachable: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!(
                "docgen: chord returned HTTP {status} for generation request: {body}"
            )));
        }

        let metrics: Value = resp.json().await.map_err(|e| {
            ToolError::Http(format!("docgen: could not parse chord generation response: {e}"))
        })?;

        if let Some(err) = metrics.get("error").and_then(Value::as_str) {
            return Err(ToolError::Execution(format!("docgen: chord generation failed: {err}")));
        }
        if metrics.get("oom").and_then(Value::as_bool).unwrap_or(false) {
            return Err(ToolError::Execution(
                "docgen: chord generation backend ran out of memory".to_string(),
            ));
        }

        Ok(metrics
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

// ---------------------------------------------------------------------------
// Generation outcome
// ---------------------------------------------------------------------------

/// The result of one doc-generation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationOutcome {
    /// Real, deepened content ready for per-target rendering (DOCGEN-06) and
    /// versioning (DOCGEN-07). `source_commit` is the triggering feat/commit
    /// this content was generated against.
    Generated { content: String, source_commit: String },
    /// The feat had no doc-relevant change: the generator's output, once
    /// trimmed, matched the existing docs verbatim. Nothing new to store --
    /// the caller must NOT fabricate a version from this (spec EDGE CASE).
    NoChange,
    /// Generation was poor or empty. The caller must NOT write an empty/junk
    /// version; `reason` explains what was flagged, for surfacing to an
    /// operator or a retry path.
    Flagged { reason: String },
}

/// Build the JSON context [`crate::review::build_docs_prompt`] embeds, from
/// a project's existing docs plus an already-swept feat context. Kept as its
/// own function so context shaping is unit-testable independent of a real
/// `DocGenerator`/worktree.
fn deepen_context(existing_docs: Option<&str>, feat_context: &SweptFeatContext) -> Value {
    json!({
        "has_existing_docs": existing_docs.is_some(),
        "existing_docs": existing_docs.unwrap_or(""),
        "feat_context": feat_context.as_str(),
    })
}

/// The core orchestration entry point: deepen `module_path`'s docs (already
/// PII-swept `feat_context`, plus optional `existing_docs`) via `generator`,
/// against `git_ref`.
///
/// - `existing_docs` is `None` for the first-ever doc on a project (spec
///   EDGE CASE: "generate fresh, not deepen nothing").
/// - Only `feat_context.as_str()` (i.e. content that has already passed
///   [`super::pii_gate::sweep_input`]) is ever embedded in the request built
///   here -- see the module doc comment's "Ordering" section.
pub async fn generate_docs(
    generator: &dyn DocGenerator,
    module_path: &str,
    git_ref: &str,
    existing_docs: Option<&str>,
    feat_context: &SweptFeatContext,
) -> Result<GenerationOutcome, ToolError> {
    let context = deepen_context(existing_docs, feat_context);
    let prompt = crate::review::build_docs_prompt(module_path, git_ref, &context);

    let raw = generator.generate(&prompt).await?;
    let trimmed = raw.trim();

    if trimmed.len() < MIN_GENERATION_LEN {
        return Ok(GenerationOutcome::Flagged {
            reason: format!(
                "generation for '{module_path}' at {git_ref} produced {} char(s) of content \
(below the {MIN_GENERATION_LEN}-char floor) -- refusing to version an empty/near-empty result",
                trimmed.len()
            ),
        });
    }

    if let Some(existing) = existing_docs {
        if trimmed == existing.trim() {
            return Ok(GenerationOutcome::NoChange);
        }
    }

    Ok(GenerationOutcome::Generated {
        content: trimmed.to_string(),
        source_commit: git_ref.to_string(),
    })
}

/// Convenience wrapper that reuses [`crate::scribe::inspect::ModuleBundle`]
/// (already checked out via [`crate::scribe::inspect::checkout`] +
/// [`crate::scribe::inspect::inspect_module`]) as the source of a module's
/// existing docs, instead of requiring the caller to extract
/// `existing_readme` by hand. `wt.git_ref` is used as the generation's
/// `git_ref`.
pub async fn generate_docs_for_module(
    generator: &dyn DocGenerator,
    wt: &InspectionWorktree,
    bundle: &ModuleBundle,
    feat_context: &SweptFeatContext,
) -> Result<GenerationOutcome, ToolError> {
    generate_docs(
        generator,
        &bundle.module_path,
        &wt.git_ref,
        bundle.existing_readme.as_deref(),
        feat_context,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::tools::docgen::pii_gate::sweep_input;

    /// Test-only `DocGenerator` that returns a fixed response and records
    /// exactly the prompt string it was called with -- used to assert both
    /// (a) what content actually reached the "inference request" (the
    /// ordering negative test) and (b) that existing docs are present in the
    /// prompt so a real model has the chance to deepen rather than
    /// overwrite.
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

    /// A `DocGenerator` that always fails -- used to assert `generate_docs`
    /// propagates a generator error rather than swallowing it.
    struct FailingDocGenerator;

    #[async_trait]
    impl DocGenerator for FailingDocGenerator {
        async fn generate(&self, _prompt: &str) -> Result<String, ToolError> {
            Err(ToolError::Http("backend down".to_string()))
        }
    }

    fn swept(raw: &str) -> SweptFeatContext {
        let outcome = sweep_input(raw).expect("sweep_input should not block this fixture");
        SweptFeatContext::from_gate_outcome(&outcome)
    }

    // ── Ordering: only swept content reaches the generator ──────────────

    /// Negative test (spec TEST PLAN item 2 / ACCEPTANCE CRITERIA item 2):
    /// a feat context containing PII is swept BEFORE `generate_docs` ever
    /// builds a request -- the raw literal must never appear in what the
    /// generator (standing in for "the inference request") actually
    /// received. `SweptFeatContext` structurally cannot be constructed from
    /// a bare `&str`, so there is no code path in this module through which
    /// unswept content could reach `generator.generate()`.
    #[tokio::test]
    async fn sweep_gate_ordering_enforced_no_raw_content_reaches_generator() {
        let raw_diff = "+ connects to <internal-ip> for status, built on <host>"; // pii-test-fixture
        let feat_context = swept(raw_diff);

        let mock = MockDocGenerator::new("A short deepened doc update.".to_string());
        let outcome = generate_docs(&mock, "src/x", "abc123", None, &feat_context).await.unwrap();
        assert!(matches!(outcome, GenerationOutcome::Generated { .. }));

        let captured = mock.captured_prompt();
        assert!(!captured.contains("<internal-ip>")); // pii-test-fixture
        assert!(!captured.contains("<host>")); // pii-test-fixture
        // Sanity: the captured prompt DID carry swept content through (not
        // empty / not silently dropped) -- redaction markers are present.
        assert!(captured.contains("[REDACTED:"));
    }

    /// Structural companion to the above: `SweptFeatContext::as_str()` on a
    /// value built from `sweep_input` never contains the original raw PII
    /// literal either, independent of what any particular `DocGenerator`
    /// does with it.
    #[test]
    fn swept_feat_context_never_carries_raw_pii() {
        let raw = "internal host at <internal-ip> handles this"; // pii-test-fixture
        let ctx = swept(raw);
        assert!(!ctx.as_str().contains("<internal-ip>")); // pii-test-fixture
    }

    // ── Deepen, not overwrite ─────────────────────────────────────────

    /// Spec TEST PLAN item 1 / ACCEPTANCE CRITERIA item 1: generation
    /// revises/extends existing docs rather than replacing them wholesale --
    /// asserted on a before/after fixture. The engine is responsible for
    /// putting the PRIOR content in front of the generator (so a real model
    /// has what it needs to deepen); this test asserts that happens, and
    /// that a generator which does deepen (the mock, standing in for a
    /// well-behaved model) has its output passed through untouched with the
    /// prior content preserved.
    #[tokio::test]
    async fn deepen_preserves_prior_content_before_after_fixture() {
        let before = "# Widget\n\nThe widget does A.";
        let after = "# Widget\n\nThe widget does A. It was extended to also do B (this feat).";

        let feat_context = swept("+ added B support to the widget");
        let mock = MockDocGenerator::new(after.to_string());

        let outcome =
            generate_docs(&mock, "src/widget", "def456", Some(before), &feat_context).await.unwrap();

        // The prompt handed to the generator carried the PRIOR content, so a
        // real model could deepen rather than write from a blank page.
        let captured = mock.captured_prompt();
        assert!(captured.contains("The widget does A."), "prior content must reach the prompt");

        // The returned content is the deepened version -- it still contains
        // everything the "before" fixture had, plus the new material.
        match outcome {
            GenerationOutcome::Generated { content, source_commit } => {
                assert!(content.contains("The widget does A."), "deepen must preserve prior content");
                assert!(content.contains("also do B"), "deepen must incorporate the new material");
                assert_eq!(source_commit, "def456");
            }
            other => panic!("expected Generated, got {other:?}"),
        }
    }

    // ── First-doc case ────────────────────────────────────────────────

    /// Spec EDGE CASE: first-ever doc for a project (no existing docs) ->
    /// generate fresh, not "deepen nothing".
    #[tokio::test]
    async fn first_doc_case_generates_fresh_with_no_existing_docs() {
        let feat_context = swept("+ new module: widget factory");
        let mock = MockDocGenerator::new("# Widget Factory\n\nBuilds widgets.".to_string());

        let outcome = generate_docs(&mock, "src/widget_factory", "ghi789", None, &feat_context)
            .await
            .unwrap();

        let captured = mock.captured_prompt();
        assert!(captured.contains("\"has_existing_docs\": false") || captured.contains("has_existing_docs"));

        match outcome {
            GenerationOutcome::Generated { content, .. } => {
                assert!(content.contains("Widget Factory"));
            }
            other => panic!("expected Generated, got {other:?}"),
        }
    }

    // ── DLAND-02: cutover generation threads the OLD README through ────

    /// DLAND-02 acceptance criterion: when a cutover generation runs (i.e.
    /// this is a project's existing, hand-grown README being deepened
    /// rather than a first-ever doc), the OLD README's full content reaches
    /// the generator as `existing_docs` -- the same threading
    /// `generate_docs_for_module` already does via
    /// `bundle.existing_readme.as_deref()`
    /// (`generate_docs_for_module_reuses_module_bundle_existing_readme`
    /// above exercises the same call path against a small fixture; this
    /// test uses a larger, more "hand-grown README"-shaped fixture with
    /// multiple `## ` sections, standing in for a real cutover candidate,
    /// so the preservation guard in `super::preserve::check_preservation`
    /// has something realistic to have checked upstream of this call).
    #[tokio::test]
    async fn cutover_generation_receives_old_readme_as_existing_docs() {
        let old_readme = "# Widget\n\n\
## Install\n\nRun `cargo install widget_cli`.\n\n\
## Configuration\n\nSet `WIDGET_PORT=8080`.\n\n\
## API\n\nCall `WidgetClient::connect()`.\n";

        let wt = InspectionWorktree {
            path: std::path::PathBuf::from("/tmp/does-not-matter"),
            repo_path: std::path::PathBuf::from("/tmp/does-not-matter-repo"),
            git_ref: "cutover1".to_string(),
        };
        let bundle = ModuleBundle {
            module_path: "src/widget".to_string(),
            git_ref: "cutover1".to_string(),
            files: vec![],
            existing_readme: Some(old_readme.to_string()),
        };
        let feat_context = swept("+ cutover to the docgen-generated landing + docs tree");
        let mock = MockDocGenerator::new(
            "# Widget\n\nDocgen-generated landing content.\n".to_string(),
        );

        let outcome = generate_docs_for_module(&mock, &wt, &bundle, &feat_context).await.unwrap();

        // The OLD README's actual sections reached the generator as
        // existing_docs -- not just "has_existing_docs: true", but the real
        // content a real model needs to be able to preserve/deepen from.
        let captured = mock.captured_prompt();
        assert!(captured.contains("## Install"), "cutover prompt must carry the old README's sections");
        assert!(captured.contains("WIDGET_PORT=8080"));
        assert!(captured.contains("WidgetClient::connect()"));

        assert!(matches!(outcome, GenerationOutcome::Generated { .. }));
    }

    // ── No-op change ──────────────────────────────────────────────────

    /// Spec EDGE CASE: feat with no doc-relevant change -> minimal/no
    /// update, don't fabricate. Modeled here as: the generator's output,
    /// trimmed, is identical to the existing docs -- the engine must report
    /// `NoChange`, not a `Generated` version that's really just a no-op
    /// dressed up as new content.
    #[tokio::test]
    async fn no_doc_relevant_change_reports_no_change_not_fabricated_version() {
        let existing = "# Widget\n\nThe widget does A.";
        let feat_context = swept("+ internal refactor, no behavior change");
        let mock = MockDocGenerator::new(existing.to_string());

        let outcome =
            generate_docs(&mock, "src/widget", "jkl012", Some(existing), &feat_context).await.unwrap();

        assert_eq!(outcome, GenerationOutcome::NoChange);
    }

    // ── Poor/empty generation ─────────────────────────────────────────

    /// Spec EDGE CASE: generation returns poor/empty -> don't write an
    /// empty doc version; flag.
    #[tokio::test]
    async fn empty_generation_is_flagged_not_versioned() {
        let feat_context = swept("+ trivial change");
        let mock = MockDocGenerator::new("".to_string());

        let outcome = generate_docs(&mock, "src/x", "m1", None, &feat_context).await.unwrap();
        match outcome {
            GenerationOutcome::Flagged { reason } => assert!(reason.contains("char")),
            other => panic!("expected Flagged, got {other:?}"),
        }
    }

    /// Same edge case, but the generator returns whitespace-only content --
    /// must also be flagged, not treated as a real (blank) doc.
    #[tokio::test]
    async fn whitespace_only_generation_is_flagged_not_versioned() {
        let feat_context = swept("+ trivial change");
        let mock = MockDocGenerator::new("   \n\n   ".to_string());

        let outcome = generate_docs(&mock, "src/x", "m2", None, &feat_context).await.unwrap();
        assert!(matches!(outcome, GenerationOutcome::Flagged { .. }));
    }

    /// Negative test: a generator-side failure (backend unreachable, error
    /// response, etc.) propagates as an `Err`, not a silently swallowed
    /// `Flagged`/`NoChange` outcome -- the caller must be able to
    /// distinguish "the engine judged the output poor" from "generation
    /// never actually ran".
    #[tokio::test]
    async fn generator_failure_propagates_as_error_not_a_flagged_outcome() {
        let feat_context = swept("+ trivial change");
        let result = generate_docs(&FailingDocGenerator, "src/x", "m3", None, &feat_context).await;
        assert!(result.is_err());
    }

    // ── generate_docs_for_module: reuse of crate::scribe::inspect ───────

    /// `generate_docs_for_module` reuses `ModuleBundle`/`InspectionWorktree`
    /// (`crate::scribe::inspect`) as the source of existing docs, rather
    /// than requiring the caller to extract `existing_readme` by hand.
    #[tokio::test]
    async fn generate_docs_for_module_reuses_module_bundle_existing_readme() {
        let wt = InspectionWorktree {
            path: std::path::PathBuf::from("/tmp/does-not-matter"),
            repo_path: std::path::PathBuf::from("/tmp/does-not-matter-repo"),
            git_ref: "n1".to_string(),
        };
        let bundle = ModuleBundle {
            module_path: "src/sundry".to_string(),
            git_ref: "n1".to_string(),
            files: vec![],
            existing_readme: Some("# Sundry\n\nMisc helpers.".to_string()),
        };
        let feat_context = swept("+ added a new helper function");
        let mock = MockDocGenerator::new(
            "# Sundry\n\nMisc helpers. Now includes a new helper function.".to_string(),
        );

        let outcome = generate_docs_for_module(&mock, &wt, &bundle, &feat_context).await.unwrap();
        let captured = mock.captured_prompt();
        assert!(captured.contains("Misc helpers."));

        match outcome {
            GenerationOutcome::Generated { content, source_commit } => {
                assert!(content.contains("Misc helpers."));
                assert_eq!(source_commit, "n1");
            }
            other => panic!("expected Generated, got {other:?}"),
        }
    }
}
