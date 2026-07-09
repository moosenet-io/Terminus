//! Typed errors for `terminus-client` (TCLI-04). Every variant's `Display`
//! is safe to log verbatim -- none interpolate the bootstrap secret, the
//! issued private key, or the JWT (matching the redaction convention the
//! server-side `terminus_rs::pki::enroll`/`pki::mtls` modules already use).
//!
//! Per the TCLI-04 spec item's APPROACH step 5: enrollment/connection
//! failure is always a typed error, never a panic -- TCLI-05/06 build
//! fallback behavior on top of these variants.

use thiserror::Error;

/// Errors from [`crate::enroll`]/[`crate::connect`] and the local
/// credential-store helpers they share.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The enrollment endpoint could not be reached at all (DNS/connect/
    /// timeout). The crate deliberately does not retry-loop on this itself
    /// (TCLI-04 edge case: "the calling program decides fallback
    /// behavior") -- one attempt, one typed error.
    #[error("failed to reach the enrollment endpoint: {0}")]
    EnrollmentUnreachable(String),
    /// The enrollment endpoint reached but rejected the request (bad
    /// shared secret, disallowed identity, endpoint not configured, etc).
    /// `status` is the HTTP status code; `body` is the (non-secret) JSON
    /// error body the server returned, if any.
    #[error("enrollment rejected by the server (HTTP {status}): {body}")]
    EnrollmentRejected { status: u16, body: String },
    /// The enrollment endpoint returned 200 but the response body didn't
    /// parse as an [`crate::enroll::EnrolledCredential`].
    #[error("enrollment response was malformed: {0}")]
    MalformedResponse(String),
    /// Reading, parsing, or writing the local credential store failed.
    /// Distinct from a *missing* store (which is a legitimate first-run
    /// state, not an error -- see `crate::enroll::load_local_credential`).
    #[error("local credential store error: {0}")]
    Store(String),
    /// Building the rustls client config (bad PEM cert/key material, or a
    /// pinned CA that failed to parse) failed.
    #[error("failed to build mTLS client configuration: {0}")]
    TlsConfig(String),
    /// The TCP dial to the mTLS listener failed (host unreachable, refused,
    /// timed out).
    #[error("failed to reach the mTLS listener at {0}: {1}")]
    DialUnreachable(String, String),
    /// The TLS handshake itself failed -- this is the crate's core security
    /// property under negative test (TCLI-04 TEST PLAN): a server
    /// presenting a cert NOT chained to the pinned CA, or an otherwise
    /// invalid/expired server cert, lands here, never silently succeeding.
    #[error("mTLS handshake failed: {0}")]
    Handshake(String),
    /// No enrolled credential is available and none was supplied -- e.g.
    /// `connect()` called before `enroll()` has ever succeeded for this
    /// identity/store.
    #[error("no enrolled credential available; call enroll() first")]
    NotEnrolled,
}
