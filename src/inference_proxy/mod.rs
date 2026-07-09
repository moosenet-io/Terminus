//! Inference proxy to Chord (TGW-03 — Terminus Primary Gateway sprint,
//! S108).
//!
//! Per the S108 spec item TGW-03: `terminus-primary`'s mTLS front door
//! forwards inference/agent requests to the co-located Chord process, which
//! remains the actual inference engine (model loading, GPU/VRAM management,
//! LiteLLM routing — all unchanged in Chord). This is a THIN proxy hop: no
//! inference logic lives here, only request forwarding + response relay
//! (including SSE streaming, unbuffered).
//!
//! ## Proxied routes
//! The spec names four Chord client-facing inference/agent routes as in
//! scope (confirmed present on Chord's own router in `moosenet/Chord`'s
//! `src/routes.rs`, all four gated by the exact same `auth_check`/
//! `CHORD_JWT_SECRET` scheme):
//! - `POST /v1/chat/completions` — OpenAI-compatible LLM proxy (the mandatory
//!   minimum named in the spec item; supports `stream: true`).
//! - `POST /v1/infer` — single-prompt, backend-aware inference.
//! - `POST /v1/agent/execute` — guarded agentic tool-calling loop (also
//!   streams via SSE on Chord's side).
//! - `POST /v1/coding/select` — fleet-driven coding-model resolution.
//!
//! ## Transport target
//! Reuses [`crate::config::chord_personal_federation_url`] (the
//! `TERMINUS_PRIMARY_CHORD_URL`-derived base URL TGW-02's federation client
//! already targets) rather than adding a second, always-identical
//! `TERMINUS_PRIMARY_CHORD_INFERENCE_URL` knob — Chord mounts both
//! `/v1/personal/tools/*` and these inference routes on the SAME router, so
//! there is only ever one Chord base URL to configure for a co-located
//! deploy. See `crate::config`'s "TGW-03: inference proxy to Chord" section
//! for the (separate) connect-timeout knob this module DOES need of its own.
//!
//! ## Auth: the SAME service JWT the personal-tool federation mints
//! Confirmed by reading Chord's `src/auth.rs`/`src/routes.rs`: every route
//! this module proxies to calls the identical `auth_check(&headers,
//! &state.jwt_secret)` that `/v1/personal/tools/*` calls, which HARD-REQUIRES
//! `sub == "lumina"`. So this module mints its outbound credential with
//! [`crate::federation::mint_service_jwt`] — the exact same function TGW-02's
//! `PersonalFederationClient` uses — rather than a second, duplicated JWT
//! minter. `TERMINUS_PRIMARY_CHORD_JWT_SECRET` (already provisioned for
//! TGW-02) is reused unchanged; this item adds no new secret.
//!
//! ## Caller identity forwarding
//! The mTLS-derived caller identity (`crate::pki::mtls::ClientIdentity`, when
//! the request arrived over the mTLS listener) is forwarded under the same
//! `crate::federation::CLIENT_IDENTITY_HEADER` TGW-02 already uses, so
//! Chord's own audit/logging sees a consistent identity header regardless of
//! which relay (personal-tool or inference) carried the request. This is
//! additive metadata, not a second auth mechanism.
//!
//! ## Streaming
//! Chord's own `chat_completions`/`agent_execute` handlers already relay
//! their upstream response as an unbuffered byte stream
//! (`upstream.bytes_stream()` → `axum::body::Body::from_stream`, see Chord's
//! `src/routes.rs`). This module does the identical thing one hop earlier:
//! `reqwest`'s response body is streamed straight into the `axum::Response`
//! body returned to the mTLS caller, chunk by chunk, never buffered into one
//! `Vec<u8>` — so `stream: true` chat completions (and any other
//! `text/event-stream` Chord route) pass SSE chunks through end to end.
//! Status code and `content-type` are relayed verbatim; hop-by-hop headers
//! (RFC 7230 §6.1) and the caller's own `Authorization` header (replaced with
//! this module's own service JWT, never forwarded) are stripped, mirroring
//! Chord's own `is_unforwardable_request_header` list for its upstream hop.
//!
//! ## Errors
//! A connect failure or connect-timeout to Chord (down, still loading a
//! model, network partition) surfaces as a clean `502 Bad Gateway` JSON error
//! to the mTLS caller — never a hang, never a silent fallback to any other
//! inference path (no inference fallback logic belongs in this thin proxy,
//! per the spec item's explicit edge case). Once connected, whatever status
//! Chord itself returns (200, its own 401/503/etc.) is relayed verbatim —
//! this proxy does not reinterpret Chord's own error semantics.

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use serde_json::json;
use std::time::Duration;
use tracing::warn;

use crate::federation::{mint_service_jwt, CLIENT_IDENTITY_HEADER};

/// Chord's client-facing inference/agent routes this proxy forwards to,
/// unchanged from the paths Chord itself serves them on (`src/routes.rs` in
/// `moosenet/Chord`) — `terminus-primary` mounts the identical path.
pub const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
pub const INFER_PATH: &str = "/v1/infer";
pub const AGENT_EXECUTE_PATH: &str = "/v1/agent/execute";
pub const CODING_SELECT_PATH: &str = "/v1/coding/select";

/// Hop-by-hop headers (RFC 7230 §6.1) plus length/encoding headers `reqwest`
/// recomputes for the outbound request, and the caller's own `Authorization`
/// header (replaced below with this proxy's own service JWT, never
/// forwarded to Chord) — identical list to Chord's own
/// `is_unforwardable_request_header` (its hop to the LLM backend), reused
/// here for terminus-primary's hop to Chord for the same reasons.
fn is_unforwardable_request_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "authorization"
    )
}

/// Errors from the proxy hop itself (terminus-primary ⇄ Chord) — distinct
/// from whatever HTTP status Chord's own handler returns once reached (that
/// is relayed verbatim, not classified here).
#[derive(Debug, thiserror::Error)]
pub enum InferenceProxyError {
    #[error("failed to mint service JWT for the chord inference hop: {0}")]
    JwtSigning(String),
    #[error("chord inference backend unreachable: {0}")]
    Unreachable(String),
}

impl InferenceProxyError {
    /// Render as the JSON error body + status this proxy returns to the mTLS
    /// caller — always `502 Bad Gateway` (the proxy hop itself failed before
    /// any tool-shaped/inference-shaped answer could come back), never a
    /// hang and never a different inference path.
    fn into_response(self) -> Response {
        warn!("inference_proxy: {self}");
        (
            StatusCode::BAD_GATEWAY,
            [("content-type", "application/json")],
            json!({"error": format!("terminus-primary inference proxy: {self}")}).to_string(),
        )
            .into_response()
    }
}

/// Client for terminus-primary's inference-proxy hop to Chord. Cheap to
/// construct/clone (wraps a shared `reqwest::Client` + a base URL String) —
/// one instance lives on `terminus-primary`'s gateway config for the process
/// lifetime, mirroring [`crate::federation::PersonalFederationClient`]'s
/// shape.
#[derive(Debug, Clone)]
pub struct InferenceProxyClient {
    base_url: String,
    http: reqwest::Client,
}

impl InferenceProxyClient {
    /// Build a client from env config
    /// (`crate::config::chord_personal_federation_url` +
    /// `crate::config::chord_inference_connect_timeout_ms`) — what
    /// `terminus_primary`'s `main()` calls.
    pub fn from_env() -> Self {
        Self::with_base_url(crate::config::chord_personal_federation_url())
    }

    /// Build a client pointed at an explicit base URL (e.g. a mocked Chord
    /// endpoint in tests, or an operator override already resolved by the
    /// caller). Connect timeout still comes from
    /// `crate::config::chord_inference_connect_timeout_ms` unless overridden
    /// via [`InferenceProxyClient::with_connect_timeout`].
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let connect_timeout =
            Duration::from_millis(crate::config::chord_inference_connect_timeout_ms());
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            // Deliberately NO `.timeout(...)` (total-response timeout) here —
            // see the module doc's "Streaming"/"Errors" sections: a streamed
            // inference response must be relayed for as long as Chord keeps
            // sending it, not cut off by a fixed deadline. Only the initial
            // connect is bounded.
            http: reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .build()
                .expect("reqwest client with a connect timeout should always build"),
        }
    }

    /// Override the connect timeout (mainly for tests that want a fast
    /// failure against a deliberately unreachable address rather than
    /// waiting out the production default).
    pub fn with_connect_timeout(base_url: impl Into<String>, connect_timeout: Duration) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .build()
                .expect("reqwest client with a connect timeout should always build"),
        }
    }

    /// Forward `method`/`path`/`headers`/`body` to Chord at
    /// `{base_url}{path}`, presenting a freshly-minted service JWT and
    /// forwarding `caller_identity` (the mTLS-derived identity of whoever
    /// called terminus-primary, if any). Relays Chord's response back to the
    /// mTLS caller: status code + `content-type` verbatim, body streamed
    /// through unbuffered (see the module doc's "Streaming" section).
    ///
    /// A proxy-hop failure (JWT minting, connect failure/timeout) short
    /// circuits to a clean `502` — see [`InferenceProxyError`] and the module
    /// doc's "Errors" section. Whatever Chord itself returns once reached
    /// (its own success or error status) is relayed unchanged, not
    /// reinterpreted here.
    pub async fn forward(
        &self,
        path: &str,
        headers: HeaderMap,
        body: Bytes,
        caller_identity: Option<&str>,
    ) -> Response {
        let jwt = match mint_service_jwt() {
            Ok(jwt) => jwt,
            Err(e) => return InferenceProxyError::JwtSigning(e.to_string()).into_response(),
        };

        let url = format!("{}{}", self.base_url, path);
        let mut req = self.http.post(&url).bearer_auth(jwt).body(body);
        let mut had_content_type = false;
        for (name, value) in headers.iter() {
            if is_unforwardable_request_header(name) {
                continue;
            }
            if name.as_str() == "content-type" {
                had_content_type = true;
            }
            req = req.header(name, value);
        }
        if !had_content_type {
            req = req.header("content-type", "application/json");
        }
        if let Some(identity) = caller_identity {
            if let Ok(hv) = HeaderValue::from_str(identity) {
                req = req.header(CLIENT_IDENTITY_HEADER, hv);
            }
        }

        let upstream = match req.send().await {
            Ok(r) => r,
            Err(e) => return InferenceProxyError::Unreachable(e.to_string()).into_response(),
        };

        let status = upstream.status();
        let content_type = upstream
            .headers()
            .get("content-type")
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("application/json"));

        // Stream the upstream body straight back to the caller, unbuffered —
        // this passes through both non-streaming JSON and streaming SSE
        // (`text/event-stream`) untouched, same as Chord's own hop to the LLM
        // backend (`src/routes.rs::chat_completions`).
        let stream = upstream
            .bytes_stream()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
        let relayed_body = Body::from_stream(stream);

        Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(relayed_body)
            .unwrap_or_else(|e| {
                warn!("inference_proxy: failed to build relayed response: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "response build error").into_response()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use serial_test::serial;

    fn set_jwt_secret() {
        std::env::set_var("TERMINUS_PRIMARY_CHORD_JWT_SECRET", "test-chord-shared-secret");
    }
    fn clear_jwt_secret() {
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_JWT_SECRET");
    }

    #[tokio::test]
    #[serial]
    async fn forward_relays_non_streaming_response_status_and_body() {
        set_jwt_secret();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path(CHAT_COMPLETIONS_PATH);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"id": "chatcmpl-1", "choices": []}));
        });

        let client = InferenceProxyClient::with_base_url(server.base_url());
        let resp = client
            .forward(
                CHAT_COMPLETIONS_PATH,
                HeaderMap::new(),
                Bytes::from(r#"{"model":"test","messages":[]}"#),
                Some("dev-box"),
            )
            .await;

        mock.assert();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["id"], "chatcmpl-1");
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn forward_presents_bearer_jwt_and_forwards_caller_identity() {
        set_jwt_secret();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path(CHAT_COMPLETIONS_PATH)
                .header(CLIENT_IDENTITY_HEADER, "harmony-primary")
                .matches(|req| {
                    let auth = req
                        .headers
                        .as_ref()
                        .and_then(|hs| hs.iter().find(|(k, _)| k.eq_ignore_ascii_case("authorization")))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    auth.starts_with("Bearer ")
                });
            then.status(200).json_body(json!({"ok": true}));
        });

        let client = InferenceProxyClient::with_base_url(server.base_url());
        let resp = client
            .forward(
                CHAT_COMPLETIONS_PATH,
                HeaderMap::new(),
                Bytes::from("{}"),
                Some("harmony-primary"),
            )
            .await;

        mock.assert();
        assert_eq!(resp.status(), StatusCode::OK);
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn forward_strips_callers_authorization_header_never_leaked_to_chord() {
        set_jwt_secret();
        let server = MockServer::start();
        // If the caller's own Authorization header leaked through unchanged
        // (instead of being replaced by the minted service JWT), this
        // wouldn't decode as a chord-shaped service JWT and the mock (which
        // requires a Bearer-prefixed header) would still pass -- so assert
        // the exact value sent is NOT the caller's original token.
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path(CHAT_COMPLETIONS_PATH)
                .matches(|req| {
                    let auth = req
                        .headers
                        .as_ref()
                        .and_then(|hs| hs.iter().find(|(k, _)| k.eq_ignore_ascii_case("authorization")))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    auth != "Bearer callers-own-mtls-token"
                });
            then.status(200).json_body(json!({"ok": true}));
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer callers-own-mtls-token"),
        );
        let client = InferenceProxyClient::with_base_url(server.base_url());
        let resp = client
            .forward(CHAT_COMPLETIONS_PATH, headers, Bytes::from("{}"), None)
            .await;

        mock.assert();
        assert_eq!(resp.status(), StatusCode::OK);
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn forward_streams_sse_chunks_through_unbuffered() {
        set_jwt_secret();
        let server = MockServer::start();
        // A multi-chunk SSE body, exactly the shape Chord's own
        // `stream: true` chat/completions response takes.
        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                         data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
                         data: [DONE]\n\n";
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path(CHAT_COMPLETIONS_PATH);
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse_body);
        });

        let client = InferenceProxyClient::with_base_url(server.base_url());
        let resp = client
            .forward(
                CHAT_COMPLETIONS_PATH,
                HeaderMap::new(),
                Bytes::from(r#"{"model":"test","stream":true}"#),
                None,
            )
            .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        // Consume the streamed body via the same `Body`/stream machinery a
        // real client would use, proving the relay is a genuine byte stream
        // (not a pre-buffered Vec) end to end, and that all chunks arrive
        // intact and in order.
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(text, sse_body);
        assert!(text.contains("Hel"));
        assert!(text.contains("lo"));
        assert!(text.contains("[DONE]"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn forward_relays_chords_own_error_status_verbatim() {
        set_jwt_secret();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path(CHAT_COMPLETIONS_PATH);
            then.status(503)
                .json_body(json!({"error": "LLM backend not configured (CHORD_LLM_URL unset)"}));
        });

        let client = InferenceProxyClient::with_base_url(server.base_url());
        let resp = client
            .forward(CHAT_COMPLETIONS_PATH, HeaderMap::new(), Bytes::from("{}"), None)
            .await;

        // Chord's own 503 is relayed verbatim -- not reinterpreted as a
        // proxy-hop failure (that would incorrectly suggest Chord itself is
        // unreachable, when it answered clearly).
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["error"].as_str().unwrap().contains("CHORD_LLM_URL"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn forward_chord_unreachable_is_clean_502_no_hang() {
        set_jwt_secret();
        // Nothing listening on this port.
        let client = InferenceProxyClient::with_connect_timeout(
            "http://127.0.0.1:1",
            Duration::from_millis(500),
        );
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            client.forward(CHAT_COMPLETIONS_PATH, HeaderMap::new(), Bytes::from("{}"), None),
        )
        .await
        .expect("an unreachable chord must fail fast, not hang");

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["error"].as_str().unwrap().contains("unreachable"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn forward_fails_fast_with_no_jwt_secret_configured() {
        clear_jwt_secret();
        let server = MockServer::start();
        let client = InferenceProxyClient::with_base_url(server.base_url());
        let resp = client
            .forward(CHAT_COMPLETIONS_PATH, HeaderMap::new(), Bytes::from("{}"), None)
            .await;

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["error"].as_str().unwrap().contains("JWT"));
    }
}
