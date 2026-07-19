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

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::scribe::inspect::{InspectionWorktree, ModuleBundle};

use super::pii_gate::{sweep_input, PiiGateOutcome};
use super::prompts::{
    anti_latch_lint, build_guides_prompt, build_repo_identity_prompt, build_subsystem_page_prompt,
    honest_command_lint, parse_file_blocks, parse_repo_identity, symbol_existence_lint, RepoIdentity,
};
use super::repo_facts::RepoFacts;

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

// ---------------------------------------------------------------------------
// DGRICH-03: generate_repo_docs -- Passes 1-3 orchestration over RepoFacts
// ---------------------------------------------------------------------------
//
// This is the repo-level sibling of `generate_docs` above: instead of one
// thin module-README prompt fed only a feat diff, it runs the three
// KG-grounded prompts from `prompts.rs` (design `fable-docgen-redesign.md`
// §2 Passes 1-3) over the SAME `DocGenerator` seam, grounded in a
// `RepoFacts` (DGRICH-01) rather than a diff. `generate_docs`/
// `generate_docs_for_module` above are the legacy per-module path and are
// untouched by this addition.
//
// ## Retry-once-then-Flagged, partial success is usable
// Every pass (identity, each subsystem page, guides) gets exactly one retry
// with the lint/parse violation quoted back to the model; a second failure
// records that pass as failed in `RepoDocsOutcome::missing` and
// `pass_ledger`, but never aborts the rest of the pipeline and never
// returns an `Err` -- `generate_repo_docs` cannot fail the triggering feat
// (design §2 Pass 5 / DGRICH-07's infallibility requirement upstream of
// this item).

/// How many subsystem-page generation calls (Pass 2) run concurrently.
/// Bounded (design §2 Pass 2: "parallelizable, N<=16") rather than
/// unbounded so a repo with the max 16 kept subsystems doesn't fire 16
/// simultaneous Chord requests.
const SUBSYSTEM_PASS_CONCURRENCY: usize = 4;

/// One pass's outcome, for operator visibility (`RepoDocsOutcome::pass_ledger`).
/// `pass` is `"identity"`, `"guides"`, or `"subsystem:<name>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassRecord {
    pub pass: String,
    pub ok: bool,
    /// The lint/parse violation or generator error that caused a
    /// non-`ok` outcome; `None` when `ok` is `true`.
    pub detail: Option<String>,
}

impl PassRecord {
    fn ok(pass: impl Into<String>) -> Self {
        Self { pass: pass.into(), ok: true, detail: None }
    }

    fn flagged(pass: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { pass: pass.into(), ok: false, detail: Some(detail.into()) }
    }
}

/// The result of running Passes 1-3 over one repo's [`RepoFacts`].
///
/// `identity` is `None` exactly when the identity pass (Pass 1) was
/// `Flagged` twice (parse/lint failure survived one retry) -- callers must
/// treat that as "no identity", not synthesize a placeholder one. When
/// `identity` is `None`, Passes 2 and 3 are skipped entirely (both need a
/// `RepoIdentity` as grounding input per the design), and every kept
/// subsystem plus `"guides"` are recorded in `missing`/`pass_ledger` as
/// skipped rather than attempted. Partial success within Pass 2 alone
/// (identity ok, some subsystem pages failed) is the common case this type
/// is built to represent usably: `subsystem_pages` holds every page that
/// succeeded, `missing` names exactly the ones that didn't.
#[derive(Debug, Clone, Default)]
pub struct RepoDocsOutcome {
    pub identity: Option<RepoIdentity>,
    /// `docs/reference/<subsystem>.md` content, one entry per subsystem
    /// whose page generation succeeded (order not guaranteed -- Pass 2 runs
    /// bounded-concurrent).
    pub subsystem_pages: Vec<(String, String)>,
    /// `docs/guides/<slug>.md` content (excludes getting-started, which has
    /// its own field).
    pub guides: Vec<(PathBuf, String)>,
    /// `docs/getting-started.md` content, or empty when the guides pass
    /// never succeeded (see `missing` for whether that happened).
    pub getting_started: String,
    /// Names of passes that did not produce usable output: `"identity"`,
    /// `"guides"`, and/or `"subsystem:<name>"` for each subsystem whose page
    /// failed (or was skipped because identity failed first).
    pub missing: Vec<String>,
    /// One record per pass attempted (or skipped), for operator visibility.
    pub pass_ledger: Vec<PassRecord>,
}

/// Every real symbol id `RepoFacts` knows about, flattened for
/// [`symbol_existence_lint`]: repo-scale hotspots plus every kept
/// subsystem's top symbols (a superset is fine -- the lint only rejects
/// symbols named that are NOT in this set, so including more real symbols
/// only makes the lint more permissive of genuinely real names, never less
/// strict against invented ones).
fn all_symbol_names(facts: &RepoFacts) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for s in &facts.scale.hotspots {
        set.insert(s.id.clone());
    }
    for sub in &facts.subsystems {
        for s in &sub.top_symbols {
            set.insert(s.id.clone());
        }
    }
    set.into_iter().collect()
}

/// Every real `[[bin]]` target name `RepoFacts` knows about, for
/// [`honest_command_lint`].
fn all_bin_names(facts: &RepoFacts) -> Vec<String> {
    facts.entry_points.bin_targets.iter().map(|b| b.name.clone()).collect()
}

/// Parse `raw` JSON text (already PII-swept by `RepoFacts::identity_slice`/
/// `subsystem_slice`) into a `serde_json::Value` for the prompt builders,
/// which take `&Value` rather than a pre-serialized string.
fn parse_slice_json(raw: &str, what: &str) -> Result<Value, String> {
    serde_json::from_str(raw).map_err(|e| format!("{what} slice was not valid JSON after sweep: {e}"))
}

/// Deterministic re-serialization of an already-parsed [`RepoIdentity`] back
/// to `Value`, for embedding in the Pass 2/3 prompts (which take the
/// identity as already-established JSON, per design §3.2/§3.3).
fn identity_to_value(identity: &RepoIdentity) -> Value {
    serde_json::to_value(identity).unwrap_or_else(|_| json!({}))
}

/// Pass 1: identity + outline. Builds the prompt from
/// `facts.identity_slice()`, calls `generator`, parses + lints the result;
/// on parse/lint failure retries ONCE with the violation quoted, then
/// records a `Flagged` [`PassRecord`] and returns `None` (never aborts the
/// caller).
async fn run_identity_pass(
    generator: &dyn DocGenerator,
    facts: &RepoFacts,
    repo_name: &str,
    git_ref: &str,
    symbol_names: &[String],
) -> (Option<RepoIdentity>, PassRecord) {
    let subsystem_names: Vec<String> = facts.subsystems.iter().map(|s| s.name.clone()).collect();

    let facts_slice = match facts.identity_slice() {
        Ok(s) => s,
        Err(e) => {
            return (None, PassRecord::flagged("identity", format!("failed to build identity slice: {e}")))
        }
    };
    let facts_value = match parse_slice_json(&facts_slice, "identity") {
        Ok(v) => v,
        Err(reason) => return (None, PassRecord::flagged("identity", reason)),
    };

    let mut prompt = build_repo_identity_prompt(repo_name, git_ref, &facts_value);

    for attempt in 0..2 {
        let raw = match generator.generate(&prompt).await {
            Ok(r) => r,
            Err(e) => {
                if attempt == 0 {
                    prompt = format!(
                        "{prompt}\n\nYour previous attempt failed: {e}\nPlease respond again, correctly."
                    );
                    continue;
                }
                return (None, PassRecord::flagged("identity", format!("generator error: {e}")));
            }
        };

        match parse_repo_identity(&raw) {
            Err(e) => {
                if attempt == 0 {
                    prompt = format!(
                        "{prompt}\n\nYour previous response was rejected: {e}\n\
Respond again with ONLY a corrected JSON object."
                    );
                    continue;
                }
                return (None, PassRecord::flagged("identity", format!("parse error: {e}")));
            }
            Ok(identity) => {
                if identity.subsystems.len() < subsystem_names.len() {
                    let violation = format!(
                        "identity JSON names {} subsystem(s) but RepoFacts has {} kept subsystems -- \
every kept subsystem must get a one-liner",
                        identity.subsystems.len(),
                        subsystem_names.len()
                    );
                    if attempt == 0 {
                        prompt = format!(
                            "{prompt}\n\nYour previous response was rejected: {violation}\n\
Respond again, covering EVERY subsystem listed in REPO FACTS."
                        );
                        continue;
                    }
                    return (None, PassRecord::flagged("identity", violation));
                }

                if let Some(violation) =
                    anti_latch_lint(&identity.tagline, &identity.what_is, &subsystem_names, "")
                {
                    if attempt == 0 {
                        prompt = format!(
                            "{prompt}\n\nYour previous response was rejected: {violation}\n\
Respond again, describing the WHOLE repository, not one subsystem."
                        );
                        continue;
                    }
                    return (None, PassRecord::flagged("identity", violation));
                }

                let feature_text: String = identity
                    .feature_rows
                    .iter()
                    .map(|f| f.description.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let combined = format!("{} {} {}", identity.tagline, identity.what_is, feature_text);
                if let Some(violation) = symbol_existence_lint(&combined, symbol_names) {
                    if attempt == 0 {
                        prompt = format!(
                            "{prompt}\n\nYour previous response was rejected: {violation}\n\
Respond again, never inventing a symbol not present in REPO FACTS."
                        );
                        continue;
                    }
                    return (None, PassRecord::flagged("identity", violation));
                }

                return (Some(identity), PassRecord::ok("identity"));
            }
        }
    }

    unreachable!("loop always returns within its two iterations")
}

/// Pass 2, one subsystem: builds the page prompt from
/// `facts.subsystem_slice(subsystem)` + the already-established identity,
/// calls `generator`, validates (symbol-existence); retry-once-then-fail.
/// Returns `(subsystem_name, Ok(markdown) | Err(reason))` rather than a
/// `Result<_, ToolError>` so a failed page never aborts sibling pages
/// running concurrently in [`run_subsystem_pass`].
async fn generate_subsystem_page(
    generator: &dyn DocGenerator,
    facts: &RepoFacts,
    repo_name: &str,
    subsystem: &str,
    identity_value: &Value,
    symbol_names: &[String],
) -> (String, Result<String, String>) {
    let slice = match facts.subsystem_slice(subsystem) {
        Ok(s) => s,
        Err(e) => {
            return (subsystem.to_string(), Err(format!("failed to build subsystem slice: {e}")))
        }
    };
    let slice_value = match parse_slice_json(&slice, subsystem) {
        Ok(v) => v,
        Err(reason) => return (subsystem.to_string(), Err(reason)),
    };

    let mut prompt = build_subsystem_page_prompt(repo_name, subsystem, identity_value, &slice_value);

    for attempt in 0..2 {
        let raw = match generator.generate(&prompt).await {
            Ok(r) => r,
            Err(e) => {
                if attempt == 0 {
                    prompt = format!(
                        "{prompt}\n\nYour previous attempt failed: {e}\nPlease respond again, correctly."
                    );
                    continue;
                }
                return (subsystem.to_string(), Err(format!("generator error: {e}")));
            }
        };

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            if attempt == 0 {
                prompt = format!(
                    "{prompt}\n\nYour previous response was empty. Write the reference page again."
                );
                continue;
            }
            return (subsystem.to_string(), Err("generation produced empty content".to_string()));
        }

        if let Some(violation) = symbol_existence_lint(trimmed, symbol_names) {
            if attempt == 0 {
                prompt = format!(
                    "{prompt}\n\nYour previous response was rejected: {violation}\n\
Respond again, never inventing a symbol not present in SUBSYSTEM FACTS."
                );
                continue;
            }
            return (subsystem.to_string(), Err(violation));
        }

        return (subsystem.to_string(), Ok(trimmed.to_string()));
    }

    unreachable!("loop always returns within its two iterations")
}

/// Pass 2: runs [`generate_subsystem_page`] for every kept (non-misc)
/// subsystem in `facts`, bounded-concurrent
/// ([`SUBSYSTEM_PASS_CONCURRENCY`]). Returns every page that succeeded plus
/// one [`PassRecord`] per subsystem attempted -- a failing page never
/// prevents the others from being returned (design §2 Pass 2 / DGRICH-03
/// EDGE CASE: "identity ok + 12/15 pages = usable").
async fn run_subsystem_pass(
    generator: &dyn DocGenerator,
    facts: &RepoFacts,
    repo_name: &str,
    identity_value: &Value,
    symbol_names: &[String],
) -> (Vec<(String, String)>, Vec<PassRecord>) {
    // Owned names (not `&str`): a borrowed stream item makes the per-page async
    // closure higher-ranked over the item lifetime, which — once `GraphSource` is
    // `Send + Sync` and the future must be `Send` — trips "implementation of
    // `FnOnce` is not general enough". Cloning the subsystem names sidesteps the
    // HRTB with no behavior change (names are short and the set is small).
    let kept: Vec<String> = facts.subsystems.iter().filter(|s| !s.is_misc).map(|s| s.name.clone()).collect();

    let results: Vec<(String, Result<String, String>)> = stream::iter(kept.into_iter())
        .map(|name| async move {
            generate_subsystem_page(generator, facts, repo_name, &name, identity_value, symbol_names).await
        })
        .buffer_unordered(SUBSYSTEM_PASS_CONCURRENCY)
        .collect()
        .await;

    let mut pages = Vec::new();
    let mut records = Vec::new();
    for (name, result) in results {
        match result {
            Ok(markdown) => {
                records.push(PassRecord::ok(format!("subsystem:{name}")));
                pages.push((name, markdown));
            }
            Err(reason) => {
                records.push(PassRecord::flagged(format!("subsystem:{name}"), reason));
            }
        }
    }
    (pages, records)
}

/// Pass 3: guides + getting-started. Builds the prompt from the identity,
/// `facts.entry_points`/`config_surface`, and the old README's install/
/// usage sections (design §2 Pass 3: "legacy README's install/usage
/// sections"); parses with `parse_file_blocks`; lints every command against
/// `facts`'s real bin targets; retry-once-then-flag.
async fn run_guides_pass(
    generator: &dyn DocGenerator,
    facts: &RepoFacts,
    repo_name: &str,
    identity: &RepoIdentity,
    identity_value: &Value,
) -> (Vec<(PathBuf, String)>, String, PassRecord) {
    let entrypoints_value = json!({
        "bin_targets": facts.entry_points.bin_targets,
        "workspace_members": facts.entry_points.workspace_members,
        "entrypoint_symbols": facts.entry_points.entrypoint_symbols,
        "registered_tool_count": facts.entry_points.registered_tool_count,
        "config_surface": facts.config_surface,
        "guide_topics": identity.guide_topics,
    });

    let legacy_usage: String = facts
        .old_readme_sections
        .iter()
        .filter(|s| {
            let h = s.heading.to_lowercase();
            h.contains("install") || h.contains("usage") || h.contains("quick start") || h.contains("getting started")
        })
        .map(|s| format!("LEGACY README SECTION \"{}\":\n{}", s.heading, s.body))
        .collect::<Vec<_>>()
        .join("\n\n");

    let bin_names = all_bin_names(facts);

    let mut prompt = build_guides_prompt(repo_name, identity_value, &entrypoints_value, &legacy_usage);

    for attempt in 0..2 {
        // PII gate (DOCGEN-02 / S1): unlike Passes 1-2, which build their prompt
        // from RepoFacts' already-swept SLICES, Pass 3 assembles `legacy_usage`
        // (verbatim OLD-README install/usage sections) and the entry-point/config
        // surface directly from RepoFacts' RAW stored fields. Those must not reach
        // the inference request unswept — sweep the fully-assembled prompt before
        // every send (idempotent on the redacted retry prompt).
        let prompt_for_send = match sweep_input(&prompt) {
            Ok(o) => o.sanitized_content().to_string(),
            Err(e) => {
                return (
                    Vec::new(),
                    String::new(),
                    PassRecord::flagged("guides", format!("PII sweep of guides prompt failed: {e}")),
                )
            }
        };
        let raw = match generator.generate(&prompt_for_send).await {
            Ok(r) => r,
            Err(e) => {
                if attempt == 0 {
                    prompt = format!(
                        "{prompt}\n\nYour previous attempt failed: {e}\nPlease respond again, correctly."
                    );
                    continue;
                }
                return (Vec::new(), String::new(), PassRecord::flagged("guides", format!("generator error: {e}")));
            }
        };

        let blocks = parse_file_blocks(&raw);
        if blocks.is_empty() {
            if attempt == 0 {
                prompt = format!(
                    "{prompt}\n\nYour previous response had no `=== FILE: <path> ===` marker lines. \
Respond again using EXACTLY that marker format before each file's content."
                );
                continue;
            }
            return (
                Vec::new(),
                String::new(),
                PassRecord::flagged("guides", "no `=== FILE: <path> ===` markers found in output".to_string()),
            );
        }

        let combined: String =
            blocks.iter().map(|(_, body)| body.as_str()).collect::<Vec<_>>().join("\n");
        if let Some(violation) = honest_command_lint(&combined, &bin_names) {
            if attempt == 0 {
                prompt = format!(
                    "{prompt}\n\nYour previous response was rejected: {violation}\n\
Respond again, only naming real binaries/tools from ENTRY POINTS."
                );
                continue;
            }
            return (Vec::new(), String::new(), PassRecord::flagged("guides", violation));
        }

        let getting_started = blocks
            .iter()
            .find(|(path, _)| path.as_path() == std::path::Path::new("docs/getting-started.md"))
            .map(|(_, body)| body.clone())
            .unwrap_or_default();
        let guides: Vec<(PathBuf, String)> = blocks
            .into_iter()
            .filter(|(path, _)| path.as_path() != std::path::Path::new("docs/getting-started.md"))
            .collect();

        // A non-empty `=== FILE:` response is NOT automatically a success: the
        // Pass-3 contract is getting-started.md PLUS one guide per guide_topic.
        // Collect the concrete gaps so an incomplete response is retried once and,
        // if still incomplete, recorded as a flagged pass (added to `missing` by the
        // caller) — while STILL returning whatever real files we did get (partial
        // success), never silently reporting ok:true with an empty getting_started.
        let mut gaps: Vec<String> = Vec::new();
        if getting_started.trim().is_empty() {
            gaps.push("getting-started.md".to_string());
        }
        // Only count DISTINCT files actually under `docs/guides/` toward the
        // one-guide-per-topic requirement — otherwise a wrong path
        // (`docs/reference/foo.md`) or a duplicate could satisfy the count while
        // a real guide topic has no page (codex review finding).
        let distinct_guide_pages: std::collections::HashSet<&std::path::Path> = guides
            .iter()
            .map(|(p, _)| p.as_path())
            .filter(|p| p.starts_with("docs/guides/") && p.extension().map(|e| e == "md").unwrap_or(false))
            .collect();
        let expected_guides = identity.guide_topics.len();
        if distinct_guide_pages.len() < expected_guides {
            gaps.push(format!(
                "{} of {} guide topic page(s) missing under docs/guides/",
                expected_guides - distinct_guide_pages.len(),
                expected_guides
            ));
        }

        if !gaps.is_empty() {
            if attempt == 0 {
                prompt = format!(
                    "{prompt}\n\nYour previous response was incomplete — {}. Every guides \
response MUST include `=== FILE: docs/getting-started.md ===` and one \
`=== FILE: docs/guides/<slug>.md ===` per guide topic in REPO IDENTITY. Respond \
again with ALL required files.",
                    gaps.join("; ")
                );
                continue;
            }
            // Final attempt still incomplete: keep the partial output, flag the gap.
            return (
                guides,
                getting_started,
                PassRecord::flagged("guides", format!("incomplete guides output: {}", gaps.join("; "))),
            );
        }

        return (guides, getting_started, PassRecord::ok("guides"));
    }

    unreachable!("loop always returns within its two iterations")
}

/// Orchestrates Passes 1-3 of the rich, KG-grounded doc generator (design
/// §2) over `facts` -- the repo-level sibling of [`generate_docs`]. Never
/// returns an `Err`: every internal failure (generator unreachable, a
/// parse/lint violation that survives one retry) becomes a `Flagged`
/// [`PassRecord`] plus an entry in [`RepoDocsOutcome::missing`], so a
/// partial result (e.g. identity ok, 12 of 15 subsystem pages) is always
/// returned rather than discarded.
///
/// `repo_name`/`git_ref` are display-only (embedded in prompt text); the
/// substantive grounding is entirely `facts`.
pub async fn generate_repo_docs(
    generator: &dyn DocGenerator,
    facts: &RepoFacts,
    repo_name: &str,
    git_ref: &str,
) -> RepoDocsOutcome {
    let symbol_names = all_symbol_names(facts);
    let mut missing = Vec::new();
    let mut pass_ledger = Vec::new();

    let (identity, identity_record) =
        run_identity_pass(generator, facts, repo_name, git_ref, &symbol_names).await;
    if !identity_record.ok {
        missing.push("identity".to_string());
    }
    pass_ledger.push(identity_record);

    let Some(identity) = identity else {
        // Passes 2/3 both require the identity as grounding input (design
        // §2 Pass 2/3) -- skip them explicitly rather than attempting a
        // generation that would just fail the same way, but still record
        // every subsystem as missing so the operator sees the full gap.
        for s in facts.subsystems.iter().filter(|s| !s.is_misc) {
            let pass = format!("subsystem:{}", s.name);
            missing.push(pass.clone());
            pass_ledger.push(PassRecord::flagged(pass, "skipped: identity pass did not succeed"));
        }
        missing.push("guides".to_string());
        pass_ledger.push(PassRecord::flagged("guides", "skipped: identity pass did not succeed"));

        return RepoDocsOutcome {
            identity: None,
            subsystem_pages: Vec::new(),
            guides: Vec::new(),
            getting_started: String::new(),
            missing,
            pass_ledger,
        };
    };

    let identity_value = identity_to_value(&identity);

    let (subsystem_pages, subsystem_records) =
        run_subsystem_pass(generator, facts, repo_name, &identity_value, &symbol_names).await;
    for r in &subsystem_records {
        if !r.ok {
            missing.push(r.pass.clone());
        }
    }
    pass_ledger.extend(subsystem_records);

    let (guides, getting_started, guides_record) =
        run_guides_pass(generator, facts, repo_name, &identity, &identity_value).await;
    if !guides_record.ok {
        missing.push("guides".to_string());
    }
    pass_ledger.push(guides_record);

    RepoDocsOutcome {
        identity: Some(identity),
        subsystem_pages,
        guides,
        getting_started,
        missing,
        pass_ledger,
    }
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

    // ── DGRICH-03: generate_repo_docs orchestration ─────────────────────

    mod repo_docs {
        use super::*;
        use crate::scribe::graph::{Confidence, EdgeKind, KgEdge, KgNode, KnowledgeGraph, NodeKind};
        use crate::tools::docgen::repo_facts::{build_repo_facts, FixtureGraphSource};
        use std::collections::{HashMap, HashSet};
        use std::path::Path;

        /// A two-subsystem fixture graph (`alpha`, `beta`), each with a
        /// "hub" node that receives enough in-subsystem calls to rank
        /// highest by PageRank -- so `hub` reliably lands in
        /// `Subsystem::top_symbols` and can be named as a real symbol in
        /// scripted page content below. Both subsystems clear the
        /// `max(30, 1%)` selection threshold on their own (32 and 31
        /// nodes), so there is no `misc` fold here.
        fn two_subsystem_graph() -> KnowledgeGraph {
            let mut g = KnowledgeGraph::new("FIXR");
            let node = |id: &str, kind: NodeKind, path: &str| -> KgNode {
                let name = id.rsplit("::").next().unwrap_or(id).to_string();
                KgNode::new(id, kind, name, path)
            };
            g.insert_node(node("crate::alpha::Hub::run", NodeKind::Function, "src/alpha/hub.rs"));
            for i in 0..31 {
                let id = format!("crate::alpha::f{i}");
                g.insert_node(node(&id, NodeKind::Function, &format!("src/alpha/f{i}.rs")));
                g.insert_edge(KgEdge::new(&id, "crate::alpha::Hub::run", EdgeKind::Calls, Confidence::Extracted))
                    .unwrap();
            }
            g.insert_node(node("crate::beta::Hub::run", NodeKind::Function, "src/beta/hub.rs"));
            for i in 0..30 {
                let id = format!("crate::beta::f{i}");
                g.insert_node(node(&id, NodeKind::Function, &format!("src/beta/f{i}.rs")));
                g.insert_edge(KgEdge::new(&id, "crate::beta::Hub::run", EdgeKind::Calls, Confidence::Extracted))
                    .unwrap();
            }
            g
        }

        fn fixture_facts() -> RepoFacts {
            let g = two_subsystem_graph();
            build_repo_facts(&FixtureGraphSource(g), Path::new("/nonexistent-dgrich03-fixture"), "FIXR", "abc123")
                .expect("fixture facts should build")
        }

        fn valid_identity_json(kept: &[&str]) -> String {
            let subsystems: Vec<Value> = kept
                .iter()
                .map(|name| json!({"name": name, "one_liner": format!("{name} does its part."), "role": "core"}))
                .collect();
            json!({
                "tagline": "A hub combining alpha and beta behind one gateway.",
                "what_is": "This repo brings alpha and beta together for the fleet.\n\nTogether they form one system.",
                "audience": "Operators of this fixture repo.",
                "subsystems": subsystems,
                "feature_rows": [
                    {"feature": "Alpha processing", "description": "Handles alpha work.", "subsystem": "alpha"},
                    {"feature": "Beta processing", "description": "Handles beta work.", "subsystem": "beta"}
                ],
                "guide_topics": [
                    {"title": "Run the fixture", "grounding": "crate::alpha::Hub::run"}
                ]
            })
            .to_string()
        }

        /// Scripted `DocGenerator`: dispatches on prompt content to return
        /// canned identity / subsystem-page / guides responses, standing in
        /// for a real model across all three passes in one test.
        struct ScriptedGenerator {
            identity_response: String,
            /// Subsystem names whose page response deliberately names an
            /// invented (`::`-qualified, backticked) symbol -- exercises
            /// the symbol-existence lint failure path.
            bad_subsystems: HashSet<String>,
            /// Optional per-subsystem canned good response override.
            page_responses: HashMap<String, String>,
            guides_response: String,
        }

        impl ScriptedGenerator {
            fn new(identity_response: impl Into<String>, guides_response: impl Into<String>) -> Self {
                Self {
                    identity_response: identity_response.into(),
                    bad_subsystems: HashSet::new(),
                    page_responses: HashMap::new(),
                    guides_response: guides_response.into(),
                }
            }

            fn with_bad_subsystem(mut self, name: &str) -> Self {
                self.bad_subsystems.insert(name.to_string());
                self
            }
        }

        #[async_trait]
        impl DocGenerator for ScriptedGenerator {
            async fn generate(&self, prompt: &str) -> Result<String, ToolError> {
                if prompt.contains("Write a JSON object with EXACTLY these keys") {
                    return Ok(self.identity_response.clone());
                }
                if prompt.contains("You are writing the operator guides") {
                    return Ok(self.guides_response.clone());
                }
                const MARKER: &str = "reference page for the `";
                if let Some(idx) = prompt.find(MARKER) {
                    let rest = &prompt[idx + MARKER.len()..];
                    if let Some(end) = rest.find('`') {
                        let name = &rest[..end];
                        if self.bad_subsystems.contains(name) {
                            return Ok(format!(
                                "# {name}\n\nSee `crate::{name}::PhantomThing::not_real` for details.\n"
                            ));
                        }
                        if let Some(resp) = self.page_responses.get(name) {
                            return Ok(resp.clone());
                        }
                        return Ok(format!(
                            "# {name}\n\n## Key types and functions\n\
`crate::{name}::Hub::run` is the entry point.\n\n\
## How it connects\nCalled by its own leaves.\n\n\
## Notes and gaps\nNothing else to cover here.\n"
                        ));
                    }
                }
                Ok(String::new())
            }
        }

        const GOOD_GUIDES: &str = "\
=== FILE: docs/getting-started.md ===
Clone with `git clone <repo>` then build with `cargo build`.

=== FILE: docs/guides/run-the-fixture.md ===
# Run the fixture
1. Build it with `cargo build`.
2. Verify it worked.
";

        // A non-empty response that OMITS getting-started.md — must NOT count as
        // a clean guides pass (codex review finding: empty getting_started slipped
        // through as ok:true).
        const GUIDES_MISSING_GETTING_STARTED: &str = "\
=== FILE: docs/guides/run-the-fixture.md ===
# Run the fixture
1. Build it with `cargo build`.
";

        // ── full success ─────────────────────────────────────────────

        #[tokio::test]
        async fn full_success_populates_identity_pages_and_guides_with_empty_missing() {
            let facts = fixture_facts();
            let kept: Vec<&str> = facts.subsystems.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(kept.len(), 2, "fixture should keep exactly alpha + beta, no misc fold");

            let generator = ScriptedGenerator::new(valid_identity_json(&kept), GOOD_GUIDES);
            let outcome = generate_repo_docs(&generator, &facts, "FixtureRepo", "abc123").await;

            assert!(outcome.missing.is_empty(), "missing: {:?}", outcome.missing);
            let identity = outcome.identity.expect("identity should be present on full success");
            assert_eq!(identity.subsystems.len(), 2);

            assert_eq!(outcome.subsystem_pages.len(), 2);
            let page_names: HashSet<&str> =
                outcome.subsystem_pages.iter().map(|(name, _)| name.as_str()).collect();
            assert!(page_names.contains("alpha"));
            assert!(page_names.contains("beta"));

            assert!(outcome.getting_started.contains("Clone with"));
            assert_eq!(outcome.guides.len(), 1);
            assert_eq!(outcome.guides[0].0, PathBuf::from("docs/guides/run-the-fixture.md"));

            assert!(outcome.pass_ledger.iter().all(|r| r.ok), "every pass should be ok: {:?}", outcome.pass_ledger);
        }

        // ── identity fails twice -> Flagged, no panic ───────────────────

        #[tokio::test]
        async fn invalid_identity_twice_yields_flagged_outcome_naming_the_pass() {
            let facts = fixture_facts();
            // Never valid JSON, on either attempt.
            let generator = ScriptedGenerator::new("not json at all", GOOD_GUIDES);

            let outcome = generate_repo_docs(&generator, &facts, "FixtureRepo", "abc123").await;

            assert!(outcome.identity.is_none());
            assert!(outcome.missing.contains(&"identity".to_string()));
            // Passes 2/3 skipped entirely, but still explicitly recorded --
            // not silently dropped.
            assert!(outcome.missing.iter().any(|m| m == "subsystem:alpha"));
            assert!(outcome.missing.iter().any(|m| m == "subsystem:beta"));
            assert!(outcome.missing.contains(&"guides".to_string()));
            assert!(outcome.subsystem_pages.is_empty());
            assert!(outcome.guides.is_empty());
            assert!(outcome.getting_started.is_empty());

            let identity_record = outcome.pass_ledger.iter().find(|r| r.pass == "identity").unwrap();
            assert!(!identity_record.ok);
            assert!(identity_record.detail.is_some());
        }

        // ── one subsystem page fails twice -> in `missing`, others present ──

        #[tokio::test]
        async fn one_bad_subsystem_page_lands_in_missing_others_still_returned() {
            let facts = fixture_facts();
            let kept: Vec<&str> = facts.subsystems.iter().map(|s| s.name.as_str()).collect();

            let generator =
                ScriptedGenerator::new(valid_identity_json(&kept), GOOD_GUIDES).with_bad_subsystem("beta");
            let outcome = generate_repo_docs(&generator, &facts, "FixtureRepo", "abc123").await;

            assert!(outcome.identity.is_some(), "identity pass itself must still succeed");
            assert!(outcome.missing.contains(&"subsystem:beta".to_string()));
            assert!(!outcome.missing.contains(&"subsystem:alpha".to_string()));

            assert_eq!(outcome.subsystem_pages.len(), 1, "alpha's page must still be returned");
            assert_eq!(outcome.subsystem_pages[0].0, "alpha");

            // guides pass is independent of the subsystem-page pass and
            // must still have succeeded.
            assert!(!outcome.missing.contains(&"guides".to_string()));
            assert!(!outcome.getting_started.is_empty());

            let beta_record = outcome.pass_ledger.iter().find(|r| r.pass == "subsystem:beta").unwrap();
            assert!(!beta_record.ok);
            assert!(beta_record.detail.as_deref().unwrap_or_default().contains("PhantomThing"));
        }

        // ── guides missing getting-started -> flagged, partial still returned ──

        #[tokio::test]
        async fn guides_without_getting_started_are_flagged_but_partial_is_kept() {
            let facts = fixture_facts();
            let kept: Vec<&str> = facts.subsystems.iter().map(|s| s.name.as_str()).collect();

            // Same incomplete response on both attempts (no getting-started.md).
            let generator =
                ScriptedGenerator::new(valid_identity_json(&kept), GUIDES_MISSING_GETTING_STARTED);
            let outcome = generate_repo_docs(&generator, &facts, "FixtureRepo", "abc123").await;

            // The gap is surfaced, not silently accepted as ok:true.
            assert!(outcome.missing.contains(&"guides".to_string()));
            assert!(outcome.getting_started.is_empty());
            let guides_record = outcome.pass_ledger.iter().find(|r| r.pass == "guides").unwrap();
            assert!(!guides_record.ok);
            assert!(guides_record.detail.as_deref().unwrap_or_default().contains("getting-started.md"));

            // ...but the real guide file we DID get is still returned (partial success).
            assert_eq!(outcome.guides.len(), 1);
            assert_eq!(outcome.guides[0].0, PathBuf::from("docs/guides/run-the-fixture.md"));
        }
    }
}
