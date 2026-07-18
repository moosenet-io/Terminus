//! CONST-18: the `/ws` real-time relay -- session-authenticated, masked
//! event stream. Replaces CONST-02's `handle_ws_stub` (a `501` scaffold; see
//! `crate::constellation::mod`'s history) with the real thing described in
//! spec §3.5.
//!
//! ## Shape of the relay
//! - **Downstream (browser) leg**: an `axum` WebSocket upgrade, gated by the
//!   SAME cookie-JWT verification `crate::constellation::auth::require_session`
//!   uses (`auth::session_from_cookie`) -- checked BEFORE the upgrade is
//!   ever accepted, so an unauthenticated caller never reaches
//!   `WebSocketUpgrade::on_upgrade` (no upstream dial, no half-open socket).
//! - **Upstream leg**: a plain `tokio-tungstenite` client dialing
//!   `crate::config::constellation_harmony_ws_url()`. Unconfigured -> the
//!   relay still ACCEPTS the browser's upgrade (the auth check already
//!   passed) but immediately sends a typed close frame and returns; the
//!   client already knows to fall back to 30s polling on any close
//!   (`aggregationClient.ts ws.connect`, audit §2) -- no new client-side
//!   branch needed for this case.
//! - **The masking property extends onto the socket**: every JSON text
//!   frame FROM Harmony is parsed, wrapped in the `{source:'harmony',
//!   event:...}` envelope (§3.5 — future sources join without a client
//!   change), passed through `crate::constellation::mask::mask_response`,
//!   and re-serialized before it ever reaches the browser. This is the same
//!   load-bearing property `crate::constellation::mask`'s doc describes for
//!   `/api/*` — extended to the one non-`/api/*` egress path this layer has.
//! - **Single-door property preserved**: like `crate::constellation::proxy`,
//!   this module is the ONLY place in the crate that dials Harmony's event
//!   socket -- no other module should grow a second ad hoc WS client for it.
//! - **Reconnect is doubly bounded**: `connect_upstream_with_backoff` bounds
//!   the retries/backoff of ONE dial attempt; `reconnect_upstream` (used by
//!   every reconnect call site in `pipe`, symmetrically on both the
//!   upstream-read AND upstream-write failure paths) additionally caps the
//!   total number of reconnects across one browser connection's lifetime
//!   and floors the gap between cycles -- guarding against a fast-closing
//!   upstream (accept, then immediately close) busy-looping the pipe with
//!   no effective backoff.
//!
//! ## What is deliberately NOT here (v1, per §3.5/CONST-18 scope)
//! - No fan-in of Chord/Muse events yet (the envelope's `source` field is
//!   the seam for that, added when those sources exist).
//! - No per-connection registry/broadcast (one operator, one browser tab at
//!   a time is the documented usage -- `docs/constellation/CONST-GUI-SPEC.md`
//!   §9 "Technical architecture").

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;

use crate::config;
use crate::constellation::{auth, mask};
use crate::mcp_server::McpServerState;

/// A frame larger than this is dropped (both directions) rather than
/// forwarded -- the relay pipes small, frequent event notifications, not
/// bulk payloads; an oversized frame is far more likely to be a
/// misbehaving/compromised upstream than legitimate data. The connection is
/// KEPT (never torn down for one oversized frame -- §10 CONST-18 edge
/// cases).
const MAX_FRAME_BYTES: usize = 1024 * 1024; // 1MB

/// Bounded reconnect attempts to the upstream Harmony event socket before
/// the relay gives up and tells the browser via a typed close frame. Kept
/// small + fast (see [`RECONNECT_BASE_BACKOFF]/[`RECONNECT_MAX_BACKOFF`])
/// so a genuinely-down Harmony fails the browser connection quickly rather
/// than holding a socket open for minutes while retrying.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const RECONNECT_BASE_BACKOFF: Duration = Duration::from_millis(250);
const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(5);

/// A SECOND, independent bound on top of [`MAX_RECONNECT_ATTEMPTS`]: the
/// total number of successful reconnects allowed within the lifetime of
/// ONE browser connection (`pipe`'s `reconnect_count`). Without this, an
/// upstream that ACCEPTS the dial and then immediately closes would make
/// every individual [`connect_upstream_with_backoff`] call succeed on its
/// very first attempt -- no backoff ever kicks in, and `pipe`'s reconnect
/// branch would busy-loop the accept-then-close cycle indefinitely (opus
/// review finding, CONST-18). [`RECONNECT_MIN_CYCLE_GAP`] additionally
/// floors the time between cycles even while this budget isn't yet
/// exhausted, so the loop can't spin hot even during its first few cycles.
const MAX_RECONNECTS_PER_CONNECTION: u32 = 5;
const RECONNECT_MIN_CYCLE_GAP: Duration = Duration::from_millis(250);

/// Close codes in the IANA-reserved "private use" range (4000-4999) --
/// distinct, machine-checkable reasons `constellation-web`'s
/// `aggregationClient.ts ws.connect` can branch on, per §3.5/CONST-18's
/// "typed close frames" requirement. Never overlaps a standard WS close
/// code (1000-2999/3000-3999 are reserved by the spec/registered
/// extensions).
const CLOSE_CODE_NO_UPSTREAM: u16 = 4000;
const CLOSE_CODE_UPSTREAM_LOST: u16 = 4001;
/// Reserved for completeness with the spec's three named cases -- in
/// practice unreachable in-process today, since an unauthenticated caller
/// is rejected with a `401` BEFORE `on_upgrade` is ever called (no socket
/// exists yet to send a close frame over). Kept as a named constant (rather
/// than inlined only where used) so the mapping from spec case -> code is
/// visible in one place, and so a future session-expiring-mid-connection
/// check (§10 CONST-18 edge cases: "Session expiring mid-connection") has
/// an obvious code to reach for without re-deriving the numbering.
#[allow(dead_code)]
const CLOSE_CODE_AUTH_FAILED: u16 = 4003;

/// Count of upstream frames dropped because they weren't valid JSON text,
/// or were binary, or exceeded [`MAX_FRAME_BYTES`] -- logged (not exposed
/// over the wire; an operator-visible signal only, via `tracing`) so a
/// misbehaving upstream shows up in logs rather than silently degrading.
static DROPPED_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

fn unauthorized_response() -> Response {
    let masked = mask::mask_response(json!({"error": "unauthorized"}));
    (StatusCode::UNAUTHORIZED, [("content-type", "application/json")], masked.to_string()).into_response()
}

/// `GET /ws` -- the relay's entry point. Deliberately takes the raw
/// [`Request`] (rather than a typed `WebSocketUpgrade` extractor parameter)
/// so the session check below runs BEFORE `WebSocketUpgrade` is even
/// extracted -- an unauthenticated request is rejected `401` without axum
/// ever attempting the upgrade handshake, matching the acceptance
/// criterion "Unauthenticated upgrade rejected before upstream dial."
pub async fn handle_ws(State(state): State<Arc<McpServerState>>, request: Request) -> Response {
    let _ = &state; // reserved: no aggregation-layer state is needed by the relay itself today
    let (mut parts, _body) = request.into_parts();

    if auth::session_from_cookie(&parts.headers).is_none() {
        tracing::warn!("constellation::ws: rejected upgrade -- no valid session");
        return unauthorized_response();
    }

    match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) => upgrade.on_upgrade(relay),
        // Malformed/non-WebSocket request (missing `Upgrade`/`Sec-WebSocket-*`
        // headers, wrong HTTP version, etc.) -- axum's own rejection
        // response, not this module's concern.
        Err(rejection) => rejection.into_response(),
    }
}

/// Build a typed close [`Message`] carrying `code` + `reason` -- the
/// "typed close frames" the spec calls for, so
/// `constellation-web`'s client can branch on `event.code` rather than
/// guessing from a bare disconnect.
fn close_message(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame { code, reason: Cow::Borrowed(reason) }))
}

/// The whole life of one browser connection to `/ws`, run after the
/// upgrade has already been accepted (auth already verified in
/// [`handle_ws`]).
async fn relay(socket: WebSocket) {
    let mut socket = socket;

    let Some(upstream_url) = config::constellation_harmony_ws_url() else {
        tracing::info!(
            "constellation::ws: CONSTELLATION_HARMONY_WS_URL is unset -- closing with a typed \
             frame, client stays on polling"
        );
        let _ = socket.send(close_message(CLOSE_CODE_NO_UPSTREAM, "no upstream configured")).await;
        let _ = socket.close().await;
        return;
    };

    let Some(upstream) = connect_upstream_with_backoff(&upstream_url).await else {
        tracing::warn!(
            upstream = %upstream_url,
            "constellation::ws: exhausted reconnect attempts dialing the upstream event socket"
        );
        let _ = socket.send(close_message(CLOSE_CODE_UPSTREAM_LOST, "upstream unreachable")).await;
        let _ = socket.close().await;
        return;
    };

    pipe(socket, upstream, &upstream_url).await;
}

type UpstreamStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Dial `url` with a bounded number of attempts and exponential (capped)
/// backoff between them. Returns `None` only after every attempt has
/// failed -- callers treat that as "upstream unreachable", never hang
/// indefinitely retrying.
async fn connect_upstream_with_backoff(url: &str) -> Option<UpstreamStream> {
    let mut backoff = RECONNECT_BASE_BACKOFF;
    for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
        match tokio_tungstenite::connect_async(url).await {
            Ok((stream, _response)) => return Some(stream),
            Err(e) => {
                tracing::warn!(
                    upstream = %url,
                    attempt,
                    max_attempts = MAX_RECONNECT_ATTEMPTS,
                    error = %e,
                    "constellation::ws: upstream connect attempt failed"
                );
                if attempt < MAX_RECONNECT_ATTEMPTS {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_MAX_BACKOFF);
                }
            }
        }
    }
    None
}

/// Attempt a bounded reconnect to the upstream leg, enforcing BOTH bounds:
/// [`connect_upstream_with_backoff`]'s per-attempt retry/backoff, AND the
/// per-connection-lifetime cap ([`MAX_RECONNECTS_PER_CONNECTION`] against
/// `*reconnect_count`) that guards against a fast-closing/flapping upstream
/// (accept-then-immediately-close) busy-looping `pipe` with no effective
/// delay between cycles. A successful reconnect always sleeps
/// [`RECONNECT_MIN_CYCLE_GAP`] before returning -- this floors the time
/// between cycles even on a dial that succeeds instantly, which is exactly
/// the busy-loop case (`connect_upstream_with_backoff`'s own backoff never
/// engages when every attempt succeeds on try 1). Returns `None` once
/// either bound is exhausted; callers treat that identically to "upstream
/// unreachable."
async fn reconnect_upstream(upstream_url: &str, reconnect_count: &mut u32) -> Option<UpstreamStream> {
    if *reconnect_count >= MAX_RECONNECTS_PER_CONNECTION {
        tracing::warn!(
            upstream = %upstream_url,
            reconnect_count = *reconnect_count,
            max = MAX_RECONNECTS_PER_CONNECTION,
            "constellation::ws: exceeded the per-connection reconnect budget -- refusing further reconnects"
        );
        return None;
    }
    let stream = connect_upstream_with_backoff(upstream_url).await?;
    *reconnect_count += 1;
    tokio::time::sleep(RECONNECT_MIN_CYCLE_GAP).await;
    Some(stream)
}

/// The bidirectional pipe: client <-> upstream, for the lifetime of one
/// browser connection. Reconnects the upstream leg (bounded, per
/// [`reconnect_upstream`]) if it drops OR fails to accept a forwarded
/// client frame; gives up and sends [`CLOSE_CODE_UPSTREAM_LOST`] to the
/// browser only once reconnect attempts are exhausted -- symmetric
/// handling on both the upstream-read and the upstream-write side (CONST-18
/// review finding: the write side used to just drop the connection with no
/// reconnect attempt and no typed close frame).
async fn pipe(socket: WebSocket, upstream: UpstreamStream, upstream_url: &str) {
    let (mut client_tx, mut client_rx) = socket.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    let mut reconnect_count: u32 = 0;

    loop {
        tokio::select! {
            client_msg = client_rx.next() => {
                match client_msg {
                    Some(Ok(msg)) => {
                        if frame_len(&msg) > MAX_FRAME_BYTES {
                            log_dropped("client->upstream frame exceeded MAX_FRAME_BYTES");
                            continue;
                        }
                        match to_upstream_message(msg) {
                            Some(forwarded) => {
                                let is_close = matches!(forwarded, UpstreamMessage::Close(_));
                                if upstream_tx.send(forwarded).await.is_err() {
                                    tracing::warn!("constellation::ws: failed forwarding client frame upstream, attempting reconnect");
                                    match reconnect_upstream(upstream_url, &mut reconnect_count).await {
                                        Some(reconnected) => {
                                            let (new_tx, new_rx) = reconnected.split();
                                            upstream_tx = new_tx;
                                            upstream_rx = new_rx;
                                            // The dropped client frame is not replayed (Sink::send
                                            // already consumed it on the failed attempt) -- the
                                            // pipe itself survives via the freshly reconnected
                                            // upstream leg, matching the read-side reconnect's own
                                            // "the pipe survives, at most one event is lost" contract.
                                            continue;
                                        }
                                        None => {
                                            let _ = client_tx
                                                .send(close_message(CLOSE_CODE_UPSTREAM_LOST, "upstream unreachable"))
                                                .await;
                                            break;
                                        }
                                    }
                                }
                                if is_close {
                                    // Client-initiated close -- end the relay; the
                                    // upstream leg is torn down when `upstream_tx`/
                                    // `upstream_rx` drop at function return.
                                    break;
                                }
                            }
                            None => log_dropped("client->upstream: unsupported frame kind"),
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!("constellation::ws: client socket error: {e}");
                        break;
                    }
                    None => break, // client disconnected
                }
            }
            upstream_msg = upstream_rx.next() => {
                match upstream_msg {
                    Some(Ok(UpstreamMessage::Text(text))) => {
                        if text.len() > MAX_FRAME_BYTES {
                            log_dropped("upstream frame exceeded MAX_FRAME_BYTES");
                            continue;
                        }
                        match envelope_and_mask(&text) {
                            Some(out) => {
                                if client_tx.send(Message::Text(out)).await.is_err() {
                                    tracing::warn!("constellation::ws: failed forwarding masked event to client");
                                    break;
                                }
                            }
                            None => log_dropped("upstream frame was not valid JSON"),
                        }
                    }
                    Some(Ok(UpstreamMessage::Close(_))) | None => {
                        // Upstream dropped -- bounded reconnect before giving up.
                        tracing::info!(upstream = %upstream_url, "constellation::ws: upstream connection lost, attempting reconnect");
                        match reconnect_upstream(upstream_url, &mut reconnect_count).await {
                            Some(reconnected) => {
                                let (new_tx, new_rx) = reconnected.split();
                                upstream_tx = new_tx;
                                upstream_rx = new_rx;
                                continue;
                            }
                            None => {
                                let _ = client_tx
                                    .send(close_message(CLOSE_CODE_UPSTREAM_LOST, "upstream unreachable"))
                                    .await;
                                break;
                            }
                        }
                    }
                    Some(Ok(_binary_or_control)) => {
                        // Ping/Pong/Frame/Binary from upstream: the relay's
                        // contract is JSON text events only (§3.5) -- drop
                        // with a counter log rather than forwarding an
                        // opaque frame the browser has no envelope for.
                        log_dropped("upstream sent a non-text frame");
                    }
                    Some(Err(e)) => {
                        tracing::warn!(upstream = %upstream_url, "constellation::ws: upstream socket error: {e}, attempting reconnect");
                        match reconnect_upstream(upstream_url, &mut reconnect_count).await {
                            Some(reconnected) => {
                                let (new_tx, new_rx) = reconnected.split();
                                upstream_tx = new_tx;
                                upstream_rx = new_rx;
                                continue;
                            }
                            None => {
                                let _ = client_tx
                                    .send(close_message(CLOSE_CODE_UPSTREAM_LOST, "upstream unreachable"))
                                    .await;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = client_tx.close().await;
    let _ = upstream_tx.close().await;
}

fn frame_len(msg: &Message) -> usize {
    match msg {
        Message::Text(t) => t.len(),
        Message::Binary(b) => b.len(),
        _ => 0,
    }
}

/// Map a browser-leg [`Message`] to the upstream-leg tungstenite message
/// type. `None` for frame kinds this relay doesn't forward upstream
/// (`Ping`/`Pong` -- handled transparently by the WebSocket layers
/// themselves, never meaningful application data to relay).
fn to_upstream_message(msg: Message) -> Option<UpstreamMessage> {
    match msg {
        Message::Text(t) => Some(UpstreamMessage::Text(t)),
        Message::Binary(b) => Some(UpstreamMessage::Binary(b)),
        Message::Close(frame) => Some(UpstreamMessage::Close(frame.map(|f| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(f.code),
                reason: Cow::Owned(f.reason.to_string()),
            }
        }))),
        Message::Ping(_) | Message::Pong(_) => None,
    }
}

fn log_dropped(reason: &str) {
    let total = DROPPED_FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(reason, total_dropped = total, "constellation::ws: dropped a frame");
}

/// Parse `raw` as JSON, wrap it in the `{source:'harmony', event:...}`
/// envelope (§3.5), and pass the WHOLE envelope through
/// `mask::mask_response` before re-serializing -- the masking property must
/// hold on the socket exactly like it does for every `/api/*` HTTP
/// response. Returns `None` when `raw` isn't valid JSON (the caller drops
/// the frame with a counter log rather than forwarding an unparseable
/// payload wrapped in a lie of a JSON envelope).
fn envelope_and_mask(raw: &str) -> Option<String> {
    let event: Value = serde_json::from_str(raw).ok()?;
    let envelope = json!({"source": "harmony", "event": event});
    let masked = mask::mask_response(envelope);
    Some(masked.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use serial_test::serial;
    use tower::ServiceExt;

    fn test_state() -> Arc<McpServerState> {
        Arc::new(McpServerState {
            registry: arc_swap::ArcSwap::from_pointee(crate::registry::ToolRegistry::new()),
            server_name: "constellation-ws-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: crate::mesh::PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
        })
    }

    fn router() -> axum::Router {
        axum::Router::new()
            .route("/ws", axum::routing::get(handle_ws))
            .with_state(test_state())
    }

    /// The load-bearing CONST-18 property: an unauthenticated caller is
    /// rejected `401` BEFORE the WebSocket upgrade handshake is even
    /// attempted -- never a half-completed upgrade, never an upstream
    /// dial. Deliberately sent WITHOUT any `Upgrade`/`Sec-WebSocket-*`
    /// headers: the auth check must reject first regardless, since
    /// `handle_ws` checks the session before ever extracting
    /// `WebSocketUpgrade` from the request.
    #[tokio::test]
    #[serial]
    async fn unauthenticated_upgrade_is_rejected_before_upstream_dial() {
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
        std::env::remove_var("CONSTELLATION_HARMONY_WS_URL");
        let req = HttpRequest::builder().method("GET").uri("/ws").body(Body::empty()).unwrap();
        let resp = router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// A tampered/garbage session cookie must be rejected exactly like no
    /// cookie at all -- `auth::session_from_cookie` already covers this
    /// (verified in `auth.rs`'s own tests); this asserts the `/ws` route
    /// wires that same check in.
    #[tokio::test]
    #[serial]
    async fn garbage_session_cookie_is_rejected() {
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", "test-signing-key-const18");
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/ws")
            .header("cookie", "constellation_session=not-a-real-jwt")
            .body(Body::empty())
            .unwrap();
        let resp = router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    #[test]
    fn no_upstream_close_frame_carries_the_documented_code() {
        let msg = close_message(CLOSE_CODE_NO_UPSTREAM, "no upstream configured");
        match msg {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.code, CLOSE_CODE_NO_UPSTREAM);
                assert_eq!(frame.reason, "no upstream configured");
            }
            _ => panic!("expected a Close frame"),
        }
    }

    #[test]
    fn upstream_lost_close_frame_carries_the_documented_code() {
        let msg = close_message(CLOSE_CODE_UPSTREAM_LOST, "upstream unreachable");
        match msg {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.code, CLOSE_CODE_UPSTREAM_LOST);
            }
            _ => panic!("expected a Close frame"),
        }
    }

    #[test]
    fn envelope_wraps_event_with_harmony_source() {
        let out = envelope_and_mask(r#"{"kind":"engine_start","project":"LUM"}"#).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["source"], "harmony");
        assert_eq!(parsed["event"]["kind"], "engine_start");
        assert_eq!(parsed["event"]["project"], "LUM");
    }

    #[test]
    fn non_json_upstream_frame_is_rejected_not_forwarded() {
        assert!(envelope_and_mask("not json at all").is_none());
        assert!(envelope_and_mask("").is_none());
    }

    /// Mirrors `mask.rs`'s own negative property test: a planted secret in
    /// an upstream event frame must never survive the envelope+mask step --
    /// the masking property extends onto the socket, not just `/api/*`.
    #[test]
    fn negative_property_planted_secret_never_survives_envelope_and_mask() {
        let planted = "<REDACTED-SECRET>"; // pii-test-fixture
        let raw = format!(r#"{{"kind":"provider_update","gitea_token":"{planted}","note":"totally fine"}}"#);
        let out = envelope_and_mask(&raw).unwrap();
        assert!(!out.contains(planted), "planted secret leaked through the ws relay's masking: {out}");
        assert!(out.contains("totally fine"));
        assert!(out.contains("<vault:GITEA_TOKEN>"));
    }

    #[test]
    fn frame_len_reports_text_and_binary_sizes() {
        assert_eq!(frame_len(&Message::Text("abcde".to_string())), 5);
        assert_eq!(frame_len(&Message::Binary(vec![0u8; 10])), 10);
        assert_eq!(frame_len(&Message::Ping(vec![])), 0);
    }

    #[test]
    fn to_upstream_message_forwards_text_and_binary_drops_ping_pong() {
        assert!(matches!(
            to_upstream_message(Message::Text("hi".to_string())),
            Some(UpstreamMessage::Text(t)) if t == "hi"
        ));
        assert!(matches!(
            to_upstream_message(Message::Binary(vec![1, 2, 3])),
            Some(UpstreamMessage::Binary(b)) if b == vec![1, 2, 3]
        ));
        assert!(to_upstream_message(Message::Ping(vec![])).is_none());
        assert!(to_upstream_message(Message::Pong(vec![])).is_none());
    }

    /// The behavioral counterpart to
    /// `no_upstream_close_frame_carries_the_documented_code` (which only
    /// unit-tests `close_message`'s construction): this drives an ACTUAL
    /// authenticated WebSocket upgrade against a real `TcpListener` +
    /// `axum::serve`, with `CONSTELLATION_HARMONY_WS_URL` unset, and asserts
    /// the relay accepts the upgrade (auth already passed) and then emits
    /// the real `CLOSE_CODE_NO_UPSTREAM` typed close frame over the wire --
    /// exercising `handle_ws` -> `relay` end to end, not just the frame
    /// constructor (CONST-18 review finding).
    #[tokio::test]
    #[serial]
    async fn unconfigured_upstream_relay_sends_no_upstream_close_over_a_real_socket() {
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", "test-signing-key-const18-e2e");
        std::env::remove_var("CONSTELLATION_HARMONY_WS_URL");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router().into_make_service()).await;
        });

        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("alice", 300).unwrap();
        let uri: axum::http::Uri = format!("ws://{addr}/ws").parse().unwrap();
        let request = tokio_tungstenite::tungstenite::ClientRequestBuilder::new(uri)
            .with_header("Cookie", format!("constellation_session={token}"));

        let (mut ws_stream, _resp) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .expect("upgrade timed out")
        .expect("upgrade should succeed -- the session cookie is valid, and the relay accepts \
                 the upgrade before it ever looks at CONSTELLATION_HARMONY_WS_URL");

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
            .await
            .expect("timed out waiting for the relay's close frame")
            .expect("stream ended with no message at all")
            .expect("frame-level error reading the close message");

        match msg {
            UpstreamMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), CLOSE_CODE_NO_UPSTREAM);
            }
            other => panic!("expected the relay's typed no-upstream close frame, got {other:?}"),
        }

        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }
}
