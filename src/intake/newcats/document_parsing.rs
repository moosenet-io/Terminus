//! Category: `document_parsing` — doc/form → structured-output extraction.
//!
//! Probe shape: feed the model a synthetic document/form description (plain
//! text) and an instruction to emit structured output (JSON object of
//! `field -> value`, or a markdown table with the same fields); compare the
//! model's answer against an expected answer key field-by-field.
//!
//! ## Dimension/metric convention (`task_category = "document_parsing"`)
//!   - `dimension = "ocr_extraction"`, `metric = "field_accuracy"` — fraction
//!     of expected fields whose extracted value matches the answer key
//!     (fuzzy match, see [`score_field_accuracy`]), in `[0.0, 1.0]`.
//!   - `dimension = "ocr_extraction"`, `metric = "latency_ms"` — wall-clock
//!     time for the extraction call, milliseconds.
//!   - `dimension = "ocr_extraction"`, `metric = "response_tokens"` — token
//!     count of the model's response, when the backend reports it (omitted
//!     otherwise — see [`build_scores`]).
//!
//! `judge = "derived"` for all three (no LLM-judge panel; these are computed
//! metrics, matching the `dim4_ocean` "derived" convention for non-panel rows).
//!
//! ## Parsing the model's answer
//! [`parse_structured_output`] first tries strict JSON (a `{"field": "value"}`
//! object); if that fails, it falls back to line-based `key: value` parsing
//! (handles a plain or markdown-table-ish response without over-engineering a
//! markdown-table parser). Unparseable output yields an empty field map, which
//! naturally scores `field_accuracy = 0.0`.
//!
//! ## Backend call
//! A live probe would go through `crate::intake::context::generate` /
//! `crate::intake::infer::infer_with_metrics` (same unified path as the
//! assistant dims), timed the same way `infer.rs` times inference (wall-clock
//! around the call, `InferMetrics::total_time_ms`/`response_tokens` mapped
//! straight into this module's `latency_ms`/`response_tokens` metrics). This
//! module deliberately does not hardcode that call inline — [`DocParseModel`]
//! is the seam a runner would implement against a real backend, mirroring
//! `assistant::dim4_ocean::OceanModel`'s mock-for-tests pattern. Tonight's
//! sanity tests exercise only the scoring/write logic via a synthetic mock.

use std::collections::BTreeMap;

use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "document_parsing";
/// `dimension` value this module writes.
pub const DIMENSION: &str = "ocr_extraction";

/// Minimum [`super::text_similarity::normalized_edit_similarity`] for a
/// single field value to count as "matched" (fuzzy, not exact-string, so
/// trivial formatting differences like trailing punctuation don't zero out
/// an otherwise-correct extraction).
pub const FIELD_MATCH_THRESHOLD: f64 = 0.85;

/// Outcome of one live extraction call — what a [`DocParseModel`] returns.
#[derive(Debug, Clone)]
pub struct ExtractionOutcome {
    /// The model's raw response text (JSON object or `key: value` lines).
    pub raw_output: String,
    pub latency_ms: i64,
    pub response_tokens: Option<i64>,
}

/// Seam for calling a backend, so tests inject a synthetic outcome instead of
/// a live network call (mirrors `assistant::dim4_ocean::OceanModel`).
pub trait DocParseModel {
    fn extract(&self, document_text: &str, fields_prompt: &str) -> Result<ExtractionOutcome, String>;
}

/// Parse a model's structured-output answer into a `field -> value` map.
///
/// Tries strict JSON object first (values coerced to their string form for any
/// scalar JSON type); falls back to line-based `key: value` parsing. Returns
/// an empty map (not an error) on total failure — an empty map naturally
/// yields `field_accuracy = 0.0` via [`score_field_accuracy`], which is the
/// correct "the model didn't produce usable structured output" signal.
pub fn parse_structured_output(raw: &str) -> BTreeMap<String, String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) {
        if let serde_json::Value::Object(map) = v {
            return map
                .into_iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    };
                    (k, val)
                })
                .collect();
        }
    }
    // Fallback: `key: value` per line (tolerates a leading "- " / "| " table
    // marker and trailing "|" from a naive markdown-table response).
    let mut out = BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim().trim_start_matches(['-', '|']).trim();
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().trim_matches('|').trim().to_string();
            let val = v.trim().trim_matches('|').trim().to_string();
            if !key.is_empty() && !val.is_empty() {
                out.insert(key, val);
            }
        }
    }
    out
}

/// Fraction of `expected` fields whose value in `actual` fuzzy-matches
/// (normalized edit similarity ≥ [`FIELD_MATCH_THRESHOLD`]), in `[0.0, 1.0]`.
/// A field entirely missing from `actual` counts as unmatched (similarity 0).
/// Returns `0.0` if `expected` is empty (no fields to check — avoids a NaN
/// from a divide-by-zero, and there's nothing to have gotten right).
pub fn score_field_accuracy(
    expected: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> f64 {
    if expected.is_empty() {
        return 0.0;
    }
    let matched = expected
        .iter()
        .filter(|(k, expected_v)| {
            actual
                .get(k.as_str())
                .map(|actual_v| {
                    super::text_similarity::normalized_edit_similarity(expected_v, actual_v)
                        >= FIELD_MATCH_THRESHOLD
                })
                .unwrap_or(false)
        })
        .count();
    matched as f64 / expected.len() as f64
}

/// Build the `assistant_dimension_score` rows for one document_parsing probe.
/// Writes via `insert_dimension_score_with_category(pool, run_id, score,
/// "document_parsing")` for each row returned here — callers own the DB write.
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    expected: &BTreeMap<String, String>,
    outcome: &ExtractionOutcome,
) -> Vec<DimensionScore> {
    let actual = parse_structured_output(&outcome.raw_output);
    let accuracy = score_field_accuracy(expected, &actual);

    let mut rows = vec![
        DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "field_accuracy".to_string(),
            value: accuracy,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: Some(outcome.raw_output.clone()),
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
    if let Some(tokens) = outcome.response_tokens {
        rows.push(DimensionScore {
            model_id,
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: "response_tokens".to_string(),
            value: tokens as f64,
            std_dev: None,
            judge: "derived".to_string(),
            low_confidence: false,
            raw_json: None,
        });
    }
    rows
}

/// Score one probe and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "document_parsing")`
/// — the write path the operator asked this module to use.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    expected: &BTreeMap<String, String>,
    outcome: &ExtractionOutcome,
) -> Result<(), ToolError> {
    for score in build_scores(model_id, backend_tag, expected, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_answer_key() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("invoice_number".to_string(), "INV-4471".to_string());
        m.insert("total_due".to_string(), "512.00".to_string());
        m.insert("vendor_name".to_string(), "Acme Supply Co".to_string());
        m
    }

    #[test]
    fn parses_strict_json_object() {
        let raw = r#"{"invoice_number": "INV-4471", "total_due": "512.00"}"#;
        let parsed = parse_structured_output(raw);
        assert_eq!(parsed.get("invoice_number").unwrap(), "INV-4471");
        assert_eq!(parsed.get("total_due").unwrap(), "512.00");
    }

    #[test]
    fn parses_key_value_fallback_lines() {
        let raw = "invoice_number: INV-4471\ntotal_due: 512.00\nvendor_name: Acme Supply Co";
        let parsed = parse_structured_output(raw);
        assert_eq!(parsed.get("vendor_name").unwrap(), "Acme Supply Co");
    }

    /// KNOWN-GOOD: exact-match extraction scores field_accuracy == 1.0.
    #[test]
    fn known_good_extraction_scores_full_accuracy() {
        let expected = expected_answer_key();
        let outcome = ExtractionOutcome {
            raw_output: r#"{"invoice_number": "INV-4471", "total_due": "512.00", "vendor_name": "Acme Supply Co"}"#
                .to_string(),
            latency_ms: 340,
            response_tokens: Some(42),
        };
        let rows = build_scores(ModelId::from("qwen3:8b"), BackendTag::Gpu, &expected, &outcome);
        let accuracy = rows
            .iter()
            .find(|r| r.metric == "field_accuracy")
            .unwrap()
            .value;
        assert!((accuracy - 1.0).abs() < 1e-9, "expected 1.0, got {accuracy}");

        let latency = rows.iter().find(|r| r.metric == "latency_ms").unwrap().value;
        assert_eq!(latency, 340.0);
        let tokens = rows
            .iter()
            .find(|r| r.metric == "response_tokens")
            .unwrap()
            .value;
        assert_eq!(tokens, 42.0);
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));
    }

    /// KNOWN-BAD: wrong/garbled values (and one missing field) scores
    /// field_accuracy near 0 — the important negative case.
    #[test]
    fn known_bad_extraction_scores_low_accuracy() {
        let expected = expected_answer_key();
        let outcome = ExtractionOutcome {
            raw_output: r#"{"invoice_number": "XX-0000", "total_due": "not a number"}"#.to_string(),
            latency_ms: 200,
            response_tokens: None,
        };
        let rows = build_scores(ModelId::from("bad-model:1b"), BackendTag::Cpu, &expected, &outcome);
        let accuracy = rows
            .iter()
            .find(|r| r.metric == "field_accuracy")
            .unwrap()
            .value;
        assert!(accuracy < 0.2, "expected near-zero accuracy, got {accuracy}");
        // No response_tokens row when the outcome doesn't report tokens.
        assert!(rows.iter().all(|r| r.metric != "response_tokens"));
    }

    #[test]
    fn empty_output_scores_zero_not_nan() {
        let expected = expected_answer_key();
        let actual = parse_structured_output("not structured output at all, sorry");
        let score = score_field_accuracy(&expected, &actual);
        assert_eq!(score, 0.0);
    }
}
