//! Category: `tts` (text-to-speech) — S125 SUITE-TTS.
//!
//! Probe shape: synthesize speech from a known reference text through Chord's
//! `/v1/audio/speech` route (piper `en_US-lessac-medium`, :8093 behind Chord),
//! then transcribe the produced audio back through the STT route (faster-whisper,
//! :8092 behind Chord) and score the round-trip. A TTS engine that produces
//! intelligible speech round-trips back to (nearly) the input text; a garbled or
//! silent synthesis does not. This gives an END-TO-END, reference-free-of-a-golden-
//! -audio quality signal (STT-loopback WER), plus a cheap acoustic sanity heuristic
//! (MOS-proxy) and a throughput number (RTF).
//!
//! ## Two DIFFERENT metric families, both emitted per case
//! Modeled on [`super::diffusion`]'s precedent (a use-case QUALITY signal AND a
//! PERFORMANCE signal emitted together, each on its own dimension):
//!
//!   - **INTELLIGIBILITY / QUALITY** (`dimension = "tts_intelligibility"`):
//!     - `metric = "loopback_wer"` — Word Error Rate of the STT transcript vs the
//!       input reference text (see [`super::text_similarity::word_error_rate`]).
//!       `0.0` is a perfect round-trip; lower = more intelligible; unbounded above.
//!     - `metric = "loopback_accuracy"` — `1.0 - min(WER, 1.0)` (see
//!       [`super::text_similarity::wer_to_accuracy`]), the `[0.0, 1.0]`
//!       higher-is-better convenience twin (same convention as
//!       `voice_transcription`).
//!     - `metric = "mos_proxy"` — a SIMPLE acoustic/energy heuristic in the MOS
//!       range `[1.0, 5.0]` derived from the synthesized waveform's RMS energy
//!       (see [`mos_proxy`]). Emitted only when the audio was PCM-WAV-parseable.
//!       This is explicitly a SCAFFOLD, NOT a perceptual MOS: it detects
//!       "there is coherent, non-silent audio energy here", not naturalness. A
//!       real MOS predictor (e.g. a NISQA/UTMOS model) is the follow-on; the
//!       metric name/dimension are stable so a better estimator can replace the
//!       heuristic without a schema change. `judge = "derived"`.
//!   - **PERFORMANCE** (`dimension = "tts_performance"`):
//!     - `metric = "synthesis_ms"` — wall-clock synthesis time for the speech
//!       call (from the `/v1/audio/speech` round-trip), milliseconds.
//!     - `metric = "rtf"` — Real-Time Factor = synthesis_time / audio_duration
//!       (see [`real_time_factor`]). `< 1.0` means faster-than-real-time
//!       synthesis. Emitted only when the audio duration was derivable (WAV
//!       parse succeeded and duration > 0), never a fabricated/divide-by-zero
//!       number.
//!
//! `judge = "derived"` for every row (computed metric, no LLM-judge panel).
//! `task_category = "tts"` for every row (distinct from `"voice_transcription"`,
//! `"diffusion"`, etc. — see `newcats::mod` doc on the shared `task_category`
//! column).
//!
//! ## Backend / testability
//! Mirrors the other `newcats` categories: the live path calls two Chord routes
//! via [`crate::intake::infer`] — `synthesize_with_metrics` (`/v1/audio/speech`)
//! and `transcribe_with_metrics` (`/v1/audio/transcriptions`) — but this module's
//! scoring is exercised through a small [`TtsLoopbackModel`] trait seam so unit
//! tests inject a mock round-trip (a known transcript fed back) and score with no
//! network. No IP/host is hardcoded here; the runner resolves backends from the
//! Chord registry.

use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};
use super::text_similarity::{wer_to_accuracy, word_error_rate};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "tts";
/// `dimension` value for the intelligibility/quality metrics.
pub const DIMENSION_QUALITY: &str = "tts_intelligibility";
/// `dimension` value for the performance metrics.
pub const DIMENSION_PERF: &str = "tts_performance";

/// One TTS reference case: a label, the text to synthesize, and an OPTIONAL
/// acoustic reference hint (e.g. an expected voice/style tag) carried for a
/// future golden-audio comparison — unused by the loopback scoring today, kept
/// so the corpus schema is forward-compatible.
#[derive(Debug, Clone)]
pub struct TtsCase {
    pub label: String,
    pub text: String,
    pub acoustic_ref: Option<String>,
}

/// A compact in-source default corpus: short, phonetically-varied lines that
/// exercise digits, punctuation, and ordinary prose — the shapes an assistant
/// TTS surface actually speaks. Used when `INTAKE_CORPUS_DIR` is unset or has no
/// `tts_reference.json` (so tests and an ad-hoc run work with zero config), and
/// mirrored by the shipped `src/intake/corpus/tts_reference.json` fixture.
fn default_cases() -> Vec<TtsCase> {
    [
        ("greeting", "Hello, how can I help you today?"),
        (
            "digits",
            "Your appointment is scheduled for March third at four fifteen in the afternoon.",
        ),
        (
            "pangram",
            "The quick brown fox jumps over the lazy dog.",
        ),
        (
            "instruction",
            "Please turn off the kitchen lights and lock the front door.",
        ),
    ]
    .into_iter()
    .map(|(label, text)| TtsCase {
        label: label.to_string(),
        text: text.to_string(),
        acoustic_ref: None,
    })
    .collect()
}

/// Load the TTS reference corpus.
///
/// Resolves the unified [`crate::intake::code::corpus_dir`] (`INTAKE_CORPUS_DIR`,
/// DR-02) and reads `<dir>/tts_reference.json` — a JSON array of
/// `{ "label", "text", "acoustic_ref"? }` objects. Falls back to
/// [`default_cases`] when the var is unset (`ToolError::NotConfigured`), the file
/// is absent, or it fails to parse, so a run/test never hard-fails on a missing
/// corpus — consistent with the other newcats suites carrying an in-source
/// corpus. Never panics.
pub fn load_cases() -> Vec<TtsCase> {
    let dir = match crate::intake::code::corpus_dir() {
        Ok(d) => d,
        Err(_) => return default_cases(),
    };
    let path = dir.join("tts_reference.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return default_cases(),
    };
    match serde_json::from_str::<Vec<RawTtsCase>>(&text) {
        Ok(raw) if !raw.is_empty() => raw
            .into_iter()
            .map(|r| TtsCase {
                label: r.label,
                text: r.text,
                acoustic_ref: r.acoustic_ref,
            })
            .collect(),
        _ => default_cases(),
    }
}

/// On-disk shape of one `tts_reference.json` entry.
#[derive(serde::Deserialize)]
struct RawTtsCase {
    label: String,
    text: String,
    #[serde(default)]
    acoustic_ref: Option<String>,
}

/// Coarse acoustic stats parsed from a PCM-WAV byte buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStats {
    /// Duration in seconds (`data` bytes / (sample_rate * channels * bytes_per_sample)).
    pub duration_s: f64,
    /// Root-mean-square amplitude of the 16-bit PCM samples, in `[0.0, 32768.0]`.
    pub rms: f64,
}

/// Parse a canonical little-endian PCM `.wav` (RIFF/WAVE) buffer into coarse
/// [`AudioStats`]. Dependency-free chunk scan (no `hound`/`symphonia`): finds the
/// `fmt ` chunk for sample_rate/channels/bits and the `data` chunk for the sample
/// payload, then computes duration + RMS over 16-bit samples. `None` for anything
/// that isn't parseable 16-bit PCM WAV (a non-WAV/compressed/empty body) — the
/// caller then simply omits the WAV-derived metrics rather than fabricating them.
/// Never panics (all indexing is bounds-checked).
pub fn parse_wav_stats(audio: &[u8]) -> Option<AudioStats> {
    // "RIFF"....."WAVE" then a sequence of (id[4], size[u32 LE], payload) chunks.
    if audio.len() < 12 || &audio[0..4] != b"RIFF" || &audio[8..12] != b"WAVE" {
        return None;
    }
    let read_u16 = |b: &[u8], o: usize| -> Option<u16> {
        b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
    };
    let read_u32 = |b: &[u8], o: usize| -> Option<u32> {
        b.get(o..o + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };

    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u16> = None;
    let mut bits: Option<u16> = None;
    let mut data: Option<&[u8]> = None;

    let mut pos = 12usize;
    while pos + 8 <= audio.len() {
        let id = &audio[pos..pos + 4];
        let size = read_u32(audio, pos + 4)? as usize;
        let body_start = pos + 8;
        let body_end = body_start.checked_add(size)?;
        if body_end > audio.len() {
            break; // truncated chunk — stop cleanly rather than over-read
        }
        match id {
            b"fmt " => {
                // fmt layout: audio_format[2] channels[2] sample_rate[4] byte_rate[4]
                //             block_align[2] bits_per_sample[2]
                channels = read_u16(audio, body_start + 2);
                sample_rate = read_u32(audio, body_start + 4);
                bits = read_u16(audio, body_start + 14);
            }
            b"data" => {
                data = audio.get(body_start..body_end);
            }
            _ => {}
        }
        // Chunks are word-aligned: an odd size is followed by a pad byte.
        pos = body_end + (size & 1);
    }

    let sample_rate = sample_rate?;
    let channels = channels?;
    let bits = bits?;
    let data = data?;
    if sample_rate == 0 || channels == 0 || bits != 16 {
        return None; // only 16-bit PCM is handled by the RMS pass below
    }
    let bytes_per_sample = (bits / 8) as usize; // 2
    let frame_bytes = bytes_per_sample * channels as usize;
    if frame_bytes == 0 || data.len() < frame_bytes {
        return None;
    }
    let n_frames = data.len() / frame_bytes;
    let duration_s = n_frames as f64 / sample_rate as f64;

    // RMS over every 16-bit sample (all channels).
    let mut sumsq = 0.0f64;
    let mut n = 0u64;
    let mut i = 0usize;
    while i + 2 <= data.len() {
        let s = i16::from_le_bytes([data[i], data[i + 1]]) as f64;
        sumsq += s * s;
        n += 1;
        i += 2;
    }
    let rms = if n > 0 { (sumsq / n as f64).sqrt() } else { 0.0 };
    Some(AudioStats { duration_s, rms })
}

/// SCAFFOLD acoustic MOS-proxy in `[1.0, 5.0]` from waveform RMS energy.
///
/// NOT a perceptual MOS — a placeholder that maps "is there coherent, non-silent
/// audio energy" onto the familiar 1–5 MOS scale so the metric slot is populated
/// with something monotone and bounded until a real predictor is wired. Near-
/// silence (`rms → 0`) floors at `1.0`; a healthy speech RMS (empirically a few
/// thousand on 16-bit PCM) saturates toward `5.0`. Pure, deterministic, clamped.
pub fn mos_proxy(rms: f64) -> f64 {
    // A speech waveform's RMS on 16-bit PCM is typically ~1000–6000. Map that
    // band onto (1.0, 5.0] with a smooth saturating curve; clamp both ends.
    const REF_RMS: f64 = 4000.0; // RMS treated as "clearly present speech"
    if rms <= 0.0 {
        return 1.0;
    }
    let ratio = (rms / REF_RMS).min(1.0); // 0.0 (silent) .. 1.0 (full)
    1.0 + 4.0 * ratio
}

/// Real-Time Factor = `synthesis_ms / (audio_duration_s * 1000)`. `< 1.0` means
/// faster-than-real-time. `None` when the duration is unknown or non-positive
/// (never a divide-by-zero / fabricated number). Pure.
pub fn real_time_factor(synthesis_ms: i64, audio_duration_s: Option<f64>) -> Option<f64> {
    let dur = audio_duration_s?;
    if dur <= 0.0 || synthesis_ms < 0 {
        return None;
    }
    Some(synthesis_ms as f64 / (dur * 1000.0))
}

/// Loopback WER: Word Error Rate of the STT transcript against the input
/// reference text. Thin re-export of [`super::text_similarity::word_error_rate`]
/// with argument order fixed for this suite's semantics (hypothesis = transcript,
/// reference = the text we asked the TTS engine to speak). Pure.
pub fn loopback_wer(transcript: &str, reference_text: &str) -> f64 {
    word_error_rate(transcript, reference_text)
}

/// Outcome of one (real or mock) TTS loopback attempt for a case: the STT
/// transcript of the synthesized audio, the synthesis wall-clock, and the coarse
/// audio stats derived from the WAV (both optional — a synthesis that produced no
/// parseable audio still yields WER/perf from whatever is available).
#[derive(Debug, Clone)]
pub struct TtsOutcome {
    pub loopback_transcript: String,
    /// Wall-clock synthesis time (the `/v1/audio/speech` round-trip), ms.
    pub synthesis_ms: i64,
    /// Synthesized-audio duration, seconds; `None` if the audio wasn't parseable.
    pub audio_duration_s: Option<f64>,
    /// MOS-proxy from the waveform RMS; `None` if the audio wasn't parseable.
    pub mos_proxy: Option<f64>,
}

impl TtsOutcome {
    /// Build a [`TtsOutcome`] from a transcript + synthesis time + the raw
    /// synthesized WAV bytes, deriving `audio_duration_s` and `mos_proxy` from
    /// the waveform via [`parse_wav_stats`]/[`mos_proxy`]. Both derived fields are
    /// `None` when the bytes aren't parseable 16-bit PCM WAV. This is the seam the
    /// live runner uses to turn a [`crate::intake::infer::SpeechMetrics`] +
    /// [`crate::intake::infer::TranscribeMetrics`] pair into a scorable outcome.
    pub fn from_audio(transcript: String, synthesis_ms: i64, audio: &[u8]) -> Self {
        let stats = parse_wav_stats(audio);
        TtsOutcome {
            loopback_transcript: transcript,
            synthesis_ms,
            audio_duration_s: stats.as_ref().map(|s| s.duration_s),
            mos_proxy: stats.as_ref().map(|s| mos_proxy(s.rms)),
        }
    }
}

/// Seam for a full TTS loopback (synthesize → transcribe); tests inject a mock
/// outcome. The live runner implements this against
/// [`crate::intake::infer::synthesize_with_metrics`] +
/// [`crate::intake::infer::transcribe_with_metrics`].
pub trait TtsLoopbackModel {
    fn loopback(&self, text: &str) -> Result<TtsOutcome, String>;
}

/// Build the `assistant_dimension_score` rows for one TTS loopback attempt: up to
/// three intelligibility rows (WER, accuracy, and — when audio was parseable —
/// mos_proxy) plus up to two performance rows (synthesis_ms always, rtf when
/// duration is known).
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    case: &TtsCase,
    outcome: &TtsOutcome,
) -> Vec<DimensionScore> {
    let wer = loopback_wer(&outcome.loopback_transcript, &case.text);
    let accuracy = wer_to_accuracy(wer);

    let mut rows = vec![
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION_QUALITY.to_string(),
            metric: "loopback_wer".to_string(),
            value: wer,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: Some(
                serde_json::json!({
                    "case": case.label,
                    "reference": case.text,
                    "transcript": outcome.loopback_transcript,
                })
                .to_string(),
            ),
        },
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION_QUALITY.to_string(),
            metric: "loopback_accuracy".to_string(),
            value: accuracy,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        },
    ];

    if let Some(mos) = outcome.mos_proxy {
        rows.push(DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION_QUALITY.to_string(),
            metric: "mos_proxy".to_string(),
            value: mos,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        });
    }

    rows.push(DimensionScore {
        model_id: model_id.clone(),
        backend_tag,
        dimension: DIMENSION_PERF.to_string(),
        metric: "synthesis_ms".to_string(),
        value: outcome.synthesis_ms as f64,
        std_dev: None,
        judge: "derived".to_string(),
        low_confidence: false,
        raw_json: None,
    });

    if let Some(rtf) = real_time_factor(outcome.synthesis_ms, outcome.audio_duration_s) {
        rows.push(DimensionScore {
            model_id,
            backend_tag,
            dimension: DIMENSION_PERF.to_string(),
            metric: "rtf".to_string(),
            value: rtf,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        });
    }

    rows
}

/// Score one (mock or live) TTS loopback attempt and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "tts")`.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    case: &TtsCase,
    outcome: &TtsOutcome,
) -> Result<(), ToolError> {
    for score in build_scores(model_id.clone(), backend_tag, case, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal 16-bit mono PCM WAV builder for the acoustic-stat tests: a
    /// constant-amplitude tone of `n_samples` samples at `sample_rate`.
    fn make_wav(sample_rate: u32, amplitude: i16, n_samples: u32) -> Vec<u8> {
        let channels: u16 = 1;
        let bits: u16 = 16;
        let byte_rate = sample_rate * channels as u32 * (bits / 8) as u32;
        let block_align = channels * (bits / 8);
        let data_len = n_samples * channels as u32 * (bits / 8) as u32;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&sample_rate.to_le_bytes());
        w.extend_from_slice(&byte_rate.to_le_bytes());
        w.extend_from_slice(&block_align.to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for _ in 0..n_samples {
            w.extend_from_slice(&amplitude.to_le_bytes());
        }
        w
    }

    fn case(text: &str) -> TtsCase {
        TtsCase {
            label: "t".to_string(),
            text: text.to_string(),
            acoustic_ref: None,
        }
    }

    const REFERENCE: &str = "please turn off the kitchen lights and lock the front door";

    /// KNOWN-GOOD (loopback via mock): a transcript equal to the reference scores
    /// WER 0.0 / accuracy 1.0. This is the "feed a known transcript back" case.
    #[test]
    fn perfect_loopback_scores_zero_wer() {
        let outcome = TtsOutcome {
            loopback_transcript: REFERENCE.to_string(),
            synthesis_ms: 400,
            audio_duration_s: Some(2.5),
            mos_proxy: Some(4.2),
        };
        let rows = build_scores(ModelId::from("piper:lessac-medium"), BackendTag::Gpu, &case(REFERENCE), &outcome);
        assert!(rows.iter().all(|r| r.judge == "derived"));

        let wer = rows.iter().find(|r| r.metric == "loopback_wer").unwrap().value;
        assert!(wer.abs() < 1e-9, "expected WER 0.0, got {wer}");
        let acc = rows.iter().find(|r| r.metric == "loopback_accuracy").unwrap().value;
        assert!((acc - 1.0).abs() < 1e-9, "expected accuracy 1.0, got {acc}");
    }

    /// KNOWN-BAD: a garbled loopback transcript scores high WER / low accuracy —
    /// the discriminating case (unintelligible synthesis).
    #[test]
    fn garbled_loopback_scores_high_wer() {
        let outcome = TtsOutcome {
            loopback_transcript: "zzz qqq nonsense blah random garble output".to_string(),
            synthesis_ms: 400,
            audio_duration_s: Some(2.5),
            mos_proxy: Some(4.0),
        };
        let rows = build_scores(ModelId::from("piper:lessac-medium"), BackendTag::Gpu, &case(REFERENCE), &outcome);
        let wer = rows.iter().find(|r| r.metric == "loopback_wer").unwrap().value;
        assert!(wer > 0.7, "expected high WER for garbled loopback, got {wer}");
        let acc = rows.iter().find(|r| r.metric == "loopback_accuracy").unwrap().value;
        assert!(acc < 0.3, "expected low accuracy for garbled loopback, got {acc}");
    }

    /// mos_proxy + rtf rows appear only when their inputs are present; performance
    /// synthesis_ms always appears.
    #[test]
    fn optional_metrics_only_emitted_when_derivable() {
        // No audio parsed: no mos_proxy, no rtf; still get wer/accuracy/synthesis_ms.
        let outcome = TtsOutcome {
            loopback_transcript: REFERENCE.to_string(),
            synthesis_ms: 500,
            audio_duration_s: None,
            mos_proxy: None,
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &case(REFERENCE), &outcome);
        assert!(rows.iter().any(|r| r.metric == "synthesis_ms"));
        assert!(rows.iter().all(|r| r.metric != "mos_proxy"));
        assert!(rows.iter().all(|r| r.metric != "rtf"));

        // With audio stats: mos_proxy + rtf both present.
        let outcome2 = TtsOutcome {
            loopback_transcript: REFERENCE.to_string(),
            synthesis_ms: 500,
            audio_duration_s: Some(2.0),
            mos_proxy: Some(4.1),
        };
        let rows2 = build_scores(ModelId::from("m"), BackendTag::Gpu, &case(REFERENCE), &outcome2);
        let rtf = rows2.iter().find(|r| r.metric == "rtf").unwrap().value;
        assert!((rtf - (500.0 / 2000.0)).abs() < 1e-9, "rtf = 0.5s/2.0s = 0.25, got {rtf}");
        assert!(rows2.iter().any(|r| r.metric == "mos_proxy"));
    }

    #[test]
    fn rtf_never_divides_by_zero() {
        assert_eq!(real_time_factor(500, Some(0.0)), None);
        assert_eq!(real_time_factor(500, None), None);
        assert_eq!(real_time_factor(-1, Some(2.0)), None);
        assert!(real_time_factor(500, Some(2.0)).is_some());
    }

    #[test]
    fn mos_proxy_is_clamped_and_monotone() {
        assert!((mos_proxy(0.0) - 1.0).abs() < 1e-9, "silence floors at 1.0");
        assert!((mos_proxy(-5.0) - 1.0).abs() < 1e-9, "negative floors at 1.0");
        assert!((mos_proxy(1_000_000.0) - 5.0).abs() < 1e-9, "loud saturates at 5.0");
        assert!(mos_proxy(2000.0) > mos_proxy(500.0), "monotone increasing in RMS");
        let m = mos_proxy(2000.0);
        assert!((1.0..=5.0).contains(&m));
    }

    #[test]
    fn wav_parse_recovers_duration_and_rms() {
        // 8000 Hz, amplitude 4000, 8000 samples => 1.0s, RMS == 4000.
        let wav = make_wav(8000, 4000, 8000);
        let stats = parse_wav_stats(&wav).expect("parseable WAV");
        assert!((stats.duration_s - 1.0).abs() < 1e-6, "duration 1.0s, got {}", stats.duration_s);
        assert!((stats.rms - 4000.0).abs() < 1e-6, "constant-amplitude RMS == amplitude, got {}", stats.rms);
        // And mos_proxy of a healthy-amplitude tone lands near the top of the band.
        assert!(mos_proxy(stats.rms) >= 4.0);
    }

    #[test]
    fn wav_parse_rejects_non_wav() {
        assert!(parse_wav_stats(b"not a wav file at all").is_none());
        assert!(parse_wav_stats(&[]).is_none());
        // RIFF header but truncated before any chunk — must not panic, returns None.
        assert!(parse_wav_stats(b"RIFF\x00\x00\x00\x00WAVE").is_none());
    }

    #[test]
    fn from_audio_derives_stats_or_none() {
        let wav = make_wav(16000, 3000, 16000); // 1.0s
        let o = TtsOutcome::from_audio(REFERENCE.to_string(), 300, &wav);
        assert!((o.audio_duration_s.unwrap() - 1.0).abs() < 1e-6);
        assert!(o.mos_proxy.is_some());

        let o2 = TtsOutcome::from_audio(REFERENCE.to_string(), 300, b"garbage");
        assert!(o2.audio_duration_s.is_none());
        assert!(o2.mos_proxy.is_none());
    }

    #[test]
    fn default_corpus_is_nonempty_and_well_formed() {
        let cases = default_cases();
        assert!(!cases.is_empty());
        for c in &cases {
            assert!(!c.label.is_empty());
            assert!(!c.text.is_empty());
        }
    }

    #[test]
    fn load_cases_falls_back_to_default_without_corpus_dir() {
        // Ensure the var is unset for this assertion (sequential test env).
        std::env::remove_var("INTAKE_CORPUS_DIR");
        let cases = load_cases();
        assert!(!cases.is_empty(), "must fall back to the in-source corpus");
    }
}
