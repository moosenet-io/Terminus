//! S84 ASMT-07 integration tests — Dimension 6 (embeddings).
//!
//! Black-box tests over the public API of
//! `terminus_rs::intake::assistant::dim6_embeddings`. They exercise:
//!   - the shipped corpora parse AND pass the in-code PII gate,
//!   - corpus loaders reject PII-tainted / malformed input,
//!   - IR-metric math (precision@k / recall@k / MRR / nDCG@k) on a known fixture,
//!   - a full both-corpora run (scripted embedder, no live model) yielding metric
//!     rows + per-query latency + dimensionality + the public-vs-Engram delta,
//!   - a non-embedding candidate skips cleanly (no crash, no rows),
//!   - public-absent ⇒ Engram-only run with no fabricated delta.
//!
//! The scripted embedder stands in for Chord's unified inference path
//! (`infer::embed_with_metrics`); production routes through that path, tests do not
//! need a live Ollama.

use terminus_rs::intake::assistant::dim6_embeddings::{
    compute_delta, cosine, query_metrics, run_corpus, run_dim6, scan_pii, Corpus, Doc, Embedder,
    Embedding, Query, DIMENSION,
};
use terminus_rs::intake::assistant::{BackendTag, ModelId};

// ── scripted embedder (replaces the unified embed path in tests) ───────────────

struct ScriptedEmbedder {
    id: ModelId,
    backend: BackendTag,
    table: std::collections::HashMap<String, Vec<f32>>,
    fail: bool,
}

impl ScriptedEmbedder {
    fn new(id: &str, backend: BackendTag, table: &[(&str, Vec<f32>)]) -> Self {
        ScriptedEmbedder {
            id: ModelId::from(id),
            backend,
            table: table.iter().map(|(t, v)| (t.to_string(), v.clone())).collect(),
            fail: false,
        }
    }
    fn failing(id: &str) -> Self {
        ScriptedEmbedder {
            id: ModelId::from(id),
            backend: BackendTag::Cpu,
            table: Default::default(),
            fail: true,
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
        if self.fail {
            return Err("chat model exposes no embeddings endpoint".into());
        }
        Ok(Embedding {
            vector: self.table.get(text).cloned().unwrap_or_else(|| vec![0.0, 0.0, 0.0]),
            latency_ms: 5,
        })
    }
}

// ── corpus integrity + PII gate ────────────────────────────────────────────────

#[test]
fn shipped_corpora_parse_and_pass_pii_gate() {
    let pub_raw = include_str!("../../src/intake/assistant/corpora/embeddings_public.json");
    let eng_raw = include_str!("../../src/intake/assistant/corpora/embeddings_engram.json");
    let p = Corpus::from_json(pub_raw).expect("public corpus parses + PII-clean");
    let e = Corpus::from_json(eng_raw).expect("engram corpus parses + PII-clean");
    assert_eq!(p.name, "public_ir_subset");
    assert_eq!(e.name, "engram_labeled_subset");
    // Engram is the hand-labeled domain set: a meaningful number of pairs.
    assert!(e.docs.len() >= 20, "engram docs: {}", e.docs.len());
    assert!(e.queries.len() >= 15, "engram queries: {}", e.queries.len());
}

#[test]
fn pii_gate_blocks_infra_names_and_secrets() {
    assert!(scan_pii("the node at <internal-ip> hosts it").is_some());
    assert!(scan_pii("see <host> for the orchestrator").is_some());
    assert!(scan_pii("email <email>").is_some());
    assert!(scan_pii("Authorization: Bearer abc123").is_some());
    assert!(scan_pii("the operator <operator> prefers direct feedback").is_some());
    // abstracted prose passes
    assert!(scan_pii("the orchestrator delegates bulk work to sub-agents").is_none());
}

#[test]
fn corpus_loader_rejects_pii_taint() {
    let tainted = r#"{
        "name":"bad","k":2,
        "docs":[{"id":"d1","text":"reach the box at <internal-ip> now"}],
        "queries":[{"id":"q1","text":"clean query text","relevant":["d1"]}]
    }"#;
    assert!(Corpus::from_json(tainted).is_err(), "PII doc must be rejected");
}

// ── IR-metric math against a known fixture ─────────────────────────────────────

#[test]
fn metric_math_matches_hand_computed_values() {
    // ranked d1(rel) d2 d3(rel) d4 ; relevant={d1,d3}; k=2; 4 docs.
    let ranked: Vec<String> = ["d1", "d2", "d3", "d4"].iter().map(|s| s.to_string()).collect();
    let rel = vec!["d1".to_string(), "d3".to_string()];
    let m = query_metrics("q", &ranked, &rel, 2, 4);
    assert!((m.precision_at_k - 0.5).abs() < 1e-9); // 1 of top-2
    assert!((m.recall_at_k - 0.5).abs() < 1e-9); // 1 of 2 relevant
    assert!((m.mrr - 1.0).abs() < 1e-9); // first relevant at rank 1
    let idcg = 1.0 + 1.0 / 3f64.log2();
    assert!((m.ndcg_at_k - (1.0 / idcg)).abs() < 1e-9);
}

#[test]
fn cosine_orthogonal_and_identical() {
    assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-9);
    assert!(cosine(&[1.0, 0.0], &[0.0, 9.0]).abs() < 1e-9);
}

// ── full both-corpora run with scripted embedder ───────────────────────────────

fn space_corpus(name: &str) -> Corpus {
    Corpus {
        name: name.into(),
        description: String::new(),
        k: 1,
        docs: vec![
            Doc { id: "x".into(), text: format!("{name} doc x") },
            Doc { id: "y".into(), text: format!("{name} doc y") },
            Doc { id: "z".into(), text: format!("{name} doc z") },
        ],
        queries: vec![
            Query { id: "qx".into(), text: format!("{name} query x"), relevant: vec!["x".into()] },
            Query { id: "qy".into(), text: format!("{name} query y"), relevant: vec!["y".into()] },
        ],
    }
}

#[tokio::test]
async fn full_run_yields_metrics_latency_dim_and_delta() {
    let public = space_corpus("public");
    let engram = space_corpus("engram");
    // Public: queries land exactly on their relevant doc (perfect).
    // Engram: queries land on the WRONG doc (domain mismatch) → weak retrieval.
    let emb = ScriptedEmbedder::new(
        "nomic-embed:latest",
        BackendTag::Gpu,
        &[
            ("public doc x", vec![1.0, 0.0, 0.0]),
            ("public doc y", vec![0.0, 1.0, 0.0]),
            ("public doc z", vec![0.0, 0.0, 1.0]),
            ("public query x", vec![1.0, 0.0, 0.0]),
            ("public query y", vec![0.0, 1.0, 0.0]),
            ("engram doc x", vec![1.0, 0.0, 0.0]),
            ("engram doc y", vec![0.0, 1.0, 0.0]),
            ("engram doc z", vec![0.0, 0.0, 1.0]),
            // engram queries point at the wrong axis → miss their relevant doc.
            ("engram query x", vec![0.0, 1.0, 0.0]),
            ("engram query y", vec![1.0, 0.0, 0.0]),
        ],
    );

    let report = run_dim6(&emb, Some(&public), &engram).await;

    // Dimensionality + latency captured on both corpora.
    let pub_r = report.public.as_ref().unwrap();
    assert_eq!(pub_r.dimensionality, 3);
    assert!((pub_r.mean_latency_ms - 5.0).abs() < 1e-9);
    assert_eq!(report.engram.dimensionality, 3);

    // Public is perfect; engram is a total miss.
    assert!((pub_r.mean_ndcg_at_k - 1.0).abs() < 1e-9);
    assert!((report.engram.mean_ndcg_at_k - 0.0).abs() < 1e-9);

    // Public-vs-Engram delta reported, mismatch flagged.
    let delta = report.delta.as_ref().expect("delta when both corpora ran");
    assert!((delta.ndcg_at_k - (-1.0)).abs() < 1e-9);
    assert!(delta.domain_mismatch_flag);

    // Storage rows: keyed on S83 model_id + backend_tag; dimension="embeddings".
    let rows = report.into_dimension_scores();
    assert!(rows.iter().all(|r| r.dimension == DIMENSION));
    assert!(rows.iter().all(|r| r.model_id == ModelId::from("nomic-embed:latest")));
    assert!(rows.iter().all(|r| r.backend_tag == BackendTag::Gpu));
    // 6 public + 6 engram + 4 delta = 16.
    assert_eq!(rows.len(), 16);
    assert!(rows.iter().any(|r| r.metric == "latency_ms"));
    assert!(rows.iter().any(|r| r.metric == "dimensionality"));
    assert!(rows
        .iter()
        .any(|r| r.metric == "ndcg_at_k_delta" && r.judge == "public_vs_engram"));
}

#[tokio::test]
async fn non_embedding_candidate_skipped_cleanly() {
    let public = space_corpus("public");
    let engram = space_corpus("engram");
    let emb = ScriptedEmbedder::failing("qwen3:8b");
    let report = run_dim6(&emb, Some(&public), &engram).await;
    assert!(report.public.as_ref().unwrap().is_skipped());
    assert!(report.engram.is_skipped());
    assert!(report.delta.is_none(), "no delta when corpora skipped");
    // Skipped ⇒ zero storage rows, no crash.
    assert!(report.into_dimension_scores().is_empty());
}

#[tokio::test]
async fn public_absent_runs_engram_only() {
    let engram = space_corpus("engram_labeled_subset");
    let emb = ScriptedEmbedder::new(
        "nomic-embed:latest",
        BackendTag::Cpu,
        &[
            ("engram_labeled_subset doc x", vec![1.0, 0.0, 0.0]),
            ("engram_labeled_subset doc y", vec![0.0, 1.0, 0.0]),
            ("engram_labeled_subset doc z", vec![0.0, 0.0, 1.0]),
            ("engram_labeled_subset query x", vec![1.0, 0.0, 0.0]),
            ("engram_labeled_subset query y", vec![0.0, 1.0, 0.0]),
        ],
    );
    let report = run_dim6(&emb, None, &engram).await;
    assert!(report.public.is_none());
    assert!(report.delta.is_none());
    assert!(compute_delta(None, &report.engram).is_none());
    let rows = report.into_dimension_scores();
    assert!(!rows.is_empty());
    // The judge label is the corpus name, so every row attributes to the Engram set.
    assert!(rows.iter().all(|r| r.judge == "engram_labeled_subset"));
}
