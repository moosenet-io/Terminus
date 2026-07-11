//! Generalized upstream MCP client + pool (MESH-02), built on MESH-01's
//! [`crate::mesh::registry::UpstreamRegistry`].
//!
//! terminus-rs's only existing outbound federation client before this item
//! was [`crate::federation::PersonalFederationClient`] — but that client is
//! NOT a generic MCP client: it is a bespoke JSON/REST relay to Chord's
//! `/v1/personal/tools/*` routes, authenticated with a short-lived service
//! JWT Chord's own `validate_jwt` requires (`sub == "lumina"`), returning a
//! `{"result": "<string>"}` / `{"error": "<string>"}` envelope that has
//! nothing to do with MCP's JSON-RPC `tools/list`/`tools/call` shape. See
//! that module's doc comment. There is consequently no existing
//! "initialize / `Mcp-Session-Id` / SSE-frame" CLIENT logic anywhere in this
//! crate to generalize — that handshake exists only on the SERVER side
//! (`crate::mcp_server`, and `terminus_personal`'s own `/mcp` handler) and,
//! separately, in the standalone `terminus-client` workspace crate (which
//! deliberately has no dependency on this crate at all — see its own module
//! docs). This module is therefore new client logic for that same
//! streamable-HTTP MCP wire shape, mirroring the request/response framing
//! `src/mcp_server.rs` already implements server-side (`initialize` ->
//! `Mcp-Session-Id` response header, `event: message\ndata: {...}\n\n` SSE
//! framing) rather than "extracting" it out of a client that never existed.
//!
//! ## Transport selection
//! - [`UpstreamTransport::Bearer`]: a plain `reqwest::Client`; the resolved
//!   [`crate::mesh::registry::ResolvedSecret`] (via
//!   `UpstreamServer::resolve_secret`, i.e. `std::env::var(secret_key)` —
//!   this crate's established secret convention, NOT a `vault::manager()`/
//!   `SecretManager` API this crate does not have) is presented as
//!   `Authorization: Bearer <token>` on every request. The token is held
//!   only as a redacted [`crate::mesh::registry::ResolvedSecret`] and is
//!   never formatted/logged.
//! - [`UpstreamTransport::Mtls`]: reuses the embedded CA
//!   ([`crate::pki::ca`]) this process already runs for its OWN inbound mTLS
//!   listener (TCLI-01/03), minting a short-lived CLIENT leaf cert via the
//!   new [`crate::pki::mtls::issue_client_cert`] (added by this item,
//!   mirroring `issue_server_cert`'s pattern with the clientAuth EKU) and
//!   trusting that same CA as the root for the upstream's presented server
//!   cert — i.e. mesh peers are assumed to share one embedded-CA trust
//!   domain, consistent with mTLS being "the same client-cert model
//!   `crate::pki` already issues for federated Terminus-to-Terminus
//!   traffic" per this item's brief.
//!
//! ## Error split
//! [`UpstreamClientError`] means the TRANSPORT never produced a tool-shaped
//! answer (unreachable, TLS handshake failure, HTTP-level rejection, an
//! unparseable body). [`UpstreamCallResult`] is what a *tool-shaped* answer
//! looks like once one comes back — including a JSON-RPC `"error"` object
//! inside an HTTP 200, which is a TOOL-level failure, not a transport one
//! (see [`UpstreamClient::call_tool`]). This mirrors
//! `crate::federation`'s `FederationError`/`FederationCallResult` split,
//! generalized to any upstream rather than only Chord's personal relay.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use thiserror::Error;

use super::registry::{ResolvedSecret, UpstreamRegistry, UpstreamServer, UpstreamTransport};

/// Path every upstream's streamable-HTTP MCP endpoint is mounted at,
/// matching `crate::mcp_server::build_router`'s `/mcp` route and
/// `terminus_personal`'s own server (see `src/mcp_server.rs`'s module doc).
pub const MCP_PATH: &str = "/mcp";

/// `protocolVersion` this client sends in `initialize` — matching the
/// version `src/mcp_server.rs`'s server-side handshake tests pin.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Per-request timeout. Deliberately generous but bounded — an upstream
/// that never responds must still surface as a clear
/// [`UpstreamClientError::Timeout`] rather than hang a caller indefinitely.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Health-probe timeout — short, since `/healthz` is expected to answer
/// near-instantly; a slow health probe is itself a signal of trouble.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Base backoff delay after a health-probe failure; doubles per consecutive
/// failure up to [`MAX_BACKOFF_SECS`]. Deliberately small starting point —
/// a transient blip should recover within one or two probe cycles.
const BASE_BACKOFF_SECS: u64 = 2;
/// Backoff ceiling — an upstream that's been down a while is re-probed at
/// most this often, so a genuinely dead upstream doesn't get hammered.
const MAX_BACKOFF_SECS: u64 = 120;

/// One tool advertised by an upstream's `tools/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolMeta {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// A tool-shaped answer that DID come back from an upstream — either
/// genuine success or a tool-level failure the upstream itself reported
/// (a JSON-RPC `"error"` object, or `result.isError: true`). Distinct from
/// [`UpstreamClientError`], which means no tool-shaped answer came back at
/// all. See the module doc's "Error split" section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamCallResult {
    pub text: String,
    pub is_error: bool,
}

/// Errors from the TRANSPORT itself never producing a tool-shaped answer —
/// the upstream is unreachable, the handshake failed, the HTTP call itself
/// was rejected, or the response body couldn't be parsed at all. Every
/// variant carries the upstream's `name` (never a secret value) so a pool
/// with several upstreams can attribute a failure to the right one in logs.
#[derive(Debug, Error)]
pub enum UpstreamClientError {
    /// Could not open a connection to the upstream at all (connection
    /// refused, DNS failure, etc).
    #[error("upstream \"{0}\" is unreachable: {1}")]
    Unreachable(String, String),
    /// The request did not complete within [`DEFAULT_TIMEOUT`] (or the
    /// health-probe's shorter [`HEALTH_PROBE_TIMEOUT`]).
    #[error("upstream \"{0}\" timed out: {1}")]
    Timeout(String, String),
    /// Building the client's TLS identity (issuing/parsing the mTLS client
    /// cert, or the upstream's pinned CA) failed — an mTLS-transport-only
    /// failure mode.
    #[error("upstream \"{0}\" mTLS client configuration failed: {1}")]
    TlsConfig(String, String),
    /// The upstream responded with a non-2xx HTTP status to the MCP
    /// envelope itself (distinct from a JSON-RPC `"error"` object inside a
    /// 200, which is a TOOL-level [`UpstreamCallResult`], not this).
    #[error("upstream \"{0}\" rejected the request (HTTP {1}): {2}")]
    Rejected(String, u16, String),
    /// The upstream's response body could not be parsed into the expected
    /// JSON-RPC / MCP shape at all.
    #[error("upstream \"{0}\" returned an unparseable response: {1}")]
    BadResponse(String, String),
    /// This upstream's `secret_key` is configured but not currently
    /// resolvable from the process environment (unset or blank) — a
    /// config/provisioning gap, surfaced so a pool can exclude this
    /// upstream with a clear warning rather than silently sending an
    /// unauthenticated request.
    #[error("upstream \"{0}\" has no usable credential: {1}")]
    SecretUnavailable(String, String),
}

/// A client dialing exactly one registered upstream, over whichever
/// transport [`UpstreamServer::transport`] specifies. Cheap to hold (one
/// `reqwest::Client` + a little session/backoff state); constructed once
/// per upstream by [`UpstreamPool::from_registry`].
pub struct UpstreamClient {
    name: String,
    base_url: String,
    namespace: String,
    transport: UpstreamTransport,
    bearer_token: Option<ResolvedSecret>,
    http: reqwest::Client,
    timeout: Duration,
    /// `Mcp-Session-Id` from this upstream's last successful `initialize`,
    /// if any — reused on subsequent calls, re-established (see
    /// [`UpstreamClient::ensure_session`]) the next time a call is made
    /// after a failure, mirroring the reconnect model
    /// `crate::mcp_server`'s server side expects a well-behaved client to
    /// follow.
    session_id: Mutex<Option<String>>,
}

impl std::fmt::Debug for UpstreamClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately never prints `bearer_token` (redacted regardless —
        // `ResolvedSecret`'s own `Debug` is redacted too, this is
        // belt-and-suspenders) or session material.
        f.debug_struct("UpstreamClient")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("namespace", &self.namespace)
            .field("transport", &self.transport)
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl UpstreamClient {
    /// Build a client for `server`, resolving its transport (mTLS client
    /// identity, or a bearer token) eagerly — cheap for mTLS (in-process
    /// cert issuance against the already-bootstrapped embedded CA), and for
    /// Bearer this is exactly the "resolve now, not per-call" behavior a
    /// pool wants so a missing secret is caught at pool-build time (see the
    /// MESH-02 edge case: "secret key in registry but absent from env ->
    /// upstream disabled with a clear warning, pool continues" —
    /// [`UpstreamPool::from_registry`] is what turns this `Err` into that
    /// warning-and-skip behavior).
    pub fn from_upstream(server: &UpstreamServer) -> Result<Self, UpstreamClientError> {
        let bearer_token = match server.transport {
            UpstreamTransport::Bearer => match server.resolve_secret() {
                None => None,
                Some(Ok(secret)) => Some(secret),
                Some(Err(e)) => {
                    return Err(UpstreamClientError::SecretUnavailable(server.name.clone(), e.to_string()))
                }
            },
            UpstreamTransport::Mtls => None,
        };

        let mut builder = reqwest::Client::builder().timeout(DEFAULT_TIMEOUT);

        if let UpstreamTransport::Mtls = server.transport {
            let ca = crate::pki::ca()
                .map_err(|e| UpstreamClientError::TlsConfig(server.name.clone(), e.to_string()))?;
            let identity_label = format!("terminus-mesh-client:{}", server.name);
            let (cert_pem, key_pem) = crate::pki::mtls::issue_client_cert(ca, &identity_label)
                .map_err(|e| UpstreamClientError::TlsConfig(server.name.clone(), e.to_string()))?;
            let identity_pem = format!("{cert_pem}\n{key_pem}");
            let identity = reqwest::Identity::from_pem(identity_pem.as_bytes())
                .map_err(|e| UpstreamClientError::TlsConfig(server.name.clone(), e.to_string()))?;
            let root_cert = reqwest::Certificate::from_pem(ca.cert_pem().as_bytes())
                .map_err(|e| UpstreamClientError::TlsConfig(server.name.clone(), e.to_string()))?;
            builder = builder
                .identity(identity)
                .add_root_certificate(root_cert)
                // This crate's mTLS trust model is "pin exactly the embedded
                // CA, never the system trust store" (see
                // `crate::pki::mtls::build_client_config`'s server-side
                // analogue in `terminus-client`) -- disable the platform CA
                // roots reqwest would otherwise also trust by default.
                .tls_built_in_root_certs(false);
        }

        let http = builder
            .build()
            .map_err(|e| UpstreamClientError::TlsConfig(server.name.clone(), e.to_string()))?;

        Ok(Self {
            name: server.name.clone(),
            base_url: server.url.trim_end_matches('/').to_string(),
            namespace: server.namespace.clone(),
            transport: server.transport,
            bearer_token,
            http,
            timeout: DEFAULT_TIMEOUT,
            session_id: Mutex::new(None),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// `tools/list` against this upstream, per the module doc's "Error
    /// split" section — this has no tool-level-result concept of its own
    /// (there is no single "tool" being called), so any failure, including
    /// a JSON-RPC `"error"` reply, is a transport-shaped
    /// [`UpstreamClientError`].
    pub async fn list_tools(&self) -> Result<Vec<ToolMeta>, UpstreamClientError> {
        self.ensure_session().await?;
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        let resp = self.post_rpc(body).await?;

        if let Some(err) = resp.get("error") {
            return Err(UpstreamClientError::BadResponse(self.name.clone(), err.to_string()));
        }

        let tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .ok_or_else(|| {
                UpstreamClientError::BadResponse(
                    self.name.clone(),
                    "response missing result.tools array".to_string(),
                )
            })?;

        Ok(tools
            .iter()
            .map(|t| ToolMeta {
                name: t.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string(),
                description: t.get("description").and_then(|d| d.as_str()).map(str::to_string),
                input_schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
            })
            .collect())
    }

    /// `tools/call` against this upstream. A JSON-RPC `"error"` object
    /// inside an HTTP 200 (the upstream reached, ran/rejected the named
    /// tool) is a TOOL-level failure — returned as `Ok(UpstreamCallResult {
    /// is_error: true, .. })`, exactly per the MESH-02 edge case "HTTP 200
    /// but a JSON-RPC error -> tool-level error, not transport death" —
    /// never an `Err`. Only a transport-shaped failure (unreachable,
    /// handshake failure, non-2xx HTTP, unparseable body) is an `Err`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<UpstreamCallResult, UpstreamClientError> {
        self.ensure_session().await?;
        let body = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        let resp = self.post_rpc(body).await?;

        if let Some(err) = resp.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| err.to_string());
            return Ok(UpstreamCallResult { text: message, is_error: true });
        }

        let result = resp.get("result").ok_or_else(|| {
            UpstreamClientError::BadResponse(
                self.name.clone(),
                "response has neither \"result\" nor \"error\"".to_string(),
            )
        })?;

        let is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
        let text = extract_text(result);
        Ok(UpstreamCallResult { text, is_error })
    }

    /// `GET /healthz` — a lightweight liveness probe distinct from the full
    /// `initialize` handshake, used by [`UpstreamPool::health_check_all`].
    /// Never returns an `Err`: a probe failure just means "not healthy right
    /// now", which the pool records rather than propagating as a call
    /// failure.
    pub async fn health_probe(&self) -> bool {
        let url = format!("{}/healthz", self.base_url);
        let mut req = self.http.get(&url).timeout(HEALTH_PROBE_TIMEOUT);
        req = self.attach_auth(req);
        matches!(req.send().await, Ok(resp) if resp.status().is_success())
    }

    /// Ensure this client has a live `Mcp-Session-Id` for this upstream,
    /// (re-)running `initialize` if one isn't already cached. Cheap when a
    /// session is already held (a `Mutex` check, no network call).
    async fn ensure_session(&self) -> Result<(), UpstreamClientError> {
        if self.session_id.lock().expect("session_id mutex poisoned").is_some() {
            return Ok(());
        }
        self.initialize().await
    }

    async fn initialize(&self) -> Result<(), UpstreamClientError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "terminus-mesh-client", "version": env!("CARGO_PKG_VERSION")},
            }
        });
        let resp = self.post_rpc(body).await?;
        if let Some(err) = resp.get("error") {
            return Err(UpstreamClientError::BadResponse(self.name.clone(), err.to_string()));
        }
        // `post_rpc` already captured `Mcp-Session-Id` (if the upstream sent
        // one) into `self.session_id` as a side effect of the response
        // headers. An upstream that never issues a session id (e.g. a
        // minimal/sessionless MCP server) is tolerated -- subsequent calls
        // just omit the header, same as the initial `initialize` did.
        Ok(())
    }

    fn attach_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (&self.transport, &self.bearer_token) {
            (UpstreamTransport::Bearer, Some(secret)) => req.bearer_auth(secret.expose()),
            _ => req,
        }
    }

    /// POST one JSON-RPC envelope to this upstream's `/mcp` endpoint,
    /// attaching auth + the cached session id, parsing either a plain JSON
    /// body or an `event: message\ndata: {...}\n\n` SSE frame (mirroring
    /// `src/mcp_server.rs`'s server-side framing — see that module's tests
    /// for the exact `data:`-line convention this parses), and caching any
    /// `Mcp-Session-Id` response header for subsequent calls.
    async fn post_rpc(&self, body: Value) -> Result<Value, UpstreamClientError> {
        let mut req = self
            .http
            .post(format!("{}{}", self.base_url, MCP_PATH))
            .timeout(self.timeout)
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        req = self.attach_auth(req);
        if let Some(sid) = self.session_id.lock().expect("session_id mutex poisoned").clone() {
            req = req.header("Mcp-Session-Id", sid);
        }

        let resp = req.send().await.map_err(|e| classify_transport_error(&self.name, &e))?;
        let status = resp.status();

        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session_id.lock().expect("session_id mutex poisoned") = Some(sid.to_string());
        }

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(UpstreamClientError::Rejected(self.name.clone(), status.as_u16(), text));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body_text = resp
            .text()
            .await
            .map_err(|e| UpstreamClientError::BadResponse(self.name.clone(), e.to_string()))?;

        let json_text = if content_type.contains("text/event-stream") {
            body_text
                .lines()
                .find(|l| l.starts_with("data:"))
                .map(|l| l.trim_start_matches("data:").trim().to_string())
                .ok_or_else(|| {
                    UpstreamClientError::BadResponse(
                        self.name.clone(),
                        "SSE response had no \"data:\" frame".to_string(),
                    )
                })?
        } else {
            body_text
        };

        serde_json::from_str(&json_text)
            .map_err(|e| UpstreamClientError::BadResponse(self.name.clone(), e.to_string()))
    }
}

/// Best-effort text extraction from an MCP `tools/call` `result` object:
/// prefers `result.content[0].text` (the standard MCP tool-content shape),
/// falling back to the raw JSON of `result` if that shape isn't present, so
/// a nonstandard-but-successful response still surfaces something instead
/// of being silently dropped.
fn extract_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("text"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| result.to_string())
}

fn classify_transport_error(name: &str, e: &reqwest::Error) -> UpstreamClientError {
    if e.is_timeout() {
        UpstreamClientError::Timeout(name.to_string(), e.to_string())
    } else {
        UpstreamClientError::Unreachable(name.to_string(), e.to_string())
    }
}

/// Per-upstream health + backoff state a pool tracks alongside its
/// [`UpstreamClient`]. Deliberately separate from `UpstreamClient` itself so
/// the client stays a plain, cheaply-shared dial primitive and this struct
/// owns the pool-level "is it worth probing again yet" policy.
struct PooledUpstream {
    client: UpstreamClient,
    healthy: std::sync::atomic::AtomicBool,
    consecutive_failures: AtomicU32,
    next_probe_at: Mutex<Instant>,
}

/// The set of [`UpstreamClient`]s built from a registry's enabled entries,
/// each with independent health/backoff tracking. An upstream whose client
/// couldn't even be constructed (e.g. [`UpstreamClientError::SecretUnavailable`]
/// for a Bearer upstream missing its env secret, or
/// [`UpstreamClientError::TlsConfig`] for an mTLS upstream when the embedded
/// CA itself failed to bootstrap) is excluded from the pool entirely, with a
/// `tracing::warn!` — never a hard error that would take down every OTHER
/// upstream too (MESH-02 acceptance: "Per-upstream health probe; unhealthy
/// upstream excluded from merge, not fatal").
pub struct UpstreamPool {
    upstreams: Vec<PooledUpstream>,
}

impl UpstreamPool {
    /// Build a pool from `registry`'s enabled upstreams. Never fails: an
    /// upstream that can't be constructed is logged and skipped rather than
    /// aborting the whole pool build (see the struct doc). A pool built from
    /// an empty/disabled registry is simply empty — not an error, mirroring
    /// [`UpstreamRegistry::empty`]'s "dormant feature, not a
    /// misconfiguration" convention.
    pub fn from_registry(registry: &UpstreamRegistry) -> Self {
        let mut upstreams = Vec::new();
        for server in registry.enabled_upstreams() {
            match UpstreamClient::from_upstream(server) {
                Ok(client) => upstreams.push(PooledUpstream {
                    client,
                    healthy: std::sync::atomic::AtomicBool::new(true),
                    consecutive_failures: AtomicU32::new(0),
                    next_probe_at: Mutex::new(Instant::now()),
                }),
                Err(e) => {
                    tracing::warn!(
                        "mesh: excluding upstream \"{}\" from the pool: {e}",
                        server.name
                    );
                }
            }
        }
        Self { upstreams }
    }

    pub fn len(&self) -> usize {
        self.upstreams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty()
    }

    /// Run `/healthz` against every upstream whose backoff window has
    /// elapsed, updating each one's `healthy` flag and backoff schedule.
    /// Upstreams still within their backoff window are skipped this cycle
    /// (lazy reconnect: no probe traffic sent to an upstream that just
    /// failed, until its delay has elapsed) — MESH-02 APPROACH step 3.
    pub async fn health_check_all(&self) {
        for u in &self.upstreams {
            let due = *u.next_probe_at.lock().expect("next_probe_at mutex poisoned") <= Instant::now();
            if !due {
                continue;
            }
            let ok = u.client.health_probe().await;
            u.healthy.store(ok, Ordering::Relaxed);
            if ok {
                u.consecutive_failures.store(0, Ordering::Relaxed);
                *u.next_probe_at.lock().expect("next_probe_at mutex poisoned") = Instant::now();
            } else {
                let failures = u.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                let delay_secs = BASE_BACKOFF_SECS.saturating_mul(1u64 << failures.min(6)).min(MAX_BACKOFF_SECS);
                *u.next_probe_at.lock().expect("next_probe_at mutex poisoned") =
                    Instant::now() + Duration::from_secs(delay_secs);
            }
        }
    }

    /// Clients currently marked healthy (or not-yet-probed, since a freshly
    /// built [`PooledUpstream`] starts `healthy: true` — optimistic until
    /// the first probe says otherwise) — what a merge step (MESH-03) should
    /// route tool calls to. An unhealthy upstream's tools simply drop out of
    /// this iterator rather than failing the whole merge.
    pub fn healthy_clients(&self) -> impl Iterator<Item = &UpstreamClient> {
        self.upstreams
            .iter()
            .filter(|u| u.healthy.load(Ordering::Relaxed))
            .map(|u| &u.client)
    }

    /// Every client regardless of health, for callers that want to see the
    /// full configured set (e.g. an operator-facing status listing).
    pub fn all_clients(&self) -> impl Iterator<Item = &UpstreamClient> {
        self.upstreams.iter().map(|u| &u.client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use serial_test::serial;

    fn bearer_upstream(base_url: &str, secret_key: &str) -> UpstreamServer {
        let json = format!(
            r#"[{{"name":"mock-bearer","url":"{base_url}","transport":"bearer","namespace":"mockb","secret_key":"{secret_key}"}}]"#
        );
        UpstreamRegistry::from_json(&json).expect("valid json").all()[0].clone()
    }

    fn initialize_response() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-upstream", "version": "0.0.0"}
        }})
    }

    // ── Bearer transport: passthrough tools/list + tools/call ──────────────

    #[tokio::test]
    #[serial]
    async fn bearer_upstream_list_and_call_tool_passthrough() {
        std::env::set_var("MESH_TEST_BEARER_TOKEN", "fixture-bearer-token"); // pii-test-fixture
        let server = MockServer::start();

        let init_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/mcp")
                .json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200)
                .header("Mcp-Session-Id", "session-abc")
                .json_body(initialize_response());
        });
        let list_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/mcp")
                .header("Mcp-Session-Id", "session-abc")
                .json_body_partial(r#"{"method": "tools/list"}"#);
            then.status(200).json_body(json!({
                "jsonrpc": "2.0", "id": 2,
                "result": {"tools": [
                    {"name": "echo", "description": "echoes input", "inputSchema": {"type": "object"}}
                ]}
            }));
        });
        let call_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/mcp")
                .header("Mcp-Session-Id", "session-abc")
                .json_body_partial(r#"{"method": "tools/call"}"#);
            then.status(200).json_body(json!({
                "jsonrpc": "2.0", "id": 3,
                "result": {"content": [{"type": "text", "text": "echo: hi"}], "isError": false}
            }));
        });

        let upstream = bearer_upstream(&server.base_url(), "MESH_TEST_BEARER_TOKEN");
        let client = UpstreamClient::from_upstream(&upstream).expect("client should build");

        let tools = client.list_tools().await.expect("list_tools should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let outcome = client
            .call_tool("echo", json!({"text": "hi"}))
            .await
            .expect("call_tool should succeed");
        assert_eq!(outcome.text, "echo: hi");
        assert!(!outcome.is_error);

        init_mock.assert();
        list_mock.assert();
        call_mock.assert();
        std::env::remove_var("MESH_TEST_BEARER_TOKEN");
    }

    #[tokio::test]
    #[serial]
    async fn bearer_token_is_resolved_from_env_and_presented_as_bearer_auth() {
        std::env::set_var("MESH_TEST_BEARER_TOKEN", "fixture-bearer-token"); // pii-test-fixture
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200).header("Mcp-Session-Id", "s1").json_body(initialize_response());
        });
        let call_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/mcp")
                .header("authorization", "Bearer fixture-bearer-token")
                .json_body_partial(r#"{"method": "tools/list"}"#);
            then.status(200).json_body(json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": []}}));
        });

        let upstream = bearer_upstream(&server.base_url(), "MESH_TEST_BEARER_TOKEN");
        let client = UpstreamClient::from_upstream(&upstream).expect("client should build");
        client.list_tools().await.expect("list_tools should succeed");

        call_mock.assert();
        std::env::remove_var("MESH_TEST_BEARER_TOKEN");
    }

    #[test]
    #[serial]
    fn bearer_debug_never_prints_the_resolved_token() {
        std::env::set_var("MESH_TEST_BEARER_TOKEN", "fixture-bearer-token"); // pii-test-fixture
        let upstream = bearer_upstream("https://mock.example.test", "MESH_TEST_BEARER_TOKEN");
        let client = UpstreamClient::from_upstream(&upstream).expect("client should build");
        let debug_output = format!("{client:?}");
        assert!(!debug_output.contains("fixture-bearer-token"));
        std::env::remove_var("MESH_TEST_BEARER_TOKEN");
    }

    #[test]
    #[serial]
    fn from_upstream_fails_clearly_when_bearer_secret_key_unset() {
        std::env::remove_var("MESH_TEST_BEARER_TOKEN_MISSING");
        let upstream = bearer_upstream("https://mock.example.test", "MESH_TEST_BEARER_TOKEN_MISSING");
        let err = UpstreamClient::from_upstream(&upstream)
            .expect_err("missing bearer secret must fail client construction");
        assert!(matches!(err, UpstreamClientError::SecretUnavailable(_, _)));
    }

    // ── HTTP 200 with a JSON-RPC error is a tool-level result, not a transport Err ─

    #[tokio::test]
    #[serial]
    async fn call_tool_json_rpc_error_in_200_is_tool_level_not_transport_error() {
        std::env::set_var("MESH_TEST_BEARER_TOKEN", "fixture-bearer-token"); // pii-test-fixture
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200).header("Mcp-Session-Id", "s1").json_body(initialize_response());
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/mcp").json_body_partial(r#"{"method": "tools/call"}"#);
            then.status(200).json_body(json!({
                "jsonrpc": "2.0", "id": 3,
                "error": {"code": -32601, "message": "tool not found: bogus"}
            }));
        });

        let upstream = bearer_upstream(&server.base_url(), "MESH_TEST_BEARER_TOKEN");
        let client = UpstreamClient::from_upstream(&upstream).expect("client should build");
        let outcome = client
            .call_tool("bogus", json!({}))
            .await
            .expect("a 200 with a JSON-RPC error must be Ok(is_error: true), not Err");
        assert!(outcome.is_error);
        assert!(outcome.text.contains("bogus"));
        std::env::remove_var("MESH_TEST_BEARER_TOKEN");
    }

    // ── Down upstream: list_tools errors cleanly, no panic ──────────────────

    #[tokio::test]
    #[serial]
    async fn list_tools_on_unreachable_upstream_errors_cleanly_no_panic() {
        std::env::set_var("MESH_TEST_BEARER_TOKEN", "fixture-bearer-token"); // pii-test-fixture
        let upstream = bearer_upstream("http://127.0.0.1:1", "MESH_TEST_BEARER_TOKEN");
        let client = UpstreamClient::from_upstream(&upstream).expect("client should build");
        let err = client.list_tools().await.expect_err("unreachable upstream must error, not panic");
        assert!(matches!(err, UpstreamClientError::Unreachable(_, _) | UpstreamClientError::Timeout(_, _)));
        std::env::remove_var("MESH_TEST_BEARER_TOKEN");
    }

    #[tokio::test]
    #[serial]
    async fn health_probe_false_on_unreachable_upstream() {
        std::env::set_var("MESH_TEST_BEARER_TOKEN", "fixture-bearer-token"); // pii-test-fixture
        let upstream = bearer_upstream("http://127.0.0.1:1", "MESH_TEST_BEARER_TOKEN");
        let client = UpstreamClient::from_upstream(&upstream).expect("client should build");
        assert!(!client.health_probe().await);
        std::env::remove_var("MESH_TEST_BEARER_TOKEN");
    }

    // ── Pool: excludes unbuildable upstreams, doesn't fail the whole pool ──

    #[test]
    #[serial]
    fn pool_excludes_upstream_with_missing_secret_but_keeps_others() {
        std::env::remove_var("MESH_TEST_BEARER_TOKEN_MISSING");
        std::env::set_var("MESH_TEST_BEARER_TOKEN_OK", "fixture-token"); // pii-test-fixture
        let json = r#"[
            {"name": "good", "url": "https://good.example.test", "transport": "bearer", "namespace": "good", "secret_key": "MESH_TEST_BEARER_TOKEN_OK"},
            {"name": "bad", "url": "https://bad.example.test", "transport": "bearer", "namespace": "bad", "secret_key": "MESH_TEST_BEARER_TOKEN_MISSING"}
        ]"#;
        let registry = UpstreamRegistry::from_json(json).expect("valid json");
        let pool = UpstreamPool::from_registry(&registry);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.all_clients().next().unwrap().name(), "good");
        std::env::remove_var("MESH_TEST_BEARER_TOKEN_OK");
    }

    #[test]
    fn pool_from_empty_registry_is_empty_not_an_error() {
        let pool = UpstreamPool::from_registry(&UpstreamRegistry::empty());
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
    }

    #[tokio::test]
    async fn pool_health_check_marks_unreachable_upstream_unhealthy() {
        std::env::set_var("MESH_TEST_BEARER_TOKEN_HC", "fixture-token"); // pii-test-fixture
        let json = r#"[{"name": "down", "url": "http://127.0.0.1:1", "transport": "bearer", "namespace": "downn", "secret_key": "MESH_TEST_BEARER_TOKEN_HC"}]"#;
        let registry = UpstreamRegistry::from_json(json).expect("valid json");
        let pool = UpstreamPool::from_registry(&registry);
        assert_eq!(pool.healthy_clients().count(), 1, "starts optimistically healthy");

        pool.health_check_all().await;
        assert_eq!(pool.healthy_clients().count(), 0, "unreachable upstream excluded after a probe");
        assert_eq!(pool.all_clients().count(), 1, "still present in the full listing, just unhealthy");
        std::env::remove_var("MESH_TEST_BEARER_TOKEN_HC");
    }

    // ── mTLS transport: client builds, dials, and gets rejected by a plain (non-mTLS) mock ──

    #[tokio::test]
    #[serial]
    async fn mtls_upstream_client_builds_against_embedded_ca_and_fails_cleanly_when_dead() {
        // A full mTLS handshake against a real peer terminus is exercised by
        // `crate::pki::mtls`'s own server-side tests and `terminus-client`'s
        // `transport` tests; this test's scope is narrower and specific to
        // this item: an Mtls-transport `UpstreamClient` builds successfully
        // -- which means it issued itself a client leaf cert against the
        // embedded CA and pinned that CA as its only trust root (this item's
        // APPROACH step 2) -- and a dial to a dead endpoint surfaces a clean
        // transport error rather than panicking. (A dial against a plain-HTTP
        // endpoint would NOT exercise TLS at all -- reqwest only performs the
        // handshake for an `https://` URL -- so an unreachable `https://`
        // target is what actually drives the mTLS-configured connector here.)
        std::env::remove_var("TERMINUS_CA_CERT");
        std::env::remove_var("TERMINUS_CA_KEY");
        let json =
            r#"[{"name":"mock-mtls","url":"https://127.0.0.1:1","transport":"mtls","namespace":"mockm"}]"#;
        let registry = UpstreamRegistry::from_json(json).expect("valid json");
        let upstream = &registry.all()[0];

        let client = UpstreamClient::from_upstream(upstream)
            .expect("mTLS client should build against the embedded CA");
        let err = client
            .list_tools()
            .await
            .expect_err("a dead mTLS endpoint must fail cleanly, not panic");
        assert!(matches!(
            err,
            UpstreamClientError::Unreachable(_, _) | UpstreamClientError::Timeout(_, _)
        ));
    }
}
