//! Obsidian-compatible vault output for Scribe (SCRB-05).
//!
//! All Scribe-generated wiki/notes/build-diary content lands in a git-backed
//! directory structure directly openable as an Obsidian vault: plain
//! Markdown, YAML frontmatter, `[[wikilink]]`-style cross-references, with
//! git itself as the sync mechanism.
//!
//! ## Directory structure
//!   - `modules/{module}/README.md`
//!   - `modules/{module}/wiki/{slug}.md`
//!   - `build-diaries/{date}-{spec_id}.md`
//!   - `blog/{date}-{title-slug}.md`
//!
//! ## Frontmatter
//! Every note gets `title`, `module`, `generated_at`, `source_commit` (the
//! exact commit the note was generated against, so staleness is always
//! detectable), and `type` (readme/wiki/build-diary/blog).
//!
//! ## Committing to the vault: this module's write surface, and its guardrails
//! Vault commit/push is Scribe's ONLY code-adjacent write surface (it writes
//! docs content, never source). Two structural guardrails, mirroring
//! `inspect.rs`'s pattern from SCRB-03/02:
//!   - Every git invocation is built from [`VaultGitOp`], a closed enum
//!     (`Clone`/`Pull`/`AddCommitAll`/`Push`) with NO force-push variant --
//!     `assert_never_force_push` (called from inside [`argv_for`] on every
//!     real invocation, not just test-constructed instances) additionally
//!     asserts no argv ever contains a force-push token, so even a future
//!     mistaken addition to an existing arm is caught immediately.
//!   - Committing/pushing to the vault shells out to `git`
//!     (`std::process::Command`), the same `RustTool` no-subprocess-contract
//!     tension SCRB-02 resolved for read-only inspection. Same resolution
//!     here: gated behind `ScribeConfig::allow_subprocess_vault_write`
//!     (default false, env `SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE`) in
//!     `src/scribe/mod.rs`'s `execute()` bodies, not unconditionally
//!     reachable. The real fix remains the same `git2` library swap noted in
//!     `inspect.rs`; not done here for the same no-registry-access reason.
//!   - "Concurrent Scribe runs writing to the same vault -- use normal git
//!     conflict handling (pull before push), don't force-push" (spec edge
//!     case): [`commit_and_push`] always pulls before pushing, and the
//!     force-push guardrail above makes a force-push structurally
//!     unreachable, not just avoided by convention.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::ToolError;

// ─── Note types, frontmatter, wikilinks (pure, no I/O) ──────────────────────

/// The four note types the spec's directory structure distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteType {
    Readme,
    Wiki,
    BuildDiary,
    Blog,
}

impl NoteType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NoteType::Readme => "readme",
            NoteType::Wiki => "wiki",
            NoteType::BuildDiary => "build-diary",
            NoteType::Blog => "blog",
        }
    }
}

/// Frontmatter fields every generated note carries.
#[derive(Debug, Clone)]
pub struct NoteFrontmatter {
    pub title: String,
    pub module: String,
    /// RFC3339 timestamp.
    pub generated_at: String,
    /// The exact commit this note was generated against, so staleness is
    /// always detectable (a note whose `source_commit` no longer matches
    /// the module's current HEAD is a candidate for regeneration).
    pub source_commit: String,
    pub note_type: NoteType,
}

/// Render a note's YAML frontmatter block. Values are placed in double
/// quotes and any embedded `"` is escaped, so a title/module containing a
/// quote character can never break the YAML block's structure.
fn render_frontmatter(fm: &NoteFrontmatter) -> String {
    fn yaml_quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
    format!(
        "---\ntitle: {title}\nmodule: {module}\ngenerated_at: {generated_at}\nsource_commit: {source_commit}\ntype: {note_type}\n---\n\n",
        title = yaml_quote(&fm.title),
        module = yaml_quote(&fm.module),
        generated_at = yaml_quote(&fm.generated_at),
        source_commit = yaml_quote(&fm.source_commit),
        note_type = fm.note_type.as_str(),
    )
}

/// Build an Obsidian-style wikilink to another note by its title.
pub fn build_wikilink(target_title: &str) -> String {
    format!("[[{target_title}]]")
}

/// Render a full note: frontmatter + body + an optional "Related" section
/// linking to other notes by title (wikilinks). At least one real wikilink
/// is included whenever `related` is non-empty, satisfying the acceptance
/// criterion that generated notes have "at least one real wikilink where
/// applicable."
pub fn render_note(fm: &NoteFrontmatter, body: &str, related: &[String]) -> String {
    let mut out = render_frontmatter(fm);
    out.push_str(body.trim_end());
    out.push('\n');
    if !related.is_empty() {
        out.push_str("\n## Related\n\n");
        for title in related {
            out.push_str("- ");
            out.push_str(&build_wikilink(title));
            out.push('\n');
        }
    }
    out
}

/// A conservative filesystem-safe slug: lowercase ASCII alphanumerics and
/// hyphens only. Anything else (including path separators and `..`) is
/// dropped, so a caller-influenced title/spec_id can never be used to escape
/// the vault's directory structure.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_sep = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep && !out.is_empty() {
            out.push('-');
            last_was_sep = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Compute the path a note lands at within the vault, per the spec's
/// directory structure convention. `module`/`slug` are slugified internally
/// so a caller can never traverse outside the vault root via `..` or a path
/// separator.
pub fn note_path(vault_root: &Path, note_type: NoteType, module: &str, slug: &str) -> PathBuf {
    let module = slugify(module);
    let slug = slugify(slug);
    match note_type {
        NoteType::Readme => vault_root.join("modules").join(module).join("README.md"),
        NoteType::Wiki => vault_root.join("modules").join(module).join("wiki").join(format!("{slug}.md")),
        NoteType::BuildDiary => vault_root.join("build-diaries").join(format!("{slug}.md")),
        NoteType::Blog => vault_root.join("blog").join(format!("{slug}.md")),
    }
}

// ─── Vault git operations (write-capable, force-push-free) ──────────────────

/// Closed set of git operations vault writing can ever perform. No variant
/// here can force-push or force-overwrite history -- see the module doc
/// comment.
#[derive(Debug, Clone, Copy)]
enum VaultGitOp<'a> {
    Pull { vault_dir: &'a Path },
    AddCommitAll { vault_dir: &'a Path, message: &'a str },
    Push { vault_dir: &'a Path },
}

const BANNED_FORCE_TOKENS: &[&str] = &["--force", "-f", "--force-with-lease"];

/// Always-on guard, called from inside [`argv_for`] for every real
/// invocation (mirrors `inspect.rs`'s `assert_read_only_argv`): panics if
/// `argv` contains a force-push token as an exact element.
fn assert_never_force_push(argv: &[String]) {
    for token in argv {
        let lower = token.to_lowercase();
        assert!(
            !BANNED_FORCE_TOKENS.contains(&lower.as_str()),
            "vault git argv contained a force-push token '{token}': {argv:?}"
        );
    }
}

fn argv_for(op: &VaultGitOp) -> (PathBuf, Vec<String>) {
    let (cwd, argv) = match op {
        VaultGitOp::Pull { vault_dir } => (
            vault_dir.to_path_buf(),
            vec!["pull".into(), "--ff-only".into()],
        ),
        VaultGitOp::AddCommitAll { vault_dir, message } => (
            vault_dir.to_path_buf(),
            vec!["commit".into(), "-a".into(), "-m".into(), message.to_string()],
        ),
        VaultGitOp::Push { vault_dir } => (
            vault_dir.to_path_buf(),
            vec!["push".into()],
        ),
    };
    assert_never_force_push(&argv);
    (cwd, argv)
}

fn run(op: VaultGitOp) -> Result<String, ToolError> {
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

/// Write `content` to `path` within an already-cloned vault working
/// directory (creating parent directories as needed), then `git add -A`
/// implicitly via `commit -a` is NOT sufficient for a brand-new untracked
/// file -- `git add` the specific path explicitly first, then commit, then
/// pull (fast-forward only, never a merge commit) before push, and finally
/// push. Never force-pushes (structurally, see [`VaultGitOp`]).
pub fn write_note_and_push(
    vault_dir: &Path,
    note_path: &Path,
    content: &str,
    commit_message: &str,
) -> Result<(), ToolError> {
    if let Some(parent) = note_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError::Execution(format!("failed to create {}: {e}", parent.display())))?;
    }
    std::fs::write(note_path, content)
        .map_err(|e| ToolError::Execution(format!("failed to write {}: {e}", note_path.display())))?;

    // `git add <path>` explicitly -- a brand-new file has no index entry yet
    // for `commit -a` (which only stages already-tracked modifications) to
    // pick up.
    let add_output = Command::new("git")
        .current_dir(vault_dir)
        .args(["add", &note_path.to_string_lossy()])
        .output()
        .map_err(|e| ToolError::Execution(format!("failed to spawn git add: {e}")))?;
    if !add_output.status.success() {
        return Err(ToolError::Execution(format!(
            "git add failed: {}",
            String::from_utf8_lossy(&add_output.stderr).trim()
        )));
    }

    run(VaultGitOp::AddCommitAll { vault_dir, message: commit_message })?;

    // Pull (fast-forward only) before push, per the spec's edge case:
    // concurrent Scribe runs use normal git conflict handling, never force.
    // A `--ff-only` pull failing (real divergent history) surfaces as a
    // clean error here rather than being silently forced through.
    run(VaultGitOp::Pull { vault_dir })?;
    run(VaultGitOp::Push { vault_dir })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    // ─── Pure function tests ─────────────────────────────────────────────────

    #[test]
    fn slugify_lowercases_and_hyphenates() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  Foo   Bar  "), "foo-bar");
        assert_eq!(slugify("S91-scribe-knowledge-infrastructure"), "s91-scribe-knowledge-infrastructure");
    }

    #[test]
    fn slugify_rejects_path_traversal_shapes() {
        // ".." and "/" are stripped to hyphens/dropped, never preserved --
        // a slug can never be used to escape the vault directory structure.
        let slug = slugify("../../etc/passwd");
        assert!(!slug.contains(".."));
        assert!(!slug.contains('/'));
    }

    #[test]
    fn build_wikilink_wraps_in_double_brackets() {
        assert_eq!(build_wikilink("Some Note"), "[[Some Note]]");
    }

    #[test]
    fn render_note_includes_frontmatter_body_and_wikilinks() {
        let fm = NoteFrontmatter {
            title: "Sundry".to_string(),
            module: "sundry".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(),  // pii-test-fixture
            source_commit: "abc123".to_string(),
            note_type: NoteType::Readme,
        };
        let note = render_note(&fm, "This module does X.", &["Build Diary Entry".to_string()]);
        assert!(note.starts_with("---\n"));
        assert!(note.contains("title: \"Sundry\""));
        assert!(note.contains("module: \"sundry\""));
        assert!(note.contains("source_commit: \"abc123\""));
        assert!(note.contains("type: readme"));
        assert!(note.contains("This module does X."));
        assert!(note.contains("[[Build Diary Entry]]"), "expected a real wikilink: {note}");
    }

    #[test]
    fn render_note_without_related_has_no_related_section() {
        let fm = NoteFrontmatter {
            title: "X".to_string(),
            module: "x".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(),  // pii-test-fixture
            source_commit: "abc".to_string(),
            note_type: NoteType::BuildDiary,
        };
        let note = render_note(&fm, "body", &[]);
        assert!(!note.contains("## Related"));
    }

    #[test]
    fn render_note_frontmatter_escapes_embedded_quotes() {
        let fm = NoteFrontmatter {
            title: "A \"quoted\" title".to_string(),
            module: "x".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(),  // pii-test-fixture
            source_commit: "abc".to_string(),
            note_type: NoteType::Wiki,
        };
        let note = render_note(&fm, "body", &[]);
        assert!(note.contains("title: \"A \\\"quoted\\\" title\""));
    }

    #[test]
    fn note_path_matches_the_spec_directory_convention() {
        let root = Path::new("/vault");
        assert_eq!(
            note_path(root, NoteType::Readme, "Sundry Module", "ignored"),
            root.join("modules/sundry-module/README.md")
        );
        assert_eq!(
            note_path(root, NoteType::Wiki, "sundry", "Some Page"),
            root.join("modules/sundry/wiki/some-page.md")
        );
        assert_eq!(
            note_path(root, NoteType::BuildDiary, "ignored", "2026-07-07-s91"),  // pii-test-fixture
            root.join("build-diaries/2026-07-07-s91.md")  // pii-test-fixture
        );
        assert_eq!(
            note_path(root, NoteType::Blog, "ignored", "2026-07-07-scribe-launch"),  // pii-test-fixture
            root.join("blog/2026-07-07-scribe-launch.md")  // pii-test-fixture
        );
    }

    #[test]
    fn note_path_module_and_slug_are_slugified_defensively() {
        let root = Path::new("/vault");
        let path = note_path(root, NoteType::Wiki, "../../etc", "../../passwd");
        assert!(!path.to_string_lossy().contains(".."));
    }

    // ─── Structural no-force-push guarantee ──────────────────────────────────

    #[test]
    fn vault_git_ops_never_contain_a_force_token() {
        let dir = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test");
        let ops = vec![
            VaultGitOp::Pull { vault_dir: &dir },
            VaultGitOp::AddCommitAll { vault_dir: &dir, message: "test" },
            VaultGitOp::Push { vault_dir: &dir },
        ];
        for op in &ops {
            let (_, argv) = argv_for(op);
            for token in &argv {
                assert!(!BANNED_FORCE_TOKENS.contains(&token.to_lowercase().as_str()));
            }
        }
    }

    #[test]
    #[should_panic(expected = "force-push token")]
    fn assert_never_force_push_actually_rejects_a_force_token() {
        assert_never_force_push(&["push".to_string(), "--force".to_string()]);
    }

    // ─── Real git: local bare repo standing in for the vault remote ─────────

    fn run_git(dir: &Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn git {args:?} in {}: {e}", dir.display()));
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Real commit+push to a vault repo, verified via a FRESH clone -- the
    /// spec's own acceptance criterion, executed against a local bare repo
    /// standing in for the real Gitea `moosenet/scribe-vault` remote (which
    /// this sandbox cannot reach -- see the status report). The git
    /// mechanics exercised here (clone, add, commit, pull --ff-only, push,
    /// and a fresh independent clone reading the result back) are identical
    /// regardless of what's on the other end of the remote URL.
    #[test]
    fn write_note_and_push_is_verifiable_via_a_fresh_clone() {
        let base = std::env::temp_dir().join(format!("scribe-vault-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let bare_remote = base.join("remote.git");
        let working_copy = base.join("working");
        let fresh_clone = base.join("fresh-clone");

        // A bare repo stands in for the Gitea vault remote.
        std::fs::create_dir_all(&bare_remote).unwrap();
        run_git(&bare_remote, &["init", "--bare", "-q"]);

        // Clone it as the working copy Scribe writes into (mirrors "vault
        // repo doesn't exist yet" being out of scope here -- SCRB-05 assumes
        // an already-cloned working copy, per vault.rs's module doc comment;
        // an empty bare repo has no commits/branch yet, so this test seeds
        // one initial commit exactly as a real first-time vault setup would).
        run_git(&base, &["clone", "-q", bare_remote.to_str().unwrap(), working_copy.to_str().unwrap()]);
        // Force the local branch to be named "main" regardless of this
        // environment's `init.defaultBranch` (git's compiled-in default is
        // "master", not "main"), so the push/upstream refspecs below are
        // unambiguous.
        run_git(&working_copy, &["checkout", "-q", "-B", "main"]);
        std::fs::write(working_copy.join("README.md"), "# Scribe Vault\n").unwrap();
        run_git(&working_copy, &["add", "README.md"]);
        run_git(&working_copy, &["-c", "user.email=<email>", "-c", "user.name=Scribe", "commit", "-q", "-m", "init"]);  // pii-test-fixture
        run_git(&working_copy, &["push", "-q", "origin", "HEAD:main"]);
        // Set the working copy's branch to track origin/main so a plain
        // `git pull`/`git push` (no refspec) -- exactly what
        // `write_note_and_push` runs -- resolves unambiguously.
        run_git(&working_copy, &["branch", "-q", "--set-upstream-to=origin/main", "main"]);
        // The bare repo's own HEAD symref still points at whatever
        // `init.defaultBranch` was at `git init --bare` time (typically
        // "master"), which never got a branch pushed to it -- without
        // fixing this, a later `git clone` of the bare repo warns "remote
        // HEAD refers to nonexistent ref" and checks out nothing, even
        // though the "main" branch and its commits are genuinely present.
        // A real first-time vault setup would do the same repair.
        run_git(&bare_remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        let fm = NoteFrontmatter {
            title: "Sundry".to_string(),
            module: "sundry".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(),  // pii-test-fixture
            source_commit: "abc123".to_string(),
            note_type: NoteType::Readme,
        };
        let content = render_note(&fm, "Sundry tools live here.", &[]);
        let note_path_in_vault = note_path(&working_copy, NoteType::Readme, "sundry", "ignored");

        // write_note_and_push needs a committer identity -- set it on the
        // working copy before calling (production deploys configure this
        // once; a test harness does the same).
        run_git(&working_copy, &["config", "user.email", "<email>"]);  // pii-test-fixture
        run_git(&working_copy, &["config", "user.name", "Scribe"]);

        write_note_and_push(
            &working_copy,
            &note_path_in_vault,
            &content,
            "scribe: add sundry README",
        )
        .expect("write_note_and_push should succeed against the local bare remote");

        // Verify via a FRESH, independent clone -- not the same working
        // copy that wrote it.
        run_git(&base, &["clone", "-q", bare_remote.to_str().unwrap(), fresh_clone.to_str().unwrap()]);
        let verified_path = fresh_clone.join("modules/sundry/README.md");
        assert!(verified_path.exists(), "expected {} to exist in the fresh clone", verified_path.display());
        let verified_content = std::fs::read_to_string(&verified_path).unwrap();
        assert_eq!(verified_content, content);
        assert!(verified_content.contains("Sundry tools live here."));
        assert!(verified_content.contains("type: readme"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_note_and_push_creates_parent_directories() {
        let base = std::env::temp_dir().join(format!("scribe-vault-test-mkdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let bare_remote = base.join("remote.git");
        let working_copy = base.join("working");
        std::fs::create_dir_all(&bare_remote).unwrap();
        run_git(&bare_remote, &["init", "--bare", "-q"]);
        run_git(&base, &["clone", "-q", bare_remote.to_str().unwrap(), working_copy.to_str().unwrap()]);
        run_git(&working_copy, &["checkout", "-q", "-B", "main"]);
        std::fs::write(working_copy.join("README.md"), "# Vault\n").unwrap();
        run_git(&working_copy, &["config", "user.email", "<email>"]);  // pii-test-fixture
        run_git(&working_copy, &["config", "user.name", "Scribe"]);
        run_git(&working_copy, &["add", "README.md"]);
        run_git(&working_copy, &["commit", "-q", "-m", "init"]);
        run_git(&working_copy, &["push", "-q", "origin", "HEAD:main"]);
        run_git(&working_copy, &["branch", "-q", "--set-upstream-to=origin/main", "main"]);
        run_git(&bare_remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        let nested_path = working_copy.join("build-diaries/2026-07-07-s91.md");  // pii-test-fixture
        assert!(!nested_path.parent().unwrap().exists());

        let fm = NoteFrontmatter {
            title: "S91".to_string(),
            module: "build-pipeline".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(),  // pii-test-fixture
            source_commit: "n/a".to_string(),
            note_type: NoteType::BuildDiary,
        };
        let content = render_note(&fm, "Narrative.", &[]);

        write_note_and_push(&working_copy, &nested_path, &content, "scribe: diary entry")
            .expect("should create build-diaries/ and succeed");
        assert!(nested_path.exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
