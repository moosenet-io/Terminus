//! DLAND-01: the repo placement writer (S119, spec `S119-docgen-landing-hierarchy`,
//! Plane project TERM).
//!
//! Every renderer in this module (`readme_layers::render_layered_readme`,
//! `render::docs_tree::build_docs_tree`, `render::render_all`,
//! `trigger::run_docgen_trigger`) is explicitly pure -- each one's own doc
//! comment says placement is "entirely the calling harness's decision" and
//! ships a negative test proving it never touches a filesystem or repo. This
//! module IS that harness-side placement step: given an explicit
//! `target_root`, an already-rendered concise landing README string (from
//! `render_layered_readme`), and the already-rendered `docs/` tree (from
//! `build_docs_tree`), it writes `README.md` and every `docs/**` file into a
//! real working tree and reports what happened.
//!
//! ## The one exception to "never touches a filesystem" in `docgen`
//! Deliberately so: every other module here defers placement so it can be
//! tested purely, in-memory, without a worktree. This module is the thing
//! those deferrals were waiting for. It still touches NO git (no add/commit/
//! push) and NO network/forge calls -- only working-tree file writes under
//! `target_root`.
//!
//! ## Atomicity + idempotency
//! Each file is written to a sibling `<path>.tmp-<pid>` path, then moved onto
//! the final path with `std::fs::rename` -- a concurrent reader never
//! observes a partially-written file. A write is skipped entirely (the file
//! is never opened for writing) when the on-disk content already matches the
//! new content byte-for-byte, so re-placing unchanged artifacts produces an
//! empty diff.
//!
//! ## Path-traversal guard
//! `DocsTreeFile::path` values come from this crate's own renderers today,
//! but `place_docs` treats every one as untrusted input regardless: an
//! absolute path, or a path containing a `..` (or any other root-escaping)
//! component, is refused and recorded in [`PlacementReport::skipped`] with a
//! reason -- it is never joined onto `target_root`, so it is never written
//! anywhere, inside or outside the root.

use std::path::{Component, Path, PathBuf};

use super::render::docs_tree::DocsTreeFile;

/// Repo-relative path the rendered landing README is always placed at.
pub const README_PATH: &str = "README.md";

/// One refused or failed placement: the repo-relative path involved, and a
/// human-readable reason it was not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: String,
}

/// What [`place_docs`] actually did, in terms of repo-relative paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlacementReport {
    /// Paths newly written, or whose on-disk content changed.
    pub written: Vec<String>,
    /// Paths whose on-disk content already matched byte-for-byte -- no write
    /// was performed (idempotent no-op).
    pub unchanged: Vec<String>,
    /// Paths refused (path-traversal/absolute) or that failed to write (I/O
    /// error), each with a reason. Never a panic -- every failure mode this
    /// module can hit lands here instead.
    pub skipped: Vec<SkippedEntry>,
}

impl PlacementReport {
    fn skip(&mut self, path: impl Into<String>, reason: impl Into<String>) {
        self.skipped.push(SkippedEntry { path: path.into(), reason: reason.into() });
    }
}

/// Is `path` safe to join onto a `target_root` without ever escaping it? A
/// path is safe iff it is relative and contains no `..`/root/prefix
/// component -- the only ways a nominally repo-relative string could
/// otherwise land outside `target_root`.
fn is_safe_relative_path(path: &str) -> bool {
    let p = Path::new(path);
    if p.is_absolute() {
        return false;
    }
    !p.components().any(|c| {
        matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    })
}

/// Build the atomic-write temp path for `final_path`: the same path with a
/// `.tmp-<pid>` suffix appended (never replacing an existing extension, so
/// `docs/index.md` becomes `docs/index.md.tmp-1234`, not `docs/index.tmp-1234`).
/// Per-process monotonic nonce so concurrent placements never contend on the
/// same temp name (pid alone is not enough within one process).
static TMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp_path_for(final_path: &Path, nonce: u64) -> PathBuf {
    let mut os = final_path.as_os_str().to_os_string();
    os.push(format!(".tmp-{}-{}", std::process::id(), nonce));
    PathBuf::from(os)
}

/// Write `content` to `final_path` atomically via an EXCLUSIVELY-created sibling
/// temp file (`create_new` = `O_CREAT|O_EXCL`) then `rename`. `O_EXCL` refuses
/// to open a path that already exists — crucially it will NOT follow a
/// pre-existing symlink at the temp path (which `std::fs::write` would, letting
/// a write escape `target_root`), and it can never clobber a stale/concurrent
/// temp. On the rare name collision we retry with a fresh nonce. Returns the io
/// error to surface (as a skip) on failure, or `Ok(())` on success.
fn write_atomic(final_path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..16 {
        let nonce = TMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = tmp_path_for(final_path, nonce);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp_path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(content.as_bytes()) {
                    drop(f);
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                drop(f);
                if let Err(e) = std::fs::rename(&tmp_path, final_path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                return Ok(());
            }
            // The temp name is taken (stale temp or a planted symlink/file):
            // never write through it — pick a new nonce and try again.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AlreadyExists, "could not create a unique temp file after retries")
    }))
}

/// Write `content` to `target_root/relative_path` atomically (tempfile +
/// rename), skipping the write entirely when the file already holds
/// byte-identical content. Records the outcome (written/unchanged/skipped)
/// on `report`. Never panics -- every I/O failure is caught and recorded.
fn place_one(target_root: &Path, relative_path: &str, content: &str, report: &mut PlacementReport) {
    if !is_safe_relative_path(relative_path) {
        report.skip(
            relative_path,
            "path is absolute or escapes target_root via a '..'/root component -- refused",
        );
        return;
    }
    let final_path = target_root.join(relative_path);

    if let Ok(existing) = std::fs::read(&final_path) {
        if existing == content.as_bytes() {
            report.unchanged.push(relative_path.to_string());
            return;
        }
    }

    if let Some(parent) = final_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            report.skip(relative_path, format!("failed to create parent directory: {e}"));
            return;
        }
    }

    if let Err(e) = write_atomic(&final_path, content) {
        report.skip(relative_path, format!("failed to write atomically: {e}"));
        return;
    }

    report.written.push(relative_path.to_string());
}

/// Place the rendered landing README and the full `docs/` tree into a real
/// working tree rooted at `target_root`.
///
/// - `target_root`: an explicit, already-existing directory (typically a
///   worktree root). Must exist and be a directory -- if not, nothing is
///   written and the report explains why.
/// - `landing`: the already-rendered concise landing README body (from
///   [`super::readme_layers::render_layered_readme`]). Written to
///   `target_root/README.md`. A blank/whitespace-only `landing` is refused
///   rather than written -- this never overwrites a real README with empty
///   content (see the module test
///   `place_docs_writes_no_readme_for_blank_landing`).
/// - `docs_tree`: the already-rendered `docs/` tree (from
///   [`super::render::docs_tree::build_docs_tree`]) -- consumed as-is; this
///   function never re-derives or second-guesses any of its paths, it only
///   validates each is safe to place (see [`is_safe_relative_path`]).
///
/// Touches no git and no network -- working-tree writes only, atomic and
/// idempotent per file. Never panics.
pub fn place_docs(target_root: &Path, landing: &str, docs_tree: &[DocsTreeFile]) -> PlacementReport {
    let mut report = PlacementReport::default();

    if !target_root.exists() {
        report.skip(".", format!("target_root '{}' does not exist", target_root.display()));
        return report;
    }
    if !target_root.is_dir() {
        report.skip(".", format!("target_root '{}' is not a directory", target_root.display()));
        return report;
    }

    if landing.trim().is_empty() {
        report.skip(README_PATH, "landing content is empty -- refusing to write or overwrite README.md");
    } else {
        place_one(target_root, README_PATH, landing, &mut report);
    }

    for file in docs_tree {
        place_one(target_root, &file.path, &file.content, &mut report);
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("docgen-place-test-{label}-{}-{}", std::process::id(), fastrand_seed()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Cheap, dependency-free per-call uniqueness so parallel tests never
    // collide on the same temp directory.
    fn fastrand_seed() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    }

    fn sample_docs_tree() -> Vec<DocsTreeFile> {
        vec![
            DocsTreeFile { path: "docs/index.md".to_string(), content: "# Index\n".to_string() },
            DocsTreeFile {
                path: "docs/guides/index.md".to_string(),
                content: "# Guides\n".to_string(),
            },
        ]
    }

    // ── Happy path: writes landing + every docs/** file at expected paths ──

    #[test]
    fn place_docs_writes_landing_and_every_docs_tree_file() {
        let root = unique_tmp_dir("happy-path");
        let report = place_docs(&root, "# Hello\n\nA landing README.\n", &sample_docs_tree());

        assert!(root.join("README.md").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("README.md")).unwrap(),
            "# Hello\n\nA landing README.\n"
        );
        assert!(root.join("docs/index.md").exists());
        assert!(root.join("docs/guides/index.md").exists());
        assert_eq!(std::fs::read_to_string(root.join("docs/index.md")).unwrap(), "# Index\n");

        assert_eq!(
            report.written,
            vec![
                README_PATH.to_string(),
                "docs/index.md".to_string(),
                "docs/guides/index.md".to_string(),
            ]
        );
        assert!(report.unchanged.is_empty());
        assert!(report.skipped.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Idempotency: second identical call writes nothing (empty diff) ─────

    #[test]
    fn place_docs_second_identical_call_writes_nothing() {
        let root = unique_tmp_dir("idempotent");
        let landing = "# Hello\n\nA landing README.\n";
        let tree = sample_docs_tree();

        let first = place_docs(&root, landing, &tree);
        assert_eq!(first.written.len(), 3);

        let second = place_docs(&root, landing, &tree);
        assert!(second.written.is_empty(), "second identical call must write nothing: {second:?}");
        assert_eq!(
            second.unchanged,
            vec![
                README_PATH.to_string(),
                "docs/index.md".to_string(),
                "docs/guides/index.md".to_string(),
            ]
        );
        assert!(second.skipped.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Negative: path traversal / absolute entries refused, never written ─

    #[test]
    fn place_docs_skips_path_traversal_and_absolute_entries_without_writing_them() {
        let root = unique_tmp_dir("traversal");
        let malicious = vec![
            DocsTreeFile {
                path: "docs/../../escape.md".to_string(),
                content: "should never land outside target_root".to_string(),
            },
            DocsTreeFile {
                path: "/etc/escape-absolute.md".to_string(),
                content: "should never be written at all".to_string(),
            },
            DocsTreeFile { path: "docs/index.md".to_string(), content: "# Index\n".to_string() },
        ];

        let report = place_docs(&root, "# Hello\n", &malicious);

        assert_eq!(report.skipped.len(), 2);
        assert!(report.skipped.iter().any(|s| s.path == "docs/../../escape.md"));
        assert!(report.skipped.iter().any(|s| s.path == "/etc/escape-absolute.md"));
        for entry in &report.skipped {
            assert!(!entry.reason.is_empty());
        }

        // Only the legitimate file was written.
        assert_eq!(report.written, vec![README_PATH.to_string(), "docs/index.md".to_string()]);

        // The escape targets truly never exist anywhere reachable from here.
        assert!(!root.join("escape.md").exists());
        assert!(!std::path::Path::new("/etc/escape-absolute.md").exists());
        // Nothing was written above target_root either.
        if let Some(parent) = root.parent() {
            assert!(!parent.join("escape.md").exists());
        }

        std::fs::remove_dir_all(&root).ok();
    }

    // ── An existing hand-written README.md is replaced atomically ──────────

    #[test]
    fn place_docs_replaces_an_existing_readme_atomically() {
        let root = unique_tmp_dir("replace-readme");
        std::fs::write(root.join("README.md"), "# Old hand-written README\n").unwrap();

        let report = place_docs(&root, "# New generated README\n", &[]);

        assert_eq!(report.written, vec![README_PATH.to_string()]);
        assert_eq!(
            std::fs::read_to_string(root.join("README.md")).unwrap(),
            "# New generated README\n"
        );
        // No stray temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "atomic write must leave no .tmp- file behind: {leftovers:?}");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Zero-length landing never overwrites a real README with empty ──────

    #[test]
    fn place_docs_writes_no_readme_for_blank_landing() {
        let root = unique_tmp_dir("blank-landing");
        std::fs::write(root.join("README.md"), "# Real hand-written content, keep me\n").unwrap();

        let report = place_docs(&root, "   \n\n  ", &[]);

        assert!(report.written.is_empty());
        assert!(report.skipped.iter().any(|s| s.path == README_PATH));
        assert_eq!(
            std::fs::read_to_string(root.join("README.md")).unwrap(),
            "# Real hand-written content, keep me\n",
            "a blank landing must never overwrite an existing README"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn place_docs_writes_no_readme_for_blank_landing_when_none_existed() {
        let root = unique_tmp_dir("blank-landing-fresh");
        let report = place_docs(&root, "", &[]);
        assert!(report.written.is_empty());
        assert!(!root.join("README.md").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    // ── target_root missing / not a directory: clear report, nothing written ─

    #[test]
    fn place_docs_reports_error_for_missing_target_root() {
        let root = std::env::temp_dir().join(format!(
            "docgen-place-test-does-not-exist-{}-{}",
            std::process::id(),
            fastrand_seed()
        ));
        assert!(!root.exists());

        let report = place_docs(&root, "# Hello\n", &sample_docs_tree());

        assert!(report.written.is_empty());
        assert!(report.unchanged.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(!root.exists(), "place_docs must never create target_root itself");
    }

    #[test]
    fn place_docs_reports_error_for_target_root_that_is_a_file() {
        let dir = unique_tmp_dir("root-is-file-parent");
        let file_root = dir.join("not-a-directory.txt");
        std::fs::write(&file_root, "i am a file, not a directory").unwrap();

        let report = place_docs(&file_root, "# Hello\n", &sample_docs_tree());

        assert!(report.written.is_empty());
        assert_eq!(report.skipped.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── docs/ path whose parent dir doesn't exist: created, not an error ───

    #[test]
    fn place_docs_creates_missing_parent_directories() {
        let root = unique_tmp_dir("missing-parents");
        let tree = vec![DocsTreeFile {
            path: "docs/reference/deep/nested/page.md".to_string(),
            content: "# Deep page\n".to_string(),
        }];

        let report = place_docs(&root, "# Hello\n", &tree);

        assert!(report.skipped.is_empty());
        assert!(root.join("docs/reference/deep/nested/page.md").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("docs/reference/deep/nested/page.md")).unwrap(),
            "# Deep page\n"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Read-only target: per-file I/O error surfaced in skipped, no panic ──

    #[test]
    fn place_docs_surfaces_io_error_when_readme_path_is_blocked_by_a_directory() {
        // Force a write failure that is deterministic across every uid and
        // filesystem — including a build host that runs tests as root, where a
        // read-only *mode bit* would simply be bypassed. Pre-create README.md
        // AS A NON-EMPTY DIRECTORY: renaming the freshly-written temp file onto
        // that path then fails (IsADirectory / DirectoryNotEmpty) for root too.
        // place_docs must surface this as a skipped entry, never a panic.
        let root = unique_tmp_dir("readme-blocked");
        std::fs::create_dir_all(root.join(README_PATH).join("occupied")).unwrap();

        let report = place_docs(&root, "# Hello\n", &[]);

        assert!(
            report.written.is_empty(),
            "nothing should be written when the README path is blocked: {:?}",
            report.written
        );
        assert_eq!(
            report.skipped.len(),
            1,
            "the blocked README should be the single skipped entry: {:?}",
            report.skipped
        );
        assert_eq!(report.skipped[0].path, README_PATH);

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Placement touches no git/network: only files under target_root exist ─

    #[test]
    fn place_docs_never_writes_outside_target_root() {
        // A dedicated, exclusively-owned container directory (not the shared
        // system temp dir) so nothing another concurrently-running test
        // creates can be mistaken for an escape out of `root`.
        let container = unique_tmp_dir("no-escape-container");
        let root = container.join("root");
        std::fs::create_dir_all(&root).unwrap();

        let _ = place_docs(&root, "# Hello\n", &sample_docs_tree());

        let container_entries: Vec<_> = std::fs::read_dir(&container)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            container_entries,
            vec![std::ffi::OsString::from("root")],
            "place_docs must never create anything outside target_root"
        );

        std::fs::remove_dir_all(&container).ok();
    }
}
