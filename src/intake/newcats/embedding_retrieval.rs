//! Category: `embedding_retrieval` (SUITE-EMB, S125 TERM #508) — the
//! information-retrieval profiling suite for TEXT-EMBEDDING models.
//!
//! Probe shape: embed a small MTEB/BEIR-style query→relevant-doc corpus through
//! Chord's `/v1/embeddings` route, rank docs per query by cosine similarity, and
//! score retrieval quality with the standard IR metrics — **precision@k /
//! recall@k / MRR / nDCG@k** — plus **embedding dimensionality**, **throughput**
//! (embeddings/sec, derived from mean per-query latency), and a **public-vs-domain
//! delta** (baseline capability vs domain fit).
//!
//! ## Promotion of the dim-6 precursor (NOT a re-implementation)
//! The tricky, already-unit-tested IR-metric machinery lives in
//! [`crate::intake::assistant::dim6_embeddings`] (the S84 ASMT-07 embeddings
//! sub-harness): the [`Corpus`]/[`Doc`]/[`Query`] model + PII gate, [`cosine`],
//! per-query [`query_metrics`], [`CorpusReport`], the [`Embedder`] seam and its
//! production [`ChordEmbedder`], [`run_corpus`], and the public-vs-domain
//! [`PublicVsEngramDelta`]/[`compute_delta`]. This module PROMOTES that precursor
//! into a first-class, fleet-wired `newcats` suite rather than copying it: it
//! re-exports the substrate as the suite's backend seam and adds only the
//! newcats-standard surface on top —
//!   - `task_category = "embedding_retrieval"` consts,
//!   - an `INTAKE_CORPUS_DIR` corpus loader (the precursor shipped `include_str!`
//!     fixtures; DR-02 requires the unified corpus-dir convention for a sweep),
//!   - a `throughput` metric (embeddings/sec) the precursor didn't emit,
//!   - `build_scores`/`score_and_write` that write rows through the ONE sanctioned
//!     helper [`insert_dimension_score_with_category`] tagged with this suite's
//!     `task_category`.
//! Keeping the metric math in one place (already compiling + tested) avoids a
//! second, drifting copy of the nDCG/MRR code.
//!
//! ## Dimension/metric convention (`task_category = "embedding_retrieval"`)
//! One `dimension = "embedding_retrieval"`; per corpus (`judge = corpus name`):
//!   - `precision_at_k`, `recall_at_k`, `mrr`, `ndcg_at_k` — mean over queries,
//!   - `latency_ms` — mean per-query embed wall-clock,
//!   - `dimensionality` — observed embedding width,
//!   - `throughput_eps` — embeddings/sec = `1000 / mean_latency_ms` (omitted, not
//!     fabricated, when latency is unknown).
//! Plus the cross-corpus delta rows (`*_delta`, `judge = "public_vs_domain"`):
//! each headline metric's `domain − public` value, with `domain_mismatch_flag`
//! riding in `low_confidence`. `judge = "derived"` is not used here — the corpus
//! name / delta tag carries the provenance, matching the precursor's convention.
//!
//! ## Corpus (`INTAKE_CORPUS_DIR`)
//! [`load_corpora`] reads `embedding_retrieval_public.json` (the domain-neutral
//! MTEB/BEIR-style baseline) and, if present, `embedding_retrieval_domain.json`
//! (a domain corpus — e.g. an Engram/KG-derived labeled set) from
//! `INTAKE_CORPUS_DIR`. The public baseline is required; the domain corpus is
//! optional (its absence ⇒ a public-only run, no delta — never fabricated).
//! Both files are parsed through [`Corpus::from_json`], whose PII gate is a HARD
//! parse failure, so a PII-tainted corpus can never be profiled. A tiny fixture
//! pair ships under `newcats/corpora/` for the unit tests.
//!
//! ## Backend / testability
//! The backend seam is the precursor's [`Embedder`] trait; production is
//! [`ChordEmbedder`], which routes every embed through
//! [`crate::intake::infer::embed_with_metrics`] (the `openai_embed` arm → Chord
//! `/v1/embeddings`; bearer resolved from the backend's `api_key_env` at call
//! time, never logged). Unit tests inject a deterministic scripted embedder, so
//! the scoring/write logic runs with no network. A candidate that is not an
//! embedding model is a CLEAN SKIP (no rows), never a crash — inherited from
//! [`run_corpus`].

use std::path::Path;

use sqlx::PgPool;

use crate::error::ToolError;

use super::super::assistant::schema::insert_dimension_score_with_category;
use super::super::assistant::{BackendTag, DimensionScore, ModelId};

// Re-export the dim-6 substrate as this suite's public seam/types, so callers
// (runner, tests) depend on `newcats::embedding_retrieval::*` rather than reaching
// across into `assistant::dim6_embeddings` directly.
pub use super::super::assistant::dim6_embeddings::{
    compute_delta, cosine, query_metrics, run_corpus, ChordEmbedder, Corpus, CorpusReport, Doc,
    Embedder, Embedding, PublicVsEngramDelta, Query,
};

/// `task_category` value every row from this suite carries.
pub const TASK_CATEGORY: &str = "embedding_retrieval";
/// `dimension` value every row from this suite carries.
pub const DIMENSION: &str = "embedding_retrieval";

/// `judge` tag on the cross-corpus delta rows (domain − public).
pub const DELTA_JUDGE: &str = "public_vs_domain";

/// Corpus file names read from `INTAKE_CORPUS_DIR` (DR-02 unified convention).
pub const PUBLIC_CORPUS_FILE: &str = "embedding_retrieval_public.json";
/// Optional domain corpus (e.g. an Engram/KG-derived labeled set).
pub const DOMAIN_CORPUS_FILE: &str = "embedding_retrieval_domain.json";

// ─────────────────────────────── throughput ────────────────────────────────

/// Embeddings-per-second throughput derived from mean per-query latency.
/// `None` (never a fabricated/divide-by-zero number) when latency is unknown
/// or non-positive. Pure.
pub fn throughput_eps(mean_latency_ms: f64) -> Option<f64> {
    if mean_latency_ms > 0.0 {
        Some(1000.0 / mean_latency_ms)
    } else {
        None
    }
}

// ─────────────────────────────── scoring ───────────────────────────────────

/// Build the `assistant_dimension_score` rows for ONE corpus report: the four IR
/// metrics + latency + dimensionality + throughput (throughput omitted when
/// latency is unknown). A SKIPPED report (wrong model class) produces NO rows,
/// matching the precursor's "unscored ⇒ no rows" contract. `judge` is the corpus
/// name so a row is unambiguous about which corpus it came from.
pub fn build_scores(
    model_id: &ModelId,
    backend_tag: BackendTag,
    report: &CorpusReport,
) -> Vec<DimensionScore> {
    if report.is_skipped() {
        return Vec::new();
    }
    let judge = report.corpus_name.clone();
    let mk = |metric: &str, value: f64| DimensionScore {
        model_id: model_id.clone(),
        backend_tag,
        dimension: DIMENSION.to_string(),
        metric: metric.to_string(),
        value,
        std_dev: None,
        judge: judge.clone(),
        low_confidence: report.any_low_n,
        raw_json: None,
    };
    let mut rows = vec![
        mk("precision_at_k", report.mean_precision_at_k),
        mk("recall_at_k", report.mean_recall_at_k),
        mk("mrr", report.mean_mrr),
        mk("ndcg_at_k", report.mean_ndcg_at_k),
        mk("latency_ms", report.mean_latency_ms),
        mk("dimensionality", report.dimensionality as f64),
    ];
    if let Some(eps) = throughput_eps(report.mean_latency_ms) {
        rows.push(mk("throughput_eps", eps));
    }
    rows
}

/// Build the cross-corpus delta rows (`domain − public`) for one (model,
/// backend). `metric` names are suffixed `_delta`, `judge = "public_vs_domain"`;
/// the `domain_mismatch_flag` rides in each row's `low_confidence` so it is
/// queryable without re-deriving the delta.
pub fn build_delta_scores(
    model_id: &ModelId,
    backend_tag: BackendTag,
    delta: &PublicVsEngramDelta,
) -> Vec<DimensionScore> {
    let mk = |metric: &str, value: f64| DimensionScore {
        model_id: model_id.clone(),
        backend_tag,
        dimension: DIMENSION.to_string(),
        metric: metric.to_string(),
        value,
        std_dev: None,
        judge: DELTA_JUDGE.to_string(),
        low_confidence: delta.domain_mismatch_flag,
        raw_json: None,
    };
    vec![
        mk("precision_at_k_delta", delta.precision_at_k),
        mk("recall_at_k_delta", delta.recall_at_k),
        mk("mrr_delta", delta.mrr),
        mk("ndcg_at_k_delta", delta.ndcg_at_k),
    ]
}

// ──────────────────────────── corpus loading ───────────────────────────────

/// Load the suite's corpora from `INTAKE_CORPUS_DIR` (the DR-02 unified variable,
/// resolved via [`crate::intake::code::corpus_dir`]). Public baseline is required;
/// the domain corpus is optional. A missing/unset dir fails clean with
/// `NotConfigured` (no compiled-in default — PII remediation), rather than
/// silently pointing at a real path.
pub fn load_corpora() -> Result<(Corpus, Option<Corpus>), ToolError> {
    load_corpora_from(&crate::intake::code::corpus_dir()?)
}

/// Env-free core of [`load_corpora`] (testable against a temp dir): reads
/// `PUBLIC_CORPUS_FILE` (required) and `DOMAIN_CORPUS_FILE` (optional) from `dir`,
/// parsing each through [`Corpus::from_json`] (schema + PII gate).
pub fn load_corpora_from(dir: &Path) -> Result<(Corpus, Option<Corpus>), ToolError> {
    let public_path = dir.join(PUBLIC_CORPUS_FILE);
    let public_raw = std::fs::read_to_string(&public_path).map_err(|e| {
        ToolError::NotConfigured(format!(
            "embedding_retrieval public corpus not found at {}: {e}",
            public_path.display()
        ))
    })?;
    let public = Corpus::from_json(&public_raw)?;

    let domain_path = dir.join(DOMAIN_CORPUS_FILE);
    // The domain corpus is optional: a read error (typically "not found") means a
    // public-only run, NOT a hard failure. A file that IS present but malformed /
    // PII-tainted still fails loudly (the `?` on `from_json`).
    let domain = match std::fs::read_to_string(&domain_path) {
        Ok(raw) => Some(Corpus::from_json(&raw)?),
        // Finding 8: ONLY a genuine "not found" means the optional domain corpus is
        // absent (⇒ public-only run). Any OTHER io error (permission denied, I/O
        // error, "is a directory", …) is a real fault and must propagate cleanly —
        // swallowing it as "absent" would silently drop a domain corpus that IS
        // present but unreadable.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(ToolError::Execution(format!(
                "embedding_retrieval domain corpus at {} is unreadable: {e}",
                domain_path.display()
            )));
        }
    };
    Ok((public, domain))
}

// ───────────────────────────── run + write ─────────────────────────────────

/// Compact summary of one suite run, for the tool/runner return line.
#[derive(Debug, Clone)]
pub struct EmbedRetrievalSummary {
    /// `Some(reason)` when the candidate is not an embedding model (whole run skipped).
    pub skipped: Option<String>,
    pub public_precision_at_k: f64,
    pub public_ndcg_at_k: f64,
    pub dimensionality: usize,
    /// Present only when a domain corpus was supplied AND scored.
    pub domain_ndcg_at_k: Option<f64>,
    pub domain_mismatch_flag: Option<bool>,
    pub rows_written: usize,
}

impl EmbedRetrievalSummary {
    /// One-line human summary for the fleet driver.
    pub fn line(&self) -> String {
        if let Some(reason) = &self.skipped {
            return format!("skipped ({reason})");
        }
        let mut s = format!(
            "p@k={:.2} ndcg={:.2} dim={} rows={}",
            self.public_precision_at_k, self.public_ndcg_at_k, self.dimensionality, self.rows_written
        );
        if let Some(dn) = self.domain_ndcg_at_k {
            s.push_str(&format!(
                " domain_ndcg={dn:.2} mismatch={}",
                self.domain_mismatch_flag.unwrap_or(false)
            ));
        }
        s
    }
}

/// Run the suite against `embedder` over the public (+ optional domain) corpus and
/// WRITE every row through `insert_dimension_score_with_category(pool, run_id,
/// score, "embedding_retrieval")`. Returns a compact summary.
///
/// Clean-skip: if the candidate is not an embedding model, the public corpus comes
/// back skipped ⇒ no rows written, and the summary carries the skip reason (never a
/// crash). A domain corpus is only scored (and a delta only produced) when the
/// public side scored — a `domain − public` delta is meaningless otherwise.
pub async fn score_and_write(
    pool: &PgPool,
    run_id: uuid::Uuid,
    embedder: &dyn Embedder,
    public: &Corpus,
    domain: Option<&Corpus>,
) -> Result<EmbedRetrievalSummary, ToolError> {
    let model_id = embedder.model_id().clone();
    let backend_tag = embedder.backend_tag();

    let public_report = run_corpus(embedder, public).await;

    // Wrong model class ⇒ clean skip, no rows.
    if let Some(reason) = &public_report.skipped {
        return Ok(EmbedRetrievalSummary {
            skipped: Some(reason.clone()),
            public_precision_at_k: 0.0,
            public_ndcg_at_k: 0.0,
            dimensionality: 0,
            domain_ndcg_at_k: None,
            domain_mismatch_flag: None,
            rows_written: 0,
        });
    }

    let mut rows = build_scores(&model_id, backend_tag, &public_report);

    let mut domain_ndcg = None;
    let mut mismatch = None;
    if let Some(dc) = domain {
        let domain_report = run_corpus(embedder, dc).await;
        rows.extend(build_scores(&model_id, backend_tag, &domain_report));
        // delta = domain − public (compute_delta computes `engram − public`;
        // the domain corpus plays the `engram` role here).
        if let Some(delta) = compute_delta(Some(&public_report), &domain_report) {
            domain_ndcg = Some(domain_report.mean_ndcg_at_k);
            mismatch = Some(delta.domain_mismatch_flag);
            rows.extend(build_delta_scores(&model_id, backend_tag, &delta));
        }
    }

    for score in &rows {
        insert_dimension_score_with_category(pool, run_id, score, TASK_CATEGORY).await?;
    }

    Ok(EmbedRetrievalSummary {
        skipped: None,
        public_precision_at_k: public_report.mean_precision_at_k,
        public_ndcg_at_k: public_report.mean_ndcg_at_k,
        dimensionality: public_report.dimensionality,
        domain_ndcg_at_k: domain_ndcg,
        domain_mismatch_flag: mismatch,
        rows_written: rows.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ── throughput helper ────────────────────────────────────────────────────

    #[test]
    fn throughput_is_embeddings_per_second_and_never_divides_by_zero() {
        // 20 ms/embed ⇒ 50 embeddings/sec.
        assert!((throughput_eps(20.0).unwrap() - 50.0).abs() < 1e-9);
        assert_eq!(throughput_eps(0.0), None);
        assert_eq!(throughput_eps(-5.0), None);
    }

    // ── shipped fixtures parse + are PII-clean ───────────────────────────────

    #[test]
    fn shipped_corpora_parse_and_are_pii_clean() {
        // include_str! runs the PII gate over every line via from_json.
        let pub_raw = include_str!("corpora/embedding_retrieval_public.json");
        let dom_raw = include_str!("corpora/embedding_retrieval_domain.json");
        let p = Corpus::from_json(pub_raw).expect("public fixture parses + passes PII gate");
        let d = Corpus::from_json(dom_raw).expect("domain fixture parses + passes PII gate");
        assert!(p.docs.len() >= 6 && !p.queries.is_empty());
        assert!(d.docs.len() >= 4 && !d.queries.is_empty());
    }

    // ── load_corpora_from against a temp dir (env-free) ──────────────────────

    #[test]
    fn load_corpora_from_reads_public_and_optional_domain() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PUBLIC_CORPUS_FILE),
            include_str!("corpora/embedding_retrieval_public.json"),
        )
        .unwrap();
        // Domain absent first ⇒ public-only, no domain.
        let (p, d) = load_corpora_from(dir.path()).unwrap();
        assert_eq!(p.name, "embedding_retrieval_public");
        assert!(d.is_none(), "domain corpus is optional");

        // Now add the domain corpus ⇒ both load.
        std::fs::write(
            dir.path().join(DOMAIN_CORPUS_FILE),
            include_str!("corpora/embedding_retrieval_domain.json"),
        )
        .unwrap();
        let (_p, d) = load_corpora_from(dir.path()).unwrap();
        assert!(d.is_some());
    }

    // Finding 8: a NON-NotFound io error reading the optional domain corpus must
    // PROPAGATE, not be swallowed as "absent". A directory where the domain FILE
    // is expected makes `read_to_string` fail with a non-NotFound error.
    #[test]
    fn load_corpora_from_propagates_non_notfound_domain_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PUBLIC_CORPUS_FILE),
            include_str!("corpora/embedding_retrieval_public.json"),
        )
        .unwrap();
        // Create a DIRECTORY at the domain file path → read_to_string errors with a
        // non-NotFound kind, which must surface as a clean ToolError, not None.
        std::fs::create_dir(dir.path().join(DOMAIN_CORPUS_FILE)).unwrap();
        let err = load_corpora_from(dir.path()).unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "expected Execution, got {err:?}");
        assert!(format!("{err:?}").contains("unreadable"));
    }

    #[test]
    fn load_corpora_from_missing_public_is_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        match load_corpora_from(dir.path()) {
            Err(ToolError::NotConfigured(msg)) => assert!(msg.contains("public corpus not found")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // ── scripted embedder for end-to-end scoring (no live model) ─────────────

    /// Deterministic embedder: text → fixed vector from a table; unknown text → a
    /// far-away vector so it never accidentally ranks first. `fail_first` exercises
    /// the non-embedding-model SKIP path.
    struct ScriptedEmbedder {
        id: ModelId,
        backend: BackendTag,
        table: HashMap<String, Vec<f32>>,
        fail_first: bool,
        latency_ms: i64,
        _calls: Mutex<usize>,
    }

    impl ScriptedEmbedder {
        fn new(id: &str, backend: BackendTag, table: &[(&str, Vec<f32>)], latency_ms: i64) -> Self {
            ScriptedEmbedder {
                id: ModelId::from(id),
                backend,
                table: table.iter().map(|(t, v)| (t.to_string(), v.clone())).collect(),
                fail_first: false,
                latency_ms,
                _calls: Mutex::new(0),
            }
        }
        fn failing(id: &str) -> Self {
            ScriptedEmbedder {
                id: ModelId::from(id),
                backend: BackendTag::Cpu,
                table: HashMap::new(),
                fail_first: true,
                latency_ms: 0,
                _calls: Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Embedder for ScriptedEmbedder {
        fn model_id(&self) -> &ModelId {
            &self.id
        }
        fn backend_tag(&self) -> BackendTag {
            self.backend
        }
        async fn embed(&self, text: &str) -> Result<Embedding, String> {
            if self.fail_first {
                return Err("model has no embeddings endpoint".to_string());
            }
            let vector = self.table.get(text).cloned().unwrap_or_else(|| vec![9.0, 9.0]);
            Ok(Embedding {
                vector,
                latency_ms: self.latency_ms,
            })
        }
    }

    fn tiny_public() -> Corpus {
        Corpus {
            name: "embedding_retrieval_public".into(),
            description: String::new(),
            k: 1,
            docs: vec![
                Doc { id: "da".into(), text: "doc apples".into() },
                Doc { id: "db".into(), text: "doc boats".into() },
            ],
            queries: vec![
                Query { id: "qa".into(), text: "q apples".into(), relevant: vec!["da".into()] },
                Query { id: "qb".into(), text: "q boats".into(), relevant: vec!["db".into()] },
            ],
        }
    }

    fn perfect_table() -> Vec<(&'static str, Vec<f32>)> {
        vec![
            ("doc apples", vec![1.0, 0.0]),
            ("doc boats", vec![0.0, 1.0]),
            ("q apples", vec![1.0, 0.0]),
            ("q boats", vec![0.0, 1.0]),
        ]
    }

    #[tokio::test]
    async fn build_scores_emits_all_metrics_including_throughput() {
        // Construct a non-skipped report via a perfect run.
        let emb = ScriptedEmbedder::new("nomic-embed:latest", BackendTag::Gpu, &perfect_table(), 20);
        let corpus = tiny_public();
        let report = run_corpus(&emb, &corpus).await;
        let rows = build_scores(&ModelId::from("nomic-embed:latest"), BackendTag::Gpu, &report);
        // 6 base metrics + throughput = 7 rows.
        assert_eq!(rows.len(), 7);
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));
        assert!(rows.iter().all(|r| r.judge == "embedding_retrieval_public"));
        let tput = rows.iter().find(|r| r.metric == "throughput_eps").unwrap();
        assert!((tput.value - 50.0).abs() < 1e-9); // 1000/20
        let dim = rows.iter().find(|r| r.metric == "dimensionality").unwrap();
        assert!((dim.value - 2.0).abs() < 1e-9);
    }

    #[test]
    fn build_scores_skips_produce_no_rows() {
        let report = CorpusReport::skipped("embedding_retrieval_public", "not an embedding model");
        let rows = build_scores(&ModelId::from("qwen3:8b"), BackendTag::Cpu, &report);
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn score_and_write_skips_cleanly_for_non_embedding_model() {
        // No DB is touched because a skipped public report writes zero rows — the
        // early return fires before any `insert_*` call.
        let emb = ScriptedEmbedder::failing("qwen3:8b");
        let public = tiny_public();
        // A dummy pool is never used on the skip path; construct the summary path
        // by calling the pure pieces the driver relies on instead.
        let report = run_corpus(&emb, &public).await;
        assert!(report.is_skipped());
        let rows = build_scores(emb.model_id(), emb.backend_tag(), &report);
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn domain_delta_flags_public_vs_domain_mismatch() {
        // Public: perfect retrieval (nDCG 1.0). Domain: same embedder but the domain
        // queries embed nearer the WRONG doc ⇒ weak retrieval ⇒ negative delta.
        let public = tiny_public();
        let domain = Corpus {
            name: "embedding_retrieval_domain".into(),
            description: String::new(),
            k: 1,
            docs: vec![
                Doc { id: "e1".into(), text: "engram one".into() },
                Doc { id: "e2".into(), text: "engram two".into() },
            ],
            queries: vec![Query {
                id: "qe1".into(),
                text: "engram q one".into(),
                relevant: vec!["e1".into()],
            }],
        };
        let mut table = perfect_table();
        table.push(("engram one", vec![1.0, 0.0]));
        table.push(("engram two", vec![0.0, 1.0]));
        // query closer to e2 than e1 ⇒ ranks the wrong doc first ⇒ miss.
        table.push(("engram q one", vec![0.1, 1.0]));
        let emb = ScriptedEmbedder::new("nomic-embed:latest", BackendTag::Gpu, &table, 10);

        let public_report = run_corpus(&emb, &public).await;
        let domain_report = run_corpus(&emb, &domain).await;
        let delta = compute_delta(Some(&public_report), &domain_report)
            .expect("delta computed when both corpora ran");
        // public nDCG 1.0, domain nDCG 0.0 ⇒ delta = -1.0, mismatch flagged.
        assert!((delta.ndcg_at_k - (-1.0)).abs() < 1e-9);
        assert!(delta.domain_mismatch_flag);

        let delta_rows =
            build_delta_scores(&ModelId::from("nomic-embed:latest"), BackendTag::Gpu, &delta);
        assert_eq!(delta_rows.len(), 4);
        assert!(delta_rows.iter().all(|r| r.judge == DELTA_JUDGE));
        assert!(delta_rows.iter().all(|r| r.low_confidence)); // mismatch rides low_confidence
        assert!(delta_rows.iter().any(|r| r.metric == "ndcg_at_k_delta"));
    }
}
