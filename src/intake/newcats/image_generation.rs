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

use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "image_generation";
/// `dimension` value this module writes.
pub const DIMENSION: &str = "text_to_image";

/// Outcome of one (real or mock) generation attempt.
#[derive(Debug, Clone)]
pub struct GenerationOutcome {
    pub success: bool,
    pub time_to_image_ms: i64,
    pub vram_peak_mb: u64,
    /// Free-text failure reason (e.g. "oom", "backend timeout"), when
    /// `success == false`. Stored in `raw_json` for audit, never parsed.
    pub failure_reason: Option<String>,
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
    vec![
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
            model_id,
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "vram_peak_mb".to_string(),
            value: outcome.vram_peak_mb as f64,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        },
    ]
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
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &outcome);
        let metric_names: Vec<&str> = rows.iter().map(|r| r.metric.as_str()).collect();
        assert!(!metric_names.iter().any(|m| m.contains("token")));
        assert!(!metric_names.iter().any(|m| m.contains("accuracy")));
    }
}
