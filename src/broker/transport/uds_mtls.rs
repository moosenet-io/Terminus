//! T2 (default) — UDS + mTLS-over-UDS (TMOD-02).
//!
//! The strongest tier: the connection never leaves the host (a Unix Domain
//! Socket) AND is mutually authenticated with certificates issued by the
//! embedded CA ([`crate::pki::ca`], reused unmodified — see
//! [`crate::pki::mtls`]'s module doc for the CA/cert plumbing this builds
//! on). Two INDEPENDENT identity signals must both resolve to the worker's
//! configured identity before a single request is sent:
//!
//! 1. The kernel-attested `SO_PEERCRED` peer uid of the underlying Unix
//!    socket (checked on the raw [`tokio::net::UnixStream`], before the TLS
//!    handshake even begins — same mechanism as [`super::uds_peercred`]'s
//!    T1).
//! 2. The TLS peer leaf certificate's Subject CommonName, extracted after
//!    the handshake completes (rustls's own chain validation against the
//!    embedded CA has already run by that point — a cert that doesn't chain
//!    to the CA, or is outside its validity window, never gets this far).
//!
//! ## Design decision: "both agree with config", not "both agree with each
//! other"
//! The TMOD-02 spec item's phrasing ("kernel `SO_PEERCRED` peer identity AND
//! cert CN must agree") is read here as: BOTH signals must independently
//! match this worker's `expected_uid` / `expected_identity` as configured by
//! the broker operator — not merely "whatever uid and whatever CN happen to
//! match each other" (which would let a compromised process holding *some*
//! valid cert satisfy the check as long as it also runs under *some*
//! attacker-controlled uid, with no tie to what the broker actually expects
//! this worker to be). Fail-closed either way: a mismatch on EITHER signal
//! against the configured identity rejects the connection before any
//! request is written.

use std::path::PathBuf;
use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio_rustls::TlsConnector;

use crate::error::ToolError;
use crate::pki::ca::CertificateAuthority;
use crate::tool::ToolOutput;

use super::{call_over, health_over, list_over, TransportError, WorkerTransport};

/// A T2 UDS+mTLS transport to one worker.
pub struct UdsMtlsTransport {
    socket_path: PathBuf,
    expected_uid: u32,
    /// Subject CN the worker's presented TLS leaf certificate must carry.
    expected_identity: String,
    ca_cert_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

impl UdsMtlsTransport {
    /// Build a transport for a worker, minting this process's own short-lived
    /// client leaf cert against `ca` (mirrors
    /// `crate::mesh::client::UpstreamClient::from_upstream`'s mTLS-transport
    /// construction — a fresh leaf per transport build, not cached across
    /// the process lifetime). `client_identity_label` is this broker
    /// process's own presented identity (e.g. `"terminus-broker"`), separate
    /// from `expected_identity` (the WORKER's identity this transport
    /// requires on the other end).
    pub fn new(
        socket_path: impl Into<PathBuf>,
        expected_uid: u32,
        expected_identity: impl Into<String>,
        ca: &CertificateAuthority,
        client_identity_label: &str,
    ) -> Result<Self, TransportError> {
        let (client_cert_pem, client_key_pem) =
            crate::pki::mtls::issue_client_cert(ca, client_identity_label)
                .map_err(|e| TransportError::Protocol(format!("issuing client cert: {e}")))?;
        Ok(Self {
            socket_path: socket_path.into(),
            expected_uid,
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

    /// Dial the UDS, check `SO_PEERCRED`, perform the TLS-over-UDS
    /// handshake, then check the peer leaf cert's CN — all fail-closed
    /// before any tool request is ever written to the stream.
    async fn dial_verified(
        &self,
    ) -> Result<tokio_rustls::client::TlsStream<UnixStream>, TransportError> {
        let tcp = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            TransportError::Unavailable(format!(
                "connecting to {}: {e}",
                self.socket_path.display()
            ))
        })?;

        let peer_cred = tcp
            .peer_cred()
            .map_err(|e| TransportError::Protocol(format!("SO_PEERCRED read failed: {e}")))?;
        if peer_cred.uid() != self.expected_uid {
            return Err(TransportError::IdentityMismatch(format!(
                "peer uid {} does not match configured worker uid {}",
                peer_cred.uid(),
                self.expected_uid
            )));
        }

        let config = self.build_client_config()?;
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from(self.expected_identity.clone())
            .map_err(|e| TransportError::Protocol(format!("invalid worker identity as SNI: {e}")))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| TransportError::Unavailable(format!("TLS-over-UDS handshake failed: {e}")))?;

        let (_, session) = tls.get_ref();
        let leaf = session
            .peer_certificates()
            .and_then(|certs| certs.first())
            .ok_or_else(|| TransportError::IdentityMismatch("worker presented no certificate".to_string()))?;
        let cn = extract_cn_only(leaf)
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
impl WorkerTransport for UdsMtlsTransport {
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

/// Extract just the Subject CommonName from a leaf certificate's DER bytes —
/// deliberately NOT [`crate::pki::mtls::extract_verified_identity`], which
/// additionally requires the clientAuth EKU (correct when validating a
/// CLIENT cert presented TO a server, which is not this module's direction:
/// here the broker is the TLS CLIENT validating the WORKER's SERVER cert,
/// which carries serverAuth, not clientAuth). Chain-of-trust and validity
/// window are already enforced by rustls's own handshake-time verification
/// against the pinned CA root store (see [`UdsMtlsTransport::build_client_config`]);
/// this function's only job is the CN string.
pub(crate) fn extract_cn_only(leaf_der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(leaf_der).ok()?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
        .map(str::to_string);
    cn
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
    use tokio::net::UnixListener;
    use tokio_rustls::TlsAcceptor;

    /// Spawn a TLS-over-UDS "worker": accepts connections on `path`,
    /// server-authenticates with a cert for `server_identity` (signed by
    /// `ca`), requires (but does not itself further check) a client cert
    /// chained to the same CA, then answers every request with a fixed
    /// canned response.
    fn spawn_tls_worker(
        path: PathBuf,
        ca: &CertificateAuthority,
        server_identity: &str,
    ) -> tokio::task::JoinHandle<()> {
        let (server_cert_pem, server_key_pem) =
            issue_server_cert(ca, server_identity).expect("issue worker server cert");
        let server_config = build_server_config(ca.cert_pem(), &server_cert_pem, &server_key_pem)
            .expect("build worker TLS server config");
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        tokio::spawn(async move {
            let listener = UnixListener::bind(&path).expect("bind worker socket");
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
        })
    }

    fn current_uid() -> u32 {
        unsafe { libc::getuid() }
    }

    #[tokio::test]
    #[serial]
    async fn matching_peercred_and_cn_completes_handshake_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("worker-t2.sock");
        let ca = CertificateAuthority::generate().expect("generate CA");
        let worker = spawn_tls_worker(sock_path.clone(), &ca, "worker-a");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let transport = UdsMtlsTransport::new(sock_path, current_uid(), "worker-a", &ca, "test-broker")
            .expect("transport should build");

        let out = transport
            .call("echo", serde_json::json!({}))
            .await
            .expect("call should succeed when peercred and CN both match");
        assert_eq!(out.text, "echo: hi");

        let tools = transport.list().await.expect("list should succeed");
        assert_eq!(tools, vec!["echo".to_string()]);
        assert!(transport.health().await);

        worker.abort();
    }

    #[tokio::test]
    #[serial]
    async fn cn_disagreeing_with_configured_identity_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("worker-t2-badcn.sock");
        let ca = CertificateAuthority::generate().expect("generate CA");
        // Worker's cert CN is "worker-a", but the transport is configured to
        // expect "worker-b" -- CN disagrees with configured identity.
        let worker = spawn_tls_worker(sock_path.clone(), &ca, "worker-a");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let transport = UdsMtlsTransport::new(sock_path, current_uid(), "worker-b", &ca, "test-broker")
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
    #[serial]
    async fn peer_uid_mismatch_fails_closed_before_tls_handshake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("worker-t2-baduid.sock");
        let ca = CertificateAuthority::generate().expect("generate CA");
        let worker = spawn_tls_worker(sock_path.clone(), &ca, "worker-a");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let wrong_uid = current_uid().wrapping_add(1);
        let transport = UdsMtlsTransport::new(sock_path, wrong_uid, "worker-a", &ca, "test-broker")
            .expect("transport should build");

        let err = transport
            .call("echo", serde_json::json!({}))
            .await
            .expect_err("peer uid mismatch must fail closed");
        assert!(matches!(err, ToolError::Execution(_)));

        worker.abort();
    }

    #[tokio::test]
    async fn absent_socket_is_unavailable_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("does-not-exist.sock");
        let ca = CertificateAuthority::generate().expect("generate CA");
        let transport = UdsMtlsTransport::new(sock_path, current_uid(), "worker-a", &ca, "test-broker")
            .expect("transport should build even though nothing is listening yet");

        let err = transport
            .call("echo", serde_json::json!({}))
            .await
            .expect_err("an absent socket must error cleanly, not panic");
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(!transport.health().await);
    }
}
