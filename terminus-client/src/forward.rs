//! Request forwarding: local MCP call -> mTLS request to a terminus primary
//! -> response mapped back to MCP response shape (TCLI-05, layered on top of
//! TCLI-04's `enroll`/`connect`).
//!
//! ## Connection model: re-dial per forwarded request
//! This module deliberately does NOT hold a long-lived, reused mTLS
//! connection across forwarded calls. Each [`forward`] call:
//! 1. Calls [`crate::enroll::enroll`] (cheap -- reuses the still-valid local
//!    credential unless it's expired/near-expiry, per TCLI-04) to obtain a
//!    current [`crate::enroll::EnrolledCredential`].
//! 2. Calls [`crate::transport::connect`] to dial a FRESH mTLS connection to
//!    the primary.
//! 3. Drives exactly one HTTP/1.1 request/response over that connection via
//!    `hyper::client::conn::http1`, then lets the connection drop.
//!
//! This is a deliberate P2 simplification, not an oversight: it trades one
//! extra TLS handshake per tool call for a much simpler reconnect story --
//! "the primary restarted" and "the connection was never established in the
//! first place" collapse into the exact same code path (dial fails or
//! succeeds fresh, every time), so there is no separate reconnect/keep-alive
//! state machine to get wrong. A persistent, pooled mTLS connection (with
//! its own health-check/reconnect logic) is a reasonable follow-up if
//! per-call handshake latency proves material in practice, but is out of
//! scope for this item's estimate -- noted explicitly per the TCLI-05 EDGE
//! CASES rather than left as a silent gap.
//!
//! ## Payload size
//! No additional size cap is imposed on either the request or response body
//! beyond `hyper`'s own defaults, which do not artificially limit body size
//! for this client-initiated, single-request-per-connection usage pattern
//! (no `http1_max_buf_size` override is set, so it uses hyper's default,
//! effectively unbounded for the request sizes this tool-call forwarding
//! path is expected to see). Per the TCLI-05 EDGE CASES: this is the
//! configured (default, unmodified) limit, noted explicitly rather than left
//! undocumented.

use std::time::Duration;

use bytes::Bytes as ByteBuf;
use futures_core::Stream;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio_stream::StreamExt as _;

use crate::enroll::{enroll, EnrollConfig};
use crate::error::ClientError;
use crate::transport::{connect, ConnectConfig};

/// How long a single forwarded request (enroll-check + dial + HTTP
/// round-trip) is allowed to take before [`forward`] gives up and returns
/// [`ClientError::ForwardTimeout`] rather than hanging indefinitely --
/// TCLI-05 EDGE CASE: "an in-flight tool call during an outage window
/// returns a clear error ... rather than hanging past a reasonable
/// timeout." Overridable via [`PrimaryConfig::timeout`]; the daemon binary
/// sets this from `TERMINUS_CLIENT_FORWARD_TIMEOUT_SECS`.
pub const DEFAULT_FORWARD_TIMEOUT: Duration = Duration::from_secs(15);

/// Default HTTP path the primary's MCP endpoint is mounted at, matching
/// `terminus_rs::mcp_server::build_router`'s `/mcp` route.
pub const DEFAULT_MCP_PATH: &str = "/mcp";

/// How long [`forward_stream`] waits for the response's status line +
/// headers (NOT the full body -- see module doc for why streaming calls use
/// a separate, shorter-scoped timeout than [`DEFAULT_FORWARD_TIMEOUT`]'s
/// whole-call coverage). Reuses the same default duration as
/// [`DEFAULT_FORWARD_TIMEOUT`] since both cover the same phase of work
/// (enroll-check + dial + handshake + issue request); only the *meaning*
/// differs (open-only vs. whole-call).
pub const DEFAULT_STREAM_OPEN_TIMEOUT: Duration = DEFAULT_FORWARD_TIMEOUT;

/// How long [`forward_stream`] will wait between two consecutive body chunks
/// before giving up on an apparently-wedged stream (EGSSE-01 EDGE CASE: an
/// agentic turn that stalls mid-stream -- primary hung, link dead -- must
/// surface a clear error rather than block the caller forever). Deliberately
/// much longer than [`DEFAULT_STREAM_OPEN_TIMEOUT`]/[`DEFAULT_FORWARD_TIMEOUT`]:
/// legitimate SSE progressive tool-dispatch turns can go quiet for tens of
/// seconds between tool-call chunks while a tool executes server-side.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Everything [`forward`] needs to reach a terminus primary for one request.
#[derive(Clone)]
pub struct PrimaryConfig {
    pub enroll: EnrollConfig,
    pub connect: ConnectConfig,
    /// HTTP path the primary's MCP endpoint is mounted at.
    pub mcp_path: String,
    /// Per-request timeout -- see [`DEFAULT_FORWARD_TIMEOUT`].
    pub timeout: Duration,
}

impl PrimaryConfig {
    pub fn new(enroll: EnrollConfig, connect: ConnectConfig) -> Self {
        Self {
            enroll,
            connect,
            mcp_path: DEFAULT_MCP_PATH.to_string(),
            timeout: DEFAULT_FORWARD_TIMEOUT,
        }
    }
}

/// Forward one JSON-RPC request body (e.g. a `tools/list` or `tools/call`
/// envelope) to the primary over a fresh mTLS connection, returning the
/// primary's decoded JSON-RPC response body verbatim (this function does not
/// reinterpret `result`/`error` -- the caller, [`crate::mcp_server`]'s local
/// dispatch, relays it back to the local MCP client unchanged per the
/// TCLI-05 spec item's APPROACH step 4).
///
/// Attaches the enrolled JWT as `Authorization: Bearer <jwt>` -- the paired
/// application-layer claim per the S107 spec's design decision #2.
pub async fn forward(cfg: &PrimaryConfig, request_body: Value) -> Result<Value, ClientError> {
    match tokio::time::timeout(cfg.timeout, forward_inner(cfg, request_body)).await {
        Ok(result) => result,
        Err(_) => Err(ClientError::ForwardTimeout(
            format!("{}:{}", cfg.connect.host, cfg.connect.port),
            cfg.timeout,
        )),
    }
}

/// Establish (and immediately drop) one mTLS connection to the primary --
/// used by the daemon binary at startup to fail fast (TCLI-05 APPROACH step
/// 2) before it accepts any local MCP connections, without needing to send a
/// real MCP request just to prove reachability.
///
/// Bounded by `cfg.timeout` -- neither `enroll` (reqwest, no default timeout)
/// nor `connect` (raw TCP + TLS handshake) imposes its own deadline, so a
/// primary that accepts the TCP connection but then stalls the TLS handshake
/// or HTTP response would otherwise hang startup indefinitely. Wrapping the
/// whole check here keeps the daemon's "fail fast, no partial startup, no
/// hang" contract intact against a half-open primary, symmetric with
/// [`forward`]'s own timeout.
pub async fn establish_initial_connection(cfg: &PrimaryConfig) -> Result<(), ClientError> {
    match tokio::time::timeout(cfg.timeout, establish_initial_connection_inner(cfg)).await {
        Ok(result) => result,
        Err(_) => Err(ClientError::ForwardTimeout(
            format!("{}:{}", cfg.connect.host, cfg.connect.port),
            cfg.timeout,
        )),
    }
}

async fn establish_initial_connection_inner(cfg: &PrimaryConfig) -> Result<(), ClientError> {
    let credential = enroll(&cfg.enroll).await?;
    connect(&credential, &cfg.connect).await?;
    Ok(())
}

async fn forward_inner(cfg: &PrimaryConfig, request_body: Value) -> Result<Value, ClientError> {
    let credential = enroll(&cfg.enroll).await?;
    let transport = connect(&credential, &cfg.connect).await?;
    let io = TokioIo::new(transport.into_io());

    let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await.map_err(|e| {
        ClientError::Handshake(format!("HTTP/1.1 handshake over mTLS stream failed: {e}"))
    })?;

    // `hyper::client::conn::http1`'s `SendRequest` cannot make progress
    // unless something polls the paired `Connection` future -- drive it in
    // the background for the lifetime of this one request/response, then
    // let it end (this module's per-request re-dial model, see module doc).
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::debug!("terminus_client::forward: connection ended: {e}");
        }
    });

    let addr = format!("{}:{}", cfg.connect.host, cfg.connect.port);

    let body_bytes = serde_json::to_vec(&request_body)
        .map_err(|e| ClientError::TlsConfig(format!("serializing forwarded request body: {e}")))?;

    let req = Request::builder()
        .method("POST")
        .uri(&cfg.mcp_path)
        .header("host", &cfg.connect.server_name)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {}", credential.jwt))
        .body(Full::new(ByteBuf::from(body_bytes)))
        .map_err(|e| ClientError::TlsConfig(format!("building forwarded HTTP request: {e}")))?;

    let resp = sender.send_request(req).await.map_err(|e| {
        ClientError::DialUnreachable(addr.clone(), format!("HTTP request over mTLS stream failed: {e}"))
    })?;

    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| ClientError::MalformedResponse(format!("reading response body: {e}")))?
        .to_bytes();
    let raw = String::from_utf8_lossy(&body);

    if !status.is_success() {
        return Err(ClientError::ForwardRejected {
            status: status.as_u16(),
            body: raw.to_string(),
        });
    }

    parse_mcp_response_body(&raw)
}

/// Forward one JSON request body to an arbitrary `path` on the primary over
/// a fresh mTLS connection (same dial/enroll model as [`forward`]), and
/// return the response body as an incrementally-consumable async
/// [`Stream`] of [`ByteBuf`] chunks -- for progressive/SSE-shaped endpoints
/// (e.g. Chord's `/v1/agent/execute`, `/v1/chat/completions`, `/v1/infer`,
/// `/v1/coding/select`) that [`forward`]'s buffer-the-whole-body-then-return
/// shape cannot represent: a caller driving lumina's `agent_loop` needs each
/// `event:`/`data:` SSE frame as it arrives, not after the whole turn has
/// finished.
///
/// ## What this function does NOT do
/// It does not parse SSE framing itself (no `event:`/`data:` splitting, no
/// JSON decoding of frame payloads) -- it hands the caller raw bytes exactly
/// as `hyper` delivers them off the wire, unbuffered, the same posture
/// Chord's own inference proxy takes with `bytes_stream`/`Body::from_stream`
/// on the far side of this same mTLS link. The caller is responsible for
/// buffering partial frames across chunk boundaries and splitting on the SSE
/// record separator -- see "Driving the stream" below.
///
/// ## Two-phase timeout model
/// Unlike [`forward`] (one timeout covers the whole call), this function
/// splits its timeout coverage in two, because a streaming call's body may
/// legitimately run far longer than any reasonable whole-call bound:
/// 1. **Open phase** (enroll-check + dial + handshake + send request + read
///    response headers): bounded by `cfg.timeout`
///    ([`DEFAULT_STREAM_OPEN_TIMEOUT`] by default) -- a
///    [`ClientError::StreamOpenTimeout`] if headers don't arrive in time.
/// 2. **Body phase** (each chunk read after that): bounded by `idle_timeout`
///    -- a stream item is `Err(`[`ClientError::StreamIdleTimeout`]`)` if no
///    new chunk arrives within that window since the last one (or since the
///    stream opened). The stream ends (returns `None`) once the primary
///    closes the response body normally.
///
/// ## Driving the stream
/// ```rust,ignore
/// use tokio_stream::StreamExt;
///
/// let mut stream = forward_stream(&cfg, "/v1/agent/execute", request_body).await?;
/// let mut pending = Vec::new(); // holds bytes since the last complete SSE record
/// while let Some(chunk) = stream.next().await {
///     let chunk = chunk?; // StreamRead / StreamIdleTimeout surfaces here
///     pending.extend_from_slice(&chunk);
///     // split `pending` on "\n\n", handing each complete `event:`/`data:`
///     // record to the caller's own SSE-frame decoder (e.g. lumina's
///     // agent_loop tool-dispatch handling), retaining any trailing partial
///     // record in `pending` for the next chunk.
/// }
/// ```
///
/// Attaches the enrolled JWT as `Authorization: Bearer <jwt>`, matching
/// [`forward`].
pub async fn forward_stream(
    cfg: &PrimaryConfig,
    path: &str,
    request_body: Value,
) -> Result<impl Stream<Item = Result<ByteBuf, ClientError>>, ClientError> {
    forward_stream_with_idle_timeout(cfg, path, request_body, DEFAULT_STREAM_IDLE_TIMEOUT).await
}

/// Same as [`forward_stream`], but with an explicit per-chunk idle timeout
/// instead of [`DEFAULT_STREAM_IDLE_TIMEOUT`] -- exposed separately so a
/// caller with a known-different workload shape (e.g. a much shorter-lived
/// streaming endpoint) isn't stuck with the agentic-turn-sized default.
pub async fn forward_stream_with_idle_timeout(
    cfg: &PrimaryConfig,
    path: &str,
    request_body: Value,
    idle_timeout: Duration,
) -> Result<impl Stream<Item = Result<ByteBuf, ClientError>>, ClientError> {
    let addr = format!("{}:{}", cfg.connect.host, cfg.connect.port);

    let resp = match tokio::time::timeout(cfg.timeout, open_stream_inner(cfg, path, request_body)).await {
        Ok(result) => result?,
        Err(_) => return Err(ClientError::StreamOpenTimeout(addr, cfg.timeout)),
    };

    let idle_timeout_addr = addr.clone();
    let data_stream = resp.into_body().into_data_stream();
    let mapped = tokio_stream::StreamExt::timeout(data_stream, idle_timeout).map(move |item| match item {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(ClientError::StreamRead(e.to_string())),
        Err(_elapsed) => Err(ClientError::StreamIdleTimeout(idle_timeout_addr.clone(), idle_timeout)),
    });

    Ok(mapped)
}

/// The open phase of [`forward_stream`]: enroll, dial, handshake, issue the
/// request, and read the response status + headers (NOT the body). Returns
/// the still-streaming [`Response`] on a 2xx status; on a non-2xx status,
/// reads the (expected-small) error body and returns
/// [`ClientError::ForwardRejected`] -- mirroring [`forward_inner`]'s error
/// shape for the non-streaming case.
async fn open_stream_inner(
    cfg: &PrimaryConfig,
    path: &str,
    request_body: Value,
) -> Result<Response<Incoming>, ClientError> {
    let credential = enroll(&cfg.enroll).await?;
    let transport = connect(&credential, &cfg.connect).await?;
    let io = TokioIo::new(transport.into_io());

    let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await.map_err(|e| {
        ClientError::Handshake(format!("HTTP/1.1 handshake over mTLS stream failed: {e}"))
    })?;

    // Same "drive the connection in the background, let it end when the
    // response body finishes / the caller drops the stream" model as
    // `forward_inner` -- see module doc's "Connection model" section.
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::debug!("terminus_client::forward_stream: connection ended: {e}");
        }
    });

    let addr = format!("{}:{}", cfg.connect.host, cfg.connect.port);

    let body_bytes = serde_json::to_vec(&request_body)
        .map_err(|e| ClientError::TlsConfig(format!("serializing forwarded stream request body: {e}")))?;

    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", &cfg.connect.server_name)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream, application/json")
        .header("authorization", format!("Bearer {}", credential.jwt))
        .body(Full::new(ByteBuf::from(body_bytes)))
        .map_err(|e| ClientError::TlsConfig(format!("building forwarded streaming HTTP request: {e}")))?;

    let resp = sender.send_request(req).await.map_err(|e| {
        ClientError::DialUnreachable(addr.clone(), format!("streaming HTTP request over mTLS stream failed: {e}"))
    })?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| ClientError::MalformedResponse(format!("reading rejected stream response body: {e}")))?
            .to_bytes();
        let raw = String::from_utf8_lossy(&body).to_string();
        return Err(ClientError::ForwardRejected { status: status.as_u16(), body: raw });
    }

    Ok(resp)
}

/// Parse a terminus primary `/mcp` response body -- either SSE-framed
/// (`event: message\ndata: {...}\n\n`, matching
/// `terminus_rs::mcp_server::sse_response`) or plain JSON, mirroring the
/// same "find a `data:` line, else fall back to plain JSON" parsing Chord's
/// `McpSession::send_request` already does against the same server-side
/// endpoint.
fn parse_mcp_response_body(raw: &str) -> Result<Value, ClientError> {
    let json_str = raw
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .unwrap_or(raw.trim());

    if json_str.is_empty() {
        return Err(ClientError::MalformedResponse(
            "empty response body from primary".to_string(),
        ));
    }

    serde_json::from_str(json_str)
        .map_err(|e| ClientError::MalformedResponse(format!("parsing primary's MCP response: {e}")))
}

/// Deny-list of JSON object keys whose string values are redacted before a
/// tool-call payload (arguments or results) is ever passed to `tracing`, per
/// the TCLI-05 spec item's S6 sanitization requirement ("tool args may
/// contain sensitive values -- truncate/redact ... even though this is a
/// client-side log, not the primary's official audit trail"). Matched
/// case-insensitively against the key name, mirroring
/// `terminus_rs::bin::review_daemon::sanitize`'s `SECRET_PATTERNS`
/// convention (substring match against the uppercased key).
const REDACT_KEY_PATTERNS: &[&str] = &[
    "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH", "JWT", "KEY", "COOKIE",
];

/// Render `value` as a string safe to pass to `tracing::debug!`/`info!`/etc:
/// any object key matching [`REDACT_KEY_PATTERNS`] has its value replaced
/// with `"<redacted>"`, recursively through nested objects/arrays. Used for
/// every daemon-side log line that might otherwise echo a forwarded tool
/// call's arguments or a primary response verbatim.
pub fn sanitize_for_log(value: &Value) -> String {
    redact(value.clone()).to_string()
}

fn redact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let upper = k.to_uppercase();
                    if REDACT_KEY_PATTERNS.iter().any(|pat| upper.contains(pat)) {
                        (k, Value::String("<redacted>".to_string()))
                    } else {
                        (k, redact(v))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_stream::StreamExt;

    #[test]
    fn sanitize_for_log_redacts_secret_shaped_keys_recursively() {
        let value = json!({
            "name": "gitea_create_repo",
            "arguments": {
                "auth_token": "super-secret-value",
                "nested": {"jwt": "<REDACTED-SECRET>", "note": "keep me"}
            }
        });
        let rendered = sanitize_for_log(&value);
        assert!(!rendered.contains("super-secret-value"));
        assert!(!rendered.contains("<REDACTED-SECRET>"));
        assert!(rendered.contains("keep me"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn parse_mcp_response_body_handles_sse_framing() {
        let raw = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let parsed = parse_mcp_response_body(raw).expect("should parse SSE-framed body");
        assert_eq!(parsed["result"]["ok"], true);
    }

    #[test]
    fn parse_mcp_response_body_handles_plain_json_fallback() {
        let raw = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}";
        let parsed = parse_mcp_response_body(raw).expect("should parse plain JSON body");
        assert_eq!(parsed["result"]["ok"], true);
    }

    #[test]
    fn parse_mcp_response_body_rejects_empty_body() {
        let err = parse_mcp_response_body("").expect_err("empty body must be a typed error");
        assert!(matches!(err, ClientError::MalformedResponse(_)));
    }

    // ── Integration tests: real mTLS + real HTTP/1.1 request/response ──────
    // against a mock terminus-primary-shaped server, via `test_support`
    // below (also reused by `crate::mcp_server`'s own tests).

    use std::time::Instant;
    use test_support::*;

    #[tokio::test]
    async fn forward_tools_list_returns_the_primarys_catalog() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_primary(&ca, move |req| {
            assert_eq!(req["method"], "tools/list");
            json!({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{"name": "health", "description": "Health check"}]}})
        })
        .await;

        let cfg = primary_config(&credential, host, port);
        let resp = forward(&cfg, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await
            .expect("forward should succeed against a real mock mTLS primary");
        assert_eq!(resp["result"]["tools"][0]["name"], "health");
    }

    #[tokio::test]
    async fn forward_tools_call_relays_the_primarys_response_unchanged() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_primary(&ca, move |req| {
            assert_eq!(req["method"], "tools/call");
            assert_eq!(req["params"]["name"], "health");
            json!({"jsonrpc": "2.0", "id": req["id"], "result": {"content": [{"type": "text", "text": "ok"}], "isError": false}})
        })
        .await;

        let cfg = primary_config(&credential, host, port);
        let resp = forward(
            &cfg,
            json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "health", "arguments": {}}}),
        )
        .await
        .expect("forward should succeed");
        assert_eq!(resp["result"]["content"][0]["text"], "ok");
        assert_eq!(resp["result"]["isError"], false);
    }

    #[tokio::test]
    async fn forward_reconnects_after_a_dropped_connection() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_primary_first_connection_dropped(&ca, |req| {
            json!({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": []}})
        })
        .await;

        let cfg = primary_config(&credential, host.clone(), port);

        // First call: the mock primary accepts the TCP+TLS connection then
        // closes it without responding -- simulates a primary restart /
        // network blip mid-connection.
        let first = forward(&cfg, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
        assert!(first.is_err(), "first call against a dropped connection must fail cleanly, not panic");

        // Second call: this module's per-request re-dial model (see module
        // doc) means "reconnect" is just "dial again" -- no special retry
        // state to get wrong. The daemon does not crash between calls.
        let second = forward(&cfg, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await
            .expect("second call should succeed once the primary is responding again");
        assert_eq!(second["result"]["tools"], json!([]));
    }

    #[tokio::test]
    async fn forward_times_out_cleanly_instead_of_hanging() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_primary_never_responds(&ca).await;

        let mut cfg = primary_config(&credential, host, port);
        cfg.timeout = Duration::from_millis(250);

        let started = Instant::now();
        let err = forward(&cfg, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await
            .expect_err("a primary that never responds must time out, not hang");
        assert!(matches!(err, ClientError::ForwardTimeout(_, _)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must be bounded by the configured timeout, not left hanging: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn establish_initial_connection_fails_fast_when_primary_unreachable() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let cfg = primary_config(&credential, "127.0.0.1".to_string(), 1); // nothing listens on port 1

        let started = Instant::now();
        let err = establish_initial_connection(&cfg)
            .await
            .expect_err("daemon startup connectivity check must fail fast against an unreachable primary");
        assert!(matches!(err, ClientError::DialUnreachable(_, _)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn establish_initial_connection_times_out_against_a_stalled_tls_handshake() {
        // A "primary" that accepts the TCP connection but never speaks TLS at
        // all -- the mTLS handshake in `connect` would otherwise block
        // forever. The startup check must fail fast on its own timeout (agy
        // P2, TCLI-05 review), not hang.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                // Accept and hold the socket open without ever writing the
                // TLS ServerHello.
                match listener.accept().await {
                    Ok((sock, _)) => {
                        tokio::spawn(async move {
                            let _held = sock;
                            std::future::pending::<()>().await;
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let mut cfg = primary_config(&credential, addr.ip().to_string(), addr.port());
        cfg.timeout = Duration::from_millis(250);

        let started = Instant::now();
        let err = establish_initial_connection(&cfg)
            .await
            .expect_err("a stalled TLS handshake must time out, not hang");
        assert!(matches!(err, ClientError::ForwardTimeout(_, _)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "startup connectivity check must be bounded by its timeout: {:?}",
            started.elapsed()
        );
    }

    // ── forward_stream tests (EGSSE-01) ─────────────────────────────────────

    #[tokio::test]
    async fn forward_stream_yields_chunks_incrementally_not_buffered_whole() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let chunks: Vec<ByteBuf> = vec![
            ByteBuf::from_static(b"event: message\ndata: {\"delta\":\"one\"}\n\n"),
            ByteBuf::from_static(b"event: message\ndata: {\"delta\":\"two\"}\n\n"),
            ByteBuf::from_static(b"event: message\ndata: {\"delta\":\"three\"}\n\n"),
        ];
        let (host, port) =
            spawn_mock_streaming_primary(&ca, chunks.clone(), Duration::from_millis(30)).await;

        let cfg = primary_config(&credential, host, port);
        let mut stream = Box::pin(
            forward_stream(&cfg, "/v1/agent/execute", json!({"turn": 1}))
                .await
                .expect("opening the stream against a real mock mTLS primary should succeed"),
        );

        // Each server-side chunk arrives as its own stream item -- proves
        // this is progressive delivery, not the whole SSE body collected
        // and split after the fact (which `forward`'s buffered model would
        // do).
        let mut received = Vec::new();
        while let Some(item) = stream.next().await {
            let bytes = item.expect("each chunk should decode cleanly");
            received.push(bytes);
        }

        assert_eq!(received.len(), chunks.len(), "expected one stream item per server-side chunk");
        for (got, want) in received.iter().zip(chunks.iter()) {
            assert_eq!(got, want);
        }
    }

    #[tokio::test]
    async fn forward_stream_ends_cleanly_when_the_primary_closes_the_body() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) =
            spawn_mock_streaming_primary(&ca, vec![ByteBuf::from_static(b"data: {\"ok\":true}\n\n")], Duration::ZERO)
                .await;

        let cfg = primary_config(&credential, host, port);
        let mut stream = Box::pin(
            forward_stream(&cfg, "/v1/agent/execute", json!({}))
                .await
                .expect("open should succeed"),
        );

        let first = stream.next().await.expect("one chunk expected").expect("chunk should decode");
        assert_eq!(first, ByteBuf::from_static(b"data: {\"ok\":true}\n\n"));
        assert!(stream.next().await.is_none(), "stream must end once the primary closes the body");
    }

    #[tokio::test]
    async fn forward_stream_rejects_a_non_2xx_status_without_ever_yielding_chunks() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_streaming_primary_rejecting(&ca, 401, "unauthorized").await;

        let cfg = primary_config(&credential, host, port);
        let err = match forward_stream(&cfg, "/v1/agent/execute", json!({})).await {
            Ok(_) => panic!("a non-2xx status must be rejected before any stream is handed back"),
            Err(e) => e,
        };
        assert!(matches!(err, ClientError::ForwardRejected { status: 401, .. }));
    }

    #[tokio::test]
    async fn forward_stream_open_phase_times_out_cleanly_instead_of_hanging() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_primary_never_responds(&ca).await;

        let mut cfg = primary_config(&credential, host, port);
        cfg.timeout = Duration::from_millis(250);

        let started = Instant::now();
        let err = match forward_stream(&cfg, "/v1/agent/execute", json!({})).await {
            Ok(_) => panic!("a primary that never responds must time out opening the stream, not hang"),
            Err(e) => e,
        };
        assert!(matches!(err, ClientError::StreamOpenTimeout(_, _)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn forward_stream_surfaces_idle_timeout_when_the_body_stalls_mid_stream() {
        let ca = generate_test_ca();
        let credential = enrolled_credential(&ca, "test-daemon");
        let (host, port) = spawn_mock_streaming_primary_then_stalls(
            &ca,
            ByteBuf::from_static(b"data: {\"delta\":\"first\"}\n\n"),
        )
        .await;

        let cfg = primary_config(&credential, host, port);
        let mut stream = Box::pin(
            forward_stream_with_idle_timeout(&cfg, "/v1/agent/execute", json!({}), Duration::from_millis(200))
                .await
                .expect("open should succeed"),
        );

        let first = stream.next().await.expect("first chunk expected").expect("first chunk should decode");
        assert_eq!(first, ByteBuf::from_static(b"data: {\"delta\":\"first\"}\n\n"));

        let started = Instant::now();
        let second = stream
            .next()
            .await
            .expect("stream must yield an idle-timeout error item, not end silently")
            .expect_err("a stalled body must surface as a typed error, not hang forever");
        assert!(matches!(second, ClientError::StreamIdleTimeout(_, _)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "idle timeout must be bounded by the configured duration, not left hanging: {:?}",
            started.elapsed()
        );
    }
}

/// Shared test-support: a mock terminus-primary-shaped mTLS + HTTP/1.1
/// server, reused by [`crate::mcp_server`]'s own integration tests so both
/// layers are exercised against the exact same real handshake + framing
/// this crate drives in production. A sibling of `mod tests` (not nested
/// inside it) specifically so it's reachable as `crate::forward::test_support`
/// from `crate::mcp_server`'s own `#[cfg(test)]` code.
#[cfg(test)]
pub(crate) mod test_support {
        use std::sync::Arc;
        use std::time::SystemTime;

        use bytes::Bytes as ByteBuf;
        use http_body_util::{BodyExt, Full};
        use hyper_util::rt::TokioIo;
        use rcgen::{
            CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
        };
        use rustls::server::WebPkiClientVerifier;
        use rustls::RootCertStore;
        use serde_json::Value;
        use tokio::net::TcpListener;
        use tokio_stream::StreamExt;

        use crate::enroll::{EnrollConfig, EnrolledCredential};
        use crate::forward::PrimaryConfig;
        use crate::transport::ConnectConfig;

        pub(crate) struct TestCa {
            pub cert_pem: String,
            pub issuer: Issuer<'static, KeyPair>,
        }

        pub(crate) fn generate_test_ca() -> TestCa {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            params.distinguished_name.push(DnType::CommonName, "test-ca");
            params.key_usages.push(KeyUsagePurpose::KeyCertSign);
            let key_pair = KeyPair::generate().unwrap();
            let cert = params.self_signed(&key_pair).unwrap();
            let cert_pem = cert.pem();
            let issuer = Issuer::new(params, key_pair);
            TestCa { cert_pem, issuer }
        }

        fn issue_leaf(ca: &TestCa, identity: &str, eku: ExtendedKeyUsagePurpose) -> (String, String) {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.distinguished_name.push(DnType::CommonName, identity);
            params.subject_alt_names = vec![SanType::DnsName(identity.to_string().try_into().unwrap())];
            params.key_usages.push(KeyUsagePurpose::DigitalSignature);
            params.extended_key_usages.push(eku);
            let key_pair = KeyPair::generate().unwrap();
            let leaf = params.signed_by(&key_pair, &ca.issuer).unwrap();
            (leaf.pem(), key_pair.serialize_pem())
        }

        fn now_plus(secs: i64) -> i64 {
            SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + secs
        }

        pub(crate) fn enrolled_credential(ca: &TestCa, identity: &str) -> EnrolledCredential {
            let (cert_pem, key_pem) = issue_leaf(ca, identity, ExtendedKeyUsagePurpose::ClientAuth);
            EnrolledCredential {
                cert_pem,
                key_pem,
                ca_cert_pem: ca.cert_pem.clone(),
                jwt: "test.jwt.token".to_string(),
                expires_at: now_plus(3600),
            }
        }

        /// Build a [`PrimaryConfig`] pointed at `host:port`, with `credential`
        /// pre-seeded into a throwaway local store so [`crate::enroll::enroll`]
        /// reuses it without attempting a real `/enroll` HTTP call.
        pub(crate) fn primary_config(credential: &EnrolledCredential, host: String, port: u16) -> PrimaryConfig {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut store_path = std::env::temp_dir();
            store_path.push(format!("terminus-client-forward-test-{n}-{}", std::process::id()));
            store_path.push("credential.json");
            if let Some(parent) = store_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&store_path, serde_json::to_vec(credential).unwrap()).unwrap();

            let enroll_cfg = EnrollConfig { store_path, ..EnrollConfig::new("http://127.0.0.1:1", "test-daemon", "unused") };
            let connect_cfg = ConnectConfig { host, port, server_name: "terminus-primary-test".to_string() };
            PrimaryConfig::new(enroll_cfg, connect_cfg)
        }

        fn pem_to_der_certs(pem: &str) -> Vec<rustls::pki_types::CertificateDer<'static>> {
            rustls_pemfile::certs(&mut pem.as_bytes()).collect::<Result<Vec<_>, _>>().unwrap()
        }

        fn pem_to_der_key(pem: &str) -> rustls::pki_types::PrivateKeyDer<'static> {
            rustls_pemfile::private_key(&mut pem.as_bytes()).unwrap().unwrap()
        }

        fn server_tls_config(ca: &TestCa, require_client_auth: bool) -> rustls::ServerConfig {
            let (server_cert_pem, server_key_pem) = issue_leaf(ca, "terminus-primary-test", ExtendedKeyUsagePurpose::ServerAuth);
            let certs = pem_to_der_certs(&server_cert_pem);
            let key = pem_to_der_key(&server_key_pem);

            if require_client_auth {
                let mut roots = RootCertStore::empty();
                for der in pem_to_der_certs(&ca.cert_pem) {
                    roots.add(der).unwrap();
                }
                let verifier = WebPkiClientVerifier::builder(Arc::new(roots)).build().unwrap();
                rustls::ServerConfig::builder()
                    .with_client_cert_verifier(verifier)
                    .with_single_cert(certs, key)
                    .unwrap()
            } else {
                rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(certs, key).unwrap()
            }
        }

        /// Spawn a mock terminus-primary: binds loopback, accepts every
        /// connection, TLS-terminates requiring a client cert chained to
        /// `ca`, and serves `/mcp` by decoding the JSON-RPC request body,
        /// calling `responder`, and framing the result the same way
        /// `terminus_rs::mcp_server::sse_response` does.
        pub(crate) async fn spawn_mock_primary(
            ca: &TestCa,
            responder: impl Fn(Value) -> Value + Send + Sync + 'static,
        ) -> (String, u16) {
            let tls_config = server_tls_config(ca, true);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let responder = Arc::new(responder);

            tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
                loop {
                    let (tcp, _) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let acceptor = acceptor.clone();
                    let responder = responder.clone();
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        serve_one_connection(tls, responder).await;
                    });
                }
            });

            (addr.ip().to_string(), addr.port())
        }

        /// Like [`spawn_mock_primary`], but the FIRST accepted connection is
        /// completed at the TLS layer then dropped with no HTTP response at
        /// all (simulating a primary restart / network blip mid-connection)
        /// -- every connection after that is served normally.
        pub(crate) async fn spawn_mock_primary_first_connection_dropped(
            ca: &TestCa,
            responder: impl Fn(Value) -> Value + Send + Sync + 'static,
        ) -> (String, u16) {
            let tls_config = server_tls_config(ca, true);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let responder = Arc::new(responder);
            let first = Arc::new(std::sync::atomic::AtomicBool::new(true));

            tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
                loop {
                    let (tcp, _) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let acceptor = acceptor.clone();
                    let responder = responder.clone();
                    let is_first = first.swap(false, std::sync::atomic::Ordering::SeqCst);
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        if is_first {
                            // Complete the handshake, then drop -- no HTTP
                            // response is ever sent for this connection.
                            drop(tls);
                            return;
                        }
                        serve_one_connection(tls, responder).await;
                    });
                }
            });

            (addr.ip().to_string(), addr.port())
        }

        /// A mock primary that completes the mTLS handshake but never reads
        /// or responds to the HTTP request that follows -- exercises the
        /// forwarding path's own timeout (a hung primary, not an
        /// unreachable one).
        pub(crate) async fn spawn_mock_primary_never_responds(ca: &TestCa) -> (String, u16) {
            let tls_config = server_tls_config(ca, true);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
                loop {
                    let (tcp, _) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        // Hold the connection open forever without reading
                        // or writing anything further.
                        let _tls = tls;
                        std::future::pending::<()>().await;
                    });
                }
            });

            (addr.ip().to_string(), addr.port())
        }

        async fn serve_one_connection(
            tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
            responder: Arc<dyn Fn(Value) -> Value + Send + Sync>,
        ) {
            let io = TokioIo::new(tls);
            let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let responder = responder.clone();
                async move {
                    let body = req.into_body().collect().await.unwrap().to_bytes();
                    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let result = responder(parsed);
                    let sse = format!("event: message\ndata: {result}\n\n");
                    let resp = hyper::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Full::new(ByteBuf::from(sse)))
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
        }

        /// Spawn a mock terminus-primary that serves exactly one request per
        /// connection by writing `chunks` to the response body one at a
        /// time (sleeping `delay_between` between each), then closing the
        /// body normally -- exercises [`crate::forward::forward_stream`]'s
        /// incremental delivery against a real chunked HTTP/1.1 response,
        /// not a single buffered write.
        pub(crate) async fn spawn_mock_streaming_primary(
            ca: &TestCa,
            chunks: Vec<ByteBuf>,
            delay_between: std::time::Duration,
        ) -> (String, u16) {
            let tls_config = server_tls_config(ca, true);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let chunks = Arc::new(chunks);

            tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
                loop {
                    let (tcp, _) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let acceptor = acceptor.clone();
                    let chunks = chunks.clone();
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        serve_one_streaming_connection(tls, chunks, delay_between).await;
                    });
                }
            });

            (addr.ip().to_string(), addr.port())
        }

        /// Spawn a mock terminus-primary that immediately rejects every
        /// request with a fixed non-2xx `status` and a small plain-text
        /// `body` -- exercises [`crate::forward::forward_stream`]'s
        /// open-phase rejection path (no stream ever handed back).
        pub(crate) async fn spawn_mock_streaming_primary_rejecting(
            ca: &TestCa,
            status: u16,
            body: &'static str,
        ) -> (String, u16) {
            let tls_config = server_tls_config(ca, true);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
                loop {
                    let (tcp, _) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        let io = TokioIo::new(tls);
                        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| async move {
                            let _ = req.into_body().collect().await;
                            let resp = hyper::Response::builder()
                                .status(status)
                                .header("content-type", "text/plain")
                                .body(Full::new(ByteBuf::from(body)))
                                .unwrap();
                            Ok::<_, std::convert::Infallible>(resp)
                        });
                        let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
                    });
                }
            });

            (addr.ip().to_string(), addr.port())
        }

        /// Spawn a mock terminus-primary that writes exactly `first_chunk`
        /// to the response body, then holds the connection open forever
        /// without writing any more data or closing the body -- exercises
        /// [`crate::forward::forward_stream`]'s per-chunk idle-timeout path
        /// (a stream that opened and yielded real data, then wedged).
        pub(crate) async fn spawn_mock_streaming_primary_then_stalls(
            ca: &TestCa,
            first_chunk: ByteBuf,
        ) -> (String, u16) {
            let tls_config = server_tls_config(ca, true);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
                loop {
                    let (tcp, _) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let acceptor = acceptor.clone();
                    let first_chunk = first_chunk.clone();
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        serve_one_streaming_connection_then_stall(tls, first_chunk).await;
                    });
                }
            });

            (addr.ip().to_string(), addr.port())
        }

        /// Build a hyper-compatible response body backed by a
        /// `tokio::sync::mpsc` channel: `tx.send(chunk)` makes `chunk`
        /// available as the next body frame, and dropping `tx` closes the
        /// body normally -- the test-side equivalent of hyper 0.14's
        /// `Body::channel()`, which has no direct hyper-1.x/http-body-util
        /// counterpart.
        fn streaming_response_channel() -> (
            tokio::sync::mpsc::Sender<ByteBuf>,
            http_body_util::StreamBody<
                impl futures_core::Stream<Item = Result<hyper::body::Frame<ByteBuf>, std::convert::Infallible>>,
            >,
        ) {
            let (tx, rx) = tokio::sync::mpsc::channel::<ByteBuf>(8);
            let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
                .map(|chunk| Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(chunk)));
            (tx, http_body_util::StreamBody::new(stream))
        }

        async fn serve_one_streaming_connection(
            tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
            chunks: Arc<Vec<ByteBuf>>,
            delay_between: std::time::Duration,
        ) {
            let io = TokioIo::new(tls);
            let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let chunks = chunks.clone();
                async move {
                    let _ = req.into_body().collect().await;

                    let (tx, rx_body) = streaming_response_channel();
                    tokio::spawn(async move {
                        for (i, chunk) in chunks.iter().enumerate() {
                            if i > 0 && !delay_between.is_zero() {
                                tokio::time::sleep(delay_between).await;
                            }
                            if tx.send(chunk.clone()).await.is_err() {
                                return; // client hung up
                            }
                        }
                        // Dropping `tx` here closes the body normally.
                    });

                    let resp = hyper::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(rx_body)
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
        }

        async fn serve_one_streaming_connection_then_stall(
            tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
            first_chunk: ByteBuf,
        ) {
            let io = TokioIo::new(tls);
            let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let first_chunk = first_chunk.clone();
                async move {
                    let _ = req.into_body().collect().await;

                    let (tx, rx_body) = streaming_response_channel();
                    tokio::spawn(async move {
                        if tx.send(first_chunk).await.is_err() {
                            return;
                        }
                        // Hold the sender open forever without sending more
                        // data or dropping it -- the body never closes and
                        // no further chunk ever arrives, simulating a
                        // wedged mid-stream primary.
                        std::future::pending::<()>().await;
                    });

                    let resp = hyper::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(rx_body)
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
        }
    }

