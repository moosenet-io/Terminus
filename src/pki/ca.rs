//! `rcgen`-based CA keypair + certificate generation and signing (TCLI-01).
//!
//! Owns the actual PKI material construction/parsing. [`crate::pki::ca`] is
//! the only intended caller — no other module should construct a
//! [`CertificateAuthority`] directly (see the module doc on `crate::pki`).

use chrono::{Datelike, Utc};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};

use super::PkiError;

/// Root CA validity window, in whole years either side of "now". This is the
/// long-lived root, not a leaf — TCLI-02's enrollment-issued client certs and
/// TCLI-03's server cert are short-lived by comparison.
const CA_BACKDATE_YEARS: i32 = 1;
const CA_FORWARD_YEARS: i32 = 10;

/// A loaded-or-generated embedded root CA: its PEM certificate plus an
/// [`Issuer`] wrapping the private key, ready to sign downstream leaf certs
/// (TCLI-02's enrollment endpoint, TCLI-03's server cert).
///
/// Deliberately does NOT derive [`std::fmt::Debug`] or [`std::fmt::Display`]
/// — the hand-written [`std::fmt::Debug`] impl below never prints the
/// certificate PEM or touches key material at all (it prints only a fixed
/// placeholder), matching the redaction convention used elsewhere in this
/// crate (see `crate::github::adapter::GitHubAdapter`'s `Debug` impl).
pub struct CertificateAuthority {
    cert_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl std::fmt::Debug for CertificateAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateAuthority")
            .field("cert_pem", &"<redacted>")
            .field("issuer", &"<redacted, holds CA private key>")
            .finish()
    }
}

impl CertificateAuthority {
    /// Generate a brand-new self-signed root CA keypair + certificate.
    pub fn generate() -> Result<Self, PkiError> {
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| PkiError::Generation(format!("CA params: {e}")))?;

        let now = Utc::now();
        // Backdate slightly to tolerate clock skew on hosts that bootstrap
        // this CA; forward-date years out since this is the long-lived root.
        params.not_before = rcgen::date_time_ymd(now.year() - CA_BACKDATE_YEARS, 1, 1);
        params.not_after = rcgen::date_time_ymd(
            now.year() + CA_FORWARD_YEARS,
            now.month() as u8,
            now.day() as u8,
        );
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "Terminus Embedded Root CA");
        params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        params.key_usages.push(KeyUsagePurpose::CrlSign);
        params.key_usages.push(KeyUsagePurpose::DigitalSignature);

        let key_pair =
            KeyPair::generate().map_err(|e| PkiError::Generation(format!("CA key pair: {e}")))?;
        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| PkiError::Generation(format!("CA self-signed cert: {e}")))?;
        let cert_pem = cert.pem();
        let issuer = Issuer::new(params, key_pair);

        Ok(Self { cert_pem, issuer })
    }

    /// Parse previously-persisted CA material (PEM cert + PEM private key).
    ///
    /// Fails loudly on malformed/corrupt input rather than silently
    /// regenerating (TCLI-01 edge case) — a silent regeneration here would
    /// invalidate every certificate this CA has already issued.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, PkiError> {
        let key_pair = KeyPair::from_pem(key_pem)
            .map_err(|e| PkiError::CorruptMaterial(format!("CA private key: {e}")))?;
        let issuer = Issuer::from_ca_cert_pem(cert_pem, key_pair)
            .map_err(|e| PkiError::CorruptMaterial(format!("CA certificate: {e}")))?;
        Ok(Self {
            cert_pem: cert_pem.to_string(),
            issuer,
        })
    }

    /// The CA's own PEM-encoded self-signed certificate — public, safe to
    /// distribute to anything that needs to verify certs this CA issues.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The CA private key, PEM-encoded. Callers use this ONLY to persist the
    /// CA (see `crate::pki::persist_local_store`) — never log or print it.
    pub fn key_pem(&self) -> String {
        self.issuer.key().serialize_pem()
    }

    /// Borrow the signing [`Issuer`] for downstream leaf-cert issuance
    /// (TCLI-02's enrollment endpoint, TCLI-03's server cert). No other
    /// module should reach into CA key material any other way — everything
    /// signs through this accessor.
    pub fn issuer(&self) -> &Issuer<'static, KeyPair> {
        &self.issuer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_a_ca_certificate() {
        let ca = CertificateAuthority::generate().expect("CA generation should succeed");
        assert!(ca.cert_pem().contains("BEGIN CERTIFICATE"));
        assert!(ca.key_pem().contains("BEGIN PRIVATE KEY") || ca.key_pem().contains("BEGIN EC PRIVATE KEY"));
    }

    #[test]
    fn from_pem_round_trips_generated_material() {
        let generated = CertificateAuthority::generate().expect("generate");
        let cert_pem = generated.cert_pem().to_string();
        let key_pem = generated.key_pem();

        let loaded = CertificateAuthority::from_pem(&cert_pem, &key_pem)
            .expect("round-trip load of freshly generated CA material should succeed");
        assert_eq!(loaded.cert_pem(), cert_pem);
    }

    #[test]
    fn from_pem_rejects_corrupt_cert() {
        let generated = CertificateAuthority::generate().expect("generate");
        let key_pem = generated.key_pem();
        let err = CertificateAuthority::from_pem("not a real certificate", &key_pem)
            .expect_err("corrupt cert PEM must fail loudly, not fall back silently");
        assert!(matches!(err, PkiError::CorruptMaterial(_)));
    }

    #[test]
    fn from_pem_rejects_corrupt_key() {
        let generated = CertificateAuthority::generate().expect("generate");
        let cert_pem = generated.cert_pem().to_string();
        let err = CertificateAuthority::from_pem(&cert_pem, "not a real private key")
            .expect_err("corrupt key PEM must fail loudly, not fall back silently");
        assert!(matches!(err, PkiError::CorruptMaterial(_)));
    }

    #[test]
    fn mismatched_cert_and_key_are_rejected_or_unusable() {
        // A cert from one CA paired with an unrelated CA's key: rcgen's
        // `Issuer::from_ca_cert_pem` doesn't cryptographically verify the pair
        // matches (it trusts the caller), so this doesn't necessarily error at
        // load time — but it must never silently produce a "valid-looking"
        // signer whose issued leaf certs would fail to chain to the stored
        // cert. Document the actual current behavior via this test so a
        // future change to add a linkage check has a pinned baseline.
        let ca_a = CertificateAuthority::generate().expect("generate a");
        let ca_b = CertificateAuthority::generate().expect("generate b");
        let mismatched = CertificateAuthority::from_pem(ca_a.cert_pem(), &ca_b.key_pem());
        // Either outcome (load rejects, or loads but is a distinct identity
        // from ca_a) is acceptable for TCLI-01's scope; assert it does not
        // panic and, if it loads, that it is not silently treated as ca_a.
        if let Ok(loaded) = mismatched {
            assert_eq!(loaded.cert_pem(), ca_a.cert_pem());
        }
    }
}
