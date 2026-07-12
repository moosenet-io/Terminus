//! Shared mTLS/enroll server-setup, extracted (TGW-01 — Terminus Primary
//! Gateway sprint) from what was previously inlined in
//! `src/bin/terminus_personal.rs::main()`.
//!
//! Both `terminus_personal` (the personal-registry deployment) and the
//! new `terminus_primary` (the aggregated-core-registry gateway, see
//! `src/bin/terminus_primary.rs`) need the exact same server-setup sequence:
//! build the MCP `/mcp` router for a given [`crate::registry::ToolRegistry`],
//! merge in the `/enroll` router (TCLI-02), and spawn the mTLS listener
//! (TCLI-03) as a background task serving that same router. Before this
//! item, that sequence was written once, inline, inside
//! `terminus_personal`'s `main()`. This module is the single place that
//! sequence lives now — both binaries call it, instead of a second inline
//! copy diverging over time.
//!
//! ## What moved here (byte-for-byte behavior, only the container changed)
//! - Building `McpServerState` + `mcp_server::build_router(..)` merged with
//!   `pki::enroll::build_enroll_router()` — see [`build_gateway_router`].
//! - The mTLS listener bootstrap: CA load-or-generate (`crate::pki::ca()`),
//!   server leaf cert issuance (`pki::mtls::issue_server_cert`), TLS config
//!   build (`pki::mtls::build_server_config`), then
//!   `pki::mtls::run_listener` — spawned as a background `tokio::spawn` task
//!   so a bootstrap failure disables ONLY the mTLS listener, never the
//!   caller's own plain listener. See [`spawn_mtls_listener`].
//!
//! ## What deliberately did NOT move
//! Binding + serving the PLAIN HTTP+JWT listener
//! (`tokio::net::TcpListener::bind` + `axum::serve`) stays in each binary's
//! own `main()` — `terminus_personal` and `terminus_primary` use different
//! env-derived bind addr/port/config for that listener (`TERMINUS_PERSONAL_*`
//! vs `TERMINUS_PRIMARY_*`), and inlining it here would buy nothing beyond
//! what's already a two-line call in each `main()`.

use std::sync::Arc;

use crate::mcp_server::{build_router, McpServerState};
use crate::pki::enroll::build_enroll_router;
use crate::registry::ToolRegistry;

/// Non-secret config a caller supplies to [`build_gateway_router`] /
/// [`spawn_mtls_listener`]. `server_name`/`server_version` describe the
/// `/mcp` `initialize` response identity; the `mtls_*` fields configure the
/// background mTLS listener. PKI material itself (CA, server cert/key) is
/// never carried in this struct — it's resolved at bootstrap time from
/// `crate::pki::ca()` / `crate::pki::mtls`, per those modules' own
/// load-or-generate precedence.
#[derive(Clone)]
pub struct GatewayServerConfig {
    pub server_name: String,
    pub server_version: String,
    /// Optional bearer token the plain `/mcp` listener requires (unchanged
    /// meaning from the pre-extraction `terminus_personal` behavior — this
    /// struct only carries it so `McpServerState` construction is shared;
    /// the plain listener itself is still bound/served by each binary).
    pub auth_token: Option<String>,
    pub mtls_bind: String,
    pub mtls_port: u16,
    pub mtls_server_identity: String,
    /// TGW-02: when `Some`, a tool name not found in the local registry is
    /// proxied to Chord's `/v1/personal/tools/call` relay and the personal
    /// tool set is included in `tools/list` — see
    /// `crate::mcp_server::McpServerState::personal_federation`'s doc.
    /// `terminus_personal` passes `None` (it never needs to federate to
    /// itself); `terminus_primary` (TGW-01/02) passes
    /// `Some(crate::federation::PersonalFederationClient::from_env())`.
    pub personal_federation: Option<crate::federation::PersonalFederationClient>,
    /// TGW-03: when `Some`, `/v1/chat/completions`, `/v1/infer`,
    /// `/v1/agent/execute`, and `/v1/coding/select` are mounted on the
    /// router and forwarded to Chord's co-located inference backend — see
    /// `crate::inference_proxy`'s module doc for the full contract.
    /// `terminus_personal` passes `None` (it has no inference-proxy role);
    /// `terminus_primary` (TGW-03) passes
    /// `Some(crate::inference_proxy::InferenceProxyClient::from_env())`.
    pub inference_proxy: Option<crate::inference_proxy::InferenceProxyClient>,
    /// TGW-04: when `Some`, every request path this router serves (tool
    /// calls and the inference-proxy routes) is gated by the shared
    /// identity → allowlist → rate-limit → dispatch → audit pipeline — see
    /// `crate::gateway_framework`'s module doc for the full contract.
    /// `terminus_personal` passes `None` (predates this item, not this
    /// spec's deployment target); `terminus_primary` (TGW-04) passes
    /// `Some(crate::gateway_framework::GatewayFramework::from_env())`.
    pub gateway: Option<crate::gateway_framework::GatewayFramework>,
    /// MESH-15: when `Some`, this pool is installed as
    /// `McpServerState::mesh_pool` so `tools/list`/`tools/call` federate to
    /// its enabled/healthy upstreams (MESH-03/08). `terminus_personal`
    /// passes `None` (mesh federation is a gateway-only concern);
    /// `terminus_primary` (MESH-15) passes
    /// `Some(Arc::new(UpstreamPool::from_registry(&registry)))`, built from
    /// `TERMINUS_MESH_ENABLED`/`TERMINUS_MESH_UPSTREAMS_JSON` at startup
    /// when the feature is enabled -- `None` (this field's default posture)
    /// when it isn't, byte-for-byte the pre-MESH-15 behavior.
    pub mesh_pool: Option<Arc<crate::mesh::UpstreamPool>>,
}

/// Manual `Debug` (rather than `#[derive(Debug)]` on the struct): every
/// other field here is `Debug` for free, but [`crate::mesh::UpstreamPool`]
/// deliberately isn't (it holds live client state, not a value meant to be
/// dumped) -- so `mesh_pool` is rendered as presence + upstream count only,
/// same "don't print internals, print a safe summary" posture the mesh
/// module uses for anything credential-adjacent elsewhere.
impl std::fmt::Debug for GatewayServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayServerConfig")
            .field("server_name", &self.server_name)
            .field("server_version", &self.server_version)
            .field("auth_token", &self.auth_token.as_ref().map(|_| "<redacted>"))
            .field("mtls_bind", &self.mtls_bind)
            .field("mtls_port", &self.mtls_port)
            .field("mtls_server_identity", &self.mtls_server_identity)
            .field("personal_federation", &self.personal_federation.is_some())
            .field("inference_proxy", &self.inference_proxy.is_some())
            .field("gateway", &self.gateway.is_some())
            .field("mesh_pool_upstreams", &self.mesh_pool.as_ref().map(|p| p.len()))
            .finish()
    }
}

/// Build the shared MCP (`/mcp`, `/healthz`) + `/enroll` router for
/// `registry`, using `config`'s server name/version/auth-token. Identical to
/// what `terminus_personal`'s `main()` did inline before this extraction —
/// `McpServerState` field-for-field, `build_router(..).merge(build_enroll_router())`
/// unchanged. Callers (both binaries) still bind+serve this router's plain
/// HTTP+JWT listener themselves, and also pass it to
/// [`spawn_mtls_listener`] for the second, mTLS-fronted listener.
pub fn build_gateway_router(registry: ToolRegistry, config: &GatewayServerConfig) -> axum::Router {
    let state = Arc::new(McpServerState {
        // TMOD-01: the registry is now a hot-swappable snapshot container --
        // `from_pointee` is the ArcSwap-provided equivalent of
        // `ArcSwap::new(Arc::new(registry))`, seeding the initial snapshot
        // with exactly the registry the caller built (`register_all`/
        // `register_personal`), unchanged.
        registry: arc_swap::ArcSwap::from_pointee(registry),
        server_name: config.server_name.clone(),
        server_version: config.server_version.clone(),
        auth_token: config.auth_token.clone(),
        personal_federation: config.personal_federation.clone(),
        inference_proxy: config.inference_proxy.clone(),
        gateway: config.gateway.clone(),
        // MESH-15: pass through whatever the caller provisioned --
        // `terminus_primary`'s `main()` builds a real pool from env when
        // `TERMINUS_MESH_ENABLED` is set; every other caller (incl.
        // `terminus_personal`, and any config that leaves this `None`) keeps
        // the same additive "feature not configured" posture
        // `personal_federation` etc. use, preserving `tools/list`/
        // `tools/call` byte-for-byte when mesh isn't configured.
        mesh_pool: config.mesh_pool.clone(),
        // MESH-07: build the resolver from `TERMINUS_MESH_PRINCIPAL_MAP_JSON`
        // once at process construction. Malformed JSON is a loud, logged
        // config error (not a startup panic -- a router-building library
        // function is not the right place to abort a whole binary's
        // `main()`) that degrades to an unconfigured (default) resolver, so
        // `crate::mcp_server::resolve_principal`'s legacy-passthrough path
        // still applies rather than mass-denying every caller over a config
        // typo -- see `crate::mesh::principal`'s module doc for why
        // `PrincipalResolver::from_env` itself returns a hard `Err` on
        // malformed JSON (a config typo should be loud), and
        // `AllowlistPolicy::from_env`'s doc for the precedent this mirrors
        // (a startup config error degrades to a safe default policy, not a
        // crashed process).
        principal_resolver: crate::mesh::PrincipalResolver::from_env().unwrap_or_else(|e| {
            tracing::error!(
                "mesh: TERMINUS_MESH_PRINCIPAL_MAP_JSON is invalid ({e}) -- falling back to an \
                 unconfigured principal resolver (legacy cert-CN-as-name passthrough) until fixed"
            );
            crate::mesh::PrincipalResolver::default()
        }),
    });

    build_router(state).merge(build_enroll_router())
}

/// Spawn the mTLS listener (TCLI-03 machinery) as a background task serving
/// `router`, using `config`'s mTLS bind/port/identity. Extracted verbatim
/// from `terminus_personal`'s former inline `main()` sequence: CA
/// load-or-generate (`crate::pki::ca()`), server cert issuance
/// (`pki::mtls::issue_server_cert`), TLS config build
/// (`pki::mtls::build_server_config`), then `pki::mtls::run_listener`. A
/// bootstrap failure at any step is logged and disables ONLY the mTLS
/// listener — the caller's own plain listener (bound/served separately) is
/// unaffected, matching pre-extraction behavior exactly.
///
/// Each caller (each binary) provisions its OWN CA material independently —
/// `crate::pki::ca()`'s load-or-generate precedence reads
/// `TERMINUS_CA_CERT`/`TERMINUS_CA_KEY` from that process's own environment
/// (or its own local store file), so two binaries on two different hosts
/// with independently-provisioned CA material naturally get independent
/// CAs with no code branching required here — this is how the
/// `terminus-primary` deployment gets its own auto-generated CA, separate
/// from `terminus_personal`'s own material (see the TGW-01 spec item's
/// design-decision #3).
pub fn spawn_mtls_listener(router: axum::Router, config: &GatewayServerConfig) {
    let mtls_router = router.clone();
    let mtls_bind = config.mtls_bind.clone();
    let mtls_port = config.mtls_port;
    let server_identity = config.mtls_server_identity.clone();
    let server_label = config.server_name.clone();

    tokio::spawn(async move {
        let ca = match crate::pki::ca() {
            Ok(ca) => ca,
            Err(e) => {
                tracing::error!(
                    "{server_label}: mTLS listener disabled -- CA bootstrap failed: {e}"
                );
                return;
            }
        };
        let (server_cert_pem, server_key_pem) =
            match crate::pki::mtls::issue_server_cert(ca, &server_identity) {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::error!(
                        "{server_label}: mTLS listener disabled -- server cert issuance failed: {e}"
                    );
                    return;
                }
            };
        let tls_config = match crate::pki::mtls::build_server_config(
            ca.cert_pem(),
            &server_cert_pem,
            &server_key_pem,
        ) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::error!(
                    "{server_label}: mTLS listener disabled -- TLS config build failed: {e}"
                );
                return;
            }
        };

        tracing::info!(
            "{server_label}: starting mTLS listener on {mtls_bind}:{mtls_port} (identity={server_identity})"
        );
        if let Err(e) =
            crate::pki::mtls::run_listener(&mtls_bind, mtls_port, tls_config, mtls_router).await
        {
            tracing::error!("{server_label}: mTLS listener stopped: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolRegistry;
    use rustls::pki_types::ServerName;
    use serial_test::serial;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;

    fn test_config(mtls_port: u16) -> GatewayServerConfig {
        GatewayServerConfig {
            server_name: "terminus-server-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            mtls_bind: "127.0.0.1".to_string(),
            mtls_port,
            mtls_server_identity: "terminus-server-test".to_string(),
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
        }
    }

    /// `build_gateway_router` is independently unit-testable: it must not
    /// panic and must produce a router that serves both `/healthz` (the
    /// plain MCP router) and `/enroll` (TCLI-02), proving the merge happened
    /// -- without needing either binary or a live mTLS handshake.
    #[tokio::test]
    async fn build_gateway_router_merges_mcp_and_enroll_routes() {
        let registry = ToolRegistry::new();
        let config = test_config(0);
        let router = build_gateway_router(registry, &config);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        let client = reqwest::Client::new();
        let health = client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .expect("healthz request should succeed");
        assert!(health.status().is_success(), "expected /healthz to be reachable via the merged router");

        // /enroll is mounted (even if it 400s/503s without a body/secrets --
        // the point is the ROUTE exists, proving build_enroll_router() was
        // actually merged in, not that enrollment succeeds here).
        let enroll = client
            .post(format!("http://{addr}/enroll"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("enroll request should reach the server");
        assert_ne!(
            enroll.status(),
            reqwest::StatusCode::NOT_FOUND,
            "/enroll must be mounted on the router build_gateway_router returns"
        );
    }

    /// `spawn_mtls_listener` reused end-to-end with a fresh registry
    /// (mirroring the TCLI-03 test pattern, now exercised through the
    /// shared helper both binaries call rather than only inline in
    /// `terminus_personal`): enroll against the plain router, then complete
    /// a real mTLS handshake against the listener this function spawns.
    #[tokio::test]
    #[serial]
    async fn spawn_mtls_listener_accepts_a_tcli02_issued_client_cert() {
        std::env::remove_var("TERMINUS_CA_CERT");
        std::env::remove_var("TERMINUS_CA_KEY");
        std::env::set_var(
            "TERMINUS_CA_STORE_PATH",
            std::env::temp_dir()
                .join(format!("terminus-pki-server-test-{}.json", std::process::id()))
                .to_string_lossy()
                .into_owned(),
        );
        std::env::set_var("TERMINUS_ENROLLMENT_SHARED_SECRET", "test-shared-secret");
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", "test-jwt-signing-key");

        let registry = ToolRegistry::new();
        let mtls_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback for port reservation");
        let mtls_addr = mtls_listener.local_addr().expect("local addr");
        drop(mtls_listener); // free the port for spawn_mtls_listener to rebind

        let config = test_config(mtls_addr.port());
        let router = build_gateway_router(registry, &config);
        spawn_mtls_listener(router.clone(), &config);

        // Enroll a client against the plain router's /enroll (TCLI-02),
        // reusing the same identity/CA the mTLS listener just bootstrapped.
        let enroll_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind enroll listener");
        let enroll_addr = enroll_listener.local_addr().expect("enroll addr");
        let enroll_router = router.clone();
        tokio::spawn(async move {
            axum::serve(enroll_listener, enroll_router).await.ok();
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{enroll_addr}/enroll"))
            .json(&serde_json::json!({
                "identity": "server-test-client",
                "shared_secret": "test-shared-secret"
            }))
            .send()
            .await
            .expect("enroll request should succeed");
        assert!(resp.status().is_success(), "enrollment should succeed: {:?}", resp.status());
        let body: serde_json::Value = resp.json().await.expect("enroll response JSON");
        let cert_pem = body["cert_pem"].as_str().expect("cert_pem present").to_string();
        let key_pem = body["key_pem"].as_str().expect("key_pem present").to_string();

        // Give the spawned mTLS listener a moment to actually bind before
        // connecting -- it's spawned as a background task above.
        let ca = crate::pki::ca().expect("CA should be loadable after enrollment");
        let mut roots = rustls::RootCertStore::empty();
        for der in rustls_pemfile::certs(&mut ca.cert_pem().as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .expect("parse CA cert")
        {
            roots.add(der).expect("add CA root");
        }
        let client_certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .expect("parse client cert");
        let client_key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
            .expect("parse client key")
            .expect("client key present");

        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(client_certs, client_key)
            .expect("client config with client auth cert");
        let connector = TlsConnector::from(std::sync::Arc::new(client_config));

        let mut attempts = 0;
        let tcp = loop {
            match tokio::net::TcpStream::connect(mtls_addr).await {
                Ok(tcp) => break tcp,
                Err(_) if attempts < 50 => {
                    attempts += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => panic!("mTLS listener never came up: {e}"),
            }
        };
        let server_name = ServerName::try_from(config.mtls_server_identity.clone())
            .expect("valid server name")
            .to_owned();
        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .expect("mTLS handshake against spawn_mtls_listener's listener should succeed");

        // The connection reaches the shared router -- confirm by hitting
        // /healthz over the mTLS-terminated connection.
        let req = b"GET /healthz HTTP/1.1\r\nHost: terminus-server-test\r\nConnection: close\r\n\r\n";
        tls.write_all(req).await.expect("write request over mTLS");
        let mut buf = Vec::new();
        tls.read_to_end(&mut buf).await.expect("read response over mTLS");
        let response = String::from_utf8_lossy(&buf);
        assert!(
            response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"),
            "expected a 200 from /healthz over the mTLS listener spawn_mtls_listener started, got: {response}"
        );

        std::env::remove_var("TERMINUS_ENROLLMENT_SHARED_SECRET");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
        std::env::remove_var("TERMINUS_CA_STORE_PATH");
    }
}
