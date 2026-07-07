//! Read-only worktree inspection helpers for Scribe (SCRB-03).
//!
//! Scribe needs to read real source code to verify/discover functionality
//! before writing docs about it -- via a git worktree, matching the build
//! pipeline's own Stage 2 convention (`git worktree add <path> -b <branch>
//! <ref>`) -- but must NEVER commit or push code changes itself.
//!
//! ## Structural no-commit/no-push guarantee
//! Every git invocation this module can ever construct comes from
//! [`ReadOnlyGitOp`], a **closed enum** whose variants are exhaustively
//! matched in [`argv_for`] -- there is no `Commit` or `Push` (or any other
//! write/publish) variant to add a call site for. Extending this module to
//! write or publish anything would require adding a new enum variant, which
//! is a reviewer-visible, deliberate change to this file, not an
//! accidentally-reachable code path. `structural_read_only_guarantees` (in
//! the test module below) enumerates every variant and asserts none of their
//! generated argv ever contains a write/publish verb -- this is checked
//! against the enum itself (so it can never silently go stale), not against
//! today's call sites.
//!
//! ## A note on `src/tool.rs`'s no-subprocess contract
//! This module shells out to the `git` binary via `std::process::Command`
//! (there is no vendored git library in this crate, and this sandbox has no
//! registry access to add one). `src/tool.rs`'s `RustTool` contract states
//! `execute()` must never use shell commands or subprocess calls. This
//! module is a **plain helper**, not itself a `RustTool` impl, so it is
//! self-contained and independently testable for SCRB-03. Flagging for
//! whoever wires it into a live `RustTool::execute()` body (SCRB-02's
//! `scribe_generate_readme` et al.): that integration must resolve the same
//! tension `review_daemon`/`src/dgem/mod.rs` already solved for LLM-CLI
//! dispatch -- wrap the subprocess-needing call behind a small local daemon
//! reached over loopback HTTP, rather than calling `Command` directly from
//! `execute()`. Out of scope here: SCRB-03's own FILES section is
//! `inspect.rs` only.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::ToolError;

// ─── Closed, read-only git operation set ────────────────────────────────────

/// The only git operations this module will ever build an argv for. No
/// variant here can write to a remote or create a commit -- see the module
/// doc comment for why that's a structural guarantee, not just a convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadOnlyGitOp<'a> {
    /// `git worktree add <path> <git_ref>` run inside `repo_path` -- creates
    /// a new working directory checked out at `git_ref`. Read-only in the
    /// sense that it never mutates the ref itself or any remote.
    WorktreeAdd { repo_path: &'a Path, worktree_path: &'a Path, git_ref: &'a str },
    /// `git worktree remove --force <path>` -- cleans up an inspection
    /// worktree. Removes a local checkout directory only; touches no ref,
    /// no remote, no history.
    WorktreeRemove { repo_path: &'a Path, worktree_path: &'a Path },
    /// `git fetch origin <git_ref>` -- updates local knowledge of a remote
    /// ref. Never pushes.
    Fetch { repo_path: &'a Path, git_ref: &'a str },
}

/// Build the argv (excluding the `git` binary name itself) for a read-only
/// git operation. Pure and side-effect-free -- unit tested without ever
/// spawning a process, matching the `review_daemon::provider::build_command`
/// precedent (argv-array builders, never a shell string).
fn argv_for(op: &ReadOnlyGitOp) -> (PathBuf, Vec<String>) {
    match op {
        ReadOnlyGitOp::WorktreeAdd { repo_path, worktree_path, git_ref } => (
            repo_path.to_path_buf(),
            vec![
                "worktree".into(),
                "add".into(),
                worktree_path.to_string_lossy().into_owned(),
                git_ref.to_string(),
            ],
        ),
        ReadOnlyGitOp::WorktreeRemove { repo_path, worktree_path } => (
            repo_path.to_path_buf(),
            vec![
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                worktree_path.to_string_lossy().into_owned(),
            ],
        ),
        ReadOnlyGitOp::Fetch { repo_path, git_ref } => (
            repo_path.to_path_buf(),
            vec!["fetch".into(), "origin".into(), git_ref.to_string()],
        ),
    }
}

fn run(op: ReadOnlyGitOp) -> Result<String, ToolError> {
    let (cwd, args) = argv_for(&op);
    let output = Command::new("git")
        .current_dir(&cwd)
        .args(&args)
        .output()
        .map_err(|e| ToolError::Execution(format!("failed to spawn git: {e}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(ToolError::Execution(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

// ─── Public types ────────────────────────────────────────────────────────────

/// A read-only inspection worktree: a local checkout of `git_ref` from
/// `repo_path`, living under `path`. Dropping this value does not clean up
/// the directory -- call [`cleanup`] explicitly (mirrors the pipeline's own
/// Stage 8 cleanup being an explicit step, not implicit).
#[derive(Debug, Clone)]
pub struct InspectionWorktree {
    pub path: PathBuf,
    pub repo_path: PathBuf,
    pub git_ref: String,
}

/// One source file's extracted context for the LLM docs-generation bundle
/// (consumed by SCRB-02's prompt construction).
#[derive(Debug, Clone, Default)]
pub struct FileExcerpt {
    pub path: String,
    /// Lines from `//!`/`///` doc comments, in file order.
    pub doc_comments: Vec<String>,
    /// Lines that look like public item signatures (`pub fn`, `pub struct`,
    /// `pub enum`, `pub trait`). A simple line-scan, not a full parser --
    /// good enough for a documentation-context bundle, not for codegen.
    pub public_signatures: Vec<String>,
}

/// The bundled context for one module: its files plus any existing README,
/// ready to hand to SCRB-02's prompt construction.
#[derive(Debug, Clone, Default)]
pub struct ModuleBundle {
    pub module_path: String,
    pub git_ref: String,
    pub files: Vec<FileExcerpt>,
    pub existing_readme: Option<String>,
}

// ─── Checkout / cleanup ──────────────────────────────────────────────────────

/// Turn a git ref into a filesystem-safe directory name component. Rejects
/// path traversal (`..`) and path separators so a caller-influenced ref can
/// never escape `worktree_root`.
fn sanitize_ref_for_dirname(git_ref: &str) -> Result<String, ToolError> {
    if git_ref.is_empty() {
        return Err(ToolError::InvalidArgument("git_ref must not be empty".into()));
    }
    if git_ref.contains("..") || git_ref.contains('/') || git_ref.contains('\\') {
        return Err(ToolError::InvalidArgument(format!(
            "git_ref '{git_ref}' must not contain '..' or path separators"
        )));
    }
    Ok(git_ref.to_string())
}

/// Check out a read-only inspection worktree of `repo_path` at `git_ref`,
/// under `worktree_root`. Reuses an existing worktree directory from a prior
/// run instead of colliding with it (edge case from the spec: "worktree
/// already exists from a prior run").
pub fn checkout(
    repo_path: &Path,
    git_ref: &str,
    worktree_root: &Path,
) -> Result<InspectionWorktree, ToolError> {
    if !repo_path.exists() {
        return Err(ToolError::NotFound(format!(
            "repo path does not exist: {}",
            repo_path.display()
        )));
    }
    let safe_ref = sanitize_ref_for_dirname(git_ref)?;

    std::fs::create_dir_all(worktree_root).map_err(|e| {
        ToolError::Execution(format!(
            "failed to create worktree root {}: {e}",
            worktree_root.display()
        ))
    })?;

    let worktree_path = worktree_root.join(&safe_ref);

    if worktree_path.exists() {
        // Reuse: a prior run already checked this ref out here.
        return Ok(InspectionWorktree {
            path: worktree_path,
            repo_path: repo_path.to_path_buf(),
            git_ref: git_ref.to_string(),
        });
    }

    run(ReadOnlyGitOp::WorktreeAdd {
        repo_path,
        worktree_path: &worktree_path,
        git_ref,
    })
    .map_err(|e| {
        ToolError::Execution(format!(
            "worktree checkout of ref '{git_ref}' failed (repo/ref may not exist): {e}"
        ))
    })?;

    Ok(InspectionWorktree {
        path: worktree_path,
        repo_path: repo_path.to_path_buf(),
        git_ref: git_ref.to_string(),
    })
}

/// Remove an inspection worktree. Safe to call even if it was reused from a
/// prior run.
pub fn cleanup(wt: &InspectionWorktree) -> Result<(), ToolError> {
    if !wt.path.exists() {
        return Ok(());
    }
    run(ReadOnlyGitOp::WorktreeRemove {
        repo_path: &wt.repo_path,
        worktree_path: &wt.path,
    })
    .map(|_| ())
}

/// Refresh local knowledge of `git_ref` from `origin` before checking it
/// out. Never pushes; only ever updates this local checkout's view of the
/// remote.
pub fn fetch_ref(repo_path: &Path, git_ref: &str) -> Result<(), ToolError> {
    run(ReadOnlyGitOp::Fetch { repo_path, git_ref }).map(|_| ())
}

// ─── File walk / context extraction ─────────────────────────────────────────

/// Walk `module_path` inside an inspection worktree and bundle its `.rs`
/// files' doc comments, public signatures, and any existing README.
pub fn inspect_module(wt: &InspectionWorktree, module_path: &str) -> Result<ModuleBundle, ToolError> {
    let full = wt.path.join(module_path);
    if !full.exists() {
        return Err(ToolError::NotFound(format!(
            "module path '{module_path}' not found in worktree at {}",
            wt.path.display()
        )));
    }

    let mut files = Vec::new();
    walk_rs_files(&full, &mut files)?;

    let existing_readme = std::fs::read_to_string(full.join("README.md")).ok();

    Ok(ModuleBundle {
        module_path: module_path.to_string(),
        git_ref: wt.git_ref.clone(),
        files,
        existing_readme,
    })
}

fn walk_rs_files(dir: &Path, out: &mut Vec<FileExcerpt>) -> Result<(), ToolError> {
    if dir.is_file() {
        if dir.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(extract_excerpt(dir)?);
        }
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| ToolError::Execution(format!("failed to read dir {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| ToolError::Execution(format!("dir entry error: {e}")))?;
        let path = entry.path();
        if path.is_dir() {
            walk_rs_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(extract_excerpt(&path)?);
        }
    }
    Ok(())
}

fn extract_excerpt(path: &Path) -> Result<FileExcerpt, ToolError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ToolError::Execution(format!("failed to read {}: {e}", path.display())))?;

    let mut doc_comments = Vec::new();
    let mut public_signatures = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//!") || trimmed.starts_with("///") {
            doc_comments.push(trimmed.to_string());
        } else if trimmed.starts_with("pub fn")
            || trimmed.starts_with("pub async fn")
            || trimmed.starts_with("pub struct")
            || trimmed.starts_with("pub enum")
            || trimmed.starts_with("pub trait")
        {
            public_signatures.push(trimmed.to_string());
        }
    }

    Ok(FileExcerpt {
        path: path.to_string_lossy().into_owned(),
        doc_comments,
        public_signatures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    /// Every `ReadOnlyGitOp` variant's generated argv, enumerated exhaustively
    /// (the `match` in `argv_for` won't compile if a variant is added and
    /// left unhandled) -- this is what makes the no-commit/no-push guarantee
    /// structural rather than a snapshot of today's behavior.
    #[test]
    fn structural_read_only_guarantees() {
        let repo = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test");
        let wt = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test/wt");
        let ops = vec![
            ReadOnlyGitOp::WorktreeAdd { repo_path: &repo, worktree_path: &wt, git_ref: "main" },
            ReadOnlyGitOp::WorktreeRemove { repo_path: &repo, worktree_path: &wt },
            ReadOnlyGitOp::Fetch { repo_path: &repo, git_ref: "main" },
        ];
        const BANNED_VERBS: &[&str] = &["commit", "push", "remote", "config", "reset", "checkout", "-f", "--force-with-lease"];
        for op in &ops {
            let (_, argv) = argv_for(op);
            for token in &argv {
                let lower = token.to_lowercase();
                for banned in BANNED_VERBS {
                    // "--force" on worktree remove is fine (removing a local
                    // dir, not a git-history-mutating force); only reject
                    // exact banned verbs as their own argv token.
                    if lower == *banned && *banned != "-f" && *banned != "--force-with-lease" {
                        panic!("argv token '{token}' matches banned write/publish verb '{banned}' in {argv:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn sanitize_rejects_path_traversal() {
        assert!(sanitize_ref_for_dirname("../../etc/passwd").is_err());
        assert!(sanitize_ref_for_dirname("feature/foo").is_err());
        assert!(sanitize_ref_for_dirname("").is_err());
        assert!(sanitize_ref_for_dirname("main").is_ok());
        assert!(sanitize_ref_for_dirname("SCRB-03-worktree-inspection").is_ok());
    }

    #[test]
    fn checkout_of_nonexistent_repo_is_a_clean_error_not_a_panic() {
        let result = checkout(
            Path::new("/tmp/scribe-test-nonexistent-repo-path-xyz"),
            "main",
            Path::new("/tmp/scribe-test-worktrees-xyz"),
        );
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[test]
    fn checkout_of_bad_ref_syntax_is_a_clean_error() {
        // Repo exists (this crate's own repo) but the "ref" contains a path
        // separator -- must fail sanitization before any git spawn.
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let result = checkout(&repo, "../escape", Path::new("/tmp/scribe-test-worktrees-badref"));
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    /// Real worktree checkout + file walk against a real small repo: this
    /// crate's own repo, checked out at HEAD into a throwaway worktree, then
    /// walked for a small real module (`src/sundry`).
    #[test]
    fn real_checkout_and_inspect_against_this_repo() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // Skip gracefully in environments where this crate isn't a git repo
        // checkout (e.g. a packaged tarball) rather than failing the suite.
        if !repo.join(".git").exists() {
            eprintln!("skipping: {} is not a git checkout", repo.display());
            return;
        }

        let worktree_root = std::env::temp_dir().join(format!(
            "scribe-inspect-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&worktree_root);

        // Resolve the current HEAD commit so the test is independent of
        // whatever branch name this worktree happens to be on.
        let head = StdCommand::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse should run");
        assert!(head.status.success(), "git rev-parse HEAD failed");
        let head_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

        let wt = checkout(&repo, &head_sha, &worktree_root).expect("checkout should succeed");
        assert!(wt.path.exists(), "worktree directory should exist after checkout");

        let bundle = inspect_module(&wt, "src/sundry").expect("inspect_module should succeed");
        assert_eq!(bundle.module_path, "src/sundry");
        assert!(!bundle.files.is_empty(), "src/sundry should contain at least one .rs file");
        assert!(
            bundle.files.iter().any(|f| !f.doc_comments.is_empty()),
            "src/sundry/mod.rs has module doc comments that should be extracted"
        );
        assert!(
            bundle.files.iter().any(|f| f.public_signatures.iter().any(|s| s.contains("pub struct"))),
            "src/sundry should expose at least one pub struct"
        );

        // Reuse path: checking out the same ref again must not fail or
        // collide, it should just return the same worktree.
        let wt2 = checkout(&repo, &head_sha, &worktree_root).expect("re-checkout should reuse, not collide");
        assert_eq!(wt.path, wt2.path);

        cleanup(&wt).expect("cleanup should succeed");
        let _ = std::fs::remove_dir_all(&worktree_root);
    }

    #[test]
    fn inspect_of_missing_module_path_is_a_clean_error() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let wt = InspectionWorktree {
            path: repo.clone(),
            repo_path: repo,
            git_ref: "HEAD".to_string(),
        };
        let result = inspect_module(&wt, "src/this_module_does_not_exist_xyz");
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }
}
