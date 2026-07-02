//! Category: `voice_transcription` (ASR) — transcript vs. reference probe.
//!
//! Probe shape: feed a Whisper-style ASR backend an audio sample and compare
//! its transcription against a known reference transcript using Word Error
//! Rate (WER) — the standard ASR-quality metric.
//!
//! ## Dimension/metric convention (`task_category = "voice_transcription"`)
//!   - `dimension = "asr_transcription"`, `metric = "word_error_rate"` — raw
//!     WER (see [`super::text_similarity::word_error_rate`]): `(substitutions
//!     + insertions + deletions) / len(reference words)`. `0.0` is a perfect
//!     transcript; unbounded above (a wildly wrong/garbled hypothesis can
//!     exceed `1.0`).
//!   - `dimension = "asr_transcription"`, `metric = "transcription_accuracy"`
//!     — `1.0 - min(WER, 1.0)` (see [`super::text_similarity::wer_to_accuracy`]),
//!     a `[0.0, 1.0]` convenience so dashboards/reports that assume
//!     "higher value = better" (true for every other metric in this table)
//!     don't need a special case for WER being lower-is-better and unbounded.
//!     Both rows are written so either convention is queryable directly.
//!   - `dimension = "asr_transcription"`, `metric = "latency_ms"` — wall-clock
//!     time for the transcription call, milliseconds.
//!
//! `judge = "derived"` for all three (computed metric, no LLM-judge panel).
//!
//! ## Backend
//! whisper.cpp (`whisper-cli`/`whisper-server`) was NOT found on this box
//! (`which whisper-cli whisper-server whisper` returned nothing, no
//! `WHISPER_URL`-style env var set). ASR may live on a different host — no IP
//! is hardcoded here or anywhere in this module; [`AsrModel`] is the seam a
//! runner would implement against whatever host/URL is configured via env var
//! once available. Tonight this module is exercised only via a mock
//! [`TranscriptionOutcome`], same pattern as the other three categories.

use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};
use super::text_similarity::{word_error_rate, wer_to_accuracy};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "voice_transcription";
/// `dimension` value this module writes.
pub const DIMENSION: &str = "asr_transcription";

/// Outcome of one live transcription call.
#[derive(Debug, Clone)]
pub struct TranscriptionOutcome {
    pub transcript: String,
    pub latency_ms: i64,
}

/// Seam for calling an ASR backend; tests inject a mock outcome.
pub trait AsrModel {
    fn transcribe(&self, audio_bytes: &[u8]) -> Result<TranscriptionOutcome, String>;
}

/// Build the `assistant_dimension_score` rows for one voice_transcription probe.
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    reference_transcript: &str,
    outcome: &TranscriptionOutcome,
) -> Vec<DimensionScore> {
    let wer = word_error_rate(&outcome.transcript, reference_transcript);
    let accuracy = wer_to_accuracy(wer);
    vec![
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "word_error_rate".to_string(),
            value: wer,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: Some(outcome.transcript.clone()),
        },
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "transcription_accuracy".to_string(),
            value: accuracy,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        },
        DimensionScore {
            model_id,
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "latency_ms".to_string(),
            value: outcome.latency_ms as f64,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        },
    ]
}

/// Score one probe and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "voice_transcription")`.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    reference_transcript: &str,
    outcome: &TranscriptionOutcome,
) -> Result<(), ToolError> {
    for score in build_scores(model_id, backend_tag, reference_transcript, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFERENCE: &str = "please schedule a meeting with the team for tomorrow afternoon";

    /// KNOWN-GOOD: a perfect transcript scores WER 0.0 / accuracy 1.0.
    #[test]
    fn known_good_transcript_scores_zero_wer() {
        let outcome = TranscriptionOutcome {
            transcript: REFERENCE.to_string(),
            latency_ms: 650,
        };
        let rows = build_scores(ModelId::from("whisper-fake:base"), BackendTag::Gpu, REFERENCE, &outcome);
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));

        let wer = rows.iter().find(|r| r.metric == "word_error_rate").unwrap().value;
        assert!(wer.abs() < 1e-9, "expected WER 0.0, got {wer}");

        let acc = rows
            .iter()
            .find(|r| r.metric == "transcription_accuracy")
            .unwrap()
            .value;
        assert!((acc - 1.0).abs() < 1e-9, "expected accuracy 1.0, got {acc}");
    }

    /// KNOWN-BAD: a completely garbled transcript scores high WER / low
    /// accuracy — the discriminating case.
    #[test]
    fn known_bad_transcript_scores_high_wer() {
        let outcome = TranscriptionOutcome {
            transcript: "asdf jkl random garble zzz nonsense output blah".to_string(),
            latency_ms: 650,
        };
        let rows = build_scores(ModelId::from("whisper-fake:base"), BackendTag::Gpu, REFERENCE, &outcome);

        let wer = rows.iter().find(|r| r.metric == "word_error_rate").unwrap().value;
        assert!(wer > 0.7, "expected high WER for garbled transcript, got {wer}");

        let acc = rows
            .iter()
            .find(|r| r.metric == "transcription_accuracy")
            .unwrap()
            .value;
        assert!(acc < 0.3, "expected low accuracy for garbled transcript, got {acc}");
    }
}
