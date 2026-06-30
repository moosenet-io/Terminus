//! S84 ASMT-07 — Dimension 6: embeddings sub-harness.
//!
//! A SEPARATE sub-harness for the embedding model class (NOT chat models). It
//! measures information-retrieval quality of a candidate embedding model on TWO
//! corpora and reports the gap between them:
//!   - a public IR benchmark subset (`corpora/embeddings_public.json`,
//!     MTEB/BEIR-style) — the model's BASELINE capability, and
//!   - a small hand-labeled set drawn from real Engram data
//!     (`corpora/embeddings_engram.json`) — the model's DOMAIN FIT.
//!
//! The **public-vs-Engram delta** is the calibration signal: a model strong on
//! public IR but weak on Engram is a *domain-mismatch flag*, not a bad model.
//!
//! ## Metrics (per corpus, per model)
//! For each query we rank every doc by cosine similarity to the query embedding,
//! then compute (at cutoff `k`):
//!   - **precision@k** — fraction of the top-k that are relevant,
//!   - **recall@k** — fraction of the relevant docs that appear in the top-k,
//!   - **MRR** — reciprocal rank of the first relevant doc,
//!   - **nDCG@k** — rank-discounted gain normalized by the ideal ordering.
//! We also record **per-query latency** (ms, from the unified embed path) and the
//! embedding **dimensionality**. Corpus-level metrics are the mean over queries.
//!
//! ## How inference runs (CRITICAL — unified Chord path, NOT direct Ollama)
//! Every embedding is produced through Chord's unified inference path
//! ([`crate::intake::infer::embed_with_metrics`], which performs P5 backend
//! routing via `infer::resolve_backend`). The [`Embedder`] trait is the seam:
//! [`ChordEmbedder`] is the production implementation that calls
//! `embed_with_metrics`; tests substitute a deterministic scripted embedder so the
//! metric math is exercised without a live model. This module is a *client* of the
//! unified path — it NEVER opens an Ollama socket itself.
//!
//! ## Storage
//! Results are flattened into `assistant_dimension_score` rows with
//! `dimension = "embeddings"`, keyed on the S83-identical
//! [`ModelId`](super::ModelId) + [`BackendTag`](super::BackendTag). The `metric`
//! column carries `precision_at_k` / `recall_at_k` / `mrr` / `ndcg_at_k` /
//! `latency_ms` / `dimensionality`, each tagged in `judge` with the corpus name so
//! a row is unambiguous. The public-vs-Engram delta rows use metric names suffixed
//! `_delta` with `judge = "public_vs_engram"`.
//!
//! ## PII (hard block)
//! Both corpora ship ONLY abstracted text. [`Corpus::from_json`] runs every doc
//! and query through [`scan_pii`] and REJECTS the corpus if any line matches a PII
//! pattern (private IPs, container ids like `<host>`, email addresses, names in a
//! small blocklist, or secret-looking tokens). The pii_gate pre-push hook is the
//! outer guard; this loader is the in-code guard so a PII-tainted corpus can never
//! be parsed, let alone profiled.
//!
//! ## Edge cases (per spec)
//!   - A candidate that is not actually an embedding model (wrong class) →
//!     [`run_corpus`] returns [`CorpusReport::skipped`], a clean skip with a logged
//!     note, never a crash.
//!   - Engram corpus smaller than `k` for some query → metrics computed at the
//!     available depth and the query flagged low-n (see [`QueryMetrics::low_n`]).
//!   - Public benchmark absent at setup → run Engram-only; the public side is
//!     marked absent (never fabricated) and no delta is produced.

use std::time::Duration;

use crate::config;
use crate::error::ToolError;
use crate::intake::infer;

use super::{BackendTag, DimensionScore, ModelId};

/// Dimension label written into every row produced by this runner.
pub const DIMENSION: &str = "embeddings";

// ───────────────────────────── corpus model ────────────────────────────────

/// One document in an IR corpus.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Doc {
    pub id: String,
    pub text: String,
}

/// One labeled query: its text plus the set of relevant doc ids.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Query {
    pub id: String,
    pub text: String,
    pub relevant: Vec<String>,
}

/// An IR corpus: a doc set, a labeled query set, and a default cutoff `k`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Corpus {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Default retrieval cutoff for @k metrics.
    pub k: usize,
    pub docs: Vec<Doc>,
    pub queries: Vec<Query>,
}

impl Corpus {
    /// Parse + validate a corpus from JSON text. Validates the schema (non-empty
    /// docs/queries, every `relevant` id resolvable, `k >= 1`) AND runs the PII
    /// gate over every doc and query — a PII match is a HARD parse failure.
    pub fn from_json(s: &str) -> Result<Corpus, ToolError> {
        let corpus: Corpus = serde_json::from_str(s)
            .map_err(|e| ToolError::NotConfigured(format!("invalid embeddings corpus: {e}")))?;
        corpus.validate()?;
        Ok(corpus)
    }

    /// Schema + relevance + PII validation. Separated so tests can build a corpus
    /// in memory and assert the same gate fires.
    pub fn validate(&self) -> Result<(), ToolError> {
        if self.k < 1 {
            return Err(ToolError::NotConfigured(format!(
                "corpus '{}' has k < 1",
                self.name
            )));
        }
        if self.docs.is_empty() || self.queries.is_empty() {
            return Err(ToolError::NotConfigured(format!(
                "corpus '{}' has no docs or no queries",
                self.name
            )));
        }
        // PII gate over every line of text the corpus ships.
        for d in &self.docs {
            if let Some(hit) = scan_pii(&d.text) {
                return Err(ToolError::NotConfigured(format!(
                    "corpus '{}' doc '{}' contains PII ({hit}); corpora must be abstracted",
                    self.name, d.id
                )));
            }
        }
        for q in &self.queries {
            if let Some(hit) = scan_pii(&q.text) {
                return Err(ToolError::NotConfigured(format!(
                    "corpus '{}' query '{}' contains PII ({hit}); corpora must be abstracted",
                    self.name, q.id
                )));
            }
            // Every relevant id must resolve to a doc, and there must be at least one.
            if q.relevant.is_empty() {
                return Err(ToolError::NotConfigured(format!(
                    "corpus '{}' query '{}' has no relevant docs",
                    self.name, q.id
                )));
            }
            for r in &q.relevant {
                if !self.docs.iter().any(|d| &d.id == r) {
                    return Err(ToolError::NotConfigured(format!(
                        "corpus '{}' query '{}' references unknown doc '{}'",
                        self.name, q.id, r
                    )));
                }
            }
        }
        Ok(())
    }
}

// ──────────────────────────────── PII gate ─────────────────────────────────

/// Scan a single text line for PII / infra literals. Returns `Some(reason)` on
/// the FIRST match (so corpus parsing fails loudly), `None` when clean.
///
/// Patterns (the corpus is abstracted prose, so these are deliberately strict):
///   - private IPv4 ranges (`192.168.`, `10.`, `172.16`–`172.31.`),
///   - container ids `CT` followed by a digit (e.g. `<host>`),
///   - email addresses (`<email>`),
///   - secret-looking tokens (long all-caps/underscore keys, hex/base64 blobs,
///     bearer/api-key markers),
///   - a small person-name blocklist drawn from the operator/agents context.
pub fn scan_pii(text: &str) -> Option<String> {
    let lc = text.to_ascii_lowercase();

    // Private IPv4 ranges.
    if lc.contains("192.168.") || lc.contains("10.0.") {
        return Some("private IP".to_string());
    }
    for n in 16..=31 {
        if lc.contains(&format!("172.{n}.")) {
            return Some("private IP".to_string());
        }
    }
    // Generic dotted-quad that looks like an IP (four numeric octets).
    if looks_like_ipv4(text) {
        return Some("IP address".to_string());
    }

    // Container id: 'CT' immediately followed by a digit.
    let bytes = text.as_bytes();
    if bytes
        .windows(3)
        .any(|w| (w[0] == b'C' && w[1] == b'T') && w[2].is_ascii_digit())
    {
        return Some("container id".to_string());
    }

    // Email address: a run with '@' bracketed by word chars and a dotted domain.
    if looks_like_email(text) {
        return Some("email address".to_string());
    }

    // Secret-looking tokens / key markers. These target credential SYNTAX
    // (assignments, headers, key markers), not the English words "secret" /
    // "password" — abstracted prose is allowed to *discuss* secret handling
    // (e.g. "secrets are never pasted into a conversation") without tripping the
    // gate; only a literal credential shape does.
    for marker in [
        "api_key=", "apikey=", "api_key:", "secret=", "secret:", "secret_key",
        "password=", "password:", "passwd=", "bearer ", "token=", "token:",
        "authorization:", "private_key", "access_key", "-----begin",
    ] {
        if lc.contains(marker) {
            return Some("secret marker".to_string());
        }
    }
    // A long contiguous hex/base64-ish blob is treated as a credential.
    if looks_like_secret_blob(text) {
        return Some("secret blob".to_string());
    }

    // Person-name blocklist (operator + named agents from the project context).
    // Lowercased word-boundary match so substrings of ordinary words don't trip.
    const NAME_BLOCKLIST: &[&str] = &[
        "<operator>", "moose", "lumina", "<host>", "axon", "vigil", "sentinel",
    ];
    for name in NAME_BLOCKLIST {
        if word_present(&lc, name) {
            return Some(format!("name '{name}'"));
        }
    }

    None
}

/// True when `text` contains a token that parses as four 0–255 octets joined by
/// dots (a literal IPv4 address).
fn looks_like_ipv4(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .any(|tok| {
            let parts: Vec<&str> = tok.split('.').collect();
            parts.len() == 4
                && parts.iter().all(|p| {
                    !p.is_empty()
                        && p.len() <= 3
                        && p.bytes().all(|b| b.is_ascii_digit())
                        && p.parse::<u16>().map(|n| n <= 255).unwrap_or(false)
                })
        })
}

/// True when `text` contains an `@`-joined token with a dotted domain (an email).
fn looks_like_email(text: &str) -> bool {
    text.split_whitespace().any(|tok| {
        let tok = tok.trim_matches(|c: char| !c.is_alphanumeric());
        if let Some((local, domain)) = tok.split_once('@') {
            !local.is_empty()
                && domain.contains('.')
                && domain.split('.').all(|p| !p.is_empty())
                && domain
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '.' || c == '-')
        } else {
            false
        }
    })
}

/// True when `text` contains a long contiguous run (>= 20) of hex/base64-ish
/// characters with no whitespace — a credential/blob heuristic.
fn looks_like_secret_blob(text: &str) -> bool {
    text.split_whitespace().any(|tok| {
        tok.len() >= 20
            && tok
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' || c == '_')
            && tok.chars().any(|c| c.is_ascii_digit())
            && tok.chars().any(|c| c.is_ascii_alphabetic())
    })
}

/// Word-boundary presence of `needle` (already-lowercased) in `hay`.
fn word_present(hay: &str, needle: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric();
    let mut start = 0;
    while let Some(pos) = hay[start..].find(needle) {
        let i = start + pos;
        let before_ok = i == 0 || !is_word(hay[..i].chars().next_back().unwrap());
        let after = i + needle.len();
        let after_ok = after >= hay.len() || !is_word(hay[after..].chars().next().unwrap());
        if before_ok && after_ok {
            return true;
        }
        start = i + needle.len();
    }
    false
}

// ───────────────────────────── embedder seam ───────────────────────────────

/// One embedding plus the latency it took, returned by an [`Embedder`].
#[derive(Debug, Clone, Default)]
pub struct Embedding {
    pub vector: Vec<f32>,
    pub latency_ms: i64,
}

/// A candidate embedding model. The production impl routes through Chord's
/// unified inference path; tests substitute a deterministic scripted embedder.
///
/// `embed` returns the dense vector + latency, or an error string on transport
/// failure / wrong model class (a non-embedding model). The runner turns a
/// consistent error into a clean SKIP, never a crash.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// S83-byte-identical model id of the candidate.
    fn model_id(&self) -> &ModelId;

    /// Hardware the candidate is being profiled on.
    fn backend_tag(&self) -> BackendTag;

    /// Embed a single text. `Err` on transport failure or a non-embedding model.
    async fn embed(&self, text: &str) -> Result<Embedding, String>;
}

/// Production embedder: routes every call through Chord's unified inference path
/// ([`infer::embed_with_metrics`], P5 backend routing). This module is a *client*
/// of the unified path — it never talks to Ollama directly.
pub struct ChordEmbedder {
    client: reqwest::Client,
    model_id: ModelId,
    backend_tag: BackendTag,
    timeout: Duration,
}

impl ChordEmbedder {
    /// `model_id` must be the S83 registry key (byte-identical). The backend tag
    /// keys the stored rows; backend resolution itself happens in the unified path.
    pub fn new(model_id: ModelId, backend_tag: BackendTag) -> Self {
        ChordEmbedder {
            client: reqwest::Client::new(),
            model_id,
            backend_tag,
            timeout: Duration::from_secs(config::judge_timeout_secs()),
        }
    }
}

#[async_trait::async_trait]
impl Embedder for ChordEmbedder {
    fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    fn backend_tag(&self) -> BackendTag {
        self.backend_tag
    }

    async fn embed(&self, text: &str) -> Result<Embedding, String> {
        // CRITICAL: unified path (P5 backend routing), NOT a direct Ollama call.
        let m = infer::embed_with_metrics(&self.client, self.model_id.as_str(), text, self.timeout)
            .await;
        if let Some(err) = m.error {
            return Err(err);
        }
        Ok(Embedding {
            vector: m.embedding,
            latency_ms: m.latency_ms,
        })
    }
}

// ────────────────────────────── IR metrics ─────────────────────────────────

/// Cosine similarity of two equal-length vectors. Returns 0.0 for a zero-norm
/// vector or a length mismatch (defensive: never panics, never NaN).
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..a.len() {
        let (x, y) = (a[i] as f64, b[i] as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Rank a list of `(doc_id, score)` descending by score, breaking ties by doc_id
/// (deterministic ordering — two models with identical scores rank identically).
fn rank(mut scored: Vec<(String, f64)>) -> Vec<String> {
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.into_iter().map(|(id, _)| id).collect()
}

/// IR metrics for a SINGLE query at cutoff `k`.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryMetrics {
    pub query_id: String,
    pub precision_at_k: f64,
    pub recall_at_k: f64,
    /// Reciprocal rank of the first relevant doc (0.0 if none retrieved).
    pub mrr: f64,
    pub ndcg_at_k: f64,
    /// Effective cutoff used (min(k, #docs)).
    pub effective_k: usize,
    /// True when the corpus had fewer docs than `k` (metrics at available depth).
    pub low_n: bool,
}

/// Compute @k metrics for one query given the full ranked doc-id list and the
/// set of relevant ids. `k` is the requested cutoff; `total_docs` is the corpus
/// size (drives the `low_n` flag and the effective cutoff).
pub fn query_metrics(
    query_id: &str,
    ranked: &[String],
    relevant: &[String],
    k: usize,
    total_docs: usize,
) -> QueryMetrics {
    let effective_k = k.min(total_docs).max(1);
    let low_n = total_docs < k;
    let rel: std::collections::HashSet<&str> = relevant.iter().map(|s| s.as_str()).collect();
    let topk = &ranked[..effective_k.min(ranked.len())];

    // precision@k = relevant in top-k / k(effective); recall@k = relevant in top-k / |relevant|.
    let hits = topk.iter().filter(|id| rel.contains(id.as_str())).count();
    let precision_at_k = hits as f64 / effective_k as f64;
    let recall_at_k = if rel.is_empty() {
        0.0
    } else {
        hits as f64 / rel.len() as f64
    };

    // MRR over the FULL ranking (reciprocal rank of the first relevant doc).
    let mrr = ranked
        .iter()
        .position(|id| rel.contains(id.as_str()))
        .map(|p| 1.0 / (p as f64 + 1.0))
        .unwrap_or(0.0);

    // nDCG@k with binary relevance: DCG = sum_{i<k} rel_i / log2(i+2);
    // IDCG = ideal ordering (all relevant first).
    let dcg: f64 = topk
        .iter()
        .enumerate()
        .map(|(i, id)| {
            if rel.contains(id.as_str()) {
                1.0 / ((i as f64 + 2.0).log2())
            } else {
                0.0
            }
        })
        .sum();
    let ideal_hits = rel.len().min(effective_k);
    let idcg: f64 = (0..ideal_hits)
        .map(|i| 1.0 / ((i as f64 + 2.0).log2()))
        .sum();
    let ndcg_at_k = if idcg == 0.0 { 0.0 } else { dcg / idcg };

    QueryMetrics {
        query_id: query_id.to_string(),
        precision_at_k,
        recall_at_k,
        mrr,
        ndcg_at_k,
        effective_k,
        low_n,
    }
}

// ───────────────────────────── corpus report ───────────────────────────────

/// The result of running one corpus against one model: per-query metrics plus
/// the corpus-level means, dimensionality, and mean per-query latency. A
/// `skipped` report carries a reason and no metrics (wrong model class / failure).
#[derive(Debug, Clone)]
pub struct CorpusReport {
    pub corpus_name: String,
    /// `Some(reason)` ⇒ the candidate was skipped (e.g. not an embedding model);
    /// all metric fields are then their defaults and no rows are produced.
    pub skipped: Option<String>,
    pub per_query: Vec<QueryMetrics>,
    pub mean_precision_at_k: f64,
    pub mean_recall_at_k: f64,
    pub mean_mrr: f64,
    pub mean_ndcg_at_k: f64,
    /// Mean per-query embedding latency (ms).
    pub mean_latency_ms: f64,
    /// Embedding dimensionality observed (0 when skipped).
    pub dimensionality: usize,
    pub k: usize,
    /// True if any query ran at a depth below `k` (small-corpus flag).
    pub any_low_n: bool,
}

impl CorpusReport {
    /// Build a clean-skip report (wrong model class / consistent embed failure).
    pub fn skipped(corpus_name: &str, reason: impl Into<String>) -> CorpusReport {
        CorpusReport {
            corpus_name: corpus_name.to_string(),
            skipped: Some(reason.into()),
            per_query: Vec::new(),
            mean_precision_at_k: 0.0,
            mean_recall_at_k: 0.0,
            mean_mrr: 0.0,
            mean_ndcg_at_k: 0.0,
            mean_latency_ms: 0.0,
            dimensionality: 0,
            k: 0,
            any_low_n: false,
        }
    }

    /// True when the candidate was skipped for this corpus.
    pub fn is_skipped(&self) -> bool {
        self.skipped.is_some()
    }

    /// Flatten this corpus's corpus-level metrics into storage rows for one
    /// (model, backend). `judge` is set to the corpus name so a row is
    /// unambiguous about which corpus it came from. Skipped reports produce NO
    /// rows (nothing to store), matching the panel "unscored ⇒ no rows" contract.
    pub fn into_dimension_scores(
        &self,
        model_id: &ModelId,
        backend_tag: BackendTag,
    ) -> Vec<DimensionScore> {
        if self.is_skipped() {
            return Vec::new();
        }
        let audit = self.audit_json();
        let mk = |metric: &str, value: f64| DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: metric.to_string(),
            value,
            std_dev: None,
            judge: self.corpus_name.clone(),
            low_confidence: self.any_low_n,
            raw_json: Some(audit.clone()),
        };
        vec![
            mk("precision_at_k", self.mean_precision_at_k),
            mk("recall_at_k", self.mean_recall_at_k),
            mk("mrr", self.mean_mrr),
            mk("ndcg_at_k", self.mean_ndcg_at_k),
            mk("latency_ms", self.mean_latency_ms),
            mk("dimensionality", self.dimensionality as f64),
        ]
    }

    /// Redacted audit blob: corpus name, k, dimensionality, low-n flag, per-query
    /// metric summary. No corpus text (PII-clean by construction, but we still
    /// never echo it into the DB).
    fn audit_json(&self) -> String {
        let queries: Vec<serde_json::Value> = self
            .per_query
            .iter()
            .map(|q| {
                serde_json::json!({
                    "query_id": q.query_id,
                    "precision_at_k": q.precision_at_k,
                    "recall_at_k": q.recall_at_k,
                    "mrr": q.mrr,
                    "ndcg_at_k": q.ndcg_at_k,
                    "effective_k": q.effective_k,
                    "low_n": q.low_n,
                })
            })
            .collect();
        serde_json::json!({
            "corpus": self.corpus_name,
            "k": self.k,
            "dimensionality": self.dimensionality,
            "any_low_n": self.any_low_n,
            "queries": queries,
        })
        .to_string()
    }
}

/// Run one corpus against one embedder. Embeds every doc and query through the
/// embedder (unified path in production), ranks docs per query by cosine
/// similarity, and computes the IR metrics + latency + dimensionality.
///
/// Clean-skip behavior (per spec EDGE CASES): if the embedder errors on the FIRST
/// embed call (a non-embedding model / wrong class), the whole corpus returns
/// [`CorpusReport::skipped`] with the error as the logged note — no crash. A
/// later transient error on an individual doc/query degrades that item (the doc
/// is dropped from the index / the query scores 0) without failing the run.
pub async fn run_corpus(embedder: &dyn Embedder, corpus: &Corpus) -> CorpusReport {
    // Embed docs first. A failure on the very first doc ⇒ wrong model class ⇒ skip.
    let mut doc_vectors: Vec<(String, Vec<f32>)> = Vec::with_capacity(corpus.docs.len());
    let mut dimensionality = 0usize;
    let mut first = true;
    for d in &corpus.docs {
        match embedder.embed(&d.text).await {
            Ok(e) => {
                if dimensionality == 0 {
                    dimensionality = e.vector.len();
                }
                doc_vectors.push((d.id.clone(), e.vector));
            }
            Err(err) => {
                if first {
                    // Wrong model class (or backend with no embeddings support):
                    // clean skip with the error logged into the report.
                    return CorpusReport::skipped(
                        &corpus.name,
                        format!("candidate is not an embedding model: {err}"),
                    );
                }
                // Otherwise drop this doc and continue (degrade, never panic).
            }
        }
        first = false;
    }

    if doc_vectors.is_empty() {
        return CorpusReport::skipped(&corpus.name, "no docs embedded");
    }

    let total_docs = doc_vectors.len();
    let k = corpus.k;
    let mut per_query: Vec<QueryMetrics> = Vec::with_capacity(corpus.queries.len());
    let mut latencies: Vec<i64> = Vec::new();

    for q in &corpus.queries {
        let qvec = match embedder.embed(&q.text).await {
            Ok(e) => {
                latencies.push(e.latency_ms);
                e.vector
            }
            Err(_) => {
                // Transient query failure: score it as a total miss, keep going.
                per_query.push(QueryMetrics {
                    query_id: q.id.clone(),
                    precision_at_k: 0.0,
                    recall_at_k: 0.0,
                    mrr: 0.0,
                    ndcg_at_k: 0.0,
                    effective_k: k.min(total_docs).max(1),
                    low_n: total_docs < k,
                });
                continue;
            }
        };
        let scored: Vec<(String, f64)> = doc_vectors
            .iter()
            .map(|(id, v)| (id.clone(), cosine(&qvec, v)))
            .collect();
        let ranked = rank(scored);
        per_query.push(query_metrics(&q.id, &ranked, &q.relevant, k, total_docs));
    }

    let n = per_query.len().max(1) as f64;
    let mean = |f: fn(&QueryMetrics) -> f64| per_query.iter().map(f).sum::<f64>() / n;
    let mean_latency_ms = if latencies.is_empty() {
        0.0
    } else {
        latencies.iter().sum::<i64>() as f64 / latencies.len() as f64
    };
    let any_low_n = per_query.iter().any(|q| q.low_n);

    CorpusReport {
        corpus_name: corpus.name.clone(),
        skipped: None,
        mean_precision_at_k: mean(|q| q.precision_at_k),
        mean_recall_at_k: mean(|q| q.recall_at_k),
        mean_mrr: mean(|q| q.mrr),
        mean_ndcg_at_k: mean(|q| q.ndcg_at_k),
        mean_latency_ms,
        dimensionality,
        k,
        any_low_n,
        per_query,
    }
}

// ──────────────────────── public-vs-Engram delta ───────────────────────────

/// The cross-corpus calibration signal: each metric's value on Engram MINUS its
/// value on the public baseline. A large NEGATIVE delta (strong public, weak
/// Engram) is the domain-mismatch flag the spec asks for.
///
/// `public` is `None` when the public benchmark was absent at setup (Engram-only
/// run) — then no delta is produced (never fabricated).
#[derive(Debug, Clone, PartialEq)]
pub struct PublicVsEngramDelta {
    pub precision_at_k: f64,
    pub recall_at_k: f64,
    pub mrr: f64,
    pub ndcg_at_k: f64,
    /// True when Engram trails the public baseline enough to flag domain mismatch.
    pub domain_mismatch_flag: bool,
}

/// Threshold below which a negative nDCG delta is treated as a domain-mismatch
/// flag (Engram nDCG is this much or more below the public baseline).
pub const DOMAIN_MISMATCH_NDCG_DROP: f64 = 0.15;

/// Compute the public-vs-Engram delta (Engram − public) for the headline metrics.
/// Returns `None` when either side was skipped or the public side is absent — a
/// delta is only meaningful when BOTH corpora produced metrics.
pub fn compute_delta(
    public: Option<&CorpusReport>,
    engram: &CorpusReport,
) -> Option<PublicVsEngramDelta> {
    let public = public?;
    if public.is_skipped() || engram.is_skipped() {
        return None;
    }
    let d_ndcg = engram.mean_ndcg_at_k - public.mean_ndcg_at_k;
    Some(PublicVsEngramDelta {
        precision_at_k: engram.mean_precision_at_k - public.mean_precision_at_k,
        recall_at_k: engram.mean_recall_at_k - public.mean_recall_at_k,
        mrr: engram.mean_mrr - public.mean_mrr,
        ndcg_at_k: d_ndcg,
        domain_mismatch_flag: d_ndcg <= -DOMAIN_MISMATCH_NDCG_DROP,
    })
}

impl PublicVsEngramDelta {
    /// Flatten the delta into storage rows (one per metric) for one (model,
    /// backend). `metric` names are suffixed `_delta`, `judge = "public_vs_engram"`.
    /// The `domain_mismatch_flag` rides in each row's `low_confidence` field so it
    /// is queryable without re-deriving the delta.
    pub fn into_dimension_scores(
        &self,
        model_id: &ModelId,
        backend_tag: BackendTag,
    ) -> Vec<DimensionScore> {
        let audit = serde_json::json!({
            "kind": "public_vs_engram_delta",
            "precision_at_k_delta": self.precision_at_k,
            "recall_at_k_delta": self.recall_at_k,
            "mrr_delta": self.mrr,
            "ndcg_at_k_delta": self.ndcg_at_k,
            "domain_mismatch_flag": self.domain_mismatch_flag,
        })
        .to_string();
        let mk = |metric: &str, value: f64| DimensionScore {
            model_id: model_id.clone(),
            backend_tag,
            dimension: DIMENSION.to_string(),
            metric: metric.to_string(),
            value,
            std_dev: None,
            judge: "public_vs_engram".to_string(),
            low_confidence: self.domain_mismatch_flag,
            raw_json: Some(audit.clone()),
        };
        vec![
            mk("precision_at_k_delta", self.precision_at_k),
            mk("recall_at_k_delta", self.recall_at_k),
            mk("mrr_delta", self.mrr),
            mk("ndcg_at_k_delta", self.ndcg_at_k),
        ]
    }
}

// ───────────────────────────── full dim-6 run ──────────────────────────────

/// The complete dim-6 result for one model: both corpus reports + the delta.
#[derive(Debug, Clone)]
pub struct EmbeddingsReport {
    pub model_id: ModelId,
    pub backend_tag: BackendTag,
    /// `None` when the public benchmark was absent at setup (Engram-only run).
    pub public: Option<CorpusReport>,
    pub engram: CorpusReport,
    /// `None` when no delta could be computed (public absent or a corpus skipped).
    pub delta: Option<PublicVsEngramDelta>,
}

impl EmbeddingsReport {
    /// All storage rows for this model: public corpus rows (if present) + Engram
    /// corpus rows + delta rows. Skipped corpora contribute no rows.
    pub fn into_dimension_scores(&self) -> Vec<DimensionScore> {
        let mut rows = Vec::new();
        if let Some(p) = &self.public {
            rows.extend(p.into_dimension_scores(&self.model_id, self.backend_tag));
        }
        rows.extend(self.engram.into_dimension_scores(&self.model_id, self.backend_tag));
        if let Some(d) = &self.delta {
            rows.extend(d.into_dimension_scores(&self.model_id, self.backend_tag));
        }
        rows
    }
}

/// Run the full dim-6 sub-harness for one embedder over both corpora and compute
/// the public-vs-Engram delta.
///
/// `public` is `None` when the public benchmark could not be loaded at setup
/// (per spec: run Engram-only, mark public absent, do NOT fabricate). When the
/// candidate is not an embedding model, BOTH corpus reports come back skipped and
/// no delta is produced — a clean, non-crashing outcome.
pub async fn run_dim6(
    embedder: &dyn Embedder,
    public: Option<&Corpus>,
    engram: &Corpus,
) -> EmbeddingsReport {
    let public_report = match public {
        Some(c) => Some(run_corpus(embedder, c).await),
        None => None,
    };
    let engram_report = run_corpus(embedder, engram).await;
    let delta = compute_delta(public_report.as_ref(), &engram_report);

    EmbeddingsReport {
        model_id: embedder.model_id().clone(),
        backend_tag: embedder.backend_tag(),
        public: public_report,
        engram: engram_report,
        delta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── PII gate ───────────────────────────────────────────────────────────

    #[test]
    fn pii_gate_rejects_infra_and_pii() {
        assert!(scan_pii("the host at <internal-ip> runs the service").is_some());
        assert!(scan_pii("see container <host> for details").is_some());
        assert!(scan_pii("mail me at <email>").is_some());
        assert!(scan_pii("api_key=abc").is_some());
        assert!(scan_pii("token=<REDACTED-SECRET>1234").is_some());
        // The bare English word "secret" must NOT trip the gate (abstracted prose
        // is allowed to discuss secret handling).
        assert!(scan_pii("secrets are never pasted into a conversation").is_none());
        assert!(scan_pii("ask <operator> about it").is_some());
        assert!(scan_pii("the orchestrator Lumina delegates work").is_some());
        // A plausible IP outside the private ranges still trips the dotted-quad rule.
        assert!(scan_pii("connect to 8.8.8.8 first").is_some());
    }

    #[test]
    fn pii_gate_passes_abstracted_text() {
        assert!(scan_pii("the orchestrator never executes bulk work itself").is_none());
        assert!(scan_pii("routine alerts come from templates, not a model").is_none());
        // 'connection' contains no blocklisted name as a whole word.
        assert!(scan_pii("a database connection pool is reused across requests").is_none());
    }

    #[test]
    fn shipped_corpora_parse_and_are_pii_clean() {
        // Both corpora must parse (which runs the PII gate over every line).
        let pub_raw = include_str!("corpora/embeddings_public.json");
        let eng_raw = include_str!("corpora/embeddings_engram.json");
        let p = Corpus::from_json(pub_raw).expect("public corpus parses + passes PII gate");
        let e = Corpus::from_json(eng_raw).expect("engram corpus parses + passes PII gate");
        assert!(p.docs.len() >= 10 && !p.queries.is_empty());
        assert!(e.docs.len() >= 20 && e.queries.len() >= 15, "engram is the labeled set");
    }

    #[test]
    fn corpus_from_json_rejects_pii_doc() {
        let bad = r#"{
            "name":"x","k":2,
            "docs":[{"id":"d1","text":"reach the box at <internal-ip>"}],
            "queries":[{"id":"q1","text":"where is the box","relevant":["d1"]}]
        }"#;
        assert!(Corpus::from_json(bad).is_err());
    }

    #[test]
    fn corpus_from_json_rejects_unknown_relevant() {
        let bad = r#"{
            "name":"x","k":2,
            "docs":[{"id":"d1","text":"a clean abstracted sentence"}],
            "queries":[{"id":"q1","text":"clean query","relevant":["d999"]}]
        }"#;
        assert!(Corpus::from_json(bad).is_err());
    }

    // ── metric math against a known fixture ──────────────────────────────────

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0); // length mismatch
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0); // zero norm
    }

    #[test]
    fn precision_recall_ndcg_mrr_known_values() {
        // ranking: d1(rel) d2 d3(rel) d4 d5 ; relevant = {d1,d3}; k=2; 5 docs.
        let ranked: Vec<String> = ["d1", "d2", "d3", "d4", "d5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rel = vec!["d1".to_string(), "d3".to_string()];
        let m = query_metrics("q", &ranked, &rel, 2, 5);
        // top-2 = [d1,d2]; one relevant → precision = 1/2.
        assert!((m.precision_at_k - 0.5).abs() < 1e-9);
        // recall@2 = 1 of 2 relevant = 0.5.
        assert!((m.recall_at_k - 0.5).abs() < 1e-9);
        // first relevant at rank 1 → MRR = 1.0.
        assert!((m.mrr - 1.0).abs() < 1e-9);
        // DCG@2 = 1/log2(2) + 0 = 1.0 ; IDCG@2 = 1/log2(2)+1/log2(3)=1+0.6309 ; nDCG≈0.6131.
        let idcg = 1.0 + 1.0 / 3f64.log2();
        assert!((m.ndcg_at_k - (1.0 / idcg)).abs() < 1e-9);
        assert!(!m.low_n);
    }

    #[test]
    fn perfect_ranking_is_ndcg_one() {
        let ranked: Vec<String> = ["d1", "d2", "d3"].iter().map(|s| s.to_string()).collect();
        let rel = vec!["d1".to_string(), "d2".to_string()];
        let m = query_metrics("q", &ranked, &rel, 2, 3);
        assert!((m.precision_at_k - 1.0).abs() < 1e-9);
        assert!((m.recall_at_k - 1.0).abs() < 1e-9);
        assert!((m.ndcg_at_k - 1.0).abs() < 1e-9);
        assert!((m.mrr - 1.0).abs() < 1e-9);
    }

    #[test]
    fn low_n_when_corpus_smaller_than_k() {
        // k=5 but only 3 docs ⇒ low_n, effective_k clamped to 3.
        let ranked: Vec<String> = ["d1", "d2", "d3"].iter().map(|s| s.to_string()).collect();
        let rel = vec!["d3".to_string()];
        let m = query_metrics("q", &ranked, &rel, 5, 3);
        assert!(m.low_n);
        assert_eq!(m.effective_k, 3);
        // relevant doc at rank 3 → MRR = 1/3.
        assert!((m.mrr - (1.0 / 3.0)).abs() < 1e-9);
    }

    // ── scripted embedder for end-to-end metric runs (no live model) ─────────

    /// Deterministic embedder: each text maps to a fixed vector from a table; an
    /// unknown text gets the zero vector. A flag makes it error on the first call
    /// (to exercise the non-embedding-model SKIP path).
    struct ScriptedEmbedder {
        id: ModelId,
        backend: BackendTag,
        table: std::collections::HashMap<String, Vec<f32>>,
        fail_first: bool,
        calls: std::sync::Mutex<usize>,
        latency: i64,
    }

    impl ScriptedEmbedder {
        fn new(id: &str, backend: BackendTag, table: &[(&str, Vec<f32>)]) -> Self {
            ScriptedEmbedder {
                id: ModelId::from(id),
                backend,
                table: table.iter().map(|(t, v)| (t.to_string(), v.clone())).collect(),
                fail_first: false,
                calls: std::sync::Mutex::new(0),
                latency: 7,
            }
        }
        fn failing(id: &str) -> Self {
            ScriptedEmbedder {
                id: ModelId::from(id),
                backend: BackendTag::Cpu,
                table: std::collections::HashMap::new(),
                fail_first: true,
                calls: std::sync::Mutex::new(0),
                latency: 0,
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
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            if self.fail_first {
                return Err("model has no embeddings endpoint".to_string());
            }
            let vector = self.table.get(text).cloned().unwrap_or_else(|| vec![0.0, 0.0]);
            Ok(Embedding {
                vector,
                latency_ms: self.latency,
            })
        }
    }

    fn tiny_corpus(name: &str, k: usize) -> Corpus {
        // 2-D space: query "a" near doc da, query "b" near doc db.
        Corpus {
            name: name.to_string(),
            description: String::new(),
            k,
            docs: vec![
                Doc { id: "da".into(), text: "doc about apples".into() },
                Doc { id: "db".into(), text: "doc about boats".into() },
                Doc { id: "dc".into(), text: "doc about clouds".into() },
            ],
            queries: vec![
                Query { id: "qa".into(), text: "query apples".into(), relevant: vec!["da".into()] },
                Query { id: "qb".into(), text: "query boats".into(), relevant: vec!["db".into()] },
            ],
        }
    }

    fn perfect_embedder(name_id: &str, backend: BackendTag) -> ScriptedEmbedder {
        ScriptedEmbedder::new(
            name_id,
            backend,
            &[
                ("doc about apples", vec![1.0, 0.0]),
                ("doc about boats", vec![0.0, 1.0]),
                ("doc about clouds", vec![0.5, 0.5]),
                ("query apples", vec![1.0, 0.0]),
                ("query boats", vec![0.0, 1.0]),
            ],
        )
    }

    #[tokio::test]
    async fn run_corpus_perfect_retrieval() {
        let emb = perfect_embedder("nomic-embed:latest", BackendTag::Gpu);
        let corpus = tiny_corpus("public_ir_subset", 1);
        let report = run_corpus(&emb, &corpus).await;
        assert!(!report.is_skipped());
        assert_eq!(report.dimensionality, 2);
        assert!((report.mean_precision_at_k - 1.0).abs() < 1e-9);
        assert!((report.mean_recall_at_k - 1.0).abs() < 1e-9);
        assert!((report.mean_mrr - 1.0).abs() < 1e-9);
        assert!((report.mean_ndcg_at_k - 1.0).abs() < 1e-9);
        assert!((report.mean_latency_ms - 7.0).abs() < 1e-9);
        // rows: 6 metric rows, all dimension="embeddings", judge=corpus name.
        let rows = report.into_dimension_scores(&ModelId::from("nomic-embed:latest"), BackendTag::Gpu);
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| r.dimension == DIMENSION));
        assert!(rows.iter().all(|r| r.judge == "public_ir_subset"));
        assert!(rows.iter().any(|r| r.metric == "dimensionality" && (r.value - 2.0).abs() < 1e-9));
    }

    #[tokio::test]
    async fn non_embedding_candidate_skips_cleanly() {
        let emb = ScriptedEmbedder::failing("qwen3:8b");
        let corpus = tiny_corpus("public_ir_subset", 1);
        let report = run_corpus(&emb, &corpus).await;
        assert!(report.is_skipped());
        assert!(report.skipped.as_ref().unwrap().contains("not an embedding model"));
        // A skipped report produces NO storage rows.
        assert!(report
            .into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Cpu)
            .is_empty());
    }

    #[tokio::test]
    async fn full_dim6_reports_public_vs_engram_delta() {
        // Public: perfect retrieval (nDCG 1.0). Engram: same embedder but the
        // engram queries map to the WRONG region → weak retrieval → negative delta.
        let public_corpus = tiny_corpus("public_ir_subset", 1);
        let engram_corpus = Corpus {
            name: "engram_labeled_subset".into(),
            description: String::new(),
            k: 1,
            docs: vec![
                Doc { id: "e1".into(), text: "engram doc one".into() },
                Doc { id: "e2".into(), text: "engram doc two".into() },
            ],
            queries: vec![
                // query embeds to a region nearer the WRONG doc → miss.
                Query { id: "qe1".into(), text: "engram query one".into(), relevant: vec!["e1".into()] },
            ],
        };
        let emb = ScriptedEmbedder::new(
            "nomic-embed:latest",
            BackendTag::Gpu,
            &[
                ("doc about apples", vec![1.0, 0.0]),
                ("doc about boats", vec![0.0, 1.0]),
                ("doc about clouds", vec![0.5, 0.5]),
                ("query apples", vec![1.0, 0.0]),
                ("query boats", vec![0.0, 1.0]),
                ("engram doc one", vec![1.0, 0.0]),
                ("engram doc two", vec![0.0, 1.0]),
                // query closer to e2 than e1 → ranks the wrong doc first → miss.
                ("engram query one", vec![0.1, 1.0]),
            ],
        );
        let report = run_dim6(&emb, Some(&public_corpus), &engram_corpus).await;
        assert!(report.public.is_some());
        let delta = report.delta.as_ref().expect("delta computed when both corpora ran");
        // public nDCG 1.0, engram nDCG 0.0 → delta = -1.0, mismatch flagged.
        assert!((delta.ndcg_at_k - (-1.0)).abs() < 1e-9);
        assert!(delta.domain_mismatch_flag);

        let rows = report.into_dimension_scores();
        // 6 public + 6 engram + 4 delta = 16 rows.
        assert_eq!(rows.len(), 16);
        assert!(rows.iter().any(|r| r.metric == "ndcg_at_k_delta" && r.judge == "public_vs_engram"));
        assert!(rows.iter().filter(|r| r.judge == "public_ir_subset").count() == 6);
        assert!(rows.iter().filter(|r| r.judge == "engram_labeled_subset").count() == 6);
    }

    #[tokio::test]
    async fn public_absent_runs_engram_only_no_delta() {
        // Spec EDGE CASE: public benchmark fetch fails → Engram-only, no fabrication.
        let engram_corpus = tiny_corpus("engram_labeled_subset", 1);
        let emb = perfect_embedder("nomic-embed:latest", BackendTag::Cpu);
        let report = run_dim6(&emb, None, &engram_corpus).await;
        assert!(report.public.is_none());
        assert!(report.delta.is_none(), "no delta when public is absent");
        // Only engram rows are produced.
        let rows = report.into_dimension_scores();
        assert!(rows.iter().all(|r| r.judge == "engram_labeled_subset"));
        assert_eq!(rows.len(), 6);
    }
}
