//! Enrollment client (TCLI-04, depends on the server-side TCLI-02
//! `/enroll` endpoint). Calls a terminus primary's enrollment endpoint with
//! a per-identity shared secret, receives a short-lived CA-signed leaf
//! cert + JWT, and persists it locally so a later process on the same host
//! doesn't have to re-enroll on every startup.
//!
//! ## Where the credential lives -- the "SecretManager-style" local store
//! This crate is meant to be embedded into other programs (Harmony, Lumina,
//! Scribe -- see the crate README), each with its own secret-store
//! convention, so it does not assume a shared `SecretManager`/`vault` API
//! is available. It instead follows the SAME pattern the terminus-rs server
//! side already established for its own local-only material
//! (`terminus_rs::pki`'s CA local store): a JSON file at a
//! restrictive-permission (0600 on unix) path, never a
//! world/group-readable plaintext file at an arbitrary location. An
//! embedding program that has a real `SecretManager`/vault of its own is
//! free to skip this and instead persist the returned [`EnrolledCredential`]
//! through that mechanism directly -- [`enroll`] returns the credential to
//! the caller either way, the local store is a convenience default, not a
//! requirement.
//!
//! The bootstrap shared secret itself is NEVER read from the environment or
//! any file by this module -- per the TCLI-04 spec item, it is supplied by
//! the CALLING program (via [`EnrollConfig::shared_secret`]), sourced
//! however that program's own secret store works. This crate does not
//! hardcode, cache, or embed a secret of its own.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::ClientError;

/// Default enrollment HTTP path, matching the server's
/// `terminus_rs::config::enrollment_path()` default.
const DEFAULT_ENROLLMENT_PATH: &str = "/enroll";

/// How long before an enrolled credential's `expires_at` a fresh
/// enrollment is triggered instead of reusing the stored one (TCLI-04
/// "renew before expiry"). Configurable via
/// `TERMINUS_CLIENT_RENEWAL_MARGIN_SECS`; defaults to 300s (5 minutes) --
/// comfortably inside the server's shortest default TTL (the 1800s JWT
/// TTL), so a client renews before either the cert or the JWT expires.
const DEFAULT_RENEWAL_MARGIN_SECS: i64 = 300;

/// Configuration for one [`enroll`] call.
#[derive(Debug, Clone)]
pub struct EnrollConfig {
    /// Base URL of the terminus primary's plain HTTP+JWT listener, e.g.
    /// `"http://127.0.0.1:8300"`. The enrollment path is appended to this
    /// (see [`EnrollConfig::enrollment_path`]).
    pub primary_url: String,
    /// The identity name this client enrolls as -- embedded in the issued
    /// cert's CN/SAN and the JWT's `sub` claim by the server. Must satisfy
    /// the server's naming pattern (lowercase alphanumerics/hyphens,
    /// 2-63 chars).
    pub identity: String,
    /// The bootstrap credential, supplied by the calling program from ITS
    /// OWN secret store. Never logged, never persisted alongside the local
    /// credential store.
    pub shared_secret: String,
    /// Path to the local credential store file. Defaults (via
    /// [`EnrollConfig::new`]) to `~/.terminus-client/credentials/<identity>.json`
    /// so multiple enrolled identities on the same host don't collide.
    pub store_path: PathBuf,
    /// HTTP path the enrollment endpoint is mounted at on `primary_url`.
    /// Defaults to `/enroll`, matching the server's own default.
    pub enrollment_path: String,
    /// Renewal margin, in seconds -- see [`DEFAULT_RENEWAL_MARGIN_SECS`].
    pub renewal_margin_secs: i64,
}

impl EnrollConfig {
    /// Build a config with the crate's documented defaults for
    /// `store_path`/`enrollment_path`/`renewal_margin_secs`.
    pub fn new(primary_url: impl Into<String>, identity: impl Into<String>, shared_secret: impl Into<String>) -> Self {
        let identity = identity.into();
        Self {
            primary_url: primary_url.into(),
            store_path: default_store_path(&identity),
            identity,
            shared_secret: shared_secret.into(),
            enrollment_path: DEFAULT_ENROLLMENT_PATH.to_string(),
            renewal_margin_secs: DEFAULT_RENEWAL_MARGIN_SECS,
        }
    }
}

fn default_store_path(identity: &str) -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".terminus-client")
        .join("credentials")
        .join(format!("{identity}.json"))
}

/// A successful (or previously persisted) enrollment result: the leaf cert
/// + private key for this identity, the pinned CA cert to validate the
/// primary's server cert against, and the paired application-layer JWT.
///
/// Field names deliberately mirror the server's
/// `terminus_rs::pki::enroll::EnrollmentResponse` JSON shape byte-for-byte
/// (this crate matches it structurally via serde, not by depending on
/// `terminus-rs` -- see this module's doc comment and the workspace
/// `Cargo.toml` note on why `terminus-client` has no path dependency on
/// `terminus-rs`).
#[derive(Clone, Serialize, Deserialize)]
pub struct EnrolledCredential {
    /// PEM-encoded leaf certificate, signed by the primary's embedded CA.
    pub cert_pem: String,
    /// PEM-encoded private key for `cert_pem`.
    pub key_pem: String,
    /// The primary's CA certificate, pinned locally -- [`crate::connect`]
    /// trusts ONLY this CA, never the system trust store.
    pub ca_cert_pem: String,
    /// Short-lived application-layer JWT, carried per-request by TCLI-05/06.
    pub jwt: String,
    /// Unix timestamp (seconds) this credential should be considered
    /// expired by.
    pub expires_at: i64,
}

impl std::fmt::Debug for EnrolledCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Same redaction convention as the server-side
        // `EnrollmentResponse::fmt` -- `key_pem`/`jwt` are secret-ish and
        // never appear in a Debug print (e.g. an accidental `{:?}` in a log
        // line further up the call stack).
        f.debug_struct("EnrolledCredential")
            .field("cert_pem", &self.cert_pem)
            .field("key_pem", &"<redacted>")
            .field("ca_cert_pem", &self.ca_cert_pem)
            .field("jwt", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl EnrolledCredential {
    /// True if this credential's `expires_at` is still further out than
    /// `margin_secs` from now -- i.e. it's safe to reuse without
    /// re-enrolling.
    fn is_still_valid(&self, margin_secs: i64) -> bool {
        let now = now_unix();
        self.expires_at - now > margin_secs
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Serialize)]
struct EnrollmentRequestBody<'a> {
    identity: &'a str,
    shared_secret: &'a str,
}

/// Enroll (or reuse a still-valid prior enrollment for) `cfg.identity`
/// against `cfg.primary_url`.
///
/// Per the TCLI-04 spec item's APPROACH step 3: if the local store at
/// `cfg.store_path` holds credential material that is still valid (more
/// than `cfg.renewal_margin_secs` from expiry), that credential is returned
/// WITHOUT calling the enrollment endpoint again -- avoids hammering
/// `/enroll` on every process start. A corrupted/unparseable store is
/// treated as "no usable credential" (self-healing re-enroll), never a hard
/// failure -- see the TCLI-04 EDGE CASES.
pub async fn enroll(cfg: &EnrollConfig) -> Result<EnrolledCredential, ClientError> {
    if let Some(existing) = load_local_credential(&cfg.store_path) {
        if existing.is_still_valid(cfg.renewal_margin_secs) {
            tracing::debug!(
                identity = %cfg.identity,
                "terminus_client::enroll: reusing still-valid local credential, skipping re-enrollment"
            );
            return Ok(existing);
        }
        tracing::info!(
            identity = %cfg.identity,
            "terminus_client::enroll: local credential is absent/expired/near-expiry; enrolling"
        );
    }

    let credential = call_enrollment_endpoint(cfg).await?;
    persist_local_credential(&cfg.store_path, &credential)?;
    Ok(credential)
}

async fn call_enrollment_endpoint(cfg: &EnrollConfig) -> Result<EnrolledCredential, ClientError> {
    let url = format!(
        "{}{}",
        cfg.primary_url.trim_end_matches('/'),
        cfg.enrollment_path
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&EnrollmentRequestBody {
            identity: &cfg.identity,
            shared_secret: &cfg.shared_secret,
        })
        .send()
        .await
        .map_err(|e| ClientError::EnrollmentUnreachable(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ClientError::EnrollmentRejected {
            status: status.as_u16(),
            body,
        });
    }

    resp.json::<EnrolledCredential>()
        .await
        .map_err(|e| ClientError::MalformedResponse(e.to_string()))
}

/// Read the local credential store, if present and parseable. Returns
/// `None` for BOTH "no file at this path" (legitimate first run) AND "file
/// present but corrupt/unparseable" -- unlike the server-side CA store
/// (`terminus_rs::pki::bootstrap`), a corrupt *credential* here is
/// deliberately self-healing (re-enroll is cheap; the credential is
/// short-lived anyway), per the TCLI-04 EDGE CASES -- never a hard startup
/// failure.
fn load_local_credential(path: &Path) -> Option<EnrolledCredential> {
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<EnrolledCredential>(&raw) {
        Ok(cred) => Some(cred),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                "terminus_client::enroll: local credential store is corrupt ({e}); re-enrolling"
            );
            None
        }
    }
}

/// Persist `credential` to `path` with restrictive (0600 on unix)
/// permissions, set before any content is written -- mirrors
/// `terminus_rs::pki::mod::persist_local_store`'s convention for the
/// server-side CA store.
fn persist_local_credential(path: &Path, credential: &EnrolledCredential) -> Result<(), ClientError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ClientError::Store(format!("failed creating credential store dir: {e}")))?;
    }

    let json = serde_json::to_string_pretty(credential)
        .map_err(|e| ClientError::Store(format!("failed serializing credential: {e}")))?;

    let mut file = open_restrictive(path)
        .map_err(|e| ClientError::Store(format!("failed opening credential store file: {e}")))?;
    use std::io::Write;
    file.write_all(json.as_bytes())
        .map_err(|e| ClientError::Store(format!("failed writing credential store file: {e}")))?;
    tighten_permissions(&file)
        .map_err(|e| ClientError::Store(format!("failed to set credential store file permissions: {e}")))?;
    Ok(())
}

#[cfg(unix)]
fn tighten_permissions(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn tighten_permissions(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn open_restrictive(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_restrictive(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Json as AxumJson;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn temp_store_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "terminus-client-test-{label}-{n}-{}",
            std::process::id()
        ));
        path.push("credential.json");
        path
    }

    fn sample_credential(identity: &str, expires_in_secs: i64) -> EnrolledCredential {
        EnrolledCredential {
            cert_pem: format!("-----BEGIN CERTIFICATE-----\n{identity}\n-----END CERTIFICATE-----\n"),
            key_pem: "<REDACTED-SECRET>\nkey\n-----END PRIVATE KEY-----\n".to_string(),
            ca_cert_pem: "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n".to_string(),
            jwt: "test.jwt.token".to_string(),
            expires_at: now_unix() + expires_in_secs,
        }
    }

    /// Spins up a mock `/enroll` endpoint on loopback returning a fixed
    /// [`EnrolledCredential`] (or a rejection), independent of any live
    /// terminus primary.
    async fn spawn_mock_enroll_server(
        expect_secret: &'static str,
        response: EnrolledCredential,
    ) -> String {
        let call_count = Arc::new(Mutex::new(0u32));
        let cc = call_count.clone();
        let app = axum::Router::new().route(
            "/enroll",
            post(move |AxumJson(body): AxumJson<serde_json::Value>| {
                let response = response.clone();
                let cc = cc.clone();
                async move {
                    *cc.lock().await += 1;
                    let secret = body.get("shared_secret").and_then(|v| v.as_str()).unwrap_or("");
                    if secret != expect_secret {
                        return (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(json!({"error": "invalid or missing enrollment shared secret"})),
                        )
                            .into_response();
                    }
                    (StatusCode::OK, axum::Json(response)).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn enroll_against_mock_endpoint_succeeds_and_persists() {
        let expected = sample_credential("dev-box-claude-code", 3600);
        let base_url = spawn_mock_enroll_server("s3cret", expected.clone()).await;
        let store_path = temp_store_path("succeeds");

        let cfg = EnrollConfig::new(base_url, "dev-box-claude-code", "s3cret");
        let cfg = EnrollConfig { store_path: store_path.clone(), ..cfg };

        let got = enroll(&cfg).await.expect("enroll should succeed");
        assert_eq!(got.cert_pem, expected.cert_pem);
        assert_eq!(got.jwt, expected.jwt);
        assert!(store_path.exists(), "enroll() must persist the credential locally");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credential store file must be 0600, not {mode:o}");
        }

        std::fs::remove_dir_all(store_path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn enroll_rejects_wrong_shared_secret() {
        let expected = sample_credential("dev-box-claude-code", 3600);
        let base_url = spawn_mock_enroll_server("correct-secret", expected).await;
        let store_path = temp_store_path("wrong-secret");

        let cfg = EnrollConfig::new(base_url, "dev-box-claude-code", "wrong-secret");
        let cfg = EnrollConfig { store_path, ..cfg };

        let err = enroll(&cfg).await.expect_err("wrong secret must be rejected");
        assert!(matches!(err, ClientError::EnrollmentRejected { status: 401, .. }));
    }

    #[tokio::test]
    async fn second_enroll_call_with_valid_material_skips_reenrollment() {
        // Pre-seed the local store with a still-valid credential directly
        // (bypassing the HTTP call), then point `enroll()` at a server that
        // would reject any real enrollment attempt -- if `enroll()` called
        // it anyway, this test would fail via `EnrollmentRejected`.
        let store_path = temp_store_path("skip-reenroll");
        let preseeded = sample_credential("dev-box-claude-code", 3600);
        persist_local_credential(&store_path, &preseeded).unwrap();

        let base_url = spawn_mock_enroll_server("never-matches", sample_credential("other", 3600)).await;
        let cfg = EnrollConfig::new(base_url, "dev-box-claude-code", "irrelevant");
        let cfg = EnrollConfig { store_path: store_path.clone(), ..cfg };

        let got = enroll(&cfg).await.expect("should reuse local credential, not re-enroll");
        assert_eq!(got.cert_pem, preseeded.cert_pem);

        std::fs::remove_dir_all(store_path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn near_expiry_credential_triggers_reenrollment() {
        let store_path = temp_store_path("near-expiry");
        // Expires in 60s, well inside the default 300s renewal margin.
        let stale = sample_credential("dev-box-claude-code", 60);
        persist_local_credential(&store_path, &stale).unwrap();

        let fresh = sample_credential("dev-box-claude-code", 3600);
        let base_url = spawn_mock_enroll_server("s3cret", fresh.clone()).await;
        let cfg = EnrollConfig::new(base_url, "dev-box-claude-code", "s3cret");
        let cfg = EnrollConfig { store_path: store_path.clone(), ..cfg };

        let got = enroll(&cfg).await.expect("should re-enroll a near-expiry credential");
        assert_eq!(got.expires_at, fresh.expires_at);
        assert_ne!(got.expires_at, stale.expires_at);

        std::fs::remove_dir_all(store_path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn corrupted_local_store_self_heals_via_reenrollment() {
        let store_path = temp_store_path("corrupt");
        std::fs::create_dir_all(store_path.parent().unwrap()).unwrap();
        std::fs::write(&store_path, b"not valid json").unwrap();

        let fresh = sample_credential("dev-box-claude-code", 3600);
        let base_url = spawn_mock_enroll_server("s3cret", fresh.clone()).await;
        let cfg = EnrollConfig::new(base_url, "dev-box-claude-code", "s3cret");
        let cfg = EnrollConfig { store_path: store_path.clone(), ..cfg };

        let got = enroll(&cfg).await.expect("corrupt store must self-heal, not hard-fail");
        assert_eq!(got.jwt, fresh.jwt);

        std::fs::remove_dir_all(store_path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn enrollment_endpoint_unreachable_returns_typed_error() {
        let store_path = temp_store_path("unreachable");
        // Nothing listening on this port -- connection must be refused.
        let cfg = EnrollConfig::new("http://127.0.0.1:1", "dev-box-claude-code", "s3cret");
        let cfg = EnrollConfig { store_path, ..cfg };

        let err = enroll(&cfg).await.expect_err("unreachable endpoint must be a typed error");
        assert!(matches!(err, ClientError::EnrollmentUnreachable(_)));
    }
}
