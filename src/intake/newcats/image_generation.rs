//! Category: `image_generation` — text-to-image synthesis probe.
//!
//! DIFFERENT metric shape from every other category in this module (explicit
//! operator instruction): NOT token/accuracy-based. A generation probe scores
//! only:
//!   - whether the backend produced an image at all (`generation_success`),
//!   - how long that took (`time_to_image_ms`),
//!   - peak VRAM used during synthesis (`vram_peak_mb`).
//!
//! ## Dimension/metric convention (`task_category = "image_generation"`)
//!   - `dimension = "text_to_image"`, `metric = "generation_success"` — `1.0`
//!     if the backend returned a non-empty image, else `0.0` (booleans are
//!     stored as `0.0`/`1.0` in the `DOUBLE PRECISION value` column — same
//!     convention as every other boolean-shaped metric in this codebase, e.g.
//!     `low_confidence` is a separate dedicated column, not a metric value, but
//!     there is no dedicated boolean column here, so we encode inline).
//!   - `dimension = "text_to_image"`, `metric = "time_to_image_ms"` —
//!     wall-clock synthesis time, milliseconds. Recorded even on failure
//!     (a slow OOM is still useful signal).
//!   - `dimension = "text_to_image"`, `metric = "vram_peak_mb"` — peak VRAM
//!     observed during the attempt, megabytes. Recorded even on failure — a
//!     failure caused by hitting a VRAM ceiling is exactly the case an
//!     operator needs this number for.
//!
//! `judge = "derived"` for all three (no LLM-judge panel; hardware/success
//! metrics, not quality metrics).
//!
//! ## No backend exists on this box yet
//! No image-generation backend (Stable Diffusion / ComfyUI / etc.) is
//! configured or reachable from this box tonight. [`ImageGenModel`] is the
//! seam a runner would implement once one exists (mirrors the other three
//! categories' backend-call traits). Because there is nothing live to call,
//! this module's tests exercise the SCORING/WRITE logic directly against a
//! constructed [`GenerationOutcome`] (a mock generation result), which is
//! exactly what the operator asked for: "a sanity test that exercises the
//! scoring/write logic itself... even though no live backend call happens."

use serde::Deserialize;
use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "image_generation";
/// `dimension` value this module writes.
pub const DIMENSION: &str = "text_to_image";

/// One text-to-image probe prompt (+ optional CLIP reference text). SUITE-IMG:
/// unlike `diffusion`'s in-source `USE_CASES`, image-generation prompts are
/// loadable from an operator corpus (`INTAKE_CORPUS_DIR/image_generation.json`)
/// via [`load_prompts`], with an in-source default set so the suite always runs.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageGenPrompt {
    /// Short stable label used in the per-prompt summary line.
    pub label: String,
    /// The text-to-image prompt sent to the backend.
    pub prompt: String,
    /// Optional CLIP prompt-adherence reference. When present AND a CLIP scorer
    /// is available, [`build_scores`] can emit a `clip_adherence` metric; when
    /// absent it is cleanly "not measured" (no fabricated score). Scaffolded —
    /// no CLIP scorer is wired on this box yet (see [`GenerationOutcome::clip_score`]).
    #[serde(default)]
    pub clip_reference: Option<String>,
}

/// A small, dependency-free default prompt set (three shapes: object, scene,
/// style). Used when no operator corpus is configured — mirrors `diffusion`'s
/// in-source `USE_CASES` so the suite is runnable without external files.
/// No PII / no host paths (spec S1).
pub fn default_prompts() -> Vec<ImageGenPrompt> {
    vec![
        ImageGenPrompt {
            label: "object".to_string(),
            prompt: "a single red cube resting on a plain white table, studio lighting".to_string(),
            clip_reference: Some("a red cube on a white table".to_string()),
        },
        ImageGenPrompt {
            label: "scene".to_string(),
            prompt: "a quiet mountain lake at sunrise with pine trees along the shore".to_string(),
            clip_reference: Some("a mountain lake at sunrise".to_string()),
        },
        ImageGenPrompt {
            label: "style".to_string(),
            prompt: "a watercolor painting of a sailboat on a calm sea".to_string(),
            clip_reference: Some("a watercolor sailboat".to_string()),
        },
    ]
}

/// Resolve the image-generation prompt corpus. SUITE-IMG follows the unified
/// corpus convention (DR-02): the operator points `INTAKE_CORPUS_DIR` at a
/// directory containing `image_generation.json` (a JSON array of
/// [`ImageGenPrompt`]). Unlike `code::corpus_dir` this does NOT hard-fail on a
/// missing var/file: an in-source [`default_prompts`] set exists, so a missing
/// or unreadable corpus falls back cleanly (a runnable suite matters more than a
/// configured corpus here — same "always runnable" stance as `diffusion`). A
/// malformed/empty corpus file also falls back to the defaults rather than
/// running zero prompts.
pub fn load_prompts() -> Vec<ImageGenPrompt> {
    if let Ok(dir) = std::env::var("INTAKE_CORPUS_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            let path = std::path::Path::new(dir).join("image_generation.json");
            if let Ok(body) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Vec<ImageGenPrompt>>(&body) {
                    if !v.is_empty() {
                        return v;
                    }
                }
            }
        }
    }
    default_prompts()
}

/// Outcome of one (real or mock) generation attempt.
#[derive(Debug, Clone)]
pub struct GenerationOutcome {
    pub success: bool,
    pub time_to_image_ms: i64,
    pub vram_peak_mb: u64,
    /// Free-text failure reason (e.g. "oom", "backend timeout"), when
    /// `success == false`. Stored in `raw_json` for audit, never parsed.
    pub failure_reason: Option<String>,
    /// CLIP prompt-adherence score in `[0.0, 1.0]` (cosine similarity between the
    /// generated image and its prompt/reference in CLIP space) when a CLIP scorer
    /// AND reference are available; `None` = NOT MEASURED (no CLIP model is wired
    /// on this box yet). Scaffolded: [`build_scores`] emits a `clip_adherence`
    /// metric ONLY when this is `Some`, so "not measured" is unambiguous (an
    /// absent row, never a fabricated `0.0`).
    pub clip_score: Option<f64>,
}

/// Seam for calling a generation backend once one is deployed; tests inject a
/// mock [`GenerationOutcome`] directly (no trait implementation needed for
/// tonight's sanity test, since there is no live call to make).
pub trait ImageGenModel {
    fn generate(&self, prompt: &str) -> Result<GenerationOutcome, String>;
}

/// Build the `assistant_dimension_score` rows for one image_generation attempt.
/// This is the function the operator asked to be sanity-tested against a mock
/// result — it contains ALL of the scoring logic (the boolean → 0.0/1.0
/// encoding) even though no live backend exists to call it against tonight.
pub fn build_scores(model_id: ModelId, backend_tag: BackendTag, outcome: &GenerationOutcome) -> Vec<DimensionScore> {
    let raw = outcome.failure_reason.clone();
    let mut rows = vec![
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "generation_success".to_string(),
            value: if outcome.success { 1.0 } else { 0.0 },
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: raw,
        },
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "time_to_image_ms".to_string(),
            value: outcome.time_to_image_ms as f64,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        },
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "vram_peak_mb".to_string(),
            value: outcome.vram_peak_mb as f64,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        },
    ];

    // CLIP prompt-adherence: emitted ONLY when a CLIP score was measured (a CLIP
    // scorer + reference were available) AND that score is a valid similarity —
    // finite and within `[0.0, 1.0]`. A NaN/inf or out-of-range value is dropped
    // rather than persisted (it would poison aggregates); "not measured" and
    // "measured but invalid" both cleanly omit the row, never a fabricated `0.0`.
    // `judge = "clip"` distinguishes it from the `"derived"` hardware/success rows.
    if let Some(clip) = outcome.clip_score.filter(|c| c.is_finite() && (0.0..=1.0).contains(c)) {
        rows.push(DimensionScore {
            model_id,
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "clip_adherence".to_string(),
            value: clip,
            std_dev: None,
            judge: "clip".to_string(),
            low_confidence: false,
            raw_json: None,
        });
    }

    rows
}

/// Score one (mock, tonight) generation attempt and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "image_generation")`.
/// Not exercised against a live backend tonight — see module doc — but this
/// is the real write path a runner would call once a generation backend
/// exists.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    outcome: &GenerationOutcome,
) -> Result<(), ToolError> {
    for score in build_scores(model_id, backend_tag, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KNOWN-GOOD: a successful generation writes generation_success == 1.0
    /// plus the two hardware/timing metrics, untouched by success/failure.
    #[test]
    fn known_good_generation_scores_success_one() {
        let outcome = GenerationOutcome {
            success: true,
            time_to_image_ms: 4200,
            vram_peak_mb: 18432,
            failure_reason: None,
            clip_score: None,
        };
        let rows = build_scores(ModelId::from("sdxl-fake:1.0"), BackendTag::Gpu, &outcome);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));

        let success = rows
            .iter()
            .find(|r| r.metric == "generation_success")
            .unwrap();
        assert_eq!(success.value, 1.0);
        assert!(success.raw_json.is_none());

        let time_ms = rows.iter().find(|r| r.metric == "time_to_image_ms").unwrap();
        assert_eq!(time_ms.value, 4200.0);

        let vram = rows.iter().find(|r| r.metric == "vram_peak_mb").unwrap();
        assert_eq!(vram.value, 18432.0);
    }

    /// KNOWN-BAD: a failed generation (e.g. OOM) writes generation_success ==
    /// 0.0 — the discriminating case — while still recording the timing/VRAM
    /// numbers that explain WHY it failed, and the failure reason in raw_json.
    #[test]
    fn known_bad_generation_scores_success_zero() {
        let outcome = GenerationOutcome {
            success: false,
            time_to_image_ms: 11800,
            vram_peak_mb: 24576,
            failure_reason: Some("oom".to_string()),
            clip_score: None,
        };
        let rows = build_scores(ModelId::from("sdxl-fake:1.0"), BackendTag::Cpu, &outcome);
        let success = rows
            .iter()
            .find(|r| r.metric == "generation_success")
            .unwrap();
        assert_eq!(success.value, 0.0);
        assert_eq!(success.raw_json.as_deref(), Some("oom"));

        // Timing/VRAM are still recorded on failure — that's the point.
        let time_ms = rows.iter().find(|r| r.metric == "time_to_image_ms").unwrap();
        assert_eq!(time_ms.value, 11800.0);
        let vram = rows.iter().find(|r| r.metric == "vram_peak_mb").unwrap();
        assert_eq!(vram.value, 24576.0);
    }

    #[test]
    fn no_token_or_accuracy_metrics_are_emitted() {
        let outcome = GenerationOutcome {
            success: true,
            time_to_image_ms: 100,
            vram_peak_mb: 1024,
            failure_reason: None,
            clip_score: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &outcome);
        let metric_names: Vec<&str> = rows.iter().map(|r| r.metric.as_str()).collect();
        assert!(!metric_names.iter().any(|m| m.contains("token")));
        assert!(!metric_names.iter().any(|m| m.contains("accuracy")));
    }

    /// SUITE-IMG: CLIP prompt-adherence is NOT MEASURED by default — no
    /// `clip_adherence` row when `clip_score` is `None` (the case on this box).
    #[test]
    fn clip_adherence_absent_when_not_measured() {
        let outcome = GenerationOutcome {
            success: true,
            time_to_image_ms: 100,
            vram_peak_mb: 1024,
            failure_reason: None,
            clip_score: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &outcome);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.metric != "clip_adherence"));
    }

    /// When a CLIP score IS available it is emitted as a `clip_adherence` metric
    /// with `judge = "clip"` (distinct from the derived hardware/success rows).
    #[test]
    fn clip_adherence_emitted_when_measured() {
        let outcome = GenerationOutcome {
            success: true,
            time_to_image_ms: 100,
            vram_peak_mb: 1024,
            failure_reason: None,
            clip_score: Some(0.78),
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &outcome);
        assert_eq!(rows.len(), 4);
        let clip = rows.iter().find(|r| r.metric == "clip_adherence").unwrap();
        assert_eq!(clip.value, 0.78);
        assert_eq!(clip.judge, "clip");
        assert_eq!(clip.dimension, DIMENSION);
    }

    /// b2fix finding 6: an invalid clip_score (NaN / inf / out of `[0,1]`) is NOT
    /// persisted — the clip_adherence row is omitted, never a poisoned value.
    #[test]
    fn clip_adherence_omitted_when_score_is_invalid() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.5, -0.1] {
            let outcome = GenerationOutcome {
                success: true,
                time_to_image_ms: 100,
                vram_peak_mb: 1024,
                failure_reason: None,
                clip_score: Some(bad),
            };
            let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &outcome);
            assert_eq!(rows.len(), 3, "no clip row for invalid score {bad}");
            assert!(
                rows.iter().all(|r| r.metric != "clip_adherence"),
                "invalid clip_score {bad} must not be persisted"
            );
        }
        // Boundary-valid scores ARE emitted.
        for good in [0.0, 1.0, 0.5] {
            let outcome = GenerationOutcome {
                success: true,
                time_to_image_ms: 100,
                vram_peak_mb: 1024,
                failure_reason: None,
                clip_score: Some(good),
            };
            let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &outcome);
            let clip = rows.iter().find(|r| r.metric == "clip_adherence");
            assert!(clip.is_some(), "valid clip_score {good} must be emitted");
            assert_eq!(clip.unwrap().value, good);
        }
    }

    /// Default prompt corpus is non-empty and well-formed (label + prompt set).
    #[test]
    fn default_prompts_are_well_formed() {
        let prompts = default_prompts();
        assert!(!prompts.is_empty());
        for p in &prompts {
            assert!(!p.label.is_empty());
            assert!(!p.prompt.is_empty());
        }
    }

    /// `load_prompts` falls back to the in-source defaults when no corpus dir is
    /// configured (a runnable suite matters more than a configured corpus).
    #[test]
    fn load_prompts_falls_back_to_defaults_without_corpus_dir() {
        let saved = std::env::var("INTAKE_CORPUS_DIR").ok();
        std::env::remove_var("INTAKE_CORPUS_DIR");
        let prompts = load_prompts();
        assert!(!prompts.is_empty());
        if let Some(v) = saved {
            std::env::set_var("INTAKE_CORPUS_DIR", v);
        }
    }

    /// `load_prompts` reads `image_generation.json` from `INTAKE_CORPUS_DIR` when
    /// present. Uses this crate's committed fixture dir.
    #[test]
    fn load_prompts_reads_corpus_fixture() {
        let saved = std::env::var("INTAKE_CORPUS_DIR").ok();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/intake-corpus-imagegen");
        std::env::set_var("INTAKE_CORPUS_DIR", fixture_dir);
        let prompts = load_prompts();
        assert!(!prompts.is_empty(), "fixture image_generation.json should load");
        assert!(prompts.iter().all(|p| !p.prompt.is_empty()));
        match saved {
            Some(v) => std::env::set_var("INTAKE_CORPUS_DIR", v),
            None => std::env::remove_var("INTAKE_CORPUS_DIR"),
        }
    }
}
