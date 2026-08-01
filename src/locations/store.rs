//! LOCREG-01 persistence: the on-disk shape of the location registry, and the
//! seam every consumer reads it through.
//!
//! ## Why a JSON file and not a new datastore
//!
//! The spec is explicit that this must reuse what Terminus already has. Three
//! candidates existed and only one works everywhere this registry has to work:
//!
//! * **Redis** (`crate::redis`) is the obvious general K/V, and
//!   `crate::plane::prefix::PrefixOverlay` is already "a small keyed registry on
//!   the shared Redis". But the CLIENT gateway deliberately runs with no Redis
//!   at all (`crate::plane::prefix` documents that as a security posture — no
//!   shared mutable state reachable from the client surface). A registry that
//!   silently has no memory on the client surface would produce exactly the
//!   failure this item exists to prevent: a stored home that reads back as "not
//!   set".
//! * **Postgres** would mean a new schema and a migration, and `crate::pg`'s own
//!   module doc draws a hard line around agent/admin access versus an
//!   application data path. Adding a table for four strings is a new datastore
//!   in everything but name.
//! * **A whole-document JSON file** is the idiom this repo already uses for
//!   small non-secret operator state — `crate::meridian::state` (mutex + atomic
//!   temp-file-and-rename), `crate::forge::git_public`'s activation ledger, and
//!   `crate::compat::prompt`'s per-user layer directory. It works identically on
//!   the client gateway and the server, needs no service to be up, and it is
//!   trivially testable.
//!
//! So: one JSON document, one in-process mutex, atomic temp-file-and-rename,
//! `0600` on Unix. The file is small by construction (a handful of short strings
//! per caller) and is only ever read/written on an explicitly entitled tool call.
//!
//! ## The one deliberate divergence from `meridian::state`
//!
//! `meridian::state::load` treats an unparsable file as "no portfolio yet" and
//! returns a fresh default. Doing that HERE would be a bug of exactly the class
//! the spec calls out: a corrupt or unreadable registry would report "you have
//! no home set", which is indistinguishable from the truthful answer and would
//! invite the assistant to fill the gap. A missing file is `Ok(empty)` — that
//! genuinely IS "nothing stored yet". Anything else is `Err` and surfaces as
//! "could not read the registry". Absence and failure never collapse into one
//! another.
//!
//! ## What is NOT in here
//!
//! No entitlement logic. This module knows how to read and write a document; it
//! has no opinion about who may. The gate lives one level up in
//! [`crate::locations`], BEFORE a store is ever touched, so an unentitled caller
//! causes zero reads rather than a read whose result is later discarded.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Env var naming the registry file. Non-secret configuration (a path), so it
/// is a plain env read rather than a `SecretManager` lookup — the same
/// treatment `MERIDIAN_STATE_PATH` and `TERMINUS_CA_STORE_PATH` get. The
/// CONTENTS are sensitive; the path is not.
pub const REGISTRY_PATH_ENV: &str = "TERMINUS_LOCATION_REGISTRY_PATH";

/// Current on-disk schema version. Bumped only for a breaking layout change; a
/// document from the future is refused rather than silently misread (see
/// [`Registry::check_version`]).
pub const SCHEMA_VERSION: u32 = 1;

/// Serializes every read-modify-write so two concurrent `location_set` calls
/// cannot interleave and lose one another's entry. IO here is a few KB and
/// synchronous, so holding this across the whole operation with no `.await` in
/// between is fine from an async tool handler — the same reasoning
/// `crate::meridian::state::STATE_LOCK` documents.
static STATE_LOCK: Mutex<()> = Mutex::new(());

/// Why a store operation failed.
///
/// Deliberately CATEGORICAL and content-free. A home address is among the most
/// sensitive values this fleet holds, and error strings end up in logs, tool
/// output and model context — so this type can carry an `io::ErrorKind` or a
/// parse category but NEVER a stored value, and never the caller's key. The
/// path is omitted too: it is operator-configurable and can embed a username.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The file exists but could not be read (permissions, IO).
    Unreadable(std::io::ErrorKind),
    /// The file was read but is not a registry document we understand.
    Corrupt,
    /// The document declares a schema version this build does not know.
    UnknownVersion(u32),
    /// The write did not land.
    WriteFailed(std::io::ErrorKind),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unreadable(k) => write!(f, "the location registry could not be read ({k:?})"),
            StoreError::Corrupt => write!(f, "the location registry file is not readable as a registry"),
            StoreError::UnknownVersion(v) => {
                write!(f, "the location registry is written in a newer format (version {v})")
            }
            StoreError::WriteFailed(k) => write!(f, "the location registry could not be written ({k:?})"),
        }
    }
}

/// One stored place.
///
/// `expires_at_unix: None` is permanent; `Some(t)` is temporary and stops
/// resolving at `t`. The expiry is stored as an absolute instant rather than a
/// duration precisely so a temporary location can never quietly become
/// permanent by being reloaded — there is no "duration since what?" to get
/// wrong, and a process that never restarts and one that restarts hourly agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredLocation {
    /// The place itself, as the user said it ("Denver", "12 Example Rd, Exampletown").
    pub value: String,
    /// Absolute expiry (Unix seconds). `None` = permanent.
    #[serde(default)]
    pub expires_at_unix: Option<i64>,
    /// When this entry was last written (Unix seconds), for `location_list`.
    pub updated_at_unix: i64,
}

impl StoredLocation {
    pub fn permanent(value: impl Into<String>, now_unix: i64) -> Self {
        Self { value: value.into(), expires_at_unix: None, updated_at_unix: now_unix }
    }

    pub fn temporary(value: impl Into<String>, expires_at_unix: i64, now_unix: i64) -> Self {
        Self { value: value.into(), expires_at_unix: Some(expires_at_unix), updated_at_unix: now_unix }
    }

    /// Has this entry passed its expiry? Permanent entries never have.
    ///
    /// The comparison is `>=` so an entry is dead ON its expiry second rather
    /// than one second after it — a boundary that only matters because getting
    /// it the other way would let an expired travel location answer one more
    /// question than the user authorised.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        matches!(self.expires_at_unix, Some(t) if now_unix >= t)
    }

    pub fn is_temporary(&self) -> bool {
        self.expires_at_unix.is_some()
    }
}

/// Everything one caller has stored, keyed by the caller's chosen NAME
/// (`home`, `work`, `current`, `mum's house`, …).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerRecord {
    #[serde(default)]
    pub locations: BTreeMap<String, StoredLocation>,
}

/// The whole document: every caller's record, keyed by
/// [`crate::locations::CallerKey::storage_key`].
///
/// One document rather than a file per caller keeps the atomic-rename story
/// simple (one temp file, one rename, no partial multi-file state) and means a
/// caller key never has to be made filesystem-safe — it is a JSON map key, so
/// there is no sanitiser to get wrong and no way for two distinct keys to
/// collide on one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub version: u32,
    #[serde(default)]
    pub callers: BTreeMap<String, CallerRecord>,
}

impl Default for Registry {
    fn default() -> Self {
        Self { version: SCHEMA_VERSION, callers: BTreeMap::new() }
    }
}

impl Registry {
    fn check_version(&self) -> Result<(), StoreError> {
        if self.version > SCHEMA_VERSION {
            // Refuse rather than guess. A newer writer may have added fields
            // whose absence changes meaning; reading it as v1 could resolve a
            // location the newer format had already retired.
            return Err(StoreError::UnknownVersion(self.version));
        }
        Ok(())
    }

    /// This caller's entries, or `None` when the caller has never stored one.
    pub fn caller(&self, storage_key: &str) -> Option<&CallerRecord> {
        self.callers.get(storage_key)
    }

    pub fn caller_mut(&mut self, storage_key: &str) -> &mut CallerRecord {
        self.callers.entry(storage_key.to_string()).or_default()
    }
}

/// The seam consumers read the registry through.
///
/// It exists for two reasons, both load-bearing:
///
/// 1. The entitlement tests must be able to prove that an unentitled caller
///    causes **zero reads** — not merely that no location appears in the answer.
///    That is only assertable against a store that COUNTS its calls, which
///    means the production path has to go through a trait object too.
/// 2. "Not set" and "could not read" must stay distinguishable all the way out
///    to the user, which means a test needs to be able to inject a failing
///    store. A path-injection seam (the repo's other file stores use one) cannot
///    produce a read failure deterministically; a trait can.
///
/// Synchronous on purpose: the document is a few KB, `crate::meridian::state`
/// sets the precedent for doing this file IO inline from an async handler, and
/// a sync trait keeps the fakes trivial.
pub trait LocationStore: Send + Sync {
    /// Read the whole document. A MISSING file is `Ok(Registry::default())` —
    /// that is genuinely "nothing stored yet". Every other failure is `Err`.
    fn load(&self) -> Result<Registry, StoreError>;

    /// Replace the whole document, atomically.
    fn save(&self, registry: &Registry) -> Result<(), StoreError>;

    /// Read, modify and write back as ONE serialized transaction.
    ///
    /// This exists because `load()` then `save()` is not a transaction: each
    /// takes the lock separately, so two concurrent `location_set` calls both
    /// read the pre-state, both write their own copy, and the later write
    /// silently discards the earlier one. A user would report that as "it forgot
    /// where I live" — a stored location vanishing with no error is exactly the
    /// data-loss-looks-like-absence failure this module was built to prevent.
    ///
    /// The closure sees the CURRENT document and returns [`Commit::Save`] to
    /// persist its mutations or [`Commit::Abort`] to leave the document
    /// untouched (the needs-confirmation and nothing-to-clear paths). It runs
    /// with the store's lock HELD, so it must not block or re-enter the store.
    ///
    /// Ordinary `load`/`save` remain for the read-only paths (`lookup`, `list`),
    /// which need no transaction — one consistent snapshot is the whole job.
    fn update(&self, f: &mut dyn FnMut(&mut Registry) -> Commit) -> Result<(), StoreError>;
}

/// What [`LocationStore::update`]'s closure decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    /// Persist the mutated document.
    Save,
    /// Persist nothing — the operation declined to write.
    Abort,
}

/// The production store: one JSON file, mutex-guarded, atomically replaced.
pub struct FileLocationStore {
    path: PathBuf,
}

impl FileLocationStore {
    /// Point the store at an explicit path. Also the test constructor — tests
    /// hand it a `tempfile::TempDir` child.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The configured path, or the default under the user's home.
    ///
    /// `~/.terminus/locations.json` mirrors where `crate::pki` puts its CA store
    /// — a per-installation, non-shared, user-owned location. If the home
    /// directory cannot be determined we fall back to the temp dir rather than
    /// failing: a registry that cannot be located degrades to "nothing stored",
    /// which is the honest and safe answer, and every write still reports
    /// success or failure truthfully.
    pub fn from_env() -> Self {
        let path = std::env::var(REGISTRY_PATH_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_path);
        Self::at(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where this store stages a write before renaming it into place.
    ///
    /// Deterministic and PROCESS-scoped: two processes get different names
    /// (different pids) and two threads in one process cannot both be here at
    /// once (every write path holds [`STATE_LOCK`]). So the only way this name
    /// is ever already occupied is a previous run of this pid that died between
    /// create and rename — a STALE file, possibly with permissions we did not
    /// choose. [`create_private`] therefore removes it rather than truncating
    /// it. Exposed to the tests so the stale-file case can be set up exactly.
    pub(crate) fn temp_path(&self) -> PathBuf {
        self.path.with_extension(format!("tmp-{}", std::process::id()))
    }

    /// `load` without the lock — for use inside an already-locked section.
    fn load_locked(&self) -> Result<Registry, StoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => {
                // An empty file is what a crashed/truncated write leaves behind.
                // Treating it as an empty registry would be the same silent
                // data-loss-looks-like-absence bug; treat it as corrupt.
                if raw.trim().is_empty() {
                    return Err(StoreError::Corrupt);
                }
                let reg: Registry = serde_json::from_str(&raw).map_err(|_| StoreError::Corrupt)?;
                reg.check_version()?;
                Ok(reg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(StoreError::Unreadable(e.kind())),
        }
    }

    /// `save` without the lock — for use inside an already-locked section.
    fn save_locked(&self, registry: &Registry) -> Result<(), StoreError> {
        let json = serde_json::to_string_pretty(registry).map_err(|_| StoreError::Corrupt)?;

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| StoreError::WriteFailed(e.kind()))?;
            }
        }

        let tmp = self.temp_path();
        {
            let mut f = create_private(&tmp).map_err(|e| StoreError::WriteFailed(e.kind()))?;
            f.write_all(json.as_bytes()).map_err(|e| StoreError::WriteFailed(e.kind()))?;
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            StoreError::WriteFailed(e.kind())
        })
    }
}

fn default_path() -> PathBuf {
    match dirs::home_dir() {
        Some(h) => h.join(".terminus").join("locations.json"),
        None => std::env::temp_dir().join("terminus-locations.json"),
    }
}

impl LocationStore for FileLocationStore {
    fn load(&self) -> Result<Registry, StoreError> {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.load_locked()
    }

    fn save(&self, registry: &Registry) -> Result<(), StoreError> {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.save_locked(registry)
    }

    /// ONE lock acquisition spanning the read, the mutation and the write — so
    /// a concurrent writer cannot slip between them and have its entry
    /// overwritten by our stale copy.
    fn update(&self, f: &mut dyn FnMut(&mut Registry) -> Commit) -> Result<(), StoreError> {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = self.load_locked()?;
        match f(&mut registry) {
            Commit::Save => self.save_locked(&registry),
            Commit::Abort => Ok(()),
        }
    }
}

/// Create the temp file fresh and owner-readable ONLY.
///
/// The registry holds home addresses, so the file must be `0600` before a
/// single byte is written rather than chmod-ed afterwards — there must be no
/// window in which a world-readable file contains one. Because the file is
/// finalised by `rename`, the destination inherits these bits.
///
/// `mode()` alone does NOT achieve that: it applies only when the open CREATES
/// the file. An existing [`FileLocationStore::temp_path`] left behind by a
/// crashed run with the same pid — created by anything, with any permissions —
/// would have been TRUNCATED and its looser mode kept, and then written full of
/// addresses. So: remove any stale file first and open with `create_new`, which
/// fails rather than reusing someone else's inode. A stale file that cannot be
/// removed is a write FAILURE, never a "well, write into it anyway".
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    remove_stale(path)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    remove_stale(path)?;
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)
}

/// Delete a leftover temp file. "Already gone" is success; anything else is a
/// real failure and must propagate, because the alternative is writing a home
/// address into a file whose permissions we did not choose.
fn remove_stale(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! Offline stores for tests: one that COUNTS reads (so "an unentitled
    //! caller causes zero reads" is assertable as a fact about the store, not
    //! about the answer) and one that always FAILS (so "could not read" is
    //! reachable deterministically).

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    pub struct CountingStore {
        pub reads: AtomicUsize,
        pub writes: AtomicUsize,
        inner: Mutex<Registry>,
    }

    impl CountingStore {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn seeded(registry: Registry) -> Self {
            Self { reads: AtomicUsize::new(0), writes: AtomicUsize::new(0), inner: Mutex::new(registry) }
        }

        pub fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }

        pub fn writes(&self) -> usize {
            self.writes.load(Ordering::SeqCst)
        }

        /// The document as it currently stands, for assertions.
        pub fn snapshot(&self) -> Registry {
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl LocationStore for CountingStore {
        fn load(&self) -> Result<Registry, StoreError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone())
        }

        fn save(&self, registry: &Registry) -> Result<(), StoreError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = registry.clone();
            Ok(())
        }

        /// Atomic like the real store: the lock is held across the whole
        /// closure. Counted as one read and (when it commits) one write, so the
        /// zero-reads entitlement assertions keep meaning what they meant.
        fn update(&self, f: &mut dyn FnMut(&mut Registry) -> Commit) -> Result<(), StoreError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut working = guard.clone();
            if f(&mut working) == Commit::Save {
                self.writes.fetch_add(1, Ordering::SeqCst);
                *guard = working;
            }
            Ok(())
        }
    }

    /// A store whose every operation fails — the "could not read" fixture.
    pub struct BrokenStore;

    impl LocationStore for BrokenStore {
        fn load(&self) -> Result<Registry, StoreError> {
            Err(StoreError::Unreadable(std::io::ErrorKind::PermissionDenied))
        }
        fn save(&self, _registry: &Registry) -> Result<(), StoreError> {
            Err(StoreError::WriteFailed(std::io::ErrorKind::PermissionDenied))
        }
        /// Fails at the READ, before the closure runs — the caller must see
        /// "could not read", not "wrote nothing".
        fn update(&self, _f: &mut dyn FnMut(&mut Registry) -> Commit) -> Result<(), StoreError> {
            Err(StoreError::Unreadable(std::io::ErrorKind::PermissionDenied))
        }
    }

    /// A store that models a COMPETING WRITER, deterministically.
    ///
    /// Every `load()` hands back the current document and then — before the
    /// caller can possibly have written anything back — applies somebody else's
    /// write to the stored document. That is the read-modify-write hazard with
    /// the timing taken out: any operation built from `load()` + `save()`
    /// necessarily saves a snapshot that predates the competing write and
    /// erases it, every single run, with no threads and no sleeps.
    ///
    /// `update()` is atomic (the lock spans the closure), so an operation built
    /// on it never observes the interleaving at all. The competing write
    /// therefore survives IFF the operation is a real transaction.
    pub struct ContendedStore {
        inner: Mutex<Registry>,
        /// The write the "other caller" performs, once, on the first `load`.
        rival: Mutex<Option<(String, String, StoredLocation)>>,
    }

    impl ContendedStore {
        /// `rival` is `(storage_key, name, entry)` — the entry another caller
        /// commits between our read and our write.
        pub fn new(seed: Registry, rival: (String, String, StoredLocation)) -> Self {
            Self { inner: Mutex::new(seed), rival: Mutex::new(Some(rival)) }
        }

        pub fn snapshot(&self) -> Registry {
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        fn let_the_rival_write(&self, reg: &mut Registry) {
            let mut slot = self.rival.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((key, name, entry)) = slot.take() {
                reg.caller_mut(&key).locations.insert(name, entry);
            }
        }
    }

    impl LocationStore for ContendedStore {
        fn load(&self) -> Result<Registry, StoreError> {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let seen = guard.clone();
            self.let_the_rival_write(&mut guard);
            Ok(seen)
        }

        fn save(&self, registry: &Registry) -> Result<(), StoreError> {
            *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = registry.clone();
            Ok(())
        }

        fn update(&self, f: &mut dyn FnMut(&mut Registry) -> Commit) -> Result<(), StoreError> {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // The rival commits here too — it is a real concurrent write, not a
            // property of the `load` path. The difference is that a transaction
            // sees it, because the read happens INSIDE the lock we then write
            // under.
            self.let_the_rival_write(&mut guard);
            let mut working = guard.clone();
            if f(&mut working) == Commit::Save {
                *guard = working;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, FileLocationStore) {
        let d = tempfile::tempdir().expect("tempdir");
        let s = FileLocationStore::at(d.path().join("nested").join("locations.json"));
        (d, s)
    }

    #[test]
    fn a_missing_file_is_an_empty_registry_not_an_error() {
        // "nothing stored yet" — the only absence that is genuinely absence.
        let (_d, s) = tmp();
        assert_eq!(s.load().expect("missing file must load as empty"), Registry::default());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_an_empty_registry() {
        // THE divergence from `meridian::state`. If this returned a default,
        // a damaged registry would read back as "you have no home set".
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("locations.json");
        std::fs::write(&p, "{ this is not json").unwrap();
        assert_eq!(FileLocationStore::at(&p).load(), Err(StoreError::Corrupt));

        std::fs::write(&p, "   ").unwrap();
        assert_eq!(FileLocationStore::at(&p).load(), Err(StoreError::Corrupt));
    }

    #[test]
    fn a_future_schema_version_is_refused_rather_than_misread() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("locations.json");
        std::fs::write(&p, r#"{"version": 99, "callers": {}}"#).unwrap();
        assert_eq!(FileLocationStore::at(&p).load(), Err(StoreError::UnknownVersion(99)));
    }

    #[test]
    fn save_then_load_round_trips_and_creates_the_parent_directory() {
        let (_d, s) = tmp();
        let mut reg = Registry::default();
        reg.caller_mut("svc:alpha")
            .locations
            .insert("home".into(), StoredLocation::permanent("1 Placeholder Way", 100)); // pii-test-fixture: obvious placeholder standing in for a home address
        s.save(&reg).expect("save");
        assert_eq!(s.load().expect("load"), reg);
    }

    #[cfg(unix)]
    #[test]
    fn the_registry_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, s) = tmp();
        s.save(&Registry::default()).expect("save");
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a file holding home addresses must not be group/world readable");
    }

    /// FINDING 4. `OpenOptions::mode` applies only when the open CREATES the
    /// file. A stale temp file left by a crashed run with the same pid — with
    /// whatever permissions it happened to have — used to be TRUNCATED and then
    /// filled with home addresses, keeping its loose mode all the way through
    /// the rename into the destination.
    #[cfg(unix)]
    #[test]
    fn a_stale_world_readable_temp_file_does_not_loosen_the_written_file() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let s = FileLocationStore::at(d.path().join("locations.json"));

        // Exactly the path the next save will stage through.
        let stale = s.temp_path();
        std::fs::write(&stale, "leftover from a crashed run").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o666)).unwrap();

        let mut reg = Registry::default();
        reg.caller_mut("svc:alpha")
            .locations
            .insert("home".into(), StoredLocation::permanent("1 Placeholder Way", 100)); // pii-test-fixture: obvious placeholder standing in for a home address
        s.save(&reg).expect("save over a stale temp file");

        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a file holding home addresses must not be group/world readable");
        assert_eq!(s.load().expect("load"), reg, "and the stale bytes must not survive");
        assert!(!stale.exists(), "the temp file must not be left behind");
    }

    /// The write must also not INHERIT a loose mode from an existing
    /// destination — `rename` replaces the destination, so the 0600 temp file's
    /// bits are what remain.
    #[cfg(unix)]
    #[test]
    fn a_world_readable_destination_is_replaced_by_an_owner_only_one() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("locations.json");
        std::fs::write(&p, r#"{"version":1,"callers":{}}"#).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        let s = FileLocationStore::at(&p);
        s.save(&Registry::default()).expect("save");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// FINDING 3, at the store level: `update` must span the read and the write.
    /// A `Save` lands; an `Abort` leaves the document byte-identical.
    #[test]
    fn update_is_a_transaction_and_abort_writes_nothing() {
        let (_d, s) = tmp();
        let mut seeded = Registry::default();
        seeded
            .caller_mut("svc:alpha")
            .locations
            .insert("home".into(), StoredLocation::permanent("1 Placeholder Way", 100)); // pii-test-fixture: obvious placeholder standing in for a home address
        s.save(&seeded).unwrap();

        // Abort: the closure sees the current document and declines.
        let mut saw = None;
        s.update(&mut |reg| {
            saw = reg.caller("svc:alpha").map(|c| c.locations.len());
            reg.caller_mut("svc:alpha").locations.clear();
            Commit::Abort
        })
        .expect("update");
        assert_eq!(saw, Some(1), "the closure must see the CURRENT document");
        assert_eq!(s.load().unwrap(), seeded, "an aborted transaction must write nothing");

        // Save: the mutation lands.
        s.update(&mut |reg| {
            reg.caller_mut("svc:bravo")
                .locations
                .insert("home".into(), StoredLocation::permanent("2 Otherplace Road", 100)); // pii-test-fixture: obvious placeholder standing in for another caller's home address
            Commit::Save
        })
        .expect("update");
        assert!(s.load().unwrap().caller("svc:bravo").is_some());
        assert!(s.load().unwrap().caller("svc:alpha").is_some(), "and must not drop the rest");
    }

    /// The other half of FINDING 3: `update` must hold the REAL lock across the
    /// closure, not merely sequence a `load` and a `save`.
    ///
    /// A transaction and a load-then-save are indistinguishable single-threaded,
    /// so this uses a second thread — but the ASSERTION is deterministic, not a
    /// race. Thread A enters its closure and waits (bounded) for thread B to
    /// finish a whole `save`. Under a real transaction B cannot start, A's wait
    /// expires, A commits, and B's later save preserves A's entry — both survive.
    /// Under load-then-save B completes immediately, A then writes back the
    /// snapshot it read before B existed, and B's entry is gone. The outcome is
    /// fixed in both cases; only the wall-clock cost of the bounded wait varies.
    #[test]
    fn update_holds_the_lock_across_the_whole_read_modify_write() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("locations.json");
        FileLocationStore::at(&path).save(&Registry::default()).unwrap();

        let b_done = Arc::new(AtomicBool::new(false));
        let a_entered = Arc::new(AtomicBool::new(false));

        let (bp, bd, ae) = (path.clone(), b_done.clone(), a_entered.clone());
        let b = std::thread::spawn(move || {
            // Wait for A to be INSIDE its transaction before competing.
            while !ae.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let s = FileLocationStore::at(&bp);
            let mut reg = s.load().expect("B load");
            reg.caller_mut("svc:bravo")
                .locations
                .insert("home".into(), StoredLocation::permanent("2 Otherplace Road", 100)); // pii-test-fixture: obvious placeholder standing in for another caller's home address
            s.save(&reg).expect("B save");
            bd.store(true, Ordering::SeqCst);
        });

        let s = FileLocationStore::at(&path);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(750);
        s.update(&mut |reg| {
            reg.caller_mut("svc:alpha")
                .locations
                .insert("home".into(), StoredLocation::permanent("1 Placeholder Way", 100)); // pii-test-fixture: obvious placeholder standing in for a home address
            a_entered.store(true, Ordering::SeqCst);
            // Give B every chance to slip in. It can only do so if this
            // closure is NOT running under the store's lock.
            while !b_done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            Commit::Save
        })
        .expect("A update");
        b.join().expect("B thread");

        let doc = FileLocationStore::at(&path).load().expect("final load");
        assert!(doc.caller("svc:alpha").is_some(), "A's write must have landed");
        assert!(
            doc.caller("svc:bravo").is_some(),
            "a concurrent writer's entry was lost — the read-modify-write is not serialized"
        );
    }

    /// A read failure aborts the transaction BEFORE the closure runs — a
    /// read-modify-write on an unreadable document would write a registry
    /// invented from nothing over whatever is actually there.
    #[test]
    fn update_refuses_to_run_on_an_unreadable_document() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("locations.json");
        std::fs::write(&p, "{ not json").unwrap();
        let s = FileLocationStore::at(&p);

        let mut ran = false;
        let r = s.update(&mut |_| {
            ran = true;
            Commit::Save
        });
        assert_eq!(r, Err(StoreError::Corrupt));
        assert!(!ran, "the closure must not run when the document could not be read");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json", "and nothing was written");
    }

    #[test]
    fn expiry_is_absolute_and_inclusive() {
        let t = StoredLocation::temporary("Somewhere", 1_000, 0);
        assert!(!t.is_expired(999));
        assert!(t.is_expired(1_000), "an entry is dead ON its expiry second");
        assert!(t.is_expired(1_001));
        assert!(!StoredLocation::permanent("Somewhere", 0).is_expired(i64::MAX));
    }

    #[test]
    fn store_errors_never_carry_content() {
        // Error text reaches logs and model context; it must not be a channel
        // for the very data the registry is protecting.
        for e in [
            StoreError::Unreadable(std::io::ErrorKind::PermissionDenied),
            StoreError::Corrupt,
            StoreError::UnknownVersion(9),
            StoreError::WriteFailed(std::io::ErrorKind::Other),
        ] {
            let s = e.to_string().to_lowercase();
            assert!(!s.contains("placeholder"));
            assert!(!s.contains("/"), "no path fragments in a store error: {s}");
        }
    }
}
