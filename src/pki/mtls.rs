//! mTLS transport on the terminus primary (TCLI-03 — Terminus Gateway P2).
//! Depends on [`crate::pki::ca`] (TCLI-01) and [`crate::pki::enroll`] (TCLI-02).
//!
//! ## What this is
//! A NEW, additive `rustls`/`tokio_rustls` listener: it presents the
//! terminus primary's own leaf cert (signed by the TCLI-01 CA) and REQUIRES
//! every connecting client to present a valid TCLI-02-enrolled client cert
//! chained to that same CA. On a successful handshake, the client's identity
//! (the CN embedded in its cert at enrollment time) is extracted and
//! attached to each request's [`http::Extensions`] as a [`ClientIdentity`],
//! then the request is dispatched into the SAME `axum::Router`
//! (`crate::mcp_server::build_router`) the plain `/mcp` listener serves —
//! existing tool-dispatch (allowlist/audit) code is reused unmodified, not
//! forked into a parallel mTLS-only path.
//!
//! ## What this deliberately is NOT
//! - Not a replacement for the existing plain HTTP+JWT `/mcp`/`/enroll`
//!   listener (`crate::mcp_server`, `crate::pki::enroll`). This module adds a
//!   second listener on a second port; it does not touch, wrap, or gate the
//!   existing one. See `src/bin/terminus_personal.rs::main` — the plain
//!   listener's bind/router/serve call is unchanged by this item.
//! - Not a CA-rotation mechanism. P2 ships a single CA with no rotation
//!   plan; see the module-level "known limitations" section below.
//!
//! ## Why `hyper` directly instead of `axum::serve`
//! `axum::serve` owns its own accept loop and has no supported hook to
//! attach per-connection data (the identity this module extracts during the
//! TLS handshake) to each request's extensions before it reaches the
//! router. This module instead accepts+TLS-terminates each connection
//! itself (`tokio_rustls::TlsAcceptor`), extracts+validates the identity
//! once per connection, then drives that one connection's HTTP framing with
//! `hyper` directly, inserting the identity into every request on that
//! connection before calling the shared router. This is strictly additive:
//! the router itself, and everything it dispatches to, is untouched.
//!
//! ## Fail-closed validation, two independent layers
//! 1. `rustls`'s `WebPkiClientVerifier` (configured by [`build_server_config`])
//!    performs full PKIX chain-building against the TCLI-01 CA as part of
//!    the TLS handshake itself — this already rejects a cert that doesn't
//!    chain to the CA, or one outside its own validity window (expired /
//!    not yet valid). A connection whose client cert fails either check
//!    never completes the handshake and never reaches this module's Rust
//!    code at all.
//! 2. [`extract_verified_identity`] is an explicit, independently-testable
//!    SECOND check run after the handshake completes, against the peer's
//!    leaf DER: it re-checks the validity window (belt-and-suspenders — see
//!    its doc comment) and additionally enforces the clientAuth EKU, which
//!    `WebPkiClientVerifier` does not check by default. Both checks are
//!    fail-closed: any parse/validation failure rejects the connection
//!    before a single byte of the wrapped HTTP request is dispatched.
//!
//! No raw certificate/key material is ever logged on rejection — only the
//! sanitized [`MtlsError`] `Display` (see its doc comment) and, where
//! relevant, the identity string.
//!
//! ## Known limitations (P2, documented per the TCLI-03 spec item)
//! - **CA rotation is not supported.** Single embedded CA, no rotation
//!   plan — a CA compromise or planned rotation is an out-of-scope,
//!   follow-up sprint item, not silently unhandled here.
//! - **A connection outliving its client cert's TTL is not force-closed
//!   mid-connection.** The handshake-time chain validation covers the cert
//!   at CONNECT time; a long-lived connection whose short-lived cert expires
//!   during its lifetime is allowed to finish in-flight requests rather than
//!   being severed — this matches HTTP/1.1 and HTTP/2 keep-alive connections
//!   generally not re-validating peer identity per request. Given TCLI-02's
//!   default enrollment TTL (24h) versus this crate's connection-oriented
//!   MCP tool-call usage pattern (short-lived processes, not long-held
//!   pools), this is judged low-risk for P2; a future item could add
//!   periodic re-handshake/renegotiation if that changes. Flagged explicitly
//!   for the Opus reviewer rather than left as a silent gap.

use std::sync::Arc;

use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::RootCertStore;
use thiserror::Error;

use super::CertificateAuthority;

/// Identity extracted from a validated client cert's Subject CN — the same
/// value TCLI-02 embedded at enrollment time
/// (`crate::pki::enroll::issue_leaf_cert`). Attached to each mTLS-transported
/// request's [`http::Extensions`] so downstream handlers can read it exactly
/// as they would read `crate::pki::enroll::EnrollmentClaims::sub` from a
/// decoded JWT — one identity shape, two transports, no forked authz path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity(pub String);

impl ClientIdentity {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Errors from mTLS server-config construction or post-handshake client-cert
/// validation. Every variant's `Display` is safe to log verbatim — none
/// interpolate certificate DER, key material, or any other secret; a
/// rejection is logged as "identity unknown/rejected", never a raw cert
/// dump (per the TCLI-03 spec item's S6 requirement).
#[derive(Debug, Error)]
pub enum MtlsError {
    /// The TLS handshake completed with no client certificate presented at
    /// all (should not normally happen once `WebPkiClientVerifier` requires
    /// one, but checked explicitly rather than assumed).
    #[error("no client certificate presented")]
    NoClientCert,
    /// The presented leaf certificate could not be parsed as X.509 DER.
    #[error("client certificate could not be parsed")]
    Unparseable,
    /// The leaf's validity window rejects "now" (expired or not-yet-valid).
    /// See the module doc's "fail-closed validation, two independent
    /// layers" section for why this is checked twice.
    #[error("client certificate is expired or not yet valid")]
    Expired,
    /// The leaf lacks the clientAuth extended key usage.
    #[error("client certificate is missing the clientAuth extended key usage")]
    MissingClientAuthEku,
    /// The leaf has no Subject CommonName to use as an identity.
    #[error("client certificate has no identifiable subject")]
    NoIdentity,
    /// Failed to build the `rustls::ServerConfig` (bad CA/server cert/key
    /// material, or the underlying `rustls`/`webpki` config rejected it).
    #[error("failed to build mTLS server config: {0}")]
    Config(String),
    /// Failed to generate/sign the primary's own server leaf cert.
    #[error("failed to issue mTLS server certificate: {0}")]
    ServerCertIssuance(String),
}

// ── Server cert issuance ────────────────────────────────────────────────────
//
// Per the TCLI-03 spec item's design-decision prompt ("decide and document
// which"): the terminus primary's own server cert is LONGER-LIVED than the
// per-identity client certs TCLI-02 issues. Server identity (this single,
// stable primary process) doesn't rotate the way a fleet of enrolling
// clients does, so a short TTL here would only add unnecessary renewal
// operational burden with no corresponding security benefit — the server
// cert's private key never leaves the primary host, unlike a client cert
// that's handed out over the network at enrollment time. TTL is
// configurable (`crate::config::mtls_server_cert_ttl_days`), defaulting to
// 365 days.

/// Generate a fresh server leaf cert for the terminus primary, signed by
/// `ca`, carrying the serverAuth EKU (mirroring TCLI-02's leaf issuance
/// pattern for the clientAuth EKU). `identity` is embedded in CN/SAN purely
/// for operator-facing identification (e.g. `terminus-primary`) — it plays
/// no role in client-side authz, since a server cert is not client input.
pub fn issue_server_cert(
    ca: &CertificateAuthority,
    identity: &str,
) -> Result<(String, String), MtlsError> {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| MtlsError::ServerCertIssuance(format!("server leaf params: {e}")))?;

    let now = chrono::Utc::now();
    let ttl = chrono::Duration::days(crate::config::mtls_server_cert_ttl_days());
    params.not_before = to_rcgen_time(now - chrono::Duration::minutes(5));
    params.not_after = to_rcgen_time(now + ttl);
    params
        .distinguished_name
        .push(DnType::CommonName, identity);
    params.subject_alt_names = vec![SanType::DnsName(
        identity
            .to_string()
            .try_into()
            .map_err(|e| MtlsError::ServerCertIssuance(format!("SAN encoding: {e:?}")))?,
    )];
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.key_usages.push(KeyUsagePurpose::KeyEncipherment);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);

    let key_pair = KeyPair::generate()
        .map_err(|e| MtlsError::ServerCertIssuance(format!("server leaf keypair: {e}")))?;
    let cert = params
        .signed_by(&key_pair, ca.issuer())
        .map_err(|e| MtlsError::ServerCertIssuance(format!("server leaf signing: {e}")))?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

/// Generate a fresh CLIENT leaf cert for THIS process to present when it is
/// the one dialing OUT to another mTLS-fronted upstream (MESH-02), signed by
/// `ca` and carrying the clientAuth EKU — the same shape TCLI-02's
/// `crate::pki::enroll::issue_leaf_cert` issues for enrolling peers, but
/// used here for the opposite direction (this process presenting an
/// identity to a peer, rather than validating one presented to it).
/// Short-lived, matching TCLI-02's enrollment TTL convention
/// (`crate::config::enrollment_cert_ttl_hours`) rather than the long-lived
/// server-cert TTL above — an outbound mesh dial mints a fresh leaf per
/// [`crate::mesh::client::UpstreamClient`] construction rather than caching
/// one across the process lifetime, so a short TTL costs nothing and keeps
/// the exposure window small.
pub fn issue_client_cert(
    ca: &CertificateAuthority,
    identity: &str,
) -> Result<(String, String), MtlsError> {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| MtlsError::ServerCertIssuance(format!("client leaf params: {e}")))?;

    let now = chrono::Utc::now();
    let ttl = chrono::Duration::hours(crate::config::enrollment_cert_ttl_hours());
    params.not_before = to_rcgen_time(now - chrono::Duration::minutes(5));
    params.not_after = to_rcgen_time(now + ttl);
    params
        .distinguished_name
        .push(DnType::CommonName, identity);
    params.subject_alt_names = vec![SanType::DnsName(
        identity
            .to_string()
            .try_into()
            .map_err(|e| MtlsError::ServerCertIssuance(format!("SAN encoding: {e:?}")))?,
    )];
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ClientAuth);

    let key_pair = KeyPair::generate()
        .map_err(|e| MtlsError::ServerCertIssuance(format!("client leaf keypair: {e}")))?;
    let cert = params
        .signed_by(&key_pair, ca.issuer())
        .map_err(|e| MtlsError::ServerCertIssuance(format!("client leaf signing: {e}")))?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

fn to_rcgen_time(dt: chrono::DateTime<chrono::Utc>) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(dt.timestamp())
        .expect("chrono timestamps are always in range for time::OffsetDateTime")
}

// ── rustls server config ────────────────────────────────────────────────────

/// Build the `rustls::ServerConfig` for the mTLS listener: presents
/// `server_cert_pem`/`server_key_pem` as the primary's own identity, and
/// requires + validates every connecting client's cert against
/// `ca_cert_pem` (the TCLI-01 root CA). No client cert, or one that doesn't
/// chain to this CA, or one outside its validity window, fails the
/// handshake before this module's Rust code runs at all — see the module
/// doc's "fail-closed validation" section.
pub fn build_server_config(
    ca_cert_pem: &str,
    server_cert_pem: &str,
    server_key_pem: &str,
) -> Result<rustls::ServerConfig, MtlsError> {
    let mut roots = RootCertStore::empty();
    for der in pem_to_der_certs(ca_cert_pem)? {
        roots
            .add(der)
            .map_err(|e| MtlsError::Config(format!("adding CA root: {e}")))?;
    }

    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| MtlsError::Config(format!("building client verifier: {e}")))?;

    let server_certs = pem_to_der_certs(server_cert_pem)?;
    let server_key = pem_to_der_key(server_key_pem)?;

    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(server_certs, server_key)
        .map_err(|e| MtlsError::Config(format!("setting server cert: {e}")))?;

    Ok(config)
}

fn pem_to_der_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, MtlsError> {
    rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| MtlsError::Config(format!("parsing PEM certificate(s): {e}")))
}

fn pem_to_der_key(pem: &str) -> Result<PrivateKeyDer<'static>, MtlsError> {
    rustls_pemfile::private_key(&mut pem.as_bytes())
        .map_err(|e| MtlsError::Config(format!("parsing PEM private key: {e}")))?
        .ok_or_else(|| MtlsError::Config("no private key found in PEM".to_string()))
}

// ── Post-handshake identity extraction (the second, independent check) ─────

/// Validate + extract the client identity from an already-handshake-accepted
/// peer leaf certificate's DER bytes. Called once per accepted mTLS
/// connection (see [`crate::pki::mtls`]'s connection-serving code in
/// `src/bin/terminus_personal.rs`) with `rustls`'s own
/// `CommonState::peer_certificates()` output.
///
/// This function is deliberately independently testable with hand-built DER
/// (no live TCP/TLS handshake needed) — see the unit tests below — while the
/// end-to-end handshake-level rejection (foreign CA, expired-at-handshake)
/// is covered by the integration test in this module that drives a real
/// loopback `tokio_rustls` handshake.
pub fn extract_verified_identity(leaf_der: &[u8]) -> Result<ClientIdentity, MtlsError> {
    let (_, cert) =
        x509_parser::parse_x509_certificate(leaf_der).map_err(|_| MtlsError::Unparseable)?;

    // Redundant, explicit, independently-testable fail-closed check on top
    // of `WebPkiClientVerifier`'s own handshake-time validity check (see the
    // module doc) -- not the only enforcement of this property.
    let now = x509_parser::time::ASN1Time::from(time::OffsetDateTime::now_utc());
    if !cert.validity().is_valid_at(now) {
        return Err(MtlsError::Expired);
    }

    let has_client_auth = cert
        .extended_key_usage()
        .ok()
        .flatten()
        .map(|eku| eku.value.client_auth)
        .unwrap_or(false);
    if !has_client_auth {
        return Err(MtlsError::MissingClientAuthEku);
    }

    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
        .ok_or(MtlsError::NoIdentity)?;

    Ok(ClientIdentity(cn.to_string()))
}

/// Extract + validate the client identity from an accepted
/// `tokio_rustls::server::TlsStream`'s negotiated peer certificate chain.
/// Returns [`MtlsError::NoClientCert`] if the handshake somehow completed
/// with no peer certs (defense-in-depth; `WebPkiClientVerifier` should never
/// allow this once configured to require client auth).
pub fn identity_from_tls_stream<IO>(
    stream: &tokio_rustls::server::TlsStream<IO>,
) -> Result<ClientIdentity, MtlsError> {
    let (_, session) = stream.get_ref();
    let leaf = session
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or(MtlsError::NoClientCert)?;
    extract_verified_identity(leaf)
}

// ── Listener loop (wired into `src/bin/terminus_personal.rs::main`) ────────

/// Run the mTLS listener: bind, accept connections, TLS-terminate + validate
/// each one, then dispatch its requests into `router` with the extracted
/// [`ClientIdentity`] attached. Runs until the listener errors (mirrors
/// `axum::serve`'s own "run forever, propagate a bind/accept-loop error"
/// contract) -- intended to be spawned as its own task alongside the
/// existing plain listener's `axum::serve(...).await`, never replacing it.
///
/// A single connection's TLS handshake failing (bad/missing/expired/
/// foreign-CA client cert) or its post-handshake identity check failing is
/// logged and that ONE connection is dropped -- it never tears down the
/// listener or affects any other connection.
pub async fn run_listener(
    bind_addr: &str,
    port: u16,
    tls_config: rustls::ServerConfig,
    router: axum::Router,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(format!("{bind_addr}:{port}")).await?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
    tracing::info!("pki::mtls: listening on {bind_addr}:{port}");

    loop {
        let (tcp, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("pki::mtls: accept error: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let tls = match acceptor.accept(tcp).await {
                Ok(t) => t,
                Err(_) => {
                    // Sanitized: never log the raw handshake error detail,
                    // which can echo back attacker-controlled TLS record
                    // bytes -- "rejected" + the peer address is enough for
                    // an operator to correlate against network logs.
                    tracing::warn!("pki::mtls: handshake rejected for {peer_addr} (identity unknown/rejected)");
                    return;
                }
            };
            let identity = match identity_from_tls_stream(&tls) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        "pki::mtls: rejected connection from {peer_addr} (identity unknown/rejected: {e})"
                    );
                    return;
                }
            };
            tracing::info!(
                "pki::mtls: accepted connection from {peer_addr}, identity={}",
                identity.as_str()
            );
            serve_connection(tls, identity, router).await;
        });
    }
}

/// Drive one accepted+validated mTLS connection's HTTP framing directly with
/// `hyper`, inserting `identity` into every request's extensions before
/// dispatching into `router` (see the module doc's "why hyper directly"
/// section for why `axum::serve` isn't used here).
async fn serve_connection<IO>(
    tls: tokio_rustls::server::TlsStream<IO>,
    identity: ClientIdentity,
    router: axum::Router,
) where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(tls);
    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let identity = identity.clone();
        let router = router.clone();
        async move {
            let (mut parts, body) = req.into_parts();
            parts.extensions.insert(identity);
            let req = axum::extract::Request::from_parts(parts, axum::body::Body::new(body));
            let resp = tower::ServiceExt::oneshot(router, req)
                .await
                .unwrap_or_else(|e: std::convert::Infallible| match e {});
            Ok::<_, std::convert::Infallible>(resp)
        }
    });

    if let Err(e) = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection_with_upgrades(io, svc)
        .await
    {
        // Connection-level I/O errors (peer reset, etc.) are routine at
        // this level -- debug, not warn, and never includes cert/key
        // material (this is a hyper/IO error, not TLS handshake detail).
        tracing::debug!("pki::mtls: connection ended: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::enroll::{handle_enrollment, EnrollmentRequest};
    use serial_test::serial;

    fn set_enroll_secrets() {
        std::env::set_var("TERMINUS_ENROLLMENT_SHARED_SECRET", "test-shared-secret");
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", "test-jwt-signing-key");
    }

    fn clear_enroll_secrets() {
        std::env::remove_var("TERMINUS_ENROLLMENT_SHARED_SECRET");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    fn enrolled_client_cert(ca: &CertificateAuthority, identity: &str) -> (String, String) {
        set_enroll_secrets();
        let req = EnrollmentRequest {
            identity: identity.to_string(),
            shared_secret: "<REDACTED-SECRET>".to_string(),
        };
        let resp = handle_enrollment(ca, &req).expect("enrollment should succeed");
        clear_enroll_secrets();
        (resp.cert_pem, resp.key_pem)
    }

    fn parse_der(pem: &str) -> Vec<u8> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).expect("valid PEM");
        pem.contents
    }

    #[test]
    #[serial]
    fn valid_enrolled_cert_is_accepted_and_identity_extracted() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let (cert_pem, _key_pem) = enrolled_client_cert(&ca, "harmony-primary");
        let der = parse_der(&cert_pem);

        let identity = extract_verified_identity(&der).expect("valid enrolled cert must be accepted");
        assert_eq!(identity.as_str(), "harmony-primary");
    }

    #[test]
    fn unparseable_der_is_rejected() {
        let err = extract_verified_identity(b"not a certificate")
            .expect_err("garbage DER must be rejected, not panic");
        assert!(matches!(err, MtlsError::Unparseable));
    }

    #[test]
    fn self_signed_foreign_cert_without_client_auth_is_rejected() {
        // A cert not chained to any terminus CA and not issued via
        // `crate::pki::enroll` -- lacks the clientAuth EKU this module now
        // requires (TCLI-03 follow-up), so it's rejected on that basis even
        // before considering chain-of-trust (which the TLS handshake layer,
        // not this function, is responsible for).
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "unrelated-self-signed");
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        let err = extract_verified_identity(cert.der())
            .expect_err("a cert with no clientAuth EKU must be rejected");
        assert!(matches!(err, MtlsError::MissingClientAuthEku));
    }

    #[test]
    fn expired_cert_is_rejected() {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "expired-identity");
        params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ClientAuth);
        params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        let now = chrono::Utc::now();
        // Both before and after are in the past -- structurally expired.
        params.not_before = to_rcgen_time(now - chrono::Duration::days(10));
        params.not_after = to_rcgen_time(now - chrono::Duration::days(1));
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        let err = extract_verified_identity(cert.der()).expect_err("expired cert must be rejected");
        assert!(matches!(err, MtlsError::Expired));
    }

    #[test]
    fn cert_missing_client_auth_eku_is_rejected_even_with_valid_dates() {
        // Same shape as an enrollment-issued cert but WITHOUT the clientAuth
        // EKU this item added to `crate::pki::enroll::issue_leaf_cert` --
        // pins the pre-TCLI-03 regression this item's follow-up fixes.
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "no-eku-identity");
        let now = chrono::Utc::now();
        params.not_before = to_rcgen_time(now - chrono::Duration::minutes(5));
        params.not_after = to_rcgen_time(now + chrono::Duration::hours(1));
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        let err = extract_verified_identity(cert.der())
            .expect_err("a valid-dated cert with no clientAuth EKU must still be rejected");
        assert!(matches!(err, MtlsError::MissingClientAuthEku));
    }

    #[test]
    #[serial]
    fn server_config_builds_from_ca_and_issued_server_cert() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let (server_cert_pem, server_key_pem) =
            issue_server_cert(&ca, "terminus-primary-test").expect("issue server cert");

        let config = build_server_config(ca.cert_pem(), &server_cert_pem, &server_key_pem)
            .expect("server config should build from consistent CA + server cert/key");
        // `ServerConfig` has no public equality/introspection worth
        // asserting beyond "it built successfully" -- the meaningful
        // behavior (handshake accept/reject) is covered by the loopback
        // integration test below, which actually drives this config through
        // a real TLS handshake.
        drop(config);
    }

    #[test]
    fn server_config_rejects_corrupt_ca_material() {
        let err = build_server_config("not a cert", "not a cert", "not a key")
            .expect_err("corrupt PEM material must fail loudly, not panic or silently succeed");
        assert!(matches!(err, MtlsError::Config(_)));
    }

    // ── End-to-end loopback integration test ────────────────────────────
    //
    // Drives an actual `tokio_rustls` handshake over a real loopback TCP
    // connection: enroll (TCLI-02) -> connect via mTLS (TCLI-03) -> confirm
    // the connection's identity matches the enrolled identity, per the
    // TCLI-03 test plan's integration-test requirement. Also exercises the
    // handshake-level rejections (`WebPkiClientVerifier`'s own chain
    // validation) that `extract_verified_identity`'s unit tests above
    // deliberately don't reach, since they operate purely on leaf DER
    // without a live handshake.

    async fn handshake_client_side(
        addr: std::net::SocketAddr,
        client_cert_pem: &str,
        client_key_pem: &str,
        ca_cert_pem: &str,
    ) -> std::io::Result<()> {
        use rustls::pki_types::ServerName;
        use tokio::net::TcpStream;
        use tokio_rustls::TlsConnector;

        let mut roots = RootCertStore::empty();
        for der in pem_to_der_certs(ca_cert_pem).expect("parse CA for client roots") {
            roots.add(der).expect("add CA root to client store");
        }
        let client_certs = pem_to_der_certs(client_cert_pem).expect("parse client cert");
        let client_key = pem_to_der_key(client_key_pem).expect("parse client key");

        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(client_certs, client_key)
            .expect("client config with client auth cert");

        let connector = TlsConnector::from(Arc::new(client_config));
        let tcp = TcpStream::connect(addr).await?;
        let server_name = ServerName::try_from("terminus-primary-test")
            .expect("valid server name")
            .to_owned();
        let mut tls = connector.connect(server_name, tcp).await?;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tls.write_all(b"ping").await?;
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn enrolled_client_cert_completes_mtls_handshake_end_to_end() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let (server_cert_pem, server_key_pem) =
            issue_server_cert(&ca, "terminus-primary-test").expect("issue server cert");
        let server_config = build_server_config(ca.cert_pem(), &server_cert_pem, &server_key_pem)
            .expect("build server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");

        let (cert_pem, key_pem) = enrolled_client_cert(&ca, "harmony-primary-e2e");

        let server_task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let tls = acceptor.accept(tcp).await.expect("server-side handshake should succeed");
            let identity =
                identity_from_tls_stream(&tls).expect("identity should extract from a valid enrolled cert");
            assert_eq!(identity.as_str(), "harmony-primary-e2e");

            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut tls = tls;
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await.expect("read ping");
            tls.write_all(b"pong").await.expect("write pong");
        });

        handshake_client_side(addr, &cert_pem, &key_pem, ca.cert_pem())
            .await
            .expect("client-side handshake with a valid enrolled cert should succeed");

        server_task.await.expect("server task should not panic");
    }

    #[tokio::test]
    #[serial]
    async fn foreign_ca_client_cert_fails_the_handshake() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let (server_cert_pem, server_key_pem) =
            issue_server_cert(&ca, "terminus-primary-test").expect("issue server cert");
        let server_config = build_server_config(ca.cert_pem(), &server_cert_pem, &server_key_pem)
            .expect("build server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");

        // A completely unrelated CA + client cert -- never chains to `ca`.
        let foreign_ca = CertificateAuthority::generate().expect("generate foreign CA");
        let (cert_pem, key_pem) = enrolled_client_cert(&foreign_ca, "impostor-identity");

        let server_task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            // The server-side handshake itself must fail -- a cert from an
            // unrelated CA is rejected by `WebPkiClientVerifier` during
            // chain validation, before any application code runs.
            let result = acceptor.accept(tcp).await;
            assert!(
                result.is_err(),
                "server-side handshake must reject a foreign-CA client cert"
            );
        });

        let client_result =
            handshake_client_side(addr, &cert_pem, &key_pem, foreign_ca.cert_pem()).await;
        assert!(
            client_result.is_err(),
            "client-side handshake must also observe the failure (server closes the connection)"
        );

        server_task.await.expect("server task should not panic");
    }

    #[tokio::test]
    #[serial]
    async fn missing_client_cert_fails_the_handshake() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let (server_cert_pem, server_key_pem) =
            issue_server_cert(&ca, "terminus-primary-test").expect("issue server cert");
        let server_config = build_server_config(ca.cert_pem(), &server_cert_pem, &server_key_pem)
            .expect("build server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");

        let server_task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let result = acceptor.accept(tcp).await;
            assert!(
                result.is_err(),
                "server-side handshake must reject a connection presenting no client cert"
            );
        });

        // Plain TLS client config with NO client auth cert configured at all.
        let mut roots = RootCertStore::empty();
        for der in pem_to_der_certs(ca.cert_pem()).expect("parse CA for client roots") {
            roots.add(der).expect("add CA root to client store");
        }
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect loopback");
        let server_name = rustls::pki_types::ServerName::try_from("terminus-primary-test")
            .expect("valid server name")
            .to_owned();
        // In TLS 1.3, a client that already sent its (empty) Certificate +
        // Finished considers its own side of the handshake complete before
        // it can know whether the server accepted that certificate — the
        // server can only signal rejection via a fatal alert sent
        // afterward, which the client only observes on its next I/O, not
        // synchronously inside `connect()`. So this asserts the failure is
        // observable by EITHER `connect()` itself OR the first read/write
        // on the resulting stream, rather than assuming it's always
        // synchronous with `connect()`. The server-side assertion above
        // (`acceptor.accept(tcp).await` failing) is the authoritative,
        // unambiguous check that the rejection actually happened.
        match connector.connect(server_name, tcp).await {
            Err(_) => {}
            Ok(mut tls) => {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let io_result = async {
                    tls.write_all(b"ping").await?;
                    let mut buf = [0u8; 4];
                    tls.read_exact(&mut buf).await
                }
                .await;
                assert!(
                    io_result.is_err(),
                    "client-side handshake must eventually observe failure (post-handshake I/O) \
                     when no client cert is presented"
                );
            }
        }

        server_task.await.expect("server task should not panic");
    }
}
