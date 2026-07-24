//! Category: `document_parsing` — doc/form → structured-output extraction.
//!
//! Probe shape: send the model (via Chord's `/v1/documents/parse`, docling
//! behind it) a document (PDF/image bytes) and compare the parser's output
//! against a ground-truth answer key on THREE axes:
//!   1. named business FIELDS (`field -> value`), field-by-field, and
//!   2. the full document TEXT, character- and word-level (CER/WER), and
//!   3. any TABLES, cell-set F1.
//!
//! ## Dimension/metric convention (`task_category = "document_parsing"`)
//! All rows carry `dimension = "ocr_extraction"`, `judge = "derived"` (computed
//! metrics, no LLM-judge panel — matching `voice_transcription`'s convention):
//!   - `metric = "field_accuracy"` — fraction of expected fields whose extracted
//!     value fuzzy-matches the answer key (see [`score_field_accuracy`]), in
//!     `[0.0, 1.0]`, higher is better.
//!   - `metric = "cer"` — Character Error Rate of the parsed text vs. the
//!     reference text (see [`char_error_rate`]). `0.0` is perfect; unbounded
//!     above; LOWER is better. Emitted only when the case ships a reference text.
//!   - `metric = "wer"` — Word Error Rate of the parsed text vs. the reference
//!     text (shared [`super::text_similarity::word_error_rate`]). Same
//!     convention as CER. Emitted only when the case ships a reference text.
//!   - `metric = "table_f1"` — cell-set F1 of the parsed tables vs. the expected
//!     tables (see [`score_table_f1`]), in `[0.0, 1.0]`, higher is better.
//!     Emitted only when the case ships expected tables.
//!   - `metric = "latency_ms"` — wall-clock time for the parse call, ms.
//!   - `metric = "response_tokens"` — token count when the backend reports it.
//!
//! ## Parsing the model's answer
//! [`parse_structured_output`] first tries strict JSON (a `{"field": "value"}`
//! object); if that fails, it falls back to line-based `key: value` parsing.
//! It is the fallback field source when the parser did not return a structured
//! `fields` map directly (an [`ExtractionOutcome`] with an empty `fields`).
//! Unparseable output yields an empty field map, which naturally scores
//! `field_accuracy = 0.0`.
//!
//! ## Backend
//! A live probe goes through [`crate::intake::infer::docparse_with_metrics`]
//! (the `openai` arm → Chord `POST /v1/documents/parse`), timed by that path's
//! wall clock; [`crate::intake::runner::run_document_parsing_suite`] is the
//! driver. [`DocParseModel`] is the in-module seam a test injects a synthetic
//! parse against, mirroring `voice_transcription::AsrModel`; the scoring logic
//! is exercised with no live network call.
//!
//! ## Corpus
//! Ground-truth cases load from `INTAKE_CORPUS_DIR/document_parsing/manifest.json`
//! (see [`load_corpus`]) — the single unified corpus env var (DR-02). No
//! compiled-in default (PII remediation): a missing var fails clean with
//! `ToolError::NotConfigured`. A compact reference fixture ships under
//! `src/intake/newcats/corpora/document_parsing/` as the authoritative format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "document_parsing";
/// `dimension` value this module writes.
pub const DIMENSION: &str = "ocr_extraction";
/// Corpus subdirectory (under `INTAKE_CORPUS_DIR`) this suite loads from.
pub const CORPUS_SUBDIR: &str = "document_parsing";

/// Minimum [`super::text_similarity::normalized_edit_similarity`] for a single
/// field value to count as "matched" (fuzzy, not exact-string, so trivial
/// formatting differences like trailing punctuation don't zero out an otherwise
/// correct extraction).
pub const FIELD_MATCH_THRESHOLD: f64 = 0.85;

/// One parsed table: row-major grid of cell strings.
pub type Table = Vec<Vec<String>>;

/// Ground truth for one document case — the answer key the parse is scored
/// against. Any axis may be empty (a fields-only case ships no `reference_text`
/// or `tables`, and the corresponding metric rows are then simply not emitted).
#[derive(Debug, Clone, Default)]
pub struct GroundTruth {
    pub fields: BTreeMap<String, String>,
    pub reference_text: String,
    pub tables: Vec<Table>,
}

/// Outcome of one live parse call — what a [`DocParseModel`] (or the live
/// [`crate::intake::infer::docparse_with_metrics`] path) returns.
#[derive(Debug, Clone, Default)]
pub struct ExtractionOutcome {
    /// The model's raw structured-answer text (JSON object or `key: value`
    /// lines); the FALLBACK field source when `fields` is empty.
    pub raw_output: String,
    /// The parser's full document text/markdown (the CER/WER source).
    pub text: String,
    /// Directly-returned key/value fields — PREFERRED over parsing `raw_output`.
    pub fields: BTreeMap<String, String>,
    /// Extracted tables (the table-F1 source).
    pub tables: Vec<Table>,
    pub latency_ms: i64,
    pub response_tokens: Option<i64>,
}

/// Seam for calling a document-parse backend, so tests inject a synthetic
/// outcome instead of a live network call (mirrors `voice_transcription::AsrModel`).
pub trait DocParseModel {
    fn parse(&self, document: &[u8], filename: &str) -> Result<ExtractionOutcome, String>;
}

/// The field map to score an outcome on: the parser's directly-returned
/// `fields` when present, else [`parse_structured_output`] of `raw_output`.
pub fn extracted_fields(outcome: &ExtractionOutcome) -> BTreeMap<String, String> {
    if !outcome.fields.is_empty() {
        outcome.fields.clone()
    } else {
        parse_structured_output(&outcome.raw_output)
    }
}

/// Parse a model's structured-output answer into a `field -> value` map.
///
/// Tries strict JSON object first (scalar values coerced to their string form);
/// falls back to line-based `key: value` parsing. Returns an empty map (not an
/// error) on total failure — an empty map naturally yields `field_accuracy = 0.0`
/// via [`score_field_accuracy`], the correct "no usable structured output" signal.
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
/// A field missing from `actual` counts as unmatched. Returns `0.0` if
/// `expected` is empty (nothing to check — avoids a divide-by-zero NaN).
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

/// Character Error Rate: `char-level levenshtein(hyp, ref) / len(ref chars)`.
/// `0.0` is a perfect transcription; unbounded above; LOWER is better. Built on
/// the shared [`super::text_similarity::levenshtein`] (over `char`s), so it adds
/// no new edit-distance implementation. Case-insensitive, whitespace-trimmed.
pub fn char_error_rate(hypothesis: &str, reference: &str) -> f64 {
    let r: Vec<char> = reference.trim().to_lowercase().chars().collect();
    let h: Vec<char> = hypothesis.trim().to_lowercase().chars().collect();
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    super::text_similarity::levenshtein(&h, &r) as f64 / r.len() as f64
}

/// Cell-set F1 of `actual` tables vs. `expected` tables, in `[0.0, 1.0]`.
///
/// A TEDS-lite: each table is flattened to a MULTISET of normalized (trimmed,
/// lowercased, non-empty) cell strings; F1 is the harmonic mean of cell
/// precision and recall over the multiset intersection (min-count per value).
/// Order- and structure-insensitive on purpose — a parser that recovers the
/// right cell CONTENT but reshapes the grid still scores well, which is the
/// signal we want here (did it extract the tabular data), not exact geometry.
/// Both-empty ⇒ `1.0`; either-side-empty (but not both) ⇒ `0.0`.
pub fn score_table_f1(expected: &[Table], actual: &[Table]) -> f64 {
    fn multiset(tables: &[Table]) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for table in tables {
            for row in table {
                for cell in row {
                    let c = cell.trim().to_lowercase();
                    if !c.is_empty() {
                        *m.entry(c).or_insert(0) += 1;
                    }
                }
            }
        }
        m
    }
    let exp = multiset(expected);
    let act = multiset(actual);
    if exp.is_empty() && act.is_empty() {
        return 1.0;
    }
    let exp_total: usize = exp.values().sum();
    let act_total: usize = act.values().sum();
    if exp_total == 0 || act_total == 0 {
        return 0.0;
    }
    let tp: usize = exp
        .iter()
        .map(|(k, ec)| act.get(k).map(|ac| (*ec).min(*ac)).unwrap_or(0))
        .sum();
    let precision = tp as f64 / act_total as f64;
    let recall = tp as f64 / exp_total as f64;
    if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

/// Build the `assistant_dimension_score` rows for one document_parsing probe:
/// `field_accuracy` + `latency_ms` always; `cer`/`wer` when the case ships a
/// reference text; `table_f1` when it ships expected tables; `response_tokens`
/// when the backend reported it. Callers own the DB write.
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    truth: &GroundTruth,
    outcome: &ExtractionOutcome,
) -> Vec<DimensionScore> {
    let derived = |metric: &str, value: f64, raw_json: Option<String>| DimensionScore {
        model_id: model_id.clone(),
        backend_tag,
        dimension: DIMENSION.to_string(),
        metric: metric.to_string(),
        value,
        std_dev: None,
        judge: "derived".to_string(),
        low_confidence: false,
        raw_json,
    };

    let actual_fields = extracted_fields(outcome);
    let accuracy = score_field_accuracy(&truth.fields, &actual_fields);

    let mut rows = vec![derived(
        "field_accuracy",
        accuracy,
        Some(
            serde_json::json!({
                "expected_fields": truth.fields,
                "actual_fields": actual_fields,
            })
            .to_string(),
        ),
    )];

    if !truth.reference_text.is_empty() {
        rows.push(derived("cer", char_error_rate(&outcome.text, &truth.reference_text), None));
        rows.push(derived(
            "wer",
            super::text_similarity::word_error_rate(&outcome.text, &truth.reference_text),
            None,
        ));
    }

    if !truth.tables.is_empty() {
        rows.push(derived("table_f1", score_table_f1(&truth.tables, &outcome.tables), None));
    }

    rows.push(derived("latency_ms", outcome.latency_ms as f64, None));

    if let Some(tokens) = outcome.response_tokens {
        rows.push(derived("response_tokens", tokens as f64, None));
    }
    rows
}

/// Score one probe and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "document_parsing")`.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    truth: &GroundTruth,
    outcome: &ExtractionOutcome,
) -> Result<(), ToolError> {
    for score in build_scores(model_id, backend_tag, truth, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

// ── Corpus (INTAKE_CORPUS_DIR/document_parsing/manifest.json) ───────────────

/// One document case in the corpus manifest: the document file (relative to the
/// corpus subdir) plus its ground-truth answer key on the three scored axes.
#[derive(Debug, Clone, Deserialize)]
pub struct DocCase {
    pub id: String,
    /// Document filename, relative to the corpus subdir (the bytes to POST).
    pub file: String,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    #[serde(default)]
    pub reference_text: String,
    #[serde(default)]
    pub tables: Vec<Table>,
}

impl DocCase {
    /// The ground-truth answer key for this case.
    pub fn ground_truth(&self) -> GroundTruth {
        GroundTruth {
            fields: self.fields.clone(),
            reference_text: self.reference_text.clone(),
            tables: self.tables.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DocManifest {
    documents: Vec<DocCase>,
}

/// Load the corpus from `INTAKE_CORPUS_DIR/document_parsing/`. Returns the
/// resolved corpus dir (so the runner can read each case's `file`) and the
/// cases. `INTAKE_CORPUS_DIR` unset ⇒ `ToolError::NotConfigured` (no compiled-in
/// default, PII remediation) — the single unified corpus var (DR-02).
pub fn load_corpus() -> Result<(PathBuf, Vec<DocCase>), ToolError> {
    let base = crate::intake::code::corpus_dir()?.join(CORPUS_SUBDIR);
    load_corpus_from(&base)
}

/// [`load_corpus`] against an explicit base dir (env-free, so unit-testable
/// without mutating the process-global `INTAKE_CORPUS_DIR`).
pub fn load_corpus_from(base: &Path) -> Result<(PathBuf, Vec<DocCase>), ToolError> {
    let manifest = base.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest).map_err(|e| {
        ToolError::NotConfigured(format!(
            "document_parsing corpus manifest not found at {}: {e}",
            manifest.display()
        ))
    })?;
    let m: DocManifest = serde_json::from_str(&raw).map_err(|e| {
        ToolError::NotConfigured(format!("document_parsing corpus manifest parse error: {e}"))
    })?;
    Ok((base.to_path_buf(), m.documents))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_answer_key() -> GroundTruth {
        let mut fields = BTreeMap::new();
        fields.insert("invoice_number".to_string(), "INV-4471".to_string());
        fields.insert("total_due".to_string(), "512.00".to_string());
        fields.insert("vendor_name".to_string(), "Acme Supply Co".to_string());
        GroundTruth {
            fields,
            reference_text: "Invoice INV-4471 Total Due 512.00 Vendor Acme Supply Co".to_string(),
            tables: vec![vec![
                vec!["Item".to_string(), "Qty".to_string(), "Price".to_string()],
                vec!["Widget".to_string(), "2".to_string(), "256.00".to_string()],
            ]],
        }
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

    #[test]
    fn extracted_fields_prefers_direct_over_raw() {
        let mut fields = BTreeMap::new();
        fields.insert("a".to_string(), "1".to_string());
        let outcome = ExtractionOutcome {
            raw_output: r#"{"b": "2"}"#.to_string(),
            fields,
            ..Default::default()
        };
        let got = extracted_fields(&outcome);
        assert_eq!(got.get("a").map(String::as_str), Some("1"));
        assert!(got.get("b").is_none(), "direct fields should win over raw_output");
    }

    /// KNOWN-GOOD: exact-match extraction scores field_accuracy == 1.0, CER/WER
    /// == 0.0, table_f1 == 1.0.
    #[test]
    fn known_good_extraction_scores_full_marks() {
        let truth = expected_answer_key();
        let outcome = ExtractionOutcome {
            raw_output: r#"{"invoice_number": "INV-4471", "total_due": "512.00", "vendor_name": "Acme Supply Co"}"#.to_string(),
            text: truth.reference_text.clone(),
            tables: truth.tables.clone(),
            latency_ms: 340,
            response_tokens: Some(42),
            ..Default::default()
        };
        let rows = build_scores(ModelId::from("docling"), BackendTag::Gpu, &truth, &outcome);
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));

        let m = |metric: &str| rows.iter().find(|r| r.metric == metric).map(|r| r.value);
        assert!((m("field_accuracy").unwrap() - 1.0).abs() < 1e-9);
        assert!(m("cer").unwrap().abs() < 1e-9, "expected CER 0.0");
        assert!(m("wer").unwrap().abs() < 1e-9, "expected WER 0.0");
        assert!((m("table_f1").unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(m("latency_ms").unwrap(), 340.0);
        assert_eq!(m("response_tokens").unwrap(), 42.0);
    }

    /// KNOWN-BAD: wrong/garbled values (and a missing field), garbled text, and
    /// a wrong table all score low.
    #[test]
    fn known_bad_extraction_scores_low() {
        let truth = expected_answer_key();
        let outcome = ExtractionOutcome {
            raw_output: r#"{"invoice_number": "XX-0000", "total_due": "not a number"}"#.to_string(),
            text: "completely unrelated garbled nonsense text here".to_string(),
            tables: vec![vec![vec!["zzz".to_string(), "qqq".to_string()]]],
            latency_ms: 200,
            response_tokens: None,
            ..Default::default()
        };
        let rows = build_scores(ModelId::from("bad-model:1b"), BackendTag::Cpu, &truth, &outcome);
        let m = |metric: &str| rows.iter().find(|r| r.metric == metric).map(|r| r.value);
        assert!(m("field_accuracy").unwrap() < 0.2, "expected near-zero field accuracy");
        assert!(m("cer").unwrap() > 0.5, "expected high CER for garbled text");
        assert!(m("wer").unwrap() > 0.5, "expected high WER for garbled text");
        assert!(m("table_f1").unwrap() < 0.2, "expected low table F1 for a wrong table");
        // No response_tokens row when the outcome doesn't report tokens.
        assert!(rows.iter().all(|r| r.metric != "response_tokens"));
    }

    /// A fields-only case (no reference text / no tables) emits neither CER/WER
    /// nor table_f1 — the optional metrics are truly optional.
    #[test]
    fn fields_only_case_omits_text_and_table_metrics() {
        let mut fields = BTreeMap::new();
        fields.insert("invoice_number".to_string(), "INV-4471".to_string());
        let truth = GroundTruth { fields, ..Default::default() };
        let outcome = ExtractionOutcome {
            raw_output: r#"{"invoice_number": "INV-4471"}"#.to_string(),
            latency_ms: 100,
            ..Default::default()
        };
        let rows = build_scores(ModelId::from("m"), BackendTag::Gpu, &truth, &outcome);
        assert!(rows.iter().any(|r| r.metric == "field_accuracy"));
        assert!(rows.iter().all(|r| r.metric != "cer"));
        assert!(rows.iter().all(|r| r.metric != "wer"));
        assert!(rows.iter().all(|r| r.metric != "table_f1"));
    }

    #[test]
    fn empty_output_scores_zero_not_nan() {
        let truth = expected_answer_key();
        let actual = parse_structured_output("not structured output at all, sorry");
        let score = score_field_accuracy(&truth.fields, &actual);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn char_error_rate_perfect_and_garbled() {
        assert!(char_error_rate("hello world", "hello world").abs() < 1e-9);
        assert!(char_error_rate("", "").abs() < 1e-9);
        assert_eq!(char_error_rate("abc", ""), 1.0);
        assert!(char_error_rate("xyz qrs", "hello world") > 0.5);
    }

    #[test]
    fn table_f1_identical_content_is_one_order_insensitive() {
        let a = vec![vec![
            vec!["Item".to_string(), "Qty".to_string()],
            vec!["Widget".to_string(), "2".to_string()],
        ]];
        // Same cells, reshaped grid — content F1 still 1.0.
        let b = vec![vec![
            vec!["widget".to_string(), "item".to_string()],
            vec!["2".to_string(), "qty".to_string()],
        ]];
        assert!((score_table_f1(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn table_f1_partial_and_empty() {
        let expected = vec![vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string(), "d".to_string()],
        ]];
        // Recover half the cells.
        let actual = vec![vec![vec!["a".to_string(), "b".to_string()]]];
        let f1 = score_table_f1(&expected, &actual);
        assert!(f1 > 0.0 && f1 < 1.0, "expected partial F1, got {f1}");
        // Both empty ⇒ 1.0; one side empty ⇒ 0.0.
        assert_eq!(score_table_f1(&[], &[]), 1.0);
        assert_eq!(score_table_f1(&expected, &[]), 0.0);
    }

    #[test]
    fn load_corpus_from_reads_manifest() {
        let dir = std::env::temp_dir().join(format!("docparse-corpus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = r#"{ "documents": [
            { "id": "inv-001", "file": "inv-001.pdf",
              "fields": { "invoice_number": "INV-4471" },
              "reference_text": "Invoice INV-4471",
              "tables": [ [ ["Item","Qty"], ["Widget","2"] ] ] }
        ] }"#;
        std::fs::write(dir.join("manifest.json"), manifest).unwrap();
        let (base, cases) = load_corpus_from(&dir).unwrap();
        assert_eq!(base, dir);
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "inv-001");
        let gt = cases[0].ground_truth();
        assert_eq!(gt.fields.get("invoice_number").map(String::as_str), Some("INV-4471"));
        assert_eq!(gt.tables.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_corpus_from_missing_manifest_is_not_configured() {
        let dir = std::env::temp_dir().join("docparse-corpus-does-not-exist-xyz");
        match load_corpus_from(&dir) {
            Err(ToolError::NotConfigured(msg)) => assert!(msg.contains("manifest")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }
}
