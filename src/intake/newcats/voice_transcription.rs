//! Category: `voice_transcription` (ASR) — transcript vs. reference probe.
//!
//! Probe shape: feed a Whisper-style ASR backend an audio sample and compare
//! its transcription against a known reference transcript using Word Error
//! Rate (WER) — the standard ASR-quality metric.
//!
//! ## Dimension/metric convention (`task_category = "voice_transcription"`)
//!   - `dimension = "asr_transcription"`, `metric = "word_error_rate"` —
//!     digit-NORMALIZED WER (see
//!     [`super::text_similarity::word_error_rate_normalized`]): spelled-out
//!     cardinal numbers are folded to their digit form on BOTH sides before
//!     scoring, so `"twenty three"` vs `"23"` is not counted as an error
//!     (SUITE-STT). `(substitutions + insertions + deletions) / len(reference
//!     words)`. `0.0` is a perfect transcript; unbounded above (a wildly
//!     wrong/garbled hypothesis can exceed `1.0`). The bundled corpus baseline
//!     is ~0.167 under this normalization.
//!   - `dimension = "asr_transcription"`, `metric = "transcription_accuracy"`
//!     — `1.0 - min(WER, 1.0)` (see [`super::text_similarity::wer_to_accuracy`]),
//!     a `[0.0, 1.0]` convenience so dashboards/reports that assume
//!     "higher value = better" (true for every other metric in this table)
//!     don't need a special case for WER being lower-is-better and unbounded.
//!     Both rows are written so either convention is queryable directly.
//!   - `dimension = "asr_transcription"`, `metric = "latency_ms"` — wall-clock
//!     time for the transcription call, milliseconds.
//!   - `dimension = "asr_transcription"`, `metric = "real_time_factor"` —
//!     RTF = processing_time / audio_duration (SUITE-STT). `< 1.0` means the
//!     model transcribes faster than real time. Only emitted when the clip's
//!     audio duration is known and positive (see [`real_time_factor`] /
//!     [`wav_duration_ms`]); never a divide-by-zero or fabricated number,
//!     mirroring `diffusion::blocks_per_sec`.
//!
//! `judge = "derived"` for all four (computed metric, no LLM-judge panel).
//!
//! ## Backend
//! ASR runs behind Chord's OpenAI-compatible `/v1/audio/transcriptions` route
//! (faster-whisper `small` on `:8092`, reference only — the suite calls Chord,
//! never the serve directly). No host/IP is hardcoded here: a runner reaches
//! the route through [`crate::intake::infer::transcribe_with_metrics`] (the
//! `openai` backend arm), and [`AsrModel`] is the in-process seam tests inject
//! a mock against so the SCORING path is exercised with no network. The
//! [`SttManifestEntry`] corpus loader reads a `manifest.json`
//! (`[{ "audio_file", "reference" }, ...]`) from the directory named by
//! `INTAKE_CORPUS_DIR`.

use std::path::Path;

use serde::Deserialize;
use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};
use super::text_similarity::{wer_to_accuracy, word_error_rate_normalized};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "voice_transcription";
/// `dimension` value this module writes.
pub const DIMENSION: &str = "asr_transcription";

/// Outcome of one live transcription call.
#[derive(Debug, Clone)]
pub struct TranscriptionOutcome {
    pub transcript: String,
    /// Wall-clock processing time for the transcription call, ms.
    pub latency_ms: i64,
    /// SUITE-STT: duration of the source audio clip, ms — used to derive the
    /// real-time factor. `None` when the clip's duration is unknown (e.g. a
    /// non-WAV clip whose header couldn't be parsed); the RTF row is then
    /// omitted rather than fabricated. See [`wav_duration_ms`].
    pub audio_duration_ms: Option<i64>,
}

/// Seam for calling an ASR backend; tests inject a mock outcome.
pub trait AsrModel {
    fn transcribe(&self, audio_bytes: &[u8]) -> Result<TranscriptionOutcome, String>;
}

/// One corpus entry: an audio file (relative to the corpus dir) and its known
/// reference transcript. Matches the bundled `manifest.json` shape
/// (`[{ "audio_file", "reference" }, ...]`).
#[derive(Debug, Clone, Deserialize)]
pub struct SttManifestEntry {
    pub audio_file: String,
    pub reference: String,
}

/// Load the STT corpus manifest (`<dir>/manifest.json`). A missing/unreadable/
/// malformed manifest is a clean [`ToolError`], never a panic — the runner
/// turns it into a skip, exactly like the corpus resolvers in `code.rs`.
pub fn load_manifest(dir: &Path) -> Result<Vec<SttManifestEntry>, ToolError> {
    let path = dir.join("manifest.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        ToolError::NotConfigured(format!("stt corpus manifest not found at {}: {e}", path.display()))
    })?;
    parse_manifest(&raw)
}

/// Pure JSON → `Vec<SttManifestEntry>` (split out so it is unit-testable
/// against an in-repo fixture without touching the filesystem).
pub fn parse_manifest(raw: &str) -> Result<Vec<SttManifestEntry>, ToolError> {
    serde_json::from_str(raw)
        .map_err(|e| ToolError::InvalidArgument(format!("stt corpus manifest parse error: {e}")))
}

/// SUITE-STT: real-time factor = `processing_time / audio_duration`, both in ms.
/// `< 1.0` ⇒ faster than real time. `None` when the audio duration is unknown
/// or non-positive (never a divide-by-zero / fabricated number). Pure — mirrors
/// [`super::diffusion::blocks_per_sec`].
pub fn real_time_factor(latency_ms: i64, audio_duration_ms: Option<i64>) -> Option<f64> {
    let dur = audio_duration_ms?;
    if dur <= 0 {
        return None;
    }
    Some(latency_ms as f64 / dur as f64)
}

/// SUITE-STT: best-effort duration (ms) of a PCM/WAV clip from its RIFF header —
/// dependency-free, so the corpus loader needs no audio crate. Reads the `fmt `
/// chunk's byte-rate and the `data` chunk's size and returns
/// `data_bytes * 1000 / byte_rate`. Returns `None` for anything that isn't a
/// well-formed RIFF/WAVE stream (a non-WAV clip → the RTF row is simply
/// omitted, never guessed). Never panics on a short/garbled buffer.
pub fn wav_duration_ms(bytes: &[u8]) -> Option<i64> {
    // RIFF header: "RIFF" <u32 size> "WAVE", then chunks: <4-byte id><u32 size><payload>.
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let le_u32 = |b: &[u8]| -> u32 { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) };
    let mut byte_rate: Option<u32> = None;
    let mut data_size: Option<u32> = None;
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = le_u32(&bytes[pos + 4..pos + 8]) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            // byte_rate is at offset 8 within the fmt chunk body.
            byte_rate = Some(le_u32(&bytes[body + 8..body + 12]));
        } else if id == b"data" {
            // Trust the header's data size, but clamp to what's actually present.
            let present = bytes.len().saturating_sub(body);
            data_size = Some(size.min(present) as u32);
        }
        // Chunks are word-aligned (padded to an even byte count).
        pos = body + size + (size & 1);
    }
    let (br, ds) = (byte_rate?, data_size?);
    if br == 0 {
        return None;
    }
    Some((ds as i64 * 1000) / br as i64)
}

/// Build the `assistant_dimension_score` rows for one voice_transcription probe.
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    reference_transcript: &str,
    outcome: &TranscriptionOutcome,
) -> Vec<DimensionScore> {
    // SUITE-STT: score on the digit-NORMALIZED WER so a spelled-vs-digit number
    // difference (e.g. "twenty three" vs "23") is not counted as an error.
    let wer = word_error_rate_normalized(&outcome.transcript, reference_transcript);
    let accuracy = wer_to_accuracy(wer);
    let mut rows = vec![
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
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "latency_ms".to_string(),
            value: outcome.latency_ms as f64,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        },
    ];

    // SUITE-STT: RTF only when the audio duration is known + positive.
    if let Some(rtf) = real_time_factor(outcome.latency_ms, outcome.audio_duration_ms) {
        rows.push(DimensionScore {
            model_id,
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "real_time_factor".to_string(),
            value: rtf,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        });
    }

    rows
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
            audio_duration_ms: Some(3000),
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
            audio_duration_ms: Some(3000),
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

    // ---- SUITE-STT: digit-normalized WER, RTF, WAV duration, manifest ----

    /// A spelled-vs-digit number difference is NOT counted as an error, because
    /// the suite scores on the digit-normalized WER.
    #[test]
    fn digit_word_difference_does_not_inflate_wer() {
        let reference = "set a timer for ten minutes";
        let outcome = TranscriptionOutcome {
            transcript: "set a timer for 10 minutes".to_string(),
            latency_ms: 400,
            audio_duration_ms: Some(2000),
        };
        let rows = build_scores(ModelId::from("faster-whisper:small"), BackendTag::Gpu, reference, &outcome);
        let wer = rows.iter().find(|r| r.metric == "word_error_rate").unwrap().value;
        assert!(wer.abs() < 1e-9, "digit/word-only difference should be WER 0.0, got {wer}");
    }

    /// RTF row is emitted when the duration is known, and equals
    /// processing_time / audio_duration.
    #[test]
    fn rtf_row_emitted_and_correct_when_duration_known() {
        let outcome = TranscriptionOutcome {
            transcript: REFERENCE.to_string(),
            latency_ms: 1500,
            audio_duration_ms: Some(3000),
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, REFERENCE, &outcome);
        let rtf = rows.iter().find(|r| r.metric == "real_time_factor").unwrap();
        assert!((rtf.value - 0.5).abs() < 1e-9, "1500ms / 3000ms = 0.5, got {}", rtf.value);
        assert_eq!(rtf.dimension, DIMENSION);
    }

    /// No RTF row (never fabricated / divide-by-zero) when the duration is
    /// unknown or non-positive — the other three rows are still recorded.
    #[test]
    fn rtf_row_omitted_when_duration_unknown() {
        for dur in [None, Some(0i64)] {
            let outcome = TranscriptionOutcome {
                transcript: REFERENCE.to_string(),
                latency_ms: 1500,
                audio_duration_ms: dur,
            };
            let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, REFERENCE, &outcome);
            assert!(rows.iter().all(|r| r.metric != "real_time_factor"), "dur={dur:?}");
            assert!(rows.iter().any(|r| r.metric == "word_error_rate"));
            assert!(rows.iter().any(|r| r.metric == "latency_ms"));
        }
    }

    #[test]
    fn real_time_factor_pure_helper_never_divides_by_zero() {
        assert_eq!(real_time_factor(1000, None), None);
        assert_eq!(real_time_factor(1000, Some(0)), None);
        assert_eq!(real_time_factor(1000, Some(-5)), None);
        assert_eq!(real_time_factor(1000, Some(2000)), Some(0.5));
    }

    /// A minimal, dependency-free 16-bit mono PCM WAV whose data chunk is
    /// exactly one second at the given sample rate → duration ≈ 1000 ms.
    fn synthetic_wav(sample_rate: u32) -> Vec<u8> {
        let bits = 16u16;
        let channels = 1u16;
        let byte_rate = sample_rate * channels as u32 * (bits as u32 / 8);
        let data_len = byte_rate; // exactly 1 second of samples
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        w.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&sample_rate.to_le_bytes());
        w.extend_from_slice(&byte_rate.to_le_bytes());
        w.extend_from_slice(&(channels * bits / 8).to_le_bytes()); // block align
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        w.extend(std::iter::repeat(0u8).take(data_len as usize));
        w
    }

    #[test]
    fn wav_duration_reads_one_second_clip() {
        let wav = synthetic_wav(16000);
        let ms = wav_duration_ms(&wav).expect("well-formed WAV parses");
        assert_eq!(ms, 1000, "1 second of 16kHz PCM should be 1000ms, got {ms}");
    }

    #[test]
    fn wav_duration_rejects_non_wav() {
        assert_eq!(wav_duration_ms(b"not a wav at all"), None);
        assert_eq!(wav_duration_ms(&[]), None);
        // RIFF header but truncated body → no panic, clean None.
        assert_eq!(wav_duration_ms(b"RIFF\x00\x00\x00\x00WAVE"), None);
    }

    /// The in-repo fixture manifest parses into typed entries — proves the
    /// loader without committing the ~3MB <host> corpus.
    #[test]
    fn manifest_fixture_parses() {
        let raw = include_str!("fixtures/stt_manifest.json");
        let entries = parse_manifest(raw).expect("fixture manifest parses");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].audio_file, "clip_001.wav");
        assert_eq!(entries[0].reference, "turn off the lights");
        assert!(!entries[1].reference.is_empty());
    }

    #[test]
    fn manifest_malformed_is_clean_error() {
        assert!(matches!(parse_manifest("{not json"), Err(ToolError::InvalidArgument(_))));
    }
}
