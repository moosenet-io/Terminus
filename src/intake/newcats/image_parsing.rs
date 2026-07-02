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
}
