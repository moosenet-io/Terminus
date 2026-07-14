//! KG semantic-embeddings client (KGEMB-02): turns text into a vector against
//! a configurable endpoint, and a deterministic "card" builder that turns a
//! [`KgNode`] (+ its 1-hop neighbor names) into the short text we embed.
//!
//! [`EmbedClient`] supports two response shapes, auto-detected from the URL:
//! - **Ollama** (`/api/embeddings`, `{"model","prompt"}` → `{"embedding":[...]}`)
//!   — mirrors `crate::intake::infer::ollama_embed`'s wire shape.
//! - **OpenAI-style** (`/v1/embeddings`, `{"model","input"}` →
//!   `{"data":[{"embedding":[...]}]}`) — for hosted providers (e.g.
//!   OpenRouter-compatible embeddings endpoints).
//!
//! This item ships only the client + card builder; nothing here is wired into
//! `scribe_kg_build` yet (that's KGEMB-03) — best-effort contract: any
//! HTTP/parse error is a [`ToolError`] the caller logs and skips, never a
//! panic and never a blocked build.
//!
//! ## Secrets
//! The optional bearer key (`EMBEDDINGS_API_KEY`, for hosted providers) is
//! secret material. This crate has no separate `SecretManager::get()` /
//! `vault::manager()` API of its own -- the runtime secret store is
//! materialized into env at deploy time, so a plain env read here already IS
//! the SecretManager read (same convention documented in `crate::pki`'s
//! module doc and used by `crate::review::dispatch`'s `OPENROUTER_API_KEY`).
//! URL/model/timeout are non-secret and come from `crate::config` instead.

use futures_util::{stream, StreamExt};
use serde_json::json;

use crate::error::ToolError;
use crate::scribe::graph::model::KgNode;

/// Bounded concurrency for `embed_batch`'s per-item Ollama fan-out.
const BATCH_CONCURRENCY: usize = 4;

/// Whether `url`'s HOST is a loopback address — parsed exactly, never a substring
/// match (which `localhost.attacker.com` / `127.0.0.1.evil.com` / a path or query
/// containing "localhost" would defeat, leaking a self-minted JWT to an external
/// host). Extracts the authority between `://` and the next `/`?`#`, strips any
/// `user@`, drops the `:port` (and unwraps a `[::1]` IPv6 literal), and matches the
/// bare host against the loopback set. No `url` crate dependency.
fn is_loopback_url(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(after_bracket) = host_port.strip_prefix('[') {
        // `[::1]:port` → the bracketed IPv6 literal.
        after_bracket.split(']').next().unwrap_or("")
    } else {
        // `host:port` or bare `host`.
        host_port.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Overall `node_card` length cap, in bytes (truncated on a char boundary).
const CARD_MAX_LEN: usize = 512;

/// Max neighbor names shown per side (callers / callees) in a card.
const CARD_MAX_NEIGHBORS: usize = 6;

/// A client that turns text into an embedding vector against a configurable
/// endpoint. Shape (Ollama vs. OpenAI) is derived once, at construction time,
/// from the URL.
#[derive(Debug, Clone)]
pub struct EmbedClient {
    http: reqwest::Client,
    url: String,
    model: String,
    api_key: Option<String>,
    /// `true` when `url` is an OpenAI-style `/v1/embeddings` endpoint;
    /// `false` for the Ollama `/api/embeddings` shape.
    openai_shape: bool,
    /// `true` when we must SELF-MINT a Chord JWT per request: no static
    /// `api_key` was supplied AND the endpoint is the co-located (loopback)
    /// Chord `/v1/embeddings` proxy, which requires a Bearer JWT (same
    /// `TERMINUS_JWT_SIGNING_KEY` this process already holds). This closes the
    /// EMBED-02 auth gap without provisioning a static `EMBEDDINGS_API_KEY`.
    self_mint_jwt: bool,
}

impl EmbedClient {
    /// Build a client from env config (`crate::config::embeddings_url`/
    /// `embeddings_model`/`embeddings_timeout_ms`), with the optional bearer
    /// key read from the env-materialized secret store (see module doc).
    pub fn from_env() -> Self {
        let url = crate::config::embeddings_url();
        let model = crate::config::embeddings_model();
        let timeout_ms = crate::config::embeddings_timeout_ms();
        let api_key = std::env::var("EMBEDDINGS_API_KEY")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        Self::new(url, model, api_key, timeout_ms)
    }

    /// Build a client from explicit fields (used by tests, and by any future
    /// caller that wants an endpoint other than the env default). Shape is
    /// derived from `url`.
    pub fn new(url: impl Into<String>, model: impl Into<String>, api_key: Option<String>, timeout_ms: u64) -> Self {
        let url = url.into();
        let openai_shape = url.contains("/v1/embeddings");
        // Self-mint a Chord JWT only for the co-located (loopback) OpenAI-shape
        // proxy when no static key is set — never for a real external host (a
        // terminus-signed JWT is meaningless to a hosted provider AND would leak
        // signing capability) and never when a key is explicitly provided. The
        // loopback test parses the URL's HOST and matches it EXACTLY — a substring
        // check would be bypassed by `localhost.attacker.com` / `127.0.0.1.evil`.
        let self_mint_jwt = api_key.is_none() && openai_shape && is_loopback_url(&url);
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms.max(1)))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http, url, model: model.into(), api_key, openai_shape, self_mint_jwt }
    }

    /// The Bearer token for a request: the explicit `api_key` if provided, else
    /// a freshly-minted Chord service JWT when [`self_mint_jwt`]. It reuses the
    /// federation module's [`crate::federation::mint_service_jwt`] — the ONE
    /// correct Chord-shaped JWT (`sub == "lumina"` — Chord's `validate_jwt` hard-
    /// rejects any other subject as `InvalidSubject` — signed with the shared
    /// `TERMINUS_PRIMARY_CHORD_JWT_SECRET`, NOT the enrollment `TERMINUS_JWT_SIGNING_KEY`).
    /// Minted PER REQUEST immediately before send (federation's standard short TTL,
    /// ~120s), so it can never expire mid-call. TTL is intentionally the federation
    /// value — terminus-primary and Chord are CO-LOCATED on the same host (loopback
    /// is the only case this fires), so they share one system clock and there is no
    /// skew for `exp` to trip. A mint failure (secret unset) degrades to no auth →
    /// the endpoint's own 401, never a panic.
    fn bearer_token(&self) -> Option<String> {
        if let Some(key) = &self.api_key {
            return Some(key.clone());
        }
        if self.self_mint_jwt {
            return crate::federation::mint_service_jwt().ok();
        }
        None
    }

    /// Embed a single piece of text. Never panics: transport, HTTP-status,
    /// and parse failures all become a [`ToolError`].
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, ToolError> {
        let body = if self.openai_shape {
            json!({ "model": self.model, "input": text })
        } else {
            json!({ "model": self.model, "prompt": text })
        };

        let mut req = self.http.post(&self.url).json(&body);
        if let Some(tok) = self.bearer_token() {
            req = req.bearer_auth(tok);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("embeddings: endpoint unreachable: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("embeddings: HTTP {status}: {detail}")));
        }

        let parsed: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("embeddings: could not parse response: {e}")))?;

        let vector = if self.openai_shape {
            parsed
                .get("data")
                .and_then(|d| d.get(0))
                .and_then(|d| d.get("embedding"))
        } else {
            parsed.get("embedding")
        };

        let vector = vector.ok_or_else(|| {
            ToolError::Http("embeddings: response missing expected embedding field".to_string())
        })?;

        serde_json::from_value::<Vec<f32>>(vector.clone())
            .map_err(|e| ToolError::Http(format!("embeddings: malformed embedding vector: {e}")))
    }

    /// Embed a batch of texts, preserving input order.
    ///
    /// OpenAI shape: sent as a single request with `input: [texts...]`
    /// (the provider's native batch). Ollama shape: `/api/embeddings` takes
    /// one prompt per call, so this fans `embed` out with bounded
    /// concurrency ([`BATCH_CONCURRENCY`]) instead.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ToolError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        if self.openai_shape {
            let body = json!({ "model": self.model, "input": texts });
            let mut req = self.http.post(&self.url).json(&body);
            if let Some(tok) = self.bearer_token() {
                req = req.bearer_auth(tok);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| ToolError::Http(format!("embeddings: endpoint unreachable: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                let detail = resp.text().await.unwrap_or_default();
                return Err(ToolError::Http(format!("embeddings: HTTP {status}: {detail}")));
            }
            let parsed: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ToolError::Http(format!("embeddings: could not parse response: {e}")))?;
            let data = parsed
                .get("data")
                .and_then(|d| d.as_array())
                .ok_or_else(|| {
                    ToolError::Http("embeddings: batch response missing 'data' array".to_string())
                })?;
            if data.len() != texts.len() {
                return Err(ToolError::Http(format!(
                    "embeddings: batch response returned {} vectors for {} inputs",
                    data.len(),
                    texts.len()
                )));
            }
            data.iter()
                .map(|d| {
                    d.get("embedding")
                        .ok_or_else(|| {
                            ToolError::Http("embeddings: batch item missing embedding".to_string())
                        })
                        .and_then(|v| {
                            serde_json::from_value::<Vec<f32>>(v.clone()).map_err(|e| {
                                ToolError::Http(format!("embeddings: malformed embedding vector: {e}"))
                            })
                        })
                })
                .collect()
        } else {
            // `buffer_unordered` completes items out of order, so tag each
            // with its input index and sort back into place afterward —
            // bounded concurrency without losing the caller's ordering.
            //
            // Each item is an OWNED `String` (not `&texts[i]`) moved into its
            // async block, rather than a borrow of `texts` -- with a borrowed
            // item, the closure's `Fn(Item<'a>) -> Fut` shape ties `Fut` to a
            // per-item lifetime `'a`, and depending on what else in the crate
            // gets type-checked alongside it, rustc's HRTB inference can fail
            // this closure with "implementation of FnOnce is not general
            // enough" (a known rustc limitation around async closures over a
            // borrowed stream item). Owning the string sidesteps the
            // per-item lifetime entirely.
            let mut ordered: Vec<(usize, Result<Vec<f32>, ToolError>)> =
                stream::iter(texts.to_vec().into_iter().enumerate())
                    .map(|(i, text)| async move { (i, self.embed(&text).await) })
                    .buffer_unordered(BATCH_CONCURRENCY.min(texts.len().max(1)))
                    .collect()
                    .await;
            ordered.sort_by_key(|(i, _)| *i);
            ordered.into_iter().map(|(_, r)| r).collect()
        }
    }

    #[cfg(test)]
    fn openai_shape(&self) -> bool {
        self.openai_shape
    }
}

/// Build the deterministic short text embedded for a [`KgNode`]: `"{kind}
/// {name} in {path}"`, plus (if any neighbors) a `" — calls: ...; called by:
/// ..."` suffix. Each neighbor list is capped at
/// [`CARD_MAX_NEIGHBORS`] names (in the given, already-desired order --
/// callers pass sorted slices for reproducibility); the whole card is capped
/// at [`CARD_MAX_LEN`] bytes, truncated on a char boundary. Never panics.
pub fn node_card(node: &KgNode, callers: &[&str], callees: &[&str]) -> String {
    let mut card = format!("{} {} in {}", node.kind.as_str(), node.name, node.path);

    if !callers.is_empty() || !callees.is_empty() {
        card.push_str(" — ");
        let mut parts = Vec::new();
        if !callees.is_empty() {
            parts.push(format!("calls: {}", capped_names(callees)));
        }
        if !callers.is_empty() {
            parts.push(format!("called by: {}", capped_names(callers)));
        }
        card.push_str(&parts.join("; "));
    }

    truncate_at_char_boundary(&card, CARD_MAX_LEN)
}

/// Sort names, dedup, then join up to [`CARD_MAX_NEIGHBORS`] with ", ".
/// Sorting INSIDE the card builder makes `node_card` independently deterministic
/// — the same node + same neighbor set always yields the identical card
/// regardless of the order the caller passed them (so the card hash, and thus
/// the "re-embed only when changed" logic, is stable across builds).
fn capped_names(names: &[&str]) -> String {
    let mut sorted: Vec<&str> = names.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
        .into_iter()
        .take(CARD_MAX_NEIGHBORS)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Truncate `s` to at most `max_len` bytes, backing off to the nearest
/// preceding char boundary so multi-byte UTF-8 is never split.
fn truncate_at_char_boundary(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::model::NodeKind;
    use httpmock::prelude::*;

    fn node(kind: NodeKind, name: &str, path: &str) -> KgNode {
        KgNode::new(format!("{path}::{name}"), kind, name, path)
    }

    // ── EMBED-02 self-mint gating ─────────────────────────────────────────
    #[test]
    fn self_mint_only_for_loopback_chord_proxy_without_a_key() {
        // co-located Chord proxy, no static key → self-mint.
        let c = EmbedClient::new("http://127.0.0.1:8099/v1/embeddings", "Qwen3-Embedding", None, 1000);
        assert!(c.self_mint_jwt);
        // an explicit key wins → never self-mint.
        let c = EmbedClient::new("http://127.0.0.1:8099/v1/embeddings", "m", Some("static".into()), 1000);
        assert!(!c.self_mint_jwt);
        assert_eq!(c.bearer_token().as_deref(), Some("static"));
        // raw Ollama shape (not /v1/embeddings) → never self-mint.
        let c = EmbedClient::new("http://127.0.0.1:11435/api/embeddings", "nomic", None, 1000);
        assert!(!c.self_mint_jwt);
        assert!(c.bearer_token().is_none());
        // an EXTERNAL /v1/embeddings host → never self-mint (a terminus JWT is
        // meaningless there; a real key would be required).
        let c = EmbedClient::new("https://openrouter.ai/api/v1/embeddings", "m", None, 1000);
        assert!(!c.self_mint_jwt);
        assert!(c.bearer_token().is_none());
    }

    #[test]
    fn is_loopback_url_matches_host_exactly_not_substring() {
        // Real loopback authorities → true (with/without port, ipv6, userinfo).
        assert!(is_loopback_url("http://127.0.0.1:8099/v1/embeddings"));
        assert!(is_loopback_url("http://localhost/v1/embeddings"));
        assert!(is_loopback_url("http://[::1]:8099/v1/embeddings"));
        assert!(is_loopback_url("http://user@127.0.0.1:8099/v1/embeddings"));
        // The bypasses both reviewers flagged → MUST be false (external hosts).
        assert!(!is_loopback_url("http://localhost.attacker.example/v1/embeddings"));
        assert!(!is_loopback_url("http://127.0.0.1.attacker.example/v1/embeddings"));
        assert!(!is_loopback_url("https://attacker.example/v1/embeddings/localhost"));
        assert!(!is_loopback_url("https://attacker.example/v1/embeddings?target=127.0.0.1"));
        assert!(!is_loopback_url("not-a-url"));
        // And the gate consequence: an attacker-substring host never self-mints.
        let c = EmbedClient::new("http://localhost.attacker.example/v1/embeddings", "m", None, 1000);
        assert!(!c.self_mint_jwt);
        assert!(c.bearer_token().is_none());
    }

    // ── shape detection ──────────────────────────────────────────────────

    #[test]
    fn shape_detected_from_url() {
        let ollama = EmbedClient::new("http://127.0.0.1:11435/api/embeddings", "m", None, 1000); // pii-test-fixture
        assert!(!ollama.openai_shape());

        let openai = EmbedClient::new("http://127.0.0.1:9/v1/embeddings", "m", None, 1000); // pii-test-fixture
        assert!(openai.openai_shape());
    }

    // ── embed: ollama shape ──────────────────────────────────────────────

    #[tokio::test]
    async fn embed_ollama_shape_parses_embedding_field() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/embeddings")
                .json_body(json!({"model": "nomic-embed-text", "prompt": "hello"}));
            then.status(200).json_body(json!({"embedding": [0.1, 0.2, 0.3]}));
        });

        let client = EmbedClient::new(
            format!("{}/api/embeddings", server.base_url()),
            "nomic-embed-text",
            None,
            5000,
        );
        let v = client.embed("hello").await.unwrap();
        assert_eq!(v, vec![0.1, 0.2, 0.3]);
        mock.assert();
    }

    // ── embed: openai shape ──────────────────────────────────────────────

    #[tokio::test]
    async fn embed_openai_shape_parses_data_embedding() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .json_body(json!({"model": "text-embedding-3-small", "input": "hello"}))
                .header("authorization", "Bearer testkey");
            then.status(200)
                .json_body(json!({"data": [{"embedding": [1.0, 2.0]}]}));
        });

        let client = EmbedClient::new(
            format!("{}/v1/embeddings", server.base_url()),
            "text-embedding-3-small",
            Some("testkey".to_string()),
            5000,
        );
        let v = client.embed("hello").await.unwrap();
        assert_eq!(v, vec![1.0, 2.0]);
        mock.assert();
    }

    #[tokio::test]
    async fn embed_no_api_key_sends_no_auth_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/embeddings").matches(|req| {
                !req.headers
                    .as_ref()
                    .map(|hs| hs.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")))
                    .unwrap_or(false)
            });
            then.status(200).json_body(json!({"embedding": [0.5]}));
        });

        let client = EmbedClient::new(format!("{}/api/embeddings", server.base_url()), "m", None, 5000);
        let v = client.embed("x").await.unwrap();
        assert_eq!(v, vec![0.5]);
        mock.assert();
    }

    // ── errors ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn embed_http_500_is_err_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/embeddings");
            then.status(500).body("boom");
        });

        let client = EmbedClient::new(format!("{}/api/embeddings", server.base_url()), "m", None, 5000);
        let err = client.embed("x").await.unwrap_err();
        assert!(matches!(err, ToolError::Http(_)));
    }

    #[tokio::test]
    async fn embed_malformed_response_is_err_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/embeddings");
            then.status(200).json_body(json!({"unexpected": "shape"}));
        });

        let client = EmbedClient::new(format!("{}/api/embeddings", server.base_url()), "m", None, 5000);
        let err = client.embed("x").await.unwrap_err();
        assert!(matches!(err, ToolError::Http(_)));
    }

    // ── embed_batch ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn embed_batch_ollama_preserves_order() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST)
                .path("/api/embeddings")
                .json_body(json!({"model": "m", "prompt": "a"}));
            then.status(200).json_body(json!({"embedding": [1.0]}));
        });
        server.mock(|when, then| {
            when.method(POST)
                .path("/api/embeddings")
                .json_body(json!({"model": "m", "prompt": "b"}));
            then.status(200).json_body(json!({"embedding": [2.0]}));
        });
        server.mock(|when, then| {
            when.method(POST)
                .path("/api/embeddings")
                .json_body(json!({"model": "m", "prompt": "c"}));
            then.status(200).json_body(json!({"embedding": [3.0]}));
        });

        let client = EmbedClient::new(format!("{}/api/embeddings", server.base_url()), "m", None, 5000);
        let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = client.embed_batch(&texts).await.unwrap();
        assert_eq!(out, vec![vec![1.0], vec![2.0], vec![3.0]]);
    }

    #[tokio::test]
    async fn embed_batch_empty_returns_empty() {
        let client = EmbedClient::new("http://127.0.0.1:1/api/embeddings", "m", None, 1000); // pii-test-fixture
        let out = client.embed_batch(&[]).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn embed_batch_openai_shape_single_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .json_body(json!({"model": "m", "input": ["a", "b"]}));
            then.status(200).json_body(json!({
                "data": [{"embedding": [1.0]}, {"embedding": [2.0]}]
            }));
        });

        let client = EmbedClient::new(format!("{}/v1/embeddings", server.base_url()), "m", None, 5000);
        let texts = vec!["a".to_string(), "b".to_string()];
        let out = client.embed_batch(&texts).await.unwrap();
        assert_eq!(out, vec![vec![1.0], vec![2.0]]);
        mock.assert();
    }

    // ── node_card ─────────────────────────────────────────────────────────

    #[test]
    fn node_card_includes_kind_name_path() {
        let n = node(NodeKind::Function, "do_thing", "src/lib.rs");
        let card = node_card(&n, &[], &[]);
        assert_eq!(card, "function do_thing in src/lib.rs");
    }

    #[test]
    fn node_card_includes_neighbors_when_present() {
        let n = node(NodeKind::Struct, "Widget", "src/widget.rs");
        let callers = vec!["make_widget", "reset_widget"];
        let callees = vec!["validate"];
        let card = node_card(&n, &callers, &callees);
        assert_eq!(
            card,
            "struct Widget in src/widget.rs — calls: validate; called by: make_widget, reset_widget"
        );
    }

    #[test]
    fn node_card_is_deterministic() {
        let n = node(NodeKind::Trait, "Runner", "src/run.rs");
        let callers = vec!["a", "b"];
        let callees = vec!["c"];
        let c1 = node_card(&n, &callers, &callees);
        let c2 = node_card(&n, &callers, &callees);
        assert_eq!(c1, c2);
    }

    #[test]
    fn node_card_is_order_independent_and_dedups() {
        // The card must be identical regardless of the order (or duplication)
        // of the neighbor names the caller passes — determinism is a property of
        // node_card itself, not of the caller pre-sorting (KGEMB-02 review).
        let n = node(NodeKind::Function, "hub", "src/hub.rs");
        let forward = node_card(&n, &["make", "reset", "init"], &["zed", "alpha"]);
        let reversed = node_card(&n, &["init", "reset", "make"], &["alpha", "zed"]);
        let with_dupes = node_card(&n, &["reset", "make", "init", "make"], &["alpha", "zed", "zed"]);
        assert_eq!(forward, reversed);
        assert_eq!(forward, with_dupes);
        // sorted + deduped inside the builder
        assert!(forward.contains("calls: alpha, zed"));
        assert!(forward.contains("called by: init, make, reset"));
    }

    #[test]
    fn node_card_caps_neighbor_list_at_six() {
        let n = node(NodeKind::Function, "hub", "src/hub.rs");
        let callers: Vec<&str> = vec!["a", "b", "c", "d", "e", "f", "g", "h"];
        let card = node_card(&n, &callers, &[]);
        assert!(card.contains("called by: a, b, c, d, e, f"));
        assert!(!card.contains('g'));
    }

    #[test]
    fn node_card_caps_overall_length() {
        let n = node(NodeKind::Function, "hub", "src/hub.rs");
        let long_names: Vec<String> = (0..50).map(|i| format!("caller_with_a_long_name_{i}")).collect();
        let callers: Vec<&str> = long_names.iter().map(String::as_str).collect();
        let card = node_card(&n, &callers, &[]);
        assert!(card.len() <= CARD_MAX_LEN);
    }
}
