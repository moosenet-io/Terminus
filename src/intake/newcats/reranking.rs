//! Category: `reranking` (SUITE-RRK) — cross-encoder reranking quality + latency.
//!
//! Probe shape: for each corpus query, hand a candidate passage set to a
//! cross-encoder reranker (Chord's `/v1/rerank`, backed by bge-reranker-v2-m3),
//! take the reranked order, and score it two ways against graded relevance
//! labels:
//!   1. **quality** — nDCG of the reranked order, and its UPLIFT over a
//!      bi-encoder (dense-embedding) baseline order carried in the corpus. A
//!      reranker earns its cost only if it lifts nDCG above the cheaper
//!      bi-encoder first stage, so uplift (not raw nDCG) is the headline metric.
//!   2. **latency** — wall-clock round-trip of the rerank call, milliseconds.
//!
//! ## Dimension/metric convention (`task_category = "reranking"`)
//! A single dimension `"rerank_relevance"` carries four metrics (mirrors
//! `image_parsing`'s one-dimension/two-metric shape, extended):
//!   - `metric = "ndcg_uplift"`    — `reranked_ndcg - baseline_ndcg` (may be
//!     negative: a reranker that reorders WORSE than the bi-encoder is a real,
//!     recordable finding, not an error).
//!   - `metric = "reranked_ndcg"`  — nDCG@k of the reranker's order in `[0,1]`.
//!   - `metric = "baseline_ndcg"`  — nDCG@k of the bi-encoder baseline order.
//!   - `metric = "latency_ms"`     — wall-clock rerank latency.
//!
//! `judge = "derived"` for all four (computed metrics, no LLM-judge panel).
//!
//! ## Scoring is pure + dependency-free
//! [`dcg`] / [`ndcg_at_k`] / [`ndcg_uplift`] are pure functions over the graded
//! relevance vector and an ordering of document indices — no crate beyond std,
//! unit-tested in isolation, so the quality math is verifiable without a live
//! reranker (same testability contract as the other `newcats` modules).
//!
//! ## Backend call
//! [`RerankModel`] is the seam a runner implements against the live reranker via
//! [`crate::intake::infer::rerank_with_metrics`] (the `kind == "openai"` arm that
//! POSTs `/v1/rerank`); unit tests inject a mock ordering, so the suite is fully
//! exercisable with synthetic data independent of whether a reranker is reachable.
//!
//! ## Corpus
//! Loaded via the unified `INTAKE_CORPUS_DIR` (DR-02) — [`load_corpus`] reads
//! `{INTAKE_CORPUS_DIR}/reranking.json`. No compiled-in default path (PII
//! remediation): a missing var fails clean with `ToolError::NotConfigured`. A
//! compact tracked fixture ships at `data/intake-corpus/reranking.json`.

use std::path::Path;

use serde::Deserialize;
use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};

/// `task_category` value this module writes.
pub const TASK_CATEGORY: &str = "reranking";
/// `dimension` value this module writes (all four metrics share it).
pub const DIMENSION: &str = "rerank_relevance";
/// Rank cutoff for nDCG@k. 10 is the conventional IR default and comfortably
/// exceeds the passage-set size in the shipped fixture (so it degrades to full
/// nDCG there), while staying meaningful for larger operator corpora.
pub const DEFAULT_K: usize = 10;

/// One reranking corpus query: a query, its candidate passages, a graded
/// relevance label per passage (higher = more relevant; 0 = irrelevant), and a
/// bi-encoder BASELINE ordering (passage indices, best-first) the reranker's
/// uplift is measured against. The baseline is carried IN the corpus rather than
/// recomputed here so the suite stays dependency-free (no embedding model needed
/// to score a reranker) and the comparison is stable across runs.
#[derive(Debug, Clone, Deserialize)]
pub struct RerankQuery {
    pub query_id: String,
    pub query: String,
    pub passages: Vec<String>,
    /// Graded relevance aligned to `passages` (index i ↔ passages[i]).
    pub relevance: Vec<f64>,
    /// Bi-encoder baseline order: passage indices, best-first.
    pub baseline_order: Vec<usize>,
}

/// Outcome of one (real or mock) rerank attempt for a query.
#[derive(Debug, Clone)]
pub struct RerankOutcome {
    /// Reranked passage indices, best-first (as returned by the backend).
    pub reranked_order: Vec<usize>,
    /// Wall-clock rerank latency, ms.
    pub latency_ms: i64,
}

/// Seam for calling a reranker; a runner implements this against
/// [`crate::intake::infer::rerank_with_metrics`], tests inject a mock ordering.
pub trait RerankModel {
    fn rerank(&self, query: &str, passages: &[String]) -> Result<RerankOutcome, String>;
}

/// Discounted Cumulative Gain of a gain sequence: `sum_i gain_i / log2(i + 2)`.
/// Pure. Position 0 has discount `log2(2) = 1` (undiscounted), matching the
/// standard IR definition.
pub fn dcg(gains: &[f64]) -> f64 {
    gains
        .iter()
        .enumerate()
        .map(|(i, g)| g / ((i + 2) as f64).log2())
        .sum()
}

/// nDCG@k of `order` (document indices, best-first) against the graded
/// `relevance` vector: `DCG@k(order) / IDCG@k`, in `[0.0, 1.0]`. Returns `0.0`
/// when the ideal DCG is `0` (no relevant document in reach — never a
/// divide-by-zero). An out-of-range index in `order` contributes `0` gain
/// (tolerant of a backend that echoes a stray index) rather than panicking.
/// Pure.
pub fn ndcg_at_k(order: &[usize], relevance: &[f64], k: usize) -> f64 {
    // A duplicated index (e.g. `[0, 0]`) would count the same relevant document
    // twice and let DCG exceed IDCG → nDCG > 1.0. De-duplicate (keep first
    // occurrence) BEFORE taking the top-k so the metric stays in `[0, 1]`.
    let mut seen = std::collections::HashSet::new();
    let gains: Vec<f64> = order
        .iter()
        .filter(|&&i| seen.insert(i))
        .take(k)
        .map(|&i| relevance.get(i).copied().unwrap_or(0.0))
        .collect();
    let actual = dcg(&gains);

    // Ideal ordering: relevance sorted descending, top-k.
    let mut ideal_rels = relevance.to_vec();
    ideal_rels.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    ideal_rels.truncate(k);
    let ideal = dcg(&ideal_rels);

    if ideal <= 0.0 {
        0.0
    } else {
        actual / ideal
    }
}

/// nDCG uplift of a reranked order over the bi-encoder baseline order:
/// `ndcg_at_k(reranked) - ndcg_at_k(baseline)`. Positive ⇒ the reranker
/// improved ordering; negative ⇒ it hurt (a real, recordable finding). Pure.
pub fn ndcg_uplift(reranked: &[usize], baseline: &[usize], relevance: &[f64], k: usize) -> f64 {
    ndcg_at_k(reranked, relevance, k) - ndcg_at_k(baseline, relevance, k)
}

/// Build the `assistant_dimension_score` rows for one reranking probe: one
/// uplift row + reranked/baseline nDCG rows + a latency row.
pub fn build_scores(
    model_id: ModelId,
    backend_tag: BackendTag,
    query: &RerankQuery,
    outcome: &RerankOutcome,
) -> Vec<DimensionScore> {
    let reranked_ndcg = ndcg_at_k(&outcome.reranked_order, &query.relevance, DEFAULT_K);
    let baseline_ndcg = ndcg_at_k(&query.baseline_order, &query.relevance, DEFAULT_K);
    let uplift = reranked_ndcg - baseline_ndcg;

    let row = |metric: &str, value: f64, raw: Option<String>| DimensionScore {
        model_id: model_id.clone(),
        backend_tag,
        dimension: DIMENSION.to_string(),
        metric: metric.to_string(),
        value,
        std_dev: None,
        judge: "derived".to_string(),
        low_confidence: false,
        raw_json: raw,
    };

    vec![
        row(
            "ndcg_uplift",
            uplift,
            Some(
                serde_json::json!({
                    "query_id": query.query_id,
                    "reranked_order": outcome.reranked_order,
                    "baseline_order": query.baseline_order,
                })
                .to_string(),
            ),
        ),
        row("reranked_ndcg", reranked_ndcg, None),
        row("baseline_ndcg", baseline_ndcg, None),
        row("latency_ms", outcome.latency_ms as f64, None),
    ]
}

/// Score one (mock or live) rerank attempt and write its rows through
/// `insert_dimension_score_with_category(pool, run_id, score, "reranking")`.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    model_id: ModelId,
    backend_tag: BackendTag,
    query: &RerankQuery,
    outcome: &RerankOutcome,
) -> Result<(), ToolError> {
    for score in build_scores(model_id.clone(), backend_tag, query, outcome) {
        insert_dimension_score_with_category(pool, run_id, &score, TASK_CATEGORY).await?;
    }
    Ok(())
}

/// Load the reranking corpus from the unified `INTAKE_CORPUS_DIR` (DR-02):
/// reads `{INTAKE_CORPUS_DIR}/reranking.json`. Fails clean with
/// `ToolError::NotConfigured` when the var is unset (no compiled-in default —
/// PII remediation) and `ToolError::Execution` on a missing/malformed file.
pub fn load_corpus() -> Result<Vec<RerankQuery>, ToolError> {
    let dir = crate::intake::code::corpus_dir()?;
    load_corpus_from(&dir)
}

/// Testable core of [`load_corpus`]: read + parse `{dir}/reranking.json`. No env
/// reads, so unit tests can point it at a temp dir without env-var races.
pub fn load_corpus_from(dir: &Path) -> Result<Vec<RerankQuery>, ToolError> {
    let path = dir.join("reranking.json");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        ToolError::Execution(format!("reranking corpus unreadable at {}: {e}", path.display()))
    })?;
    let corpus: Vec<RerankQuery> = serde_json::from_str(&text).map_err(|e| {
        ToolError::Execution(format!("reranking corpus parse error at {}: {e}", path.display()))
    })?;
    if corpus.is_empty() {
        return Err(ToolError::Execution(format!(
            "reranking corpus at {} is empty",
            path.display()
        )));
    }
    // Finding 6: validate each query up front so a malformed corpus fails clean
    // (ToolError) rather than silently producing a bad nDCG. `relevance` must be
    // aligned 1:1 with `passages`, and every `baseline_order` index must be
    // in-range and unique (a duplicate/out-of-range baseline would corrupt the
    // uplift comparison).
    for q in &corpus {
        if q.relevance.len() != q.passages.len() {
            return Err(ToolError::Execution(format!(
                "reranking corpus query {} has {} relevance labels for {} passages",
                q.query_id,
                q.relevance.len(),
                q.passages.len()
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for &idx in &q.baseline_order {
            if idx >= q.passages.len() {
                return Err(ToolError::Execution(format!(
                    "reranking corpus query {} baseline_order index {idx} out of range (passages.len() = {})",
                    q.query_id,
                    q.passages.len()
                )));
            }
            if !seen.insert(idx) {
                return Err(ToolError::Execution(format!(
                    "reranking corpus query {} baseline_order has duplicate index {idx}",
                    q.query_id
                )));
            }
        }
    }
    Ok(corpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A perfect ordering scores nDCG 1.0; a reversed ordering scores lower.
    #[test]
    fn ndcg_perfect_vs_reversed() {
        let relevance = vec![3.0, 2.0, 1.0, 0.0];
        let perfect = [0usize, 1, 2, 3];
        let reversed = [3usize, 2, 1, 0];
        let np = ndcg_at_k(&perfect, &relevance, DEFAULT_K);
        let nr = ndcg_at_k(&reversed, &relevance, DEFAULT_K);
        assert!((np - 1.0).abs() < 1e-9, "perfect order must be nDCG 1.0, got {np}");
        assert!(nr < np, "reversed order must score below perfect ({nr} !< {np})");
    }

    #[test]
    fn ndcg_all_zero_relevance_is_zero_not_nan() {
        let relevance = vec![0.0, 0.0, 0.0];
        let n = ndcg_at_k(&[0, 1, 2], &relevance, DEFAULT_K);
        assert_eq!(n, 0.0);
        assert!(!n.is_nan());
    }

    #[test]
    fn ndcg_tolerates_out_of_range_index() {
        // A stray index contributes 0 gain rather than panicking.
        let relevance = vec![3.0, 0.0];
        let n = ndcg_at_k(&[9, 0], &relevance, DEFAULT_K);
        assert!(n >= 0.0 && n <= 1.0);
    }

    /// KNOWN-GOOD: a reranker that moves the most-relevant passage to the top,
    /// above a mediocre bi-encoder baseline, shows POSITIVE uplift + high nDCG.
    #[test]
    fn known_good_reranker_shows_positive_uplift() {
        let query = RerankQuery {
            query_id: "q".into(),
            query: "capital of france".into(),
            passages: vec!["irrelevant".into(), "paris is the capital".into(), "filler".into()],
            relevance: vec![0.0, 3.0, 0.0],
            // Bi-encoder put the relevant passage second.
            baseline_order: vec![0, 1, 2],
        };
        // Reranker floats the relevant passage (index 1) to the top.
        let outcome = RerankOutcome { reranked_order: vec![1, 0, 2], latency_ms: 42 };
        let rows = build_scores(ModelId::from("bge-reranker-v2-m3"), BackendTag::Cpu, &query, &outcome);

        let uplift = rows.iter().find(|r| r.metric == "ndcg_uplift").unwrap();
        let reranked = rows.iter().find(|r| r.metric == "reranked_ndcg").unwrap();
        assert!(uplift.value > 0.0, "expected positive uplift, got {}", uplift.value);
        assert!((reranked.value - 1.0).abs() < 1e-9, "reranked order is ideal here");
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));
        assert!(rows.iter().all(|r| r.judge == "derived"));
        let latency = rows.iter().find(|r| r.metric == "latency_ms").unwrap();
        assert_eq!(latency.value, 42.0);
    }

    /// KNOWN-BAD: a reranker that reorders WORSE than the baseline shows
    /// NEGATIVE uplift — the discriminating case (still recorded, not an error).
    #[test]
    fn known_bad_reranker_shows_negative_uplift() {
        let query = RerankQuery {
            query_id: "q".into(),
            query: "q".into(),
            passages: vec!["relevant".into(), "no".into(), "no".into()],
            relevance: vec![3.0, 0.0, 0.0],
            // Bi-encoder already had it right.
            baseline_order: vec![0, 1, 2],
        };
        // Reranker buries the relevant passage last.
        let outcome = RerankOutcome { reranked_order: vec![1, 2, 0], latency_ms: 10 };
        let rows = build_scores(ModelId::from("m"), BackendTag::Cpu, &query, &outcome);
        let uplift = rows.iter().find(|r| r.metric == "ndcg_uplift").unwrap();
        assert!(uplift.value < 0.0, "expected negative uplift, got {}", uplift.value);
    }

    #[test]
    fn build_scores_emits_all_four_metrics() {
        let query = RerankQuery {
            query_id: "q".into(),
            query: "q".into(),
            passages: vec!["a".into(), "b".into()],
            relevance: vec![1.0, 0.0],
            baseline_order: vec![0, 1],
        };
        let outcome = RerankOutcome { reranked_order: vec![0, 1], latency_ms: 5 };
        let rows = build_scores(ModelId::from("m"), BackendTag::Cpu, &query, &outcome);
        let metrics: Vec<&str> = rows.iter().map(|r| r.metric.as_str()).collect();
        assert!(metrics.contains(&"ndcg_uplift"));
        assert!(metrics.contains(&"reranked_ndcg"));
        assert!(metrics.contains(&"baseline_ndcg"));
        assert!(metrics.contains(&"latency_ms"));
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn load_corpus_from_reads_and_parses_fixture() {
        let dir = std::env::temp_dir().join("reranking-corpus-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reranking.json"),
            serde_json::json!([
                {
                    "query_id": "t1",
                    "query": "what is rust",
                    "passages": ["rust is a systems language", "bananas are yellow"],
                    "relevance": [3.0, 0.0],
                    "baseline_order": [1, 0]
                }
            ])
            .to_string(),
        )
        .unwrap();
        let corpus = load_corpus_from(&dir).unwrap();
        assert_eq!(corpus.len(), 1);
        assert_eq!(corpus[0].query_id, "t1");
        assert_eq!(corpus[0].passages.len(), 2);
        assert_eq!(corpus[0].baseline_order, vec![1, 0]);
    }

    // Finding 6: a duplicated index must not push nDCG above 1.0.
    #[test]
    fn ndcg_dedupes_duplicate_indices_stays_le_one() {
        let relevance = vec![3.0, 0.0];
        // `[0, 0]` would double-count doc 0's gain without de-dup → nDCG > 1.0.
        let n = ndcg_at_k(&[0, 0], &relevance, DEFAULT_K);
        assert!(n <= 1.0 + 1e-9, "nDCG must stay <= 1.0, got {n}");
        // Deduped to just [0], which IS the ideal ordering here → exactly 1.0.
        assert!((n - 1.0).abs() < 1e-9, "deduped [0,0] scores the ideal 1.0, got {n}");
    }

    // Finding 6: relevance/passages length mismatch fails clean at load.
    #[test]
    fn load_corpus_from_rejects_relevance_length_mismatch() {
        let dir = std::env::temp_dir().join("reranking-corpus-relmismatch");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reranking.json"),
            serde_json::json!([
                { "query_id": "q", "query": "x", "passages": ["a", "b"], "relevance": [1.0], "baseline_order": [0, 1] }
            ])
            .to_string(),
        )
        .unwrap();
        let err = load_corpus_from(&dir).unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(format!("{err:?}").contains("relevance labels"));
    }

    // Finding 6: an out-of-range baseline_order index fails clean at load.
    #[test]
    fn load_corpus_from_rejects_out_of_range_baseline() {
        let dir = std::env::temp_dir().join("reranking-corpus-oobbaseline");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reranking.json"),
            serde_json::json!([
                { "query_id": "q", "query": "x", "passages": ["a", "b"], "relevance": [1.0, 0.0], "baseline_order": [0, 9] }
            ])
            .to_string(),
        )
        .unwrap();
        let err = load_corpus_from(&dir).unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(format!("{err:?}").contains("out of range"));
    }

    // Finding 6: a duplicate baseline_order index fails clean at load.
    #[test]
    fn load_corpus_from_rejects_duplicate_baseline() {
        let dir = std::env::temp_dir().join("reranking-corpus-dupbaseline");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reranking.json"),
            serde_json::json!([
                { "query_id": "q", "query": "x", "passages": ["a", "b"], "relevance": [1.0, 0.0], "baseline_order": [0, 0] }
            ])
            .to_string(),
        )
        .unwrap();
        let err = load_corpus_from(&dir).unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(format!("{err:?}").contains("duplicate index"));
    }

    #[test]
    fn load_corpus_from_missing_file_is_clean_error() {
        let dir = std::env::temp_dir().join("reranking-corpus-absent");
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("reranking.json"));
        let err = load_corpus_from(&dir).unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    /// A mock [`RerankModel`] exercises the trait seam end-to-end without a network.
    #[test]
    fn mock_rerank_model_drives_scoring() {
        struct IdealReranker;
        impl RerankModel for IdealReranker {
            fn rerank(&self, _query: &str, passages: &[String]) -> Result<RerankOutcome, String> {
                // Trivial mock: return indices in reverse (a deterministic order).
                let order: Vec<usize> = (0..passages.len()).rev().collect();
                Ok(RerankOutcome { reranked_order: order, latency_ms: 1 })
            }
        }
        let m = IdealReranker;
        let out = m.rerank("q", &["a".into(), "b".into(), "c".into()]).unwrap();
        assert_eq!(out.reranked_order, vec![2, 1, 0]);
    }
}
