//! T0 — mTLS over TCP, for an off-box worker (TMOD-02).
//!
//! Used when a worker is not reachable via a shared filesystem for a UDS at
//! all (a different host). Cryptographically authenticated the same way as
//! T2 — mutual TLS against the embedded CA ([`crate::pki::ca`]), the peer
//! leaf certificate's Subject CN checked against the worker's configured
//! identity, fail-closed on mismatch — but with no kernel `SO_PEERCRED`
//! signal available (TCP has no such concept), and network-exposed rather
//! than kernel-local. See [`super::TransportTier::security_rank`] for why
//! this ranks between T1 and T2, not equal to either — reused directly by
//! [`super::MinTierPolicy`], which floors `write_scoped`/`secret_holding`
//! workers at T2 specifically, not merely "any mTLS tier".
//!
//! Reuses [`crate::mesh::identity`]'s conceptual model (a peer identity
//! resolved from the transport, checked against configuration) in spirit,
//! though the concrete identity source here is the TLS leaf CN (mirroring
//! [`crate::pki::mtls::ClientIdentity`]'s shape) rather than
//! [`crate::mesh::identity::TailnetIdentity`] (which is tailnet-WhoIs-scoped
//! and not applicable to a plain mTLS/TCP dial).

use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::error::ToolError;
use crate::pki::ca::CertificateAuthority;
use crate::tool::ToolOutput;

use super::{call_over, health_over, list_over, TransportError, WorkerTransport};

/// A T0 mTLS-over-TCP transport to one off-box worker.
pub struct MtlsTcpTransport {
    host: String,
    port: u16,
    /// Subject CN the worker's presented TLS leaf certificate must carry.
    expected_identity: String,
    ca_cert_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

impl MtlsTcpTransport {
    /// Build a transport for an off-box worker at `host:port`, minting this
    /// process's own short-lived client leaf cert against `ca` (same pattern
    /// as [`super::uds_mtls::UdsMtlsTransport::new`] /
    /// `crate::mesh::client::UpstreamClient::from_upstream`'s mTLS-transport
    /// construction).
    pub fn new(
        host: impl Into<String>,
        port: u16,
        expected_identity: impl Into<String>,
        ca: &CertificateAuthority,
        client_identity_label: &str,
    ) -> Result<Self, TransportError> {
        let (client_cert_pem, client_key_pem) =
            crate::pki::mtls::issue_client_cert(ca, client_identity_label)
                .map_err(|e| TransportError::Protocol(format!("issuing client cert: {e}")))?;
        Ok(Self {
            host: host.into(),
            port,
            expected_identity: expected_identity.into(),
            ca_cert_pem: ca.cert_pem().to_string(),
            client_cert_pem,
            client_key_pem,
        })
    }

    fn build_client_config(&self) -> Result<rustls::ClientConfig, TransportError> {
        let mut roots = RootCertStore::empty();
        for der in pem_to_der_certs(&self.ca_cert_pem)? {
            roots
                .add(der)
                .map_err(|e| TransportError::Protocol(format!("adding CA root: {e}")))?;
        }
        let client_certs = pem_to_der_certs(&self.client_cert_pem)?;
        let client_key = pem_to_der_key(&self.client_key_pem)?;

        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(client_certs, client_key)
            .map_err(|e| TransportError::Protocol(format!("building TLS client config: {e}")))
    }

    /// Dial the TCP endpoint, perform the mTLS handshake, then check the
    /// peer leaf cert's CN — fail-closed before any tool request is ever
    /// written to the stream. No `SO_PEERCRED` equivalent exists for TCP, so
    /// the certificate CN is the ONLY identity signal at this tier.
    async fn dial_verified(&self) -> Result<tokio_rustls::client::TlsStream<TcpStream>, TransportError> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| TransportError::Unavailable(format!("connecting to {}:{}: {e}", self.host, self.port)))?;

        let config = self.build_client_config()?;
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from(self.expected_identity.clone())
            .map_err(|e| TransportError::Protocol(format!("invalid worker identity as SNI: {e}")))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| TransportError::Unavailable(format!("mTLS handshake failed: {e}")))?;

        let (_, session) = tls.get_ref();
        let leaf = session
            .peer_certificates()
            .and_then(|certs| certs.first())
            .ok_or_else(|| TransportError::IdentityMismatch("worker presented no certificate".to_string()))?;
        let cn = super::uds_mtls::extract_cn_only(leaf)
            .ok_or_else(|| TransportError::IdentityMismatch("worker certificate has no Subject CN".to_string()))?;
        if cn != self.expected_identity {
            return Err(TransportError::IdentityMismatch(format!(
                "worker certificate CN \"{cn}\" does not match configured worker identity \"{}\"",
                self.expected_identity
            )));
        }

        Ok(tls)
    }
}

#[async_trait::async_trait]
impl WorkerTransport for MtlsTcpTransport {
    async fn connect(&self) -> Result<(), TransportError> {
        self.dial_verified().await.map(|_| ())
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        let stream = self
            .dial_verified()
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        call_over(stream, name, args).await
    }

    async fn list(&self) -> Result<Vec<String>, TransportError> {
        let stream = self.dial_verified().await?;
        list_over(stream).await
    }

    async fn health(&self) -> bool {
        match self.dial_verified().await {
            Ok(stream) => health_over(stream).await,
            Err(_) => false,
        }
    }
}

fn pem_to_der_certs(
    pem: &str,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, TransportError> {
    rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TransportError::Protocol(format!("parsing PEM certificate(s): {e}")))
}

fn pem_to_der_key(pem: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>, TransportError> {
    rustls_pemfile::private_key(&mut pem.as_bytes())
        .map_err(|e| TransportError::Protocol(format!("parsing PEM private key: {e}")))?
        .ok_or_else(|| TransportError::Protocol("no private key found in PEM".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::mtls::{build_server_config, issue_server_cert};
    use serial_test::serial;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    /// Spawn a TLS-over-TCP "worker" bound to loopback (a fixture-only use of
    /// 127.0.0.1, not a real infra host — mirrors this crate's other
    /// loopback test fixtures, e.g. `crate::pki::mtls`'s own integration
    /// tests): server-authenticates as `server_identity`, requires a client
    /// cert chained to the same CA, answers every request with a fixed
    /// canned response.
    async fn spawn_tls_worker(
        ca: &CertificateAuthority,
        server_identity: &str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let (server_cert_pem, server_key_pem) =
            issue_server_cert(ca, server_identity).expect("issue worker server cert");
        let server_config = build_server_config(ca.cert_pem(), &server_cert_pem, &server_key_pem)
            .expect("build worker TLS server config");
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback"); // pii-test-fixture
        let addr = listener.local_addr().expect("local addr");

        let handle = tokio::spawn(async move {
            loop {
                let (tcp, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let mut tls = match acceptor.accept(tcp).await {
                        Ok(t) => t,
                        Err(_) => return,
                    };
                    let mut line = String::new();
                    {
                        let mut reader = BufReader::new(&mut tls);
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                    }
                    let req: Value = match serde_json::from_str(line.trim_end()) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                    let resp = match req["op"].as_str() {
                        Some("call") => serde_json::json!({"ok": true, "text": "echo: hi"}),
                        Some("list") => serde_json::json!({"ok": true, "tools": ["echo"]}),
                        Some("health") => serde_json::json!({"ok": true, "healthy": true}),
                        _ => serde_json::json!({"ok": false, "error": "unknown op"}),
                    };
                    let _ = tls.write_all(format!("{resp}\n").as_bytes()).await;
                });
            }
        });

        (addr, handle)
    }

    #[tokio::test]
    #[serial]
    async fn matching_cn_completes_handshake_and_round_trips() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let (addr, worker) = spawn_tls_worker(&ca, "worker-off-box").await;

        let transport =
            MtlsTcpTransport::new(addr.ip().to_string(), addr.port(), "worker-off-box", &ca, "test-broker")
                .expect("transport should build");

        let out = transport
            .call("echo", serde_json::json!({}))
            .await
            .expect("call should succeed when the CN matches");
        assert_eq!(out.text, "echo: hi");

        let tools = transport.list().await.expect("list should succeed");
        assert_eq!(tools, vec!["echo".to_string()]);
        assert!(transport.health().await);

        worker.abort();
    }

    #[tokio::test]
    #[serial]
    async fn cn_disagreeing_with_configured_identity_fails_closed() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let (addr, worker) = spawn_tls_worker(&ca, "worker-off-box").await;

        let transport =
            MtlsTcpTransport::new(addr.ip().to_string(), addr.port(), "worker-someone-else", &ca, "test-broker")
                .expect("transport should build");

        let err = transport
            .call("echo", serde_json::json!({}))
            .await
            .expect_err("CN disagreeing with configured identity must fail closed");
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(!transport.health().await);

        worker.abort();
    }

    #[tokio::test]
    async fn unreachable_endpoint_is_unavailable_not_a_panic() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        // Port 1 on loopback: nothing listens there.
        let transport = MtlsTcpTransport::new("127.0.0.1", 1, "worker-off-box", &ca, "test-broker") // pii-test-fixture
            .expect("transport should build even though nothing is listening");

        let err = transport
            .call("echo", serde_json::json!({}))
            .await
            .expect_err("an unreachable endpoint must error cleanly, not panic");
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(!transport.health().await);
    }
}
