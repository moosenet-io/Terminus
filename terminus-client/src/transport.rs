//! mTLS dial (TCLI-04, depends on the server-side TCLI-03 mTLS listener).
//! Builds a `rustls` client config that presents the identity enrolled via
//! [`crate::enroll`] and trusts ONLY the CA cert pinned at enrollment time
//! (never the system trust store), then dials the terminus primary's mTLS
//! listener and completes the TLS handshake. The returned [`MtlsTransport`]
//! is a reusable, already-authenticated transport TCLI-05's daemon builds
//! an HTTP client on top of.

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::enroll::EnrolledCredential;
use crate::error::ClientError;

/// Configuration for one [`connect`] dial.
#[derive(Debug, Clone)]
pub struct ConnectConfig {
    /// Host of the terminus primary's mTLS listener (TCLI-03; default port
    /// `8301` on the primary side, but this crate takes the port
    /// explicitly rather than assuming that default -- see `port`).
    pub host: String,
    /// Port of the mTLS listener.
    pub port: u16,
    /// Expected server identity -- the CN/SAN the primary's mTLS server
    /// cert was issued for (`terminus_rs::config::mtls_server_identity()`
    /// server-side; defaults to `"terminus-primary"`). Used as the TLS
    /// `ServerName` for the handshake, so rustls's own certificate
    /// hostname-verification enforces this matches the presented server
    /// cert's SAN -- a server presenting a cert for a different name (even
    /// if chained to the same pinned CA) fails the handshake.
    pub server_name: String,
}

/// An established, authenticated mTLS connection to a terminus primary.
/// Deliberately a thin wrapper -- TCLI-05's daemon is expected to drive
/// HTTP request/response framing over the wrapped stream (the same way the
/// server side's `pki::mtls::run_listener` drives `hyper` directly over its
/// accepted `TlsStream`), not this crate.
pub struct MtlsTransport {
    stream: TlsStream<TcpStream>,
    server_identity: String,
}

impl std::fmt::Debug for MtlsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The wrapped `TlsStream` carries live session key material;
        // deliberately never printed, matching this crate's redaction
        // convention for anything credential-adjacent.
        f.debug_struct("MtlsTransport")
            .field("server_identity", &self.server_identity)
            .field("stream", &"<redacted, live TLS session>")
            .finish()
    }
}

impl MtlsTransport {
    /// The server identity this transport's handshake was validated
    /// against (i.e. `ConnectConfig::server_name`).
    pub fn server_identity(&self) -> &str {
        &self.server_identity
    }

    /// Consume this transport, returning the underlying
    /// `tokio_rustls::client::TlsStream` for a caller (TCLI-05) to drive an
    /// HTTP client over. `TlsStream<TcpStream>` already implements
    /// `tokio::io::{AsyncRead, AsyncWrite}`, so it plugs directly into
    /// `hyper`/any tokio-based HTTP client without further wrapping.
    pub fn into_io(self) -> TlsStream<TcpStream> {
        self.stream
    }
}

/// Dial `cfg` and complete an mTLS handshake, presenting `credential`'s
/// leaf cert/key and trusting only `credential.ca_cert_pem`.
///
/// Per the TCLI-04 spec item's APPROACH step 4/5 and TEST PLAN: a server
/// whose presented certificate does not chain to `credential.ca_cert_pem`
/// (or otherwise fails hostname/validity checks) causes this to return
/// [`ClientError::Handshake`], never a silently-succeeded connection --
/// this is the crate's core security property.
pub async fn connect(
    credential: &EnrolledCredential,
    cfg: &ConnectConfig,
) -> Result<MtlsTransport, ClientError> {
    let tls_config = build_client_config(credential)?;

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let tcp = TcpStream::connect(&addr)
        .await
        .map_err(|e| ClientError::DialUnreachable(addr, e.to_string()))?;

    let server_name = ServerName::try_from(cfg.server_name.clone())
        .map_err(|e| ClientError::TlsConfig(format!("invalid server name '{}': {e}", cfg.server_name)))?;

    let connector = TlsConnector::from(Arc::new(tls_config));
    let stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| ClientError::Handshake(e.to_string()))?;

    Ok(MtlsTransport {
        stream,
        server_identity: cfg.server_name.clone(),
    })
}

/// Build the `rustls::ClientConfig`: presents `credential.cert_pem`/
/// `key_pem` as this client's identity, and trusts ONLY
/// `credential.ca_cert_pem` as a root -- mirrors the server-side
/// `terminus_rs::pki::mtls::build_server_config`'s "single pinned CA, no
/// system trust store" posture, from the client's side of the handshake.
fn build_client_config(credential: &EnrolledCredential) -> Result<ClientConfig, ClientError> {
    let mut roots = RootCertStore::empty();
    for der in pem_to_der_certs(&credential.ca_cert_pem)? {
        roots
            .add(der)
            .map_err(|e| ClientError::TlsConfig(format!("adding pinned CA root: {e}")))?;
    }

    let client_certs = pem_to_der_certs(&credential.cert_pem)?;
    let client_key = pem_to_der_key(&credential.key_pem)?;

    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(client_certs, client_key)
        .map_err(|e| ClientError::TlsConfig(format!("setting client cert: {e}")))?;

    Ok(config)
}

fn pem_to_der_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, ClientError> {
    rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ClientError::TlsConfig(format!("parsing PEM certificate(s): {e}")))
}

fn pem_to_der_key(pem: &str) -> Result<PrivateKeyDer<'static>, ClientError> {
    rustls_pemfile::private_key(&mut pem.as_bytes())
        .map_err(|e| ClientError::TlsConfig(format!("parsing PEM private key: {e}")))?
        .ok_or_else(|| ClientError::TlsConfig("no private key found in PEM".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
        SanType,
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::net::TcpListener;

    /// Minimal test-only CA, independent of `terminus-rs::pki` (this crate
    /// has no dependency on that crate -- see the module docs). Mints a
    /// self-signed root plus signed leaves, matching the shape
    /// TCLI-01/TCLI-02/TCLI-03 produce on the real server side closely
    /// enough to exercise this crate's handshake logic honestly. Mirrors
    /// the exact `rcgen` API `terminus_rs::pki::ca::CertificateAuthority`
    /// uses (`Issuer::new` + `params.signed_by`).
    struct TestCa {
        cert_pem: String,
        issuer: Issuer<'static, KeyPair>,
    }

    fn generate_test_ca() -> TestCa {
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
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + secs
    }

    fn credential_for(ca: &TestCa, client_identity: &str) -> EnrolledCredential {
        let (cert_pem, key_pem) =
            issue_leaf(ca, client_identity, ExtendedKeyUsagePurpose::ClientAuth);
        EnrolledCredential {
            cert_pem,
            key_pem,
            ca_cert_pem: ca.cert_pem.clone(),
            jwt: "test.jwt".to_string(),
            expires_at: now_plus(3600),
        }
    }

    async fn spawn_mock_mtls_server(
        server_cert_pem: String,
        server_key_pem: String,
        require_client_cert: bool,
        trust_ca_pem: Option<String>,
    ) -> (String, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let certs = pem_to_der_certs(&server_cert_pem).unwrap();
        let key = pem_to_der_key(&server_key_pem).unwrap();

        let server_config = if require_client_cert {
            let mut roots = RootCertStore::empty();
            for der in pem_to_der_certs(&trust_ca_pem.unwrap()).unwrap() {
                roots.add(der).unwrap();
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .unwrap();
            rustls::ServerConfig::builder()
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)
                .unwrap()
        } else {
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .unwrap()
        };

        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
            if let Ok((tcp, _)) = listener.accept().await {
                // Best-effort: complete the handshake (or fail it, for the
                // negative-path test) then drop -- these tests only assert
                // client-side outcome, they don't need to exchange bytes.
                let _ = acceptor.accept(tcp).await;
            }
        });

        (addr.ip().to_string(), addr.port())
    }

    #[tokio::test]
    async fn connect_succeeds_against_a_ca_chained_server_cert() {
        let ca = generate_test_ca();
        let (server_cert, server_key) = issue_leaf(&ca, "terminus-primary", ExtendedKeyUsagePurpose::ServerAuth);
        let credential = credential_for(&ca, "dev-box-claude-code");

        let (host, port) = spawn_mock_mtls_server(server_cert, server_key, false, None).await;

        let cfg = ConnectConfig {
            host,
            port,
            server_name: "terminus-primary".to_string(),
        };

        let transport = connect(&credential, &cfg)
            .await
            .expect("connect should succeed against a CA-chained server cert");
        assert_eq!(transport.server_identity(), "terminus-primary");
    }

    #[tokio::test]
    async fn connect_fails_against_a_foreign_ca_server_cert() {
        // The server presents a leaf signed by a DIFFERENT CA than the one
        // pinned in the client's credential -- this is the crate's core
        // security property under negative test (TCLI-04 TEST PLAN).
        let real_ca = generate_test_ca();
        let foreign_ca = generate_test_ca();
        let (server_cert, server_key) = issue_leaf(&foreign_ca, "terminus-primary", ExtendedKeyUsagePurpose::ServerAuth);
        let credential = credential_for(&real_ca, "dev-box-claude-code");

        let (host, port) = spawn_mock_mtls_server(server_cert, server_key, false, None).await;

        let cfg = ConnectConfig {
            host,
            port,
            server_name: "terminus-primary".to_string(),
        };

        let err = connect(&credential, &cfg)
            .await
            .expect_err("connect must fail against a server cert not chained to the pinned CA");
        assert!(matches!(err, ClientError::Handshake(_)));
    }

    #[tokio::test]
    async fn connect_fails_when_no_server_is_listening() {
        let ca = generate_test_ca();
        let credential = credential_for(&ca, "dev-box-claude-code");
        let cfg = ConnectConfig {
            host: "127.0.0.1".to_string(),
            port: 1, // nothing listens on port 1
            server_name: "terminus-primary".to_string(),
        };

        let err = connect(&credential, &cfg)
            .await
            .expect_err("connect must fail cleanly when unreachable");
        assert!(matches!(err, ClientError::DialUnreachable(_, _)));
    }
}
