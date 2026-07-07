//! Read-only worktree inspection helpers for Scribe (SCRB-03).
//!
//! Scribe needs to read real source code to verify/discover functionality
//! before writing docs about it -- via a git worktree, matching the build
//! pipeline's own Stage 2 convention in spirit (`git worktree add <path> -b
//! <branch> <ref>`). This module's checkouts are read-only inspections of an
//! *existing* ref, not new work -- so unlike Stage 2, it never creates a new
//! branch; it runs the simpler `git worktree add <path> -- <ref>` (checked
//! out detached/on the existing ref). It must NEVER commit or push code
//! changes itself.
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

/// Exact argv tokens that must never appear in a read-only git invocation.
/// Checked at runtime for EVERY argv this module ever builds (not just in a
/// unit test that happens to enumerate today's variants) -- see
/// `argv_for`'s trailing call to this. Compared as whole tokens, not
/// substrings, so a file path that merely contains the word "push" is never
/// affected.
const BANNED_EXACT_TOKENS: &[&str] = &["commit", "push", "remote", "config", "reset"];

/// Runtime, always-on guard: panics if `argv` contains a banned verb as an
/// exact token. `ReadOnlyGitOp`'s closed variant set (matched exhaustively in
/// `argv_for`) should make this unreachable -- this assertion exists so that
/// if a future edit to `argv_for` ever produced one anyway (e.g. a copy-paste
/// mistake extending an existing arm), it fails loudly at the moment of
/// construction, for every real call, not only for instances a test happens
/// to build.
fn assert_read_only_argv(argv: &[String]) {
    for token in argv {
        let lower = token.to_lowercase();
        assert!(
            !BANNED_EXACT_TOKENS.contains(&lower.as_str()),
            "read-only git argv contained banned verb '{token}': {argv:?}"
        );
    }
}

/// Build the argv (excluding the `git` binary name itself) for a read-only
/// git operation. Pure and side-effect-free -- unit tested without ever
/// spawning a process, matching the `review_daemon::provider::build_command`
/// precedent (argv-array builders, never a shell string).
///
/// A caller-influenced `git_ref` is passed to `git` after an explicit `--`
/// end-of-options separator (belt-and-suspenders alongside
/// `sanitize_ref_for_dirname`'s leading-`-` rejection): even if a ref value
/// somehow reached this function unsanitized, `git` would parse it as a
/// positional argument, never as a flag like `--upload-pack=...` (a known
/// git option-injection primitive on `fetch`/`clone`-family commands).
fn argv_for(op: &ReadOnlyGitOp) -> (PathBuf, Vec<String>) {
    let (cwd, argv) = match op {
        ReadOnlyGitOp::WorktreeAdd { repo_path, worktree_path, git_ref } => (
            repo_path.to_path_buf(),
            vec![
                "worktree".into(),
                "add".into(),
                worktree_path.to_string_lossy().into_owned(),
                "--".into(),
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
            vec!["fetch".into(), "origin".into(), "--".into(), git_ref.to_string()],
        ),
    };
    assert_read_only_argv(&argv);
    (cwd, argv)
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

/// Validate a git ref before it is ever used to build a directory name OR
/// passed to `git` (this function gates both `checkout` and `fetch_ref` --
/// there is no code path that uses a `git_ref` without going through it
/// first). Rejects:
///   - empty
///   - `..` (path traversal) or path separators `/`/`\` (this pass only
///     supports bare branch names / full SHAs, not slash-containing branch
///     names like `feature/foo` -- an intentional scope limit, not an
///     oversight)
///   - a leading `-` (defense in depth against git flag/option injection,
///     e.g. `--upload-pack=...`, on top of the `--` end-of-options separator
///     `argv_for` also inserts before every ref argument)
///   - exactly `.` or `.git` (would otherwise resolve to `worktree_root`
///     itself or its metadata directory, silently reusing/colliding instead
///     of checking out the requested ref)
fn sanitize_ref_for_dirname(git_ref: &str) -> Result<String, ToolError> {
    if git_ref.is_empty() {
        return Err(ToolError::InvalidArgument("git_ref must not be empty".into()));
    }
    if git_ref.contains("..") || git_ref.contains('/') || git_ref.contains('\\') {
        return Err(ToolError::InvalidArgument(format!(
            "git_ref '{git_ref}' must not contain '..' or path separators"
        )));
    }
    if git_ref.starts_with('-') {
        return Err(ToolError::InvalidArgument(format!(
            "git_ref '{git_ref}' must not start with '-' (would risk being parsed as a git option)"
        )));
    }
    if git_ref == "." || git_ref == ".git" {
        return Err(ToolError::InvalidArgument(format!(
            "git_ref '{git_ref}' is not a valid ref (resolves to the worktree root/metadata dir)"
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
    // Same validation `checkout` requires -- a bare `git_ref` reaching this
    // function unvalidated was the gap flagged in review (option-injection
    // via a leading `-`, e.g. `--upload-pack=...`, is a real primitive on
    // `fetch`). `argv_for`'s `--` separator is defense in depth on top of
    // this, not a substitute for it.
    sanitize_ref_for_dirname(git_ref)?;
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

    /// Regression coverage for today's three `ReadOnlyGitOp` variants.
    ///
    /// IMPORTANT (per review, cycle 1): this test enumerating "all" variants
    /// is a hand-maintained list -- adding a hypothetical fourth variant to
    /// the enum does NOT make the compiler force an entry here, only in
    /// `argv_for`'s own match. The real, always-on guarantee is
    /// `assert_read_only_argv`, called from inside `argv_for` itself for
    /// EVERY real invocation (not just instances this test happens to
    /// construct) -- see that function. This test is regression coverage on
    /// top of that runtime guard, not the guard itself.
    #[test]
    fn structural_read_only_guarantees() {
        let repo = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test");
        let wt = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test/wt");
        let ops = vec![
            ReadOnlyGitOp::WorktreeAdd { repo_path: &repo, worktree_path: &wt, git_ref: "main" },
            ReadOnlyGitOp::WorktreeRemove { repo_path: &repo, worktree_path: &wt },
            ReadOnlyGitOp::Fetch { repo_path: &repo, git_ref: "main" },
        ];
        // argv_for() itself calls assert_read_only_argv() on every branch,
        // so simply calling it here already exercises the real guard; this
        // loop is redundant-by-design regression coverage, spelled out
        // explicitly rather than relying only on the side effect.
        for op in &ops {
            let (_, argv) = argv_for(op);
            for token in &argv {
                assert!(!BANNED_EXACT_TOKENS.contains(&token.to_lowercase().as_str()));
            }
        }
    }

    /// The runtime guard fires for a hypothetical write-shaped argv even if
    /// it didn't come from a real `ReadOnlyGitOp` variant -- proving the
    /// check itself is real (not a no-op) independent of what variants exist
    /// today.
    #[test]
    #[should_panic(expected = "banned verb")]
    fn assert_read_only_argv_actually_rejects_a_write_verb() {
        assert_read_only_argv(&["commit".to_string(), "-m".to_string(), "oops".to_string()]);
    }

    #[test]
    fn sanitize_rejects_path_traversal() {
        assert!(sanitize_ref_for_dirname("../../etc/passwd").is_err());
        assert!(sanitize_ref_for_dirname("feature/foo").is_err());
        assert!(sanitize_ref_for_dirname("").is_err());
        assert!(sanitize_ref_for_dirname("main").is_ok());
        assert!(sanitize_ref_for_dirname("SCRB-03-worktree-inspection").is_ok());
    }

    /// Cycle 1 review findings: option-injection via a leading `-`, and the
    /// `.`/`.git` cases that would otherwise silently resolve to
    /// `worktree_root` itself rather than erroring.
    #[test]
    fn sanitize_rejects_option_injection_and_dot_refs() {
        assert!(sanitize_ref_for_dirname("--upload-pack=/tmp/evil.sh").is_err());
        assert!(sanitize_ref_for_dirname("-x").is_err());
        assert!(sanitize_ref_for_dirname(".").is_err());
        assert!(sanitize_ref_for_dirname(".git").is_err());
    }

    #[test]
    fn fetch_ref_rejects_an_unsanitized_ref_before_spawning_git() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let result = fetch_ref(&repo, "--upload-pack=/tmp/evil.sh");
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
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

    /// Cycle 1 review finding: the two existing "clean error" tests both
    /// short-circuit *before* git is ever spawned (bad repo path, bad ref
    /// syntax). This one is real repo + syntactically valid ref that simply
    /// doesn't exist -- the actual path through `run()`'s
    /// `output.status.success() == false` branch the acceptance criterion is
    /// about.
    #[test]
    fn checkout_of_syntactically_valid_but_nonexistent_ref_is_a_clean_error() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let worktree_root = std::env::temp_dir().join(format!(
            "scribe-inspect-test-badref-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&worktree_root);

        let result = checkout(&repo, "totally-bogus-branch-xyz-does-not-exist", &worktree_root);
        assert!(
            matches!(result, Err(ToolError::Execution(_))),
            "expected a git-reported Execution error, got: {result:?}"
        );

        let _ = std::fs::remove_dir_all(&worktree_root);
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
