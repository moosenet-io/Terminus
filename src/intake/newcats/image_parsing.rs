//! Category: `image_parsing` (vision) — image → description/caption probe.
//!
//! Probe shape: feed a vision-capable backend an image plus an instruction to
//! describe it, and score the response against an expected caption/description
//! by token overlap (see [`score_caption_similarity`]).
//!
//! ## Dimension/metric convention (`task_category = "image_parsing"`)
//!   - `dimension = "vision_description"`, `metric = "caption_similarity"` —
//!     [`super::text_similarity::token_jaccard`] between the model's
//!     description and the expected caption, in `[0.0, 1.0]`. Deliberately a
//!     simple bag-of-words overlap, not semantic similarity: documented here
//!     as the scoring method so a future replacement (embedding cosine sim)
//!     is a clear upgrade path, not a silent behavior change.
//!   - `dimension = "vision_description"`, `metric = "latency_ms"` —
//!     wall-clock time for the describe call, milliseconds.
//!
//! `judge = "derived"` for both (computed metric, no LLM-judge panel).
//!
//! ## Synthetic test image (no new crate dependency)
//! [`synthetic_solid_color_bmp`] emits a minimal, valid 24-bit uncompressed BMP
//! (a solid-color square) via raw byte-writing — deliberately BMP, not PNG:
//! PNG requires a DEFLATE-compressed IDAT chunk, which would need either a new
//! `flate2`/`png` dependency or a hand-rolled deflate encoder; BMP's pixel data
//! is uncompressed, so a correct encoder is ~30 lines with zero new
//! dependencies (checked: no `image`/`png` crate exists in `Cargo.toml` today).
//! An obviously-checkable expected caption ("a solid red square") pairs with
//! it for the sanity test below.
//!
//! ## Backend call
//! [`ImageParseModel`] is the seam a runner would implement against a live
//! vision backend (analogous to `document_parsing::DocParseModel`). No
//! vision-capable backend was found reachable from this box tonight (recon:
//! no vision model/endpoint configured) — this module is exercised via a
//! synthetic mock, same pattern as `document_parsing`.

use std::path::Path;

use serde::Deserialize;
use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "image_parsing";
/// `dimension` value this module writes.
pub const DIMENSION: &str = "vision_description";

/// Outcome of one live vision-describe call.
#[derive(Debug, Clone)]
pub struct DescribeOutcome {
    pub description: String,
    pub latency_ms: i64,
}

/// Seam for calling a vision-capable backend; tests inject a mock.
pub trait ImageParseModel {
    fn describe(&self, image_bytes: &[u8]) -> Result<DescribeOutcome, String>;
}

/// Caption similarity: bag-of-words Jaccard overlap in `[0.0, 1.0]` between the
/// model's description and the expected caption. See module doc for why this
/// (not semantic embedding similarity) was chosen: it's dependency-free,
/// deterministic, and good enough to separate "described the right thing" from
/// "described something else entirely" for a sanity-check probe.
pub fn score_caption_similarity(actual_description: &str, expected_caption: &str) -> f64 {
    // Finding 7: an empty reference (or empty input) must score 0.0, not the
    // Jaccard-of-two-empty-sets 1.0 that `token_jaccard` returns for empty+empty.
    // An empty expected caption paired with an empty model answer is a non-answer,
    // not a perfect caption match — scoring it 1.0 would fabricate a top score.
    if actual_description.trim().is_empty() || expected_caption.trim().is_empty() {
        return 0.0;
    }
    super::text_similarity::token_jaccard(actual_description, expected_caption)
}

/// Build the `assistant_dimension_score` rows for one image_parsing probe.
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    expected_caption: &str,
    outcome: &DescribeOutcome,
) -> Vec<DimensionScore> {
    let similarity = score_caption_similarity(&outcome.description, expected_caption);
    vec![
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "caption_similarity".to_string(),
            value: similarity,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: Some(outcome.description.clone()),
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

/// Emit a minimal, valid uncompressed 24-bit BMP of `size x size` pixels, all
/// the same `(r, g, b)` color. No external crate: BMP rows are raw pixel bytes
/// (bottom-up, each row padded to a 4-byte multiple), so no compression codec
/// is needed — unlike PNG.
pub fn synthetic_solid_color_bmp(size: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
    let row_unpadded = (size as usize) * 3;
    let row_padding = (4 - (row_unpadded % 4)) % 4;
    let row_size = row_unpadded + row_padding;
    let pixel_data_size = row_size * size as usize;
    let file_header_size = 14u32;
    let info_header_size = 40u32;
    let pixel_offset = file_header_size + info_header_size;
    let file_size = pixel_offset + pixel_data_size as u32;

    let mut buf = Vec::with_capacity(file_size as usize);
    // -- BITMAPFILEHEADER (14 bytes) --
    buf.extend_from_slice(b"BM");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved2
    buf.extend_from_slice(&pixel_offset.to_le_bytes());
    // -- BITMAPINFOHEADER (40 bytes) --
    buf.extend_from_slice(&info_header_size.to_le_bytes());
    buf.extend_from_slice(&(size as i32).to_le_bytes()); // width
    buf.extend_from_slice(&(size as i32).to_le_bytes()); // height (positive => bottom-up)
    buf.extend_from_slice(&1u16.to_le_bytes()); // planes
    buf.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    buf.extend_from_slice(&0u32.to_le_bytes()); // compression: BI_RGB (none)
    buf.extend_from_slice(&(pixel_data_size as u32).to_le_bytes());
    buf.extend_from_slice(&2835i32.to_le_bytes()); // x pixels/meter (~72 DPI)
    buf.extend_from_slice(&2835i32.to_le_bytes()); // y pixels/meter
    buf.extend_from_slice(&0u32.to_le_bytes()); // colors used
    buf.extend_from_slice(&0u32.to_le_bytes()); // important colors
    // -- Pixel data (BGR order, bottom-up rows, each row zero-padded to 4 bytes) --
    for _row in 0..size {
        for _col in 0..size {
            buf.push(b);
            buf.push(g);
            buf.push(r);
        }
        for _pad in 0..row_padding {
            buf.push(0);
        }
    }
    buf
}

/// Score one probe and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "image_parsing")`.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    expected_caption: &str,
    outcome: &DescribeOutcome,
) -> Result<(), ToolError> {
    for score in build_scores(model_id, backend_tag, expected_caption, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Vision-QA suite (SUITE-VQA, S125) — the fleet-wired probe built ON TOP of
// this module. Where the original `image_parsing` path scores a free-form
// caption against an expected description, the vision-QA path scores a SHORT
// ANSWER to a question about the image against a reference answer, and emits
// the SUITE-VQA metric family: accuracy (lenient match), caption similarity,
// hallucination flag, latency, and (when readable) VRAM. Same `task_category`
// / `dimension` / write helper as above, so it is one coherent vision family.
// ---------------------------------------------------------------------------

/// One vision-QA corpus item, matching the on-disk `manifest.json` array schema
/// (`image_file` / `question` / `answer`). `image_file` is relative to the
/// corpus directory; `answer` is the reference (ground-truth) short answer.
#[derive(Debug, Clone, Deserialize)]
pub struct VisionQaItem {
    pub image_file: String,
    pub question: String,
    pub answer: String,
}

/// Load and parse the vision-QA `manifest.json` (a JSON ARRAY of
/// [`VisionQaItem`]) from `corpus_dir`. The images live alongside it
/// (`corpus_dir/<image_file>`); this parses only the manifest. A missing file
/// or malformed JSON is a clean [`ToolError`], never a panic — the runner turns
/// it into a diagnosable failure rather than a crash.
pub fn load_vision_qa_manifest(corpus_dir: &Path) -> Result<Vec<VisionQaItem>, ToolError> {
    let path = corpus_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        ToolError::NotConfigured(format!(
            "vision_qa manifest unreadable at {}: {e}",
            path.display()
        ))
    })?;
    let items: Vec<VisionQaItem> = serde_json::from_str(&raw)
        .map_err(|e| ToolError::InvalidArgument(format!("vision_qa manifest parse error: {e}")))?;
    Ok(items)
}

/// Outcome of one (real or mock) vision-QA answer call.
#[derive(Debug, Clone)]
pub struct VisionQaOutcome {
    /// The model's answer text (empty on a transport/model error — still scored
    /// as a miss, never fabricated).
    pub answer: String,
    /// Wall-clock time for the answer call, ms.
    pub latency_ms: i64,
    /// VRAM in use at call time, MB; `None` if unreadable (CPU host / sysfs
    /// absent) — never fabricated as 0.
    pub vram_peak_mb: Option<u64>,
}

/// Seam for calling a vision-QA backend (image + question → short answer); a
/// runner implements this against the live chat/vision route, tests inject a
/// mock. Distinct from [`ImageParseModel`] (caption) — a VQA call takes a
/// question and returns a richer outcome (latency + VRAM).
pub trait VisionQaModel {
    fn answer(&self, image_bytes: &[u8], question: &str) -> Result<VisionQaOutcome, String>;
}

/// Uncertainty markers a model may emit instead of a confident (possibly wrong)
/// answer — an honest "I don't know" is a MISS, not a hallucination, so these
/// are excluded from the hallucination count.
const UNCERTAINTY_MARKERS: &[&str] = &[
    "i don't know",
    "i do not know",
    "i'm not sure",
    "not sure",
    "cannot tell",
    "can't tell",
    "unable to",
    "unclear",
    "n/a",
];

/// Normalize an answer for lenient matching: lowercase, replace every
/// non-alphanumeric char with a space, and split into tokens. So "Red.",
/// "red", and "The circle is red" all share the token `red`. Pure.
fn normalize_answer(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|t| t.to_string())
        .collect()
}

/// Lenient VQA accuracy: every token of the reference answer appears (order-
/// independent) among the model's answer tokens. This is the same lenient
/// criterion the llava baseline used ("Red" ⇒ matches "red"; "The circle is
/// red" ⇒ matches "red"). An empty reference never matches. Pure.
pub fn lenient_match(actual: &str, expected: &str) -> bool {
    let exp = normalize_answer(expected);
    if exp.is_empty() {
        return false;
    }
    let act = normalize_answer(actual);
    exp.iter().all(|e| act.iter().any(|a| a == e))
}

/// Whether the model's answer counts as a hallucination: a NON-empty,
/// CONFIDENT answer (carrying no uncertainty marker) that does NOT lenient-match
/// the reference. An empty answer or an explicit "I don't know" is a miss, not a
/// hallucination — so the hallucination rate isolates confident-but-wrong
/// answers, the failure mode that actually matters for a VLM. Pure.
pub fn is_hallucination(actual: &str, expected: &str) -> bool {
    let trimmed = actual.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    if UNCERTAINTY_MARKERS.iter().any(|m| lower.contains(m)) {
        return false;
    }
    !lenient_match(actual, expected)
}

/// Build the `assistant_dimension_score` rows for one vision-QA item attempt:
/// accuracy (0/1 lenient), caption/answer similarity (jaccard), hallucination
/// (0/1), latency, and — when readable — VRAM. Every row is `judge = "derived"`
/// under [`DIMENSION`]. The accuracy row carries an audit `raw_json` with the
/// question, model answer, and expected answer.
pub fn build_vqa_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    item: &VisionQaItem,
    outcome: &VisionQaOutcome,
) -> Vec<DimensionScore> {
    let accurate = lenient_match(&outcome.answer, &item.answer);
    let similarity = score_caption_similarity(&outcome.answer, &item.answer);
    let hallucinated = is_hallucination(&outcome.answer, &item.answer);

    let mut rows = vec![
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "accuracy".to_string(),
            value: if accurate { 1.0 } else { 0.0 },
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: Some(
                serde_json::json!({
                    "image_file": item.image_file,
                    "question": item.question,
                    "answer": outcome.answer,
                    "expected": item.answer,
                })
                .to_string(),
            ),
        },
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "caption_similarity".to_string(),
            value: similarity,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        },
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "hallucination".to_string(),
            value: if hallucinated { 1.0 } else { 0.0 },
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

    if let Some(vram) = outcome.vram_peak_mb {
        rows.push(DimensionScore {
            model_id,
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "vram_peak_mb".to_string(),
            value: vram as f64,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        });
    }

    rows
}

/// Score one (mock or live) vision-QA attempt and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "image_parsing")`.
pub async fn score_and_write_vqa(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    item: &VisionQaItem,
    outcome: &VisionQaOutcome,
) -> Result<(), ToolError> {
    for score in build_vqa_scores(model_id.clone(), backend_tag, item, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

/// Encode image `bytes` as an OpenAI-vision `data:` URL, picking the MIME type
/// from `image_file`'s extension (defaults to `application/octet-stream`). This
/// is what the chat route's `image_url` content part carries. Pure.
pub fn to_data_url(image_file: &str, bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    let lower = image_file.to_lowercase();
    let mime = if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".bmp") {
        "image/bmp"
    } else {
        "application/octet-stream"
    };
    format!("data:{mime};base64,{}", B64.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_bmp_has_valid_header_and_size() {
        let bmp = synthetic_solid_color_bmp(8, 255, 0, 0);
        assert_eq!(&bmp[0..2], b"BM");
        let file_size = u32::from_le_bytes([bmp[2], bmp[3], bmp[4], bmp[5]]);
        assert_eq!(file_size as usize, bmp.len());
        let pixel_offset = u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]);
        assert_eq!(pixel_offset, 54); // 14 + 40, no color table for 24bpp
        // First pixel (bottom-up row 0, col 0) is BGR = (0, 0, 255) for solid red.
        let px = &bmp[54..57];
        assert_eq!(px, &[0, 0, 255]);
    }

    /// KNOWN-GOOD: description matching the expected caption scores near 1.0.
    #[test]
    fn known_good_description_scores_high_similarity() {
        let outcome = DescribeOutcome {
            description: "a solid red square".to_string(),
            latency_ms: 900,
        };
        let rows = build_scores(
            ModelId::from("vision-model:7b"),
            BackendTag::Gpu,
            "a solid red square",
            &outcome,
        );
        let sim = rows
            .iter()
            .find(|r| r.metric == "caption_similarity")
            .unwrap()
            .value;
        assert!((sim - 1.0).abs() < 1e-9, "expected 1.0, got {sim}");
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));
    }

    /// Finding 7: an empty reference (or empty input) scores 0.0, never the
    /// empty-vs-empty Jaccard 1.0. `lenient_match`/`is_hallucination` are unchanged.
    #[test]
    fn caption_similarity_empty_reference_or_input_is_zero() {
        assert_eq!(score_caption_similarity("", ""), 0.0);
        assert_eq!(score_caption_similarity("a red square", ""), 0.0);
        assert_eq!(score_caption_similarity("", "a red square"), 0.0);
        assert_eq!(score_caption_similarity("   ", "  "), 0.0);
        // A build_vqa_scores row with an empty reference answer must not fabricate
        // a perfect caption_similarity.
        let outcome = VisionQaOutcome { answer: String::new(), latency_ms: 5, vram_peak_mb: None };
        let item = VisionQaItem {
            image_file: "x.png".into(),
            question: "q".into(),
            answer: String::new(),
        };
        let rows = build_vqa_scores(ModelId::from("m"), BackendTag::Gpu, &item, &outcome);
        let sim = rows.iter().find(|r| r.metric == "caption_similarity").unwrap().value;
        assert_eq!(sim, 0.0);
    }

    /// KNOWN-BAD: description of something entirely different scores near 0.
    #[test]
    fn known_bad_description_scores_low_similarity() {
        let outcome = DescribeOutcome {
            description: "a photograph of a mountain range at sunset".to_string(),
            latency_ms: 900,
        };
        let rows = build_scores(
            ModelId::from("vision-model:7b"),
            BackendTag::Gpu,
            "a solid red square",
            &outcome,
        );
        let sim = rows
            .iter()
            .find(|r| r.metric == "caption_similarity")
            .unwrap()
            .value;
        assert!(sim < 0.2, "expected near-zero similarity, got {sim}");
    }

    // ---- SUITE-VQA (vision_qa) --------------------------------------------

    fn vqa_item() -> VisionQaItem {
        VisionQaItem {
            image_file: "img_001.png".to_string(),
            question: "What color is the circle? Answer with a single color word.".to_string(),
            answer: "red".to_string(),
        }
    }

    /// The in-repo fixture manifest parses into the expected VQA items — the
    /// backend-independent parse-path test (no big corpus committed).
    #[test]
    fn fixture_manifest_parses_into_items() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/intake/newcats/testdata/vision_qa");
        let items = load_vision_qa_manifest(&dir).expect("fixture manifest parses");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].image_file, "img_001.png");
        assert_eq!(items[0].answer, "red");
        assert!(items[1].question.contains("How many"));
    }

    /// A missing manifest is a clean error, never a panic.
    #[test]
    fn missing_manifest_is_clean_error() {
        let dir = std::path::Path::new("/nonexistent/vision_qa/dir");
        assert!(load_vision_qa_manifest(dir).is_err());
    }

    /// Lenient match is case- and punctuation-insensitive and accepts the
    /// reference answer embedded in a fuller sentence (the llava-baseline rule).
    #[test]
    fn lenient_match_is_case_and_substring_tolerant() {
        assert!(lenient_match("Red", "red"));
        assert!(lenient_match("The circle is red.", "red"));
        assert!(lenient_match("2", "2"));
        assert!(!lenient_match("blue", "red"));
        assert!(!lenient_match("", "red"));
        assert!(!lenient_match("red", "")); // empty reference never matches
    }

    /// Hallucination = confident-but-wrong; empty or "don't know" is a miss, not
    /// a hallucination.
    #[test]
    fn hallucination_isolates_confident_wrong_answers() {
        assert!(is_hallucination("blue", "red")); // confident + wrong
        assert!(!is_hallucination("red", "red")); // correct
        assert!(!is_hallucination("", "red")); // empty miss
        assert!(!is_hallucination("I don't know", "red")); // honest abstention
        assert!(!is_hallucination("I'm not sure, maybe", "red"));
    }

    /// A correct answer yields accuracy=1, hallucination=0, and lands latency +
    /// VRAM rows.
    #[test]
    fn vqa_correct_answer_scores_accurate_no_hallucination() {
        let outcome = VisionQaOutcome {
            answer: "Red".to_string(),
            latency_ms: 1200,
            vram_peak_mb: Some(8000),
        };
        let rows = build_vqa_scores(ModelId::from("llava:7b"), BackendTag::Gpu, &vqa_item(), &outcome);
        let acc = rows.iter().find(|r| r.metric == "accuracy").unwrap();
        assert_eq!(acc.value, 1.0);
        let hall = rows.iter().find(|r| r.metric == "hallucination").unwrap();
        assert_eq!(hall.value, 0.0);
        let lat = rows.iter().find(|r| r.metric == "latency_ms").unwrap();
        assert_eq!(lat.value, 1200.0);
        let vram = rows.iter().find(|r| r.metric == "vram_peak_mb").unwrap();
        assert_eq!(vram.value, 8000.0);
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));
    }

    /// A confident wrong answer yields accuracy=0, hallucination=1.
    #[test]
    fn vqa_wrong_answer_scores_hallucination() {
        let outcome = VisionQaOutcome {
            answer: "blue".to_string(),
            latency_ms: 1100,
            vram_peak_mb: None,
        };
        let rows = build_vqa_scores(ModelId::from("llava:7b"), BackendTag::Gpu, &vqa_item(), &outcome);
        assert_eq!(rows.iter().find(|r| r.metric == "accuracy").unwrap().value, 0.0);
        assert_eq!(rows.iter().find(|r| r.metric == "hallucination").unwrap().value, 1.0);
        // VRAM unreadable ⇒ no vram row fabricated.
        assert!(rows.iter().all(|r| r.metric != "vram_peak_mb"));
    }

    /// The data URL carries the right MIME + base64 payload for the chat route.
    #[test]
    fn data_url_encodes_png_mime_and_base64() {
        let url = to_data_url("img_001.png", &[0u8, 1, 2, 3]);
        assert!(url.starts_with("data:image/png;base64,"));
        assert!(url.ends_with("AAECAw==")); // base64 of [0,1,2,3]
        assert!(to_data_url("x.jpg", b"a").starts_with("data:image/jpeg;base64,"));
    }
}
