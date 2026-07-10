//! Artifact version control for the doc engine (DOCGEN-07, S95, Plane
//! TERM-149).
//!
//! Core feature per the spec: every generated artifact is versioned --
//! tied to the triggering feat/commit, diffable against its prior version,
//! and rollback-able. Regenerating after each feat never clobbers good
//! docs; a bad auto-generation is just a new version you can compare and
//! revert.
//!
//! ## Independent of caller placement
//! This store is the engine's OWN record. It keys versions by
//! `(project, target)` and never touches wherever the caller (the build
//! harness / DOCGEN-06 renderer) later writes the rendered artifact on
//! disk or in a repo. Diff and rollback work purely against this store's
//! history -- they never read or write any external file path -- so they
//! keep working regardless of what the caller did with a given version's
//! content after it was returned (spec APPROACH step 4; TEST PLAN
//! "versioning independent of caller placement (asserted)").
//!
//! ## Never overwrite
//! [`VersionStore::store_version`] always **appends** a new
//! [`ArtifactVersion`] -- there is no update-in-place API on this store.
//! [`VersionStore::rollback`] restores a prior version as current by
//! appending a **new** version copying that prior content (tagged
//! [`RollbackOf`]), rather than mutating history -- so "restore a prior
//! version as current" and "prior version is never overwritten" both hold
//! at once.
//!
//! ## PII-clean, vault/config-driven backend
//! This module stores only content the caller already PII-swept upstream
//! (DOCGEN-02 gates before generation) -- it performs no PII scanning
//! itself and introduces no new PII surface. The backend here is a plain
//! in-process store (`Mutex<BTreeMap<..>>`); it reads no secret VALUES and
//! has no `std::env::var` call of its own. A future durable backend (e.g.
//! Postgres, mirroring the `sqlx` pattern used elsewhere in this crate)
//! would resolve its connection string via `vault::manager().get(...)`,
//! never a hardcoded host/path -- there is nothing hardcoded here to begin
//! with since this scaffold has no network/filesystem backend at all.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use crate::error::ToolError;

/// Identifies one artifact's version history: which project, and which
/// declared doc target within it (e.g. `"readme"`, `"wiki"` -- see
/// [`super::config::DocTargetType`]). Two different targets for the same
/// project are two entirely independent histories.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactKey {
    pub project: String,
    pub target: String,
}

impl ArtifactKey {
    pub fn new(project: impl Into<String>, target: impl Into<String>) -> Self {
        Self { project: project.into(), target: target.into() }
    }
}

impl fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.project, self.target)
    }
}

/// Why a version was created. A version created by ordinary generation
/// carries the triggering feat/commit it was generated against
/// ([`VersionOrigin::Generated`]). A version created by
/// [`VersionStore::rollback`] instead records which prior version number
/// it restored ([`VersionOrigin::RollbackOf`]) -- this is what lets a
/// rollback be a genuinely new, auditable version rather than a silent
/// history rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionOrigin {
    Generated { source_commit: String },
    RollbackOf { restored_version: u64 },
}

/// One immutable, stored version of an artifact. `version` numbers are
/// 1-based and strictly increasing per [`ArtifactKey`]; never reused, never
/// reassigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactVersion {
    pub key: ArtifactKey,
    pub version: u64,
    pub content: String,
    pub origin: VersionOrigin,
    /// RFC3339 timestamp string, as supplied by the caller. This module
    /// does not read the system clock itself so it stays deterministic and
    /// fully unit-testable (mirrors the rest of docgen's no-hidden-I/O
    /// posture).
    pub timestamp: String,
}

/// The result of diffing two versions of the same artifact: a simple,
/// line-oriented change list. Deliberately minimal (no external diff
/// crate) -- good enough to show "what the feat changed in the docs"
/// (spec Design Overview step 6) without adding a new dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDiff {
    pub from_version: u64,
    pub to_version: u64,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Unchanged(String),
    Removed(String),
    Added(String),
}

impl VersionDiff {
    /// True when the "from" side has no prior version to compare against
    /// (spec EDGE CASES: "First version (nothing to diff against) ->
    /// handled, diff is 'all new'"). Every line in such a diff is
    /// [`DiffLine::Added`].
    pub fn is_all_new(&self) -> bool {
        self.lines.iter().all(|l| matches!(l, DiffLine::Added(_)))
    }
}

/// Compute a minimal line-based diff between two texts using an LCS
/// (longest common subsequence) over lines -- a standard, dependency-free
/// approach that produces a readable unchanged/removed/added sequence.
fn diff_lines(from: &str, to: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = from.lines().collect();
    let b: Vec<&str> = to.lines().collect();
    let n = a.len();
    let m = b.len();

    // dp[i][j] = length of LCS of a[i..] and b[j..]
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine::Unchanged(a[i].to_string()));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(DiffLine::Removed(a[i].to_string()));
            i += 1;
        } else {
            out.push(DiffLine::Added(b[j].to_string()));
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine::Removed(a[i].to_string()));
        i += 1;
    }
    while j < m {
        out.push(DiffLine::Added(b[j].to_string()));
        j += 1;
    }
    out
}

/// The engine's own versioned record store. In-process, thread-safe
/// (`Mutex`-guarded so concurrent generations for the same artifact
/// serialize rather than interleave -- spec EDGE CASES: "Concurrent
/// generations for the same artifact -> serialize, last-wins with both
/// versioned"; each caller's `store_version` call fully completes,
/// producing its own distinct version, before the next one proceeds).
///
/// This store never overwrites: [`Self::store_version`] only appends.
/// [`Self::rollback`] also only appends (a new version copying prior
/// content) -- see the module doc comment.
#[derive(Default)]
pub struct VersionStore {
    inner: Mutex<BTreeMap<ArtifactKey, Vec<ArtifactVersion>>>,
}

impl VersionStore {
    pub fn new() -> Self {
        Self { inner: Mutex::new(BTreeMap::new()) }
    }

    /// Store `content` as a brand-new version for `key`, tied to
    /// `source_commit` (the triggering feat/commit) and `timestamp`.
    /// Always appends; the prior current version (if any) is left exactly
    /// as it was (spec ACCEPTANCE CRITERIA: "prior never overwritten").
    /// Returns the newly created version.
    pub fn store_version(
        &self,
        key: ArtifactKey,
        content: impl Into<String>,
        source_commit: impl Into<String>,
        timestamp: impl Into<String>,
    ) -> ArtifactVersion {
        let mut guard = self.inner.lock().expect("VersionStore mutex poisoned");
        let history = guard.entry(key.clone()).or_default();
        let next = history.last().map(|v| v.version + 1).unwrap_or(1);
        let version = ArtifactVersion {
            key,
            version: next,
            content: content.into(),
            origin: VersionOrigin::Generated { source_commit: source_commit.into() },
            timestamp: timestamp.into(),
        };
        history.push(version.clone());
        version
    }

    /// The full, ordered history for `key` (oldest first). Empty if the
    /// artifact has never been generated.
    pub fn history(&self, key: &ArtifactKey) -> Vec<ArtifactVersion> {
        let guard = self.inner.lock().expect("VersionStore mutex poisoned");
        guard.get(key).cloned().unwrap_or_default()
    }

    /// The current (latest) version for `key`, if any exists yet.
    pub fn current(&self, key: &ArtifactKey) -> Option<ArtifactVersion> {
        let guard = self.inner.lock().expect("VersionStore mutex poisoned");
        guard.get(key).and_then(|h| h.last().cloned())
    }

    /// Fetch a specific version number for `key`.
    pub fn get_version(&self, key: &ArtifactKey, version: u64) -> Option<ArtifactVersion> {
        let guard = self.inner.lock().expect("VersionStore mutex poisoned");
        guard
            .get(key)
            .and_then(|h| h.iter().find(|v| v.version == version))
            .cloned()
    }

    /// Diff two versions of the same artifact. `from_version` may be `0` to
    /// mean "compare against nothing" -- i.e. the diff of `to_version`
    /// against an empty document, which is the "first version, nothing to
    /// diff against" edge case: every line reports as
    /// [`DiffLine::Added`] and [`VersionDiff::is_all_new`] is `true`.
    ///
    /// Returns [`ToolError::NotFound`] if `to_version` (or a nonzero
    /// `from_version`) doesn't exist in this artifact's history --
    /// diffing a version that was never stored is a clear error, not a
    /// silent empty diff.
    pub fn diff(
        &self,
        key: &ArtifactKey,
        from_version: u64,
        to_version: u64,
    ) -> Result<VersionDiff, ToolError> {
        let to = self.get_version(key, to_version).ok_or_else(|| {
            ToolError::NotFound(format!(
                "artifact {key}: version {to_version} does not exist"
            ))
        })?;

        let from_content = if from_version == 0 {
            String::new()
        } else {
            let from = self.get_version(key, from_version).ok_or_else(|| {
                ToolError::NotFound(format!(
                    "artifact {key}: version {from_version} does not exist"
                ))
            })?;
            from.content
        };

        let lines = diff_lines(&from_content, &to.content);
        Ok(VersionDiff { from_version, to_version: to.version, lines })
    }

    /// Restore `version` as the new current version for `key`. This does
    /// NOT rewrite or delete any history: it appends a brand-new version
    /// whose content is a copy of `version`'s content, tagged
    /// [`VersionOrigin::RollbackOf`]. That new version becomes
    /// `current(key)`; the version being restored from, and everything in
    /// between, remain untouched in history (spec ACCEPTANCE CRITERIA:
    /// "Diff between versions works; rollback restores a prior version" --
    /// combined with the never-overwrite invariant above).
    ///
    /// Returns [`ToolError::NotFound`] if `version` doesn't exist for
    /// `key`. Rolling back to a version that referenced now-removed
    /// content (spec EDGE CASES) is intentionally NOT specially detected
    /// here -- the store restores the doc exactly as it was verbatim; any
    /// staleness note is the caller's responsibility to attach (e.g. in the
    /// content itself or surrounding tooling), since this store has no way
    /// to know what "removed content" means for a given target format.
    pub fn rollback(
        &self,
        key: &ArtifactKey,
        version: u64,
        timestamp: impl Into<String>,
    ) -> Result<ArtifactVersion, ToolError> {
        let target = self.get_version(key, version).ok_or_else(|| {
            ToolError::NotFound(format!("artifact {key}: version {version} does not exist"))
        })?;

        let mut guard = self.inner.lock().expect("VersionStore mutex poisoned");
        let history = guard.entry(key.clone()).or_default();
        let next = history.last().map(|v| v.version + 1).unwrap_or(1);
        let restored = ArtifactVersion {
            key: key.clone(),
            version: next,
            content: target.content,
            origin: VersionOrigin::RollbackOf { restored_version: version },
            timestamp: timestamp.into(),
        };
        history.push(restored.clone());
        Ok(restored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ArtifactKey {
        ArtifactKey::new("terminus", "readme")
    }

    // ── store_version: append-only, never overwrite ─────────────────────

    #[test]
    fn each_generation_creates_a_new_version_prior_preserved() {
        let store = VersionStore::new();
        let v1 = store.store_version(key(), "content v1", "abc123", "2026-07-10T00:00:00Z");
        let v2 = store.store_version(key(), "content v2", "def456", "2026-07-10T01:00:00Z");

        assert_eq!(v1.version, 1);
        assert_eq!(v2.version, 2);

        // Negative test: v1 must still exist, byte-for-byte, after v2 was
        // stored -- storing a new version never mutates or removes a prior
        // one.
        let fetched_v1 = store.get_version(&key(), 1).expect("v1 must still exist");
        assert_eq!(fetched_v1.content, "content v1");
        assert_eq!(fetched_v1.origin, VersionOrigin::Generated { source_commit: "abc123".into() });

        let history = store.history(&key());
        assert_eq!(history.len(), 2, "both versions must be present, none overwritten");

        let current = store.current(&key()).unwrap();
        assert_eq!(current.version, 2);
        assert_eq!(current.content, "content v2");
    }

    #[test]
    fn different_targets_have_independent_histories() {
        let store = VersionStore::new();
        store.store_version(
            ArtifactKey::new("terminus", "readme"),
            "readme content",
            "abc123",
            "t0",
        );
        store.store_version(ArtifactKey::new("terminus", "wiki"), "wiki content", "abc123", "t0");

        assert_eq!(store.history(&ArtifactKey::new("terminus", "readme")).len(), 1);
        assert_eq!(store.history(&ArtifactKey::new("terminus", "wiki")).len(), 1);
        assert_eq!(
            store.current(&ArtifactKey::new("terminus", "readme")).unwrap().content,
            "readme content"
        );
    }

    #[test]
    fn unknown_artifact_has_empty_history_and_no_current() {
        let store = VersionStore::new();
        assert!(store.history(&key()).is_empty());
        assert!(store.current(&key()).is_none());
    }

    // ── diff ──────────────────────────────────────────────────────────

    #[test]
    fn diff_between_two_versions_is_correct() {
        let store = VersionStore::new();
        store.store_version(key(), "line one\nline two\nline three", "c1", "t0");
        store.store_version(key(), "line one\nline TWO CHANGED\nline three\nline four", "c2", "t1");

        let d = store.diff(&key(), 1, 2).unwrap();
        assert_eq!(d.from_version, 1);
        assert_eq!(d.to_version, 2);

        assert!(d.lines.contains(&DiffLine::Unchanged("line one".to_string())));
        assert!(d.lines.contains(&DiffLine::Removed("line two".to_string())));
        assert!(d.lines.contains(&DiffLine::Added("line TWO CHANGED".to_string())));
        assert!(d.lines.contains(&DiffLine::Unchanged("line three".to_string())));
        assert!(d.lines.contains(&DiffLine::Added("line four".to_string())));
        assert!(!d.is_all_new());
    }

    #[test]
    fn diff_identical_versions_is_all_unchanged() {
        let store = VersionStore::new();
        store.store_version(key(), "same\ncontent", "c1", "t0");
        store.store_version(key(), "same\ncontent", "c2", "t1");

        let d = store.diff(&key(), 1, 2).unwrap();
        assert!(d.lines.iter().all(|l| matches!(l, DiffLine::Unchanged(_))));
    }

    /// Spec EDGE CASE: "First version (nothing to diff against) -> handled,
    /// diff is 'all new'".
    #[test]
    fn diff_against_zero_for_first_version_is_all_new() {
        let store = VersionStore::new();
        store.store_version(key(), "brand new content\nsecond line", "c1", "t0");

        let d = store.diff(&key(), 0, 1).unwrap();
        assert!(d.is_all_new());
        assert_eq!(d.lines.len(), 2);
    }

    /// Negative test: diffing a version number that was never stored is a
    /// clear error, not a panic or a silently empty diff.
    #[test]
    fn diff_against_nonexistent_version_returns_not_found_error() {
        let store = VersionStore::new();
        store.store_version(key(), "v1", "c1", "t0");

        let err = store.diff(&key(), 1, 99).unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));

        let err2 = store.diff(&key(), 99, 1).unwrap_err();
        assert!(matches!(err2, ToolError::NotFound(_)));
    }

    // ── rollback ──────────────────────────────────────────────────────

    #[test]
    fn rollback_restores_a_prior_version_as_current() {
        let store = VersionStore::new();
        store.store_version(key(), "good content", "c1", "t0");
        store.store_version(key(), "BAD auto-generated content", "c2", "t1");

        // Current is now the bad v2.
        assert_eq!(store.current(&key()).unwrap().content, "BAD auto-generated content");

        let restored = store.rollback(&key(), 1, "t2").unwrap();

        // Rollback produced a brand-new version (v3), not a rewrite of v1.
        assert_eq!(restored.version, 3);
        assert_eq!(restored.content, "good content");
        assert_eq!(restored.origin, VersionOrigin::RollbackOf { restored_version: 1 });

        // It is now current.
        let current = store.current(&key()).unwrap();
        assert_eq!(current.version, 3);
        assert_eq!(current.content, "good content");

        // Negative test: the original v1 and the bad v2 are BOTH still
        // present, untouched -- rollback never rewrites history.
        let history = store.history(&key());
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "good content");
        assert_eq!(history[1].content, "BAD auto-generated content");
    }

    /// Negative test: rolling back to a version that was never stored is a
    /// clear error, and must not create any new version as a side effect.
    #[test]
    fn rollback_to_nonexistent_version_returns_error_and_creates_nothing() {
        let store = VersionStore::new();
        store.store_version(key(), "v1", "c1", "t0");

        let err = store.rollback(&key(), 42, "t1").unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));

        // No phantom version was created by the failed rollback attempt.
        assert_eq!(store.history(&key()).len(), 1);
    }

    #[test]
    fn rollback_then_rollback_again_keeps_full_lineage() {
        let store = VersionStore::new();
        store.store_version(key(), "v1", "c1", "t0"); // v1
        store.store_version(key(), "v2", "c2", "t1"); // v2
        store.rollback(&key(), 1, "t2"); // v3 = rollback to v1
        store.store_version(key(), "v4 new work", "c3", "t3"); // v4

        let history = store.history(&key());
        assert_eq!(history.len(), 4);
        assert_eq!(history[2].origin, VersionOrigin::RollbackOf { restored_version: 1 });
        assert_eq!(store.current(&key()).unwrap().content, "v4 new work");
    }

    // ── independence from caller placement ───────────────────────────

    /// Spec TEST PLAN: "versioning works independent of caller placement
    /// (asserted)". This store never accepts or returns a filesystem path
    /// / repo location for a version -- there is no such field on
    /// `ArtifactVersion` at all, and every operation (store/history/
    /// current/diff/rollback) is keyed purely by `ArtifactKey` + version
    /// number. We assert this structurally: storing, diffing, and rolling
    /// back a version succeeds identically whether or not the caller ever
    /// "placed" (wrote out) any prior version anywhere -- the store's
    /// behavior cannot depend on placement because it has no API surface
    /// through which placement information could even be supplied.
    #[test]
    fn versioning_is_independent_of_caller_placement() {
        let store = VersionStore::new();
        let key = key();

        // Simulate: caller generates version 1 and (per spec) the engine
        // never places it anywhere -- the caller might write it to a repo,
        // a wiki, nowhere at all (a dry run), or a completely different
        // location each time. The version store's own record-keeping must
        // behave identically regardless.
        let v1 = store.store_version(key.clone(), "artifact body", "commit1", "t0");
        assert_eq!(v1.version, 1);

        // No "placement" concept exists on ArtifactVersion to even
        // check -- assert the struct's field set is exactly what the
        // module doc promises (key/version/content/origin/timestamp),
        // which is itself the structural guarantee that placement can't
        // leak in.
        let ArtifactVersion { key: k, version, content, origin: _, timestamp: _ } = v1;
        assert_eq!(k, key);
        assert_eq!(version, 1);
        assert_eq!(content, "artifact body");

        // Diff and rollback work purely from the store's own history, with
        // no dependency on any external path.
        store.store_version(key.clone(), "artifact body v2", "commit2", "t1");
        let d = store.diff(&key, 1, 2).unwrap();
        assert!(!d.is_all_new());

        let restored = store.rollback(&key, 1, "t2").unwrap();
        assert_eq!(restored.content, "artifact body");
    }

    // ── PII-clean store (structural: no vault/env access, no path field) ─

    /// This module must never read a secret VALUE and never construct a
    /// hardcoded infra literal. There is no `vault::manager()` /
    /// `SecretManager::get()` / `std::env::var` call anywhere in this
    /// module (grep-verified at the test-gate stage); this test documents
    /// that expectation structurally: a store built and used entirely
    /// in-memory, with content supplied entirely by the caller, cannot by
    /// construction leak any infra value this module itself introduced.
    #[test]
    fn store_content_round_trips_exactly_no_hidden_mutation() {
        let store = VersionStore::new();
        let swept_content = "# Docs\n\nAlready PII-swept by DOCGEN-02 upstream.";
        let v1 = store.store_version(key(), swept_content, "c1", "t0");
        assert_eq!(v1.content, swept_content);
        let fetched = store.get_version(&key(), 1).unwrap();
        assert_eq!(fetched.content, swept_content);
    }
}
