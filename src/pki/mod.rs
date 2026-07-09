//! Embedded native-Rust PKI (TCLI-01 — Terminus Gateway P2).
//!
//! Terminus is being repositioned as the single mTLS front door for MCP tool
//! traffic (see the operator-authorized Gateway design of record). That needs
//! a certificate authority terminus can stand up itself, with zero manual
//! `step-ca`/OpenSSL bootstrap — this module is that CA.
//!
//! ## What this item builds (and what it deliberately does NOT)
//! - IS: load-or-generate CA bootstrap ([`ca`]), the [`CertificateAuthority`]
//!   type itself ([`ca::CertificateAuthority`], `rcgen`-backed), and the
//!   single accessor other modules use to reach it.
//! - IS NOT: the enrollment endpoint (TCLI-02, issues short-lived leaf certs
//!   signed by this CA) or the mTLS listener (TCLI-03). Both depend on this
//!   item and will call [`ca()`] rather than constructing/loading CA material
//!   themselves — this module is the *only* place CA key material is
//!   generated, parsed, or persisted.
//!
//! ## Where the CA material lives — the "SecretManager" path in this crate
//! terminus-rs's established secret-access convention (see
//! `crate::secrets_bootstrap`, `crate::github::adapter`, `crate::forge::*`)
//! is: the runtime secret store (the same store other `*_bootstrap_*`
//! fetches in this crate use, or an operator-provisioned static env) is
//! materialized into the **process environment** at startup,
//! so a plain env read afterward already IS the "SecretManager" read — there
//! is no separate `SecretManager::get()`/`vault::manager()` API in this crate
//! the way there is in lumina-core/harmony-core. [`bootstrap`] follows that
//! same convention for `TERMINUS_CA_CERT` / `TERMINUS_CA_KEY`.
//!
//! terminus-rs has no *write* path back to that secret store (by design —
//! see the standing "no self-serve secrets" rule; this crate only ever
//! reads a pre-provisioned secret store, it does not mint new secrets into
//! it). So when no CA material is provisioned centrally yet, freshly-
//! generated CA material is persisted to a **local, restrictive-permission
//! (0600) store file** — never a plaintext file at an arbitrary/world-
//! readable path. This mirrors the "file `KeyProvider`" deployment tier
//! documented for lumina-core/harmony-core (self-hosted/homelab: key
//! material on local disk, restrictively permissioned) applied to
//! terminus-rs's simpler env-only secret model. An operator who wants
//! centralized secret-store-backed CA storage instead can provision
//! `TERMINUS_CA_CERT`/`TERMINUS_CA_KEY` directly; [`bootstrap`] prefers
//! those over the local store whenever both are present.
//!
//! ## Load-or-generate precedence
//! 1. `TERMINUS_CA_CERT` + `TERMINUS_CA_KEY` in the process environment
//!    (materialized by `secrets_bootstrap` from the runtime secret store, or
//!    provisioned directly) — loaded, never regenerated. Corrupt material
//!    here is a hard startup error (see below), never a silent regeneration.
//! 2. The local store file at [`crate::config::ca_store_path`] — loaded if
//!    present. Corrupt material here is also a hard error.
//! 3. Neither present (true first run): generate a new CA and persist it to
//!    the local store file. A persistence failure is logged as a warning
//!    (the CA still works for the current process) rather than treated as a
//!    hard error — the alternative (refusing to start because a fresh CA
//!    can't reach disk) is worse than a CA that needs regenerating on the
//!    next restart.
//!
//! ## Edge cases (see the TCLI-01 spec item)
//! - **The runtime secret store is unreachable at startup, only cached
//!   values available:** `secrets_bootstrap`'s own fetch already falls back
//!   to whatever's already in the process env per its own contract; if that
//!   leaves `TERMINUS_CA_CERT`/`_KEY` unset, [`bootstrap`] falls through to
//!   the local store (tier 2 above), which still has a previously-generated
//!   CA — so a live secret-store outage alone never causes a new CA to be
//!   minted.
//! - **SecretManager not yet initialized when the CA bootstraps:** by
//!   convention, callers integrating this module (TCLI-02/03) must invoke
//!   [`ca()`] only *after* `secrets_bootstrap::bootstrap_gitea_plane_github_secrets`
//!   (or equivalent) has run for that binary — this module does not enforce
//!   that ordering itself (it has no dependency on `secrets_bootstrap`), it
//!   only documents the requirement.
//! - **Two terminus processes bootstrapping concurrently against the same
//!   local store:** not solved in P2 (single-primary topology makes it
//!   unlikely) — a documented known limitation, not a silent race.

pub mod ca;
pub mod enroll;
pub mod mtls;

pub use ca::CertificateAuthority;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use thiserror::Error;

/// Errors from CA bootstrap, generation, or persistence.
#[derive(Debug, Error)]
pub enum PkiError {
    /// `rcgen` failed to generate a fresh CA keypair/certificate.
    #[error("failed to generate CA material: {0}")]
    Generation(String),
    /// Stored CA material (env-provisioned or local-store) failed to parse.
    /// Deliberately distinct from [`PkiError::Generation`] — the caller must
    /// treat this as a hard startup failure, never a signal to regenerate.
    #[error("stored CA material is corrupt or unparseable: {0}")]
    CorruptMaterial(String),
    /// Freshly-generated CA material could not be written to the local
    /// store. Not necessarily fatal to the caller (see module docs) but
    /// always surfaced rather than swallowed.
    #[error("failed to persist CA material: {0}")]
    Persistence(String),
}

static CA: OnceLock<CertificateAuthority> = OnceLock::new();

/// The process-wide embedded CA. Bootstraps (load-or-generate) on first call;
/// every later call in the same process returns the identical CA identity —
/// idempotent within a run, per the TCLI-01 test plan. This is the only
/// intended entry point: no other module should construct or load CA
/// material directly (enforced by `ca::CertificateAuthority`'s constructors
/// not being re-exported for that purpose beyond this accessor).
pub fn ca() -> Result<&'static CertificateAuthority, PkiError> {
    if let Some(existing) = CA.get() {
        return Ok(existing);
    }

    // Serialize bootstrap attempts within this process. Without this, two
    // threads/tasks racing to call `ca()` on a cold start could each
    // independently generate-and-persist a *different* CA before either one
    // wins the `OnceLock` — the winner's in-memory CA and the CA left on
    // disk could then diverge, so certs signed during this run would fail to
    // validate against the CA a later restart loads. The lock only costs
    // anything on the (rare) cold-start race; every later call short-circuits
    // on the `CA.get()` check above without ever touching the mutex.
    static INIT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = INIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Double-checked: another thread may have finished bootstrapping and
    // populated `CA` while we were waiting for `INIT_LOCK`.
    if let Some(existing) = CA.get() {
        return Ok(existing);
    }

    let bootstrapped = bootstrap(&crate::config::ca_store_path())?;
    // We're still holding `INIT_LOCK`, and we just re-checked `CA` is empty,
    // so this is always the value that wins `get_or_init` — no divergence
    // between the canonical in-memory CA and what `bootstrap` just persisted.
    // See the module doc's "two *processes* bootstrapping concurrently"
    // known limitation for the cross-process version of this race, which
    // this in-process lock does not (and cannot) solve.
    Ok(CA.get_or_init(move || bootstrapped))
}

/// Load-or-generate the CA from the environment, then the local store, then
/// generation-as-last-resort. Separated from [`ca()`]'s process-wide
/// singleton so tests can exercise each precedence tier independently
/// without fighting the `OnceLock`.
fn bootstrap(store_path: &str) -> Result<CertificateAuthority, PkiError> {
    match (env_nonempty("TERMINUS_CA_CERT"), env_nonempty("TERMINUS_CA_KEY")) {
        (Some(cert_pem), Some(key_pem)) => {
            tracing::info!(
                "pki: loading CA material from the runtime secret store (env-materialized)"
            );
            return CertificateAuthority::from_pem(&cert_pem, &key_pem).map_err(|e| {
                tracing::error!("pki: env-provisioned CA material is corrupt: {e}");
                e
            });
        }
        (None, None) => { /* neither provisioned — fall through to the local store tier */ }
        (cert, _key) => {
            // Exactly one of the pair is set — an operator setup error (e.g.
            // provisioned the cert but forgot the key, or vice versa). This
            // must be a hard error, never a silent fall-through to the local
            // store or fresh generation: that would mask a misconfiguration
            // as "first run" and mint a CA the operator didn't intend to use.
            let missing = if cert.is_none() { "TERMINUS_CA_CERT" } else { "TERMINUS_CA_KEY" };
            tracing::error!(
                "pki: only one of TERMINUS_CA_CERT/TERMINUS_CA_KEY is provisioned \
                 (missing {missing}) — refusing to fall back to the local store or \
                 generate a new CA"
            );
            return Err(PkiError::CorruptMaterial(format!(
                "partial CA env configuration: {missing} is unset while its pair is set"
            )));
        }
    }

    match load_local_store(store_path) {
        Ok(Some((cert_pem, key_pem))) => {
            tracing::info!("pki: loading CA material from local store at {store_path}");
            return CertificateAuthority::from_pem(&cert_pem, &key_pem).map_err(|e| {
                tracing::error!("pki: local CA store material is corrupt: {e}");
                e
            });
        }
        Ok(None) => {}
        Err(e) => {
            // A read/parse failure on an EXISTING store file is corrupt
            // material, not "absent" — must not fall through to generation
            // (that would silently mint a new CA and orphan every
            // previously-issued client cert).
            tracing::error!("pki: local CA store at {store_path} is corrupt: {e}");
            return Err(e);
        }
    }

    tracing::info!("pki: no existing CA material found; generating a new embedded root CA");
    let generated = CertificateAuthority::generate()?;
    if let Err(e) = persist_local_store(store_path, &generated) {
        tracing::warn!(
            "pki: generated a new CA but failed to persist it to {store_path}: {e} — \
             it will regenerate on next restart unless TERMINUS_CA_CERT/TERMINUS_CA_KEY \
             are provisioned via the secret store"
        );
    }
    Ok(generated)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LocalCaStore {
    cert_pem: String,
    key_pem: String,
}

/// Read the local CA store file, if present. `Ok(None)` means "no file at
/// this path" (a legitimate first-run state, not an error). `Err` means a
/// file exists but couldn't be read/parsed — corrupt material, never treated
/// as "absent".
fn load_local_store(path: &str) -> Result<Option<(String, String)>, PkiError> {
    let path = Path::new(path);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|e| {
        PkiError::CorruptMaterial(format!(
            "failed reading local CA store {}: {e}",
            path.display()
        ))
    })?;
    let parsed: LocalCaStore = serde_json::from_str(&raw).map_err(|e| {
        PkiError::CorruptMaterial(format!(
            "local CA store {} is not valid JSON: {e}",
            path.display()
        ))
    })?;
    Ok(Some((parsed.cert_pem, parsed.key_pem)))
}

/// Persist freshly-generated CA material to the local store file with
/// restrictive (0600 on unix) permissions, set BEFORE any content is
/// written. This path is the "vault-managed path" for the local-store tier —
/// never a plaintext file at an arbitrary/world-readable location.
fn persist_local_store(path: &str, ca: &CertificateAuthority) -> Result<(), PkiError> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| PkiError::Persistence(format!("failed creating CA store dir: {e}")))?;
    }

    let store = LocalCaStore {
        cert_pem: ca.cert_pem().to_string(),
        key_pem: ca.key_pem(),
    };
    let json = serde_json::to_string_pretty(&store)
        .map_err(|e| PkiError::Persistence(format!("failed serializing CA store: {e}")))?;

    let mut file = open_restrictive(path)
        .map_err(|e| PkiError::Persistence(format!("failed opening CA store file: {e}")))?;
    file.write_all(json.as_bytes())
        .map_err(|e| PkiError::Persistence(format!("failed writing CA store file: {e}")))?;
    // `.mode(0o600)` on `OpenOptions` only governs the permissions of a
    // *newly created* file — if a file already existed at this path (e.g.
    // left over at looser permissions by an older version of this code, or
    // an operator `touch`), opening it for write+truncate does not change
    // its existing mode. Explicitly (re-)tighten permissions after every
    // write so a pre-existing loose-mode file can never end up holding CA
    // key material world/group-readable.
    tighten_permissions(&file)
        .map_err(|e| PkiError::Persistence(format!("failed to set CA store file permissions: {e}")))?;
    Ok(())
}

#[cfg(unix)]
fn tighten_permissions(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn tighten_permissions(_file: &fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn open_restrictive(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_restrictive(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Each test gets its own store path (a temp dir) so tests never share
    /// on-disk state; `#[serial]` still guards the shared process-env reads
    /// (`TERMINUS_CA_CERT`/`TERMINUS_CA_KEY`) so concurrent test threads
    /// can't stomp on each other's env vars.
    fn temp_store_path(label: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "terminus-pki-test-{label}-{n}-{}",
            std::process::id()
        ));
        path.push("ca_store.json");
        path.to_string_lossy().into_owned()
    }

    fn clear_ca_env() {
        std::env::remove_var("TERMINUS_CA_CERT");
        std::env::remove_var("TERMINUS_CA_KEY");
    }

    #[test]
    #[serial]
    fn fresh_store_generates_and_persists_a_new_ca() {
        clear_ca_env();
        let path = temp_store_path("fresh");
        assert!(!Path::new(&path).exists());

        let ca = bootstrap(&path).expect("bootstrap should generate a new CA");
        assert!(ca.cert_pem().contains("BEGIN CERTIFICATE"));
        assert!(
            Path::new(&path).exists(),
            "a freshly generated CA must be persisted to the local store"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "CA store file must be 0600, not {mode:o}");
        }

        fs::remove_dir_all(Path::new(&path).parent().unwrap()).ok();
    }

    #[test]
    #[serial]
    fn preseeded_store_is_loaded_not_regenerated() {
        clear_ca_env();
        let path = temp_store_path("preseeded");

        let first = bootstrap(&path).expect("first bootstrap generates + persists");
        let first_cert = first.cert_pem().to_string();

        let second = bootstrap(&path).expect("second bootstrap should load, not regenerate");
        assert_eq!(
            second.cert_pem(),
            first_cert,
            "a pre-seeded store must be loaded verbatim, not silently regenerated"
        );

        fs::remove_dir_all(Path::new(&path).parent().unwrap()).ok();
    }

    #[test]
    #[serial]
    fn corrupt_local_store_is_a_hard_error_not_silent_regen() {
        clear_ca_env();
        let path = temp_store_path("corrupt");
        let p = Path::new(&path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"this is not valid JSON CA material").unwrap();

        let err = bootstrap(&path).expect_err("corrupt store must fail loudly");
        assert!(matches!(err, PkiError::CorruptMaterial(_)));

        fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    #[serial]
    fn env_provisioned_material_takes_precedence_over_local_store() {
        clear_ca_env();
        let path = temp_store_path("env-precedence");

        // Seed the local store with one CA.
        let local_ca = bootstrap(&path).expect("seed local store");
        let local_cert = local_ca.cert_pem().to_string();

        // Provision a DIFFERENT CA via env — this must win.
        let env_ca = CertificateAuthority::generate().expect("generate env CA");
        std::env::set_var("TERMINUS_CA_CERT", env_ca.cert_pem());
        std::env::set_var("TERMINUS_CA_KEY", env_ca.key_pem());

        let loaded = bootstrap(&path).expect("bootstrap with env vars set");
        // `env_nonempty` trims the env value (guards against a trailing
        // newline from env injection, the same convention documented in
        // `crate::github::adapter`), so compare trimmed.
        assert_eq!(loaded.cert_pem().trim(), env_ca.cert_pem().trim());
        assert_ne!(loaded.cert_pem(), local_cert);

        clear_ca_env();
        fs::remove_dir_all(Path::new(&path).parent().unwrap()).ok();
    }

    #[test]
    #[serial]
    fn corrupt_env_material_is_a_hard_error() {
        let path = temp_store_path("corrupt-env");
        std::env::set_var("TERMINUS_CA_CERT", "not a cert");
        std::env::set_var("TERMINUS_CA_KEY", "not a key");

        let err = bootstrap(&path).expect_err("corrupt env-provisioned material must fail loudly");
        assert!(matches!(err, PkiError::CorruptMaterial(_)));

        clear_ca_env();
    }

    #[test]
    #[serial]
    fn partial_env_config_is_a_hard_error_missing_key() {
        clear_ca_env();
        let path = temp_store_path("partial-env-missing-key");
        std::env::set_var(
            "TERMINUS_CA_CERT",
            CertificateAuthority::generate().unwrap().cert_pem(),
        );
        std::env::remove_var("TERMINUS_CA_KEY");

        let err = bootstrap(&path)
            .expect_err("cert set without key must fail loudly, not fall back to local store");
        assert!(matches!(err, PkiError::CorruptMaterial(_)));
        assert!(!Path::new(&path).exists(), "must not fall through to generation");

        clear_ca_env();
    }

    #[test]
    #[serial]
    fn partial_env_config_is_a_hard_error_missing_cert() {
        clear_ca_env();
        let path = temp_store_path("partial-env-missing-cert");
        std::env::set_var("TERMINUS_CA_KEY", CertificateAuthority::generate().unwrap().key_pem());
        std::env::remove_var("TERMINUS_CA_CERT");

        let err = bootstrap(&path)
            .expect_err("key set without cert must fail loudly, not fall back to local store");
        assert!(matches!(err, PkiError::CorruptMaterial(_)));

        clear_ca_env();
    }

    #[test]
    fn preexisting_loose_permissions_are_tightened_on_persist() {
        let path = temp_store_path("loose-perms");
        let p = Path::new(&path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        // Pre-create the file at a deliberately loose mode, simulating a
        // leftover file from an older version of this code or an operator
        // `touch`, then persist directly (bypassing `bootstrap`'s
        // load-vs-generate branching, which isn't what this test targets).
        fs::write(p, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let ca = CertificateAuthority::generate().expect("generate");
        persist_local_store(&path, &ca).expect("persist over the loose-permission file");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "pre-existing loose permissions must be tightened to 0600, not left at {mode:o}"
            );
        }

        fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    #[serial]
    fn process_wide_accessor_is_idempotent_within_a_run() {
        // `ca()` uses a process-wide OnceLock, so this test only proves the
        // *caching* contract (same reference every call) — it cannot
        // independently re-seed `CA` per test the way `bootstrap()` can,
        // since the singleton is shared with every other test in this
        // process. Only assert internal consistency, not any particular
        // identity, to avoid coupling to test execution order. Point
        // TERMINUS_CA_STORE_PATH at a temp dir first so this doesn't write
        // into the real default local-store path if `ca()` happens to be the
        // first caller in the test binary.
        clear_ca_env();
        std::env::set_var("TERMINUS_CA_STORE_PATH", temp_store_path("process-wide"));
        let first = ca().expect("first ca() call");
        let second = ca().expect("second ca() call");
        assert_eq!(
            first.cert_pem(),
            second.cert_pem(),
            "ca() must return the same CA identity on every call within a process"
        );
        assert!(std::ptr::eq(first, second), "ca() must return the same instance");
    }
}
