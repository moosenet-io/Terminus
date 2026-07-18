//! Category: `diffusion` — MINT-DIFF-01, the diffusion-language-model probe.
//!
//! Diffusion models (DiffusionGemma / dgem, driven by the `llama-diffusion-daemon`
//! on the loopback `dgem` daemon) generate in fixed canvas BLOCKS, not a token
//! stream, via a wholly different daemon wire path (see
//! [`crate::intake::infer::infer_with_metrics`]'s `kind == "daemon"` arm) — so
//! intake previously SKIPPED them outright (`is_non_ollama_daemon`). This
//! module is the suite that actually profiles them, once un-skipped for this
//! one suite specifically.
//!
//! ## Two DIFFERENT metric families, both emitted per use-case — read this
//! before adding a metric
//! Modeled directly on [`super::image_generation`]'s precedent ("DIFFERENT
//! metric shape... NOT token/accuracy-based" — see that module's doc): a
//! diffusion probe is not LLM-shaped either, but unlike image-generation it
//! DOES have a legible use-case output (text), so both a use-case QUALITY
//! signal and a PERFORMANCE signal are meaningful and are emitted together:
//!
//!   - **use-case QUALITY** (`dimension = "diffusion_use_case"`):
//!     - `metric = "use_case_success"` — a derived quality score in
//!       `[0.0, 1.0]` comparing the generated output against a reference
//!       answer for the use-case, via [`super::text_similarity::token_jaccard`]
//!       (order-insensitive word overlap — appropriate for "did it cover the
//!       right content" rather than exact-phrasing similarity). `judge =
//!       "derived"` (no LLM-judge panel wired for tonight's pass — see
//!       "Deferred" below).
//!   - **PERFORMANCE** (`dimension = "diffusion_performance"`):
//!     - `metric = "time_to_output_ms"` — wall-clock generation time
//!       (`InferMetrics::total_time_ms` from the daemon path; excludes model
//!       load, matching `diffusion_infer`'s doc).
//!     - `metric = "vram_peak_mb"` — VRAM in use at call time
//!       (`InferMetrics::vram_mb`; "peak" in the same best-effort sense
//!       `image_generation` uses it — a single post-call sysfs read, not a
//!       continuously-sampled peak).
//!     - `metric = "blocks_per_sec"` — diffusion-NATIVE throughput (fixed
//!       canvas blocks/sec), only emitted `Some` when the daemon reported a
//!       usable `blocks`/`time_ms` pair; token/sec is deliberately never
//!       emitted here (see `diffusion_infer`'s doc: a block-diffusion model's
//!       generation shape doesn't fit a token-stream throughput number).
//!
//! `task_category = "diffusion"` for every row (distinct from `"assistant"`/
//! `"coder"`/`"image_generation"`/etc. — see `newcats::mod` doc on the shared
//! `task_category` column).
//!
//! ## Deferred (scope note, MINT-DIFF-01)
//! An LLM-judge panel (`assistant::judges`) grading task-completion quality
//! (richer than word-overlap, e.g. "did it actually answer the question") is
//! the natural follow-on for `use_case_success` but is NOT wired here — the
//! judge harness shells out to provider CLIs and is its own integration
//! surface; bolting it onto this suite in the same pass risked an
//! incoherent partial wire-up. `token_jaccard`-derived quality is a real,
//! testable, non-trivial quality signal on its own and ships tonight.
//!
//! ## Backend / testability
//! Mirrors `image_generation`/`voice_transcription`: a small [`DiffusionModel`]
//! trait is the seam a runner calls against the live daemon
//! (`intake::infer::infer_with_metrics` with a `kind == "daemon"`-tagged
//! model); unit tests exercise [`build_scores`] directly against a
//! constructed [`DiffusionOutcome`] (no live `:8877` call in tests).

use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};
use super::text_similarity::token_jaccard;

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "diffusion";
/// `dimension` value for the use-case quality metric.
pub const DIMENSION_QUALITY: &str = "diffusion_use_case";
/// `dimension` value for the performance metrics.
pub const DIMENSION_PERF: &str = "diffusion_performance";

/// One representative diffusion use-case: a prompt, its labeled use case, and
/// a reference/expected answer the generated output is scored against.
///
/// A handful of cases in-source (not a separate corpus JSON file + loader —
/// none of this module's `newcats` siblings load an external corpus either;
/// see the module doc's "Deferred" note). Kept `pub` so a runner or a future
/// corpus-file loader can construct/replace these without touching the
/// scoring logic in [`build_scores`].
#[derive(Debug, Clone)]
pub struct DiffusionUseCase {
    pub label: &'static str,
    pub prompt: &'static str,
    pub reference: &'static str,
}

/// A small, representative diffusion-model use-case corpus: summarization,
/// short factual Q&A, and instruction-following, the three shapes DGEM-review
/// callers (`dgem_review`, `dgem_generate`) actually exercise in this fleet.
pub const USE_CASES: &[DiffusionUseCase] = &[
    DiffusionUseCase {
        label: "summarization",
        prompt: "Summarize in one sentence: The quick brown fox jumps over the lazy dog, \
                 demonstrating every letter of the English alphabet in a single short sentence.",
        reference: "The sentence is a pangram that uses every letter of the English alphabet.",
    },
    DiffusionUseCase {
        label: "factual_qa",
        prompt: "What is the capital of France? Answer in one short sentence.",
        reference: "The capital of France is Paris.",
    },
    DiffusionUseCase {
        label: "instruction_following",
        prompt: "List exactly three primary colors, comma-separated, nothing else.",
        reference: "red, blue, yellow",
    },
];

/// Outcome of one (real or mock) diffusion generation attempt for a use-case.
#[derive(Debug, Clone)]
pub struct DiffusionOutcome {
    pub output: String,
    /// Wall-clock generation time, ms (excludes model load — see module doc).
    pub time_to_output_ms: i64,
    /// VRAM in use at call time, MB; `None` if unreadable (matches
    /// `InferMetrics::vram_mb`'s own optionality).
    pub vram_peak_mb: Option<u64>,
    /// Fixed canvas blocks generated, if the daemon reported one — the
    /// diffusion-native unit of work (see module doc on why token/sec is
    /// never emitted here).
    pub blocks: Option<i64>,
}

/// Seam for calling the diffusion daemon; a runner implements this against
/// [`crate::intake::infer::infer_with_metrics`], tests inject a mock outcome.
pub trait DiffusionModel {
    fn generate(&self, prompt: &str) -> Result<DiffusionOutcome, String>;
}

/// Derived use-case quality score in `[0.0, 1.0]`: word-overlap of the
/// generated output against the use-case's reference answer. Pure.
pub fn quality_score(output: &str, reference: &str) -> f64 {
    token_jaccard(output, reference)
}

/// Blocks/sec throughput, when both `blocks` and a positive `time_to_output_ms`
/// are available; `None` otherwise (never a divide-by-zero, never a fabricated
/// number). Pure.
pub fn blocks_per_sec(blocks: Option<i64>, time_to_output_ms: i64) -> Option<f64> {
    let blocks = blocks?;
    if blocks <= 0 || time_to_output_ms <= 0 {
        return None;
    }
    Some(blocks as f64 / (time_to_output_ms as f64 / 1000.0))
}

/// Build the `assistant_dimension_score` rows for one diffusion use-case
/// attempt: one quality row + up to three performance rows (`blocks_per_sec`
/// omitted when not derivable).
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    use_case: &DiffusionUseCase,
    outcome: &DiffusionOutcome,
) -> Vec<DimensionScore> {
    let quality = quality_score(&outcome.output, use_case.reference);
    let mut rows = vec![DimensionScore {
        model_id: model_id.clone(),
        backend_tag,
        dimension: DIMENSION_QUALITY.to_string(),
        metric: "use_case_success".to_string(),
        value: quality,
        std_dev: None,
        judge: "derived".to_string(),
        low_confidence: false,
        raw_json: Some(
            serde_json::json!({
                "use_case": use_case.label,
                "output": outcome.output,
            })
            .to_string(),
        ),
    }];

    rows.push(DimensionScore {
        model_id: model_id.clone(),
        backend_tag,
        dimension: DIMENSION_PERF.to_string(),
        metric: "time_to_output_ms".to_string(),
        value: outcome.time_to_output_ms as f64,
        std_dev: None,
        judge: "derived".to_string(),
        low_confidence: false,
        raw_json: None,
    });

    if let Some(vram) = outcome.vram_peak_mb {
        rows.push(DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION_PERF.to_string(),
            metric: "vram_peak_mb".to_string(),
            value: vram as f64,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        });
    }

    if let Some(bps) = blocks_per_sec(outcome.blocks, outcome.time_to_output_ms) {
        rows.push(DimensionScore {
            model_id,
            backend_tag,
            dimension: DIMENSION_PERF.to_string(),
            metric: "blocks_per_sec".to_string(),
            value: bps,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        });
    }

    rows
}

/// Score one (mock or live) diffusion attempt and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "diffusion")`.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    use_case: &DiffusionUseCase,
    outcome: &DiffusionOutcome,
) -> Result<(), ToolError> {
    for score in build_scores(model_id.clone(), backend_tag, use_case, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KNOWN-GOOD: a close-match output scores high quality, and all three
    /// performance metrics land (blocks present).
    #[test]
    fn known_good_diffusion_scores_high_quality_and_all_perf_metrics() {
        let use_case = &USE_CASES[1]; // factual_qa
        let outcome = DiffusionOutcome {
            output: "The capital of France is Paris.".to_string(),
            time_to_output_ms: 9000,
            vram_peak_mb: Some(16384),
            blocks: Some(4),
        };
        let rows = build_scores(ModelId::from("diffusiongemma:latest"), BackendTag::Gpu, use_case, &outcome);
        assert_eq!(rows.len(), 4);

        let quality = rows.iter().find(|r| r.metric == "use_case_success").unwrap();
        assert_eq!(quality.dimension, DIMENSION_QUALITY);
        assert!(quality.value > 0.5, "expected high quality for a near-identical answer, got {}", quality.value);
        assert_eq!(quality.judge, "derived");

        let time_ms = rows.iter().find(|r| r.metric == "time_to_output_ms").unwrap();
        assert_eq!(time_ms.dimension, DIMENSION_PERF);
        assert_eq!(time_ms.value, 9000.0);

        let vram = rows.iter().find(|r| r.metric == "vram_peak_mb").unwrap();
        assert_eq!(vram.value, 16384.0);

        let bps = rows.iter().find(|r| r.metric == "blocks_per_sec").unwrap();
        // 4 blocks / 9.0s ≈ 0.444 blocks/sec
        assert!((bps.value - (4.0 / 9.0)).abs() < 1e-6);
    }

    /// KNOWN-BAD: an off-topic output scores low quality — the discriminating
    /// case for the quality metric.
    #[test]
    fn known_bad_diffusion_scores_low_quality() {
        let use_case = &USE_CASES[1]; // factual_qa
        let outcome = DiffusionOutcome {
            output: "Bananas are a good source of potassium.".to_string(),
            time_to_output_ms: 9000,
            vram_peak_mb: Some(16384),
            blocks: Some(4),
        };
        let rows = build_scores(ModelId::from("diffusiongemma:latest"), BackendTag::Gpu, use_case, &outcome);
        let quality = rows.iter().find(|r| r.metric == "use_case_success").unwrap();
        assert!(quality.value < 0.3, "expected low quality for an off-topic answer, got {}", quality.value);
    }

    /// Performance metrics are still recorded on a low-quality/failed output —
    /// same "failure is still useful signal" convention `image_generation`
    /// established.
    #[test]
    fn performance_metrics_recorded_regardless_of_quality() {
        let use_case = &USE_CASES[0];
        let outcome = DiffusionOutcome {
            output: String::new(),
            time_to_output_ms: 15000,
            vram_peak_mb: Some(20000),
            blocks: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, use_case, &outcome);
        let time_ms = rows.iter().find(|r| r.metric == "time_to_output_ms").unwrap();
        assert_eq!(time_ms.value, 15000.0);
        let vram = rows.iter().find(|r| r.metric == "vram_peak_mb").unwrap();
        assert_eq!(vram.value, 20000.0);
    }

    /// No `blocks_per_sec` row when the daemon didn't report a block count —
    /// never a fabricated/divide-by-zero throughput number.
    #[test]
    fn blocks_per_sec_omitted_when_blocks_unknown() {
        let outcome = DiffusionOutcome {
            output: "x".to_string(),
            time_to_output_ms: 5000,
            vram_peak_mb: None,
            blocks: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &USE_CASES[0], &outcome);
        assert!(rows.iter().all(|r| r.metric != "blocks_per_sec"));
        // vram_peak_mb also omitted when unreadable, not fabricated as 0.
        assert!(rows.iter().all(|r| r.metric != "vram_peak_mb"));
    }

    #[test]
    fn blocks_per_sec_pure_helper_never_divides_by_zero() {
        assert_eq!(blocks_per_sec(Some(4), 0), None);
        assert_eq!(blocks_per_sec(None, 9000), None);
        assert_eq!(blocks_per_sec(Some(0), 9000), None);
        assert!(blocks_per_sec(Some(4), 9000).is_some());
    }

    #[test]
    fn no_token_throughput_metric_is_ever_emitted() {
        let outcome = DiffusionOutcome {
            output: "The capital of France is Paris.".to_string(),
            time_to_output_ms: 9000,
            vram_peak_mb: Some(16384),
            blocks: Some(4),
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &USE_CASES[1], &outcome);
        assert!(!rows.iter().any(|r| r.metric.contains("tok_per_sec") || r.metric.contains("token")));
    }

    #[test]
    fn use_case_corpus_has_labeled_prompts_and_references() {
        assert!(!USE_CASES.is_empty());
        for uc in USE_CASES {
            assert!(!uc.label.is_empty());
            assert!(!uc.prompt.is_empty());
            assert!(!uc.reference.is_empty());
        }
    }
}
