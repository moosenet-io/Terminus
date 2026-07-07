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
        // Order matters: backslash first (so later escapes' own backslashes
        // don't get re-escaped), then the other YAML-double-quoted-scalar
        // escapes. Cycle 1 review finding: the original version escaped
        // only `\` and `"` -- a title/spec_id containing an embedded
        // literal newline (not stripped by `.trim()`, which only strips
        // leading/trailing whitespace) would land unescaped inside the
        // quoted scalar, changing how a YAML parser folds it.
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        format!("\"{escaped}\"")
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
    if out.is_empty() {
        // Cycle 1 review finding: input with no ASCII alphanumerics at all
        // (pure non-ASCII -- Cyrillic, emoji, etc.) previously produced an
        // empty string, and note_path() would then build a collision-prone
        // path like `modules//README.md` (every such module colliding on
        // the same path) or `build-diaries/.md`. A short, stable hash of
        // the ORIGINAL input guarantees a non-empty, deterministic,
        // collision-resistant slug instead.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        input.hash(&mut hasher);
        return format!("untitled-{:08x}", hasher.finish() as u32);
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
/// comment. `Add` exists so `write_note_and_push`'s `git add <path>` call
/// goes through the SAME guardrail as every other vault git invocation
/// (cycle 1 review finding: it previously used a raw `Command::new("git")`
/// call, bypassing `argv_for`/`assert_never_force_push` entirely).
#[derive(Debug, Clone, Copy)]
enum VaultGitOp<'a> {
    Add { vault_dir: &'a Path, path: &'a Path },
    Pull { vault_dir: &'a Path },
    AddCommitAll { vault_dir: &'a Path, message: &'a str },
    /// Soft-reset the most recent commit, keeping its changes staged.
    /// Used ONLY to recover the working copy after a push failure (see
    /// `write_note_and_push`) so a retry isn't permanently wedged behind an
    /// orphaned local commit -- never used to rewrite already-pushed
    /// history (it only ever targets a commit `write_note_and_push` itself
    /// just made and confirmed was never pushed).
    SoftResetLastCommit { vault_dir: &'a Path },
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
        VaultGitOp::Add { vault_dir, path } => (
            vault_dir.to_path_buf(),
            vec!["add".into(), path.to_string_lossy().into_owned()],
        ),
        VaultGitOp::Pull { vault_dir } => (
            vault_dir.to_path_buf(),
            vec!["pull".into(), "--ff-only".into()],
        ),
        VaultGitOp::AddCommitAll { vault_dir, message } => (
            vault_dir.to_path_buf(),
            vec!["commit".into(), "-a".into(), "-m".into(), message.to_string()],
        ),
        VaultGitOp::SoftResetLastCommit { vault_dir } => (
            vault_dir.to_path_buf(),
            vec!["reset".into(), "--soft".into(), "HEAD~1".into()],
        ),
        VaultGitOp::Push { vault_dir } => (
            vault_dir.to_path_buf(),
            vec!["push".into()],
        ),
    };
    assert_never_force_push(&argv);
    (cwd, argv)
}

/// Runs a git op and returns `(success, stdout_or_stderr)` instead of
/// `Result` -- some callers (see `write_note_and_push`'s commit step) need
/// to distinguish "genuinely nothing to commit" (a clean, expected outcome)
/// from a real failure without losing the raw output to classify it.
fn run_raw(op: VaultGitOp) -> (bool, String, String) {
    let (cwd, args) = argv_for(&op);
    match Command::new("git").current_dir(&cwd).args(&args).output() {
        Ok(output) => (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), format!("failed to spawn git: {e}")),
    }
}

fn run(op: VaultGitOp) -> Result<String, ToolError> {
    let (cwd, args) = argv_for(&op);
    let (ok, stdout, stderr) = run_raw(op);
    if ok {
        Ok(stdout)
    } else {
        Err(ToolError::Execution(format!("git {} (in {}) failed: {}", args.join(" "), cwd.display(), stderr.trim())))
    }
}

/// Write `content` to `path` within an already-cloned vault working
/// directory (creating parent directories as needed), add, commit, pull
/// (fast-forward only), then push. Never force-pushes (structurally, see
/// [`VaultGitOp`]).
///
/// ## Failure recovery (cycle 1 review finding)
/// If `push` fails after a local commit was made (e.g. a concurrent Scribe
/// run won the race and pushed first), the local commit is soft-reset
/// (changes kept staged, commit undone) before returning the error -- so
/// the working copy is left retry-friendly rather than wedged behind an
/// orphaned local commit that would make every subsequent `--ff-only` pull
/// fail forever.
///
/// ## "Nothing to commit" (cycle 1 review finding)
/// If `content` is byte-identical to what's already committed (Scribe
/// re-run against unchanged source), `git commit` reports "nothing to
/// commit" -- this is treated as a clean, successful no-op (no error), not
/// a failure, since the vault already reflects the desired state.
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

    // If the file already exists with byte-identical content, there is
    // nothing to do -- short-circuit before ever touching git, so a
    // no-change re-run never even attempts add/commit/push.
    if std::fs::read(note_path).ok().as_deref() == Some(content.as_bytes()) {
        return Ok(());
    }

    std::fs::write(note_path, content)
        .map_err(|e| ToolError::Execution(format!("failed to write {}: {e}", note_path.display())))?;

    // `git add <path>` explicitly -- a brand-new file has no index entry yet
    // for `commit -a` (which only stages already-tracked modifications) to
    // pick up. Routed through VaultGitOp/argv_for like every other
    // invocation here (cycle 1 review finding: this previously bypassed the
    // closed-enum guardrail via a raw Command call).
    run(VaultGitOp::Add { vault_dir, path: note_path })?;

    let (committed, _commit_stdout, commit_stderr) =
        run_raw(VaultGitOp::AddCommitAll { vault_dir, message: commit_message });
    if !committed {
        // "nothing to commit" is a clean no-op (content matched what's
        // already staged/tracked after the add above, e.g. line-ending
        // normalization made the write byte-identical after all) -- any
        // OTHER commit failure is a real error.
        if commit_stderr.to_lowercase().contains("nothing to commit") {
            return Ok(());
        }
        return Err(ToolError::Execution(format!("git commit failed: {}", commit_stderr.trim())));
    }

    // Pull (fast-forward only) before push, per the spec's edge case:
    // concurrent Scribe runs use normal git conflict handling, never force.
    // A `--ff-only` pull failing (real divergent history) surfaces as a
    // clean error here rather than being silently forced through -- the
    // commit we just made stays local (not lost), ready for a manual or
    // automated retry after a rebase/merge.
    if let Err(e) = run(VaultGitOp::Pull { vault_dir }) {
        return Err(e);
    }

    if let Err(e) = run(VaultGitOp::Push { vault_dir }) {
        // Recover the working copy: undo our local commit (keeping changes
        // staged) so the NEXT attempt starts from a clean, retry-able state
        // instead of being wedged behind a commit that can never
        // fast-forward-pull again.
        let _ = run(VaultGitOp::SoftResetLastCommit { vault_dir });
        return Err(e);
    }

    Ok(())
}

/// Test-only helper (cycle 1 review finding: this ~30-line local-bare-repo
/// setup was duplicated near-verbatim in this module's own tests AND in
/// `scribe::mod::tests`'s end-to-end `scribe_build_diary_entry` test --
/// factored out here, `pub(crate)` so both test modules share one copy).
///
/// Sets up a bare repo (standing in for the real Gitea vault remote) plus a
/// working-copy clone with an initial commit already pushed to `main`,
/// upstream tracking configured, and the bare repo's own HEAD symref fixed
/// to point at `main` (a fresh bare repo's HEAD defaults to whatever
/// `init.defaultBranch` is, typically "master", which never gets a branch
/// pushed to it in this setup -- without fixing this, a later `git clone`
/// warns "remote HEAD refers to nonexistent ref" and checks out nothing).
/// Returns `(bare_remote_path, working_copy_path)`.
#[cfg(test)]
pub(crate) fn test_setup_bare_vault(base: &Path) -> (PathBuf, PathBuf) {
    fn run_git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
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

    let bare_remote = base.join("remote.git");
    let working_copy = base.join("working");
    std::fs::create_dir_all(&bare_remote).unwrap();
    run_git(&bare_remote, &["init", "--bare", "-q"]);
    run_git(base, &["clone", "-q", bare_remote.to_str().unwrap(), working_copy.to_str().unwrap()]);
    // Force the local branch to be named "main" regardless of this
    // environment's `init.defaultBranch` (git's compiled-in default is
    // "master", not "main"), so the push/upstream refspecs below are
    // unambiguous.
    run_git(&working_copy, &["checkout", "-q", "-B", "main"]);
    std::fs::write(working_copy.join("README.md"), "# Scribe Vault\n").unwrap();
    run_git(&working_copy, &["config", "user.email", "<email>"]); // pii-test-fixture
    run_git(&working_copy, &["config", "user.name", "Scribe"]);
    run_git(&working_copy, &["add", "README.md"]);
    run_git(&working_copy, &["commit", "-q", "-m", "init"]);
    run_git(&working_copy, &["push", "-q", "origin", "HEAD:main"]);
    run_git(&working_copy, &["branch", "-q", "--set-upstream-to=origin/main", "main"]);
    run_git(&bare_remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    (bare_remote, working_copy)
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
    fn slugify_never_returns_empty_even_for_all_non_ascii_input() {
        // Cycle 1 review finding: pure non-ASCII input (no ASCII
        // alphanumerics at all) previously produced an empty string, and
        // note_path() would then build a collision-prone path like
        // `modules//README.md` (every such module colliding on the same
        // path). A deterministic fallback slug must be non-empty instead.
        let cyrillic = slugify("Модуль");
        let emoji = slugify("🎉🎊");
        assert!(!cyrillic.is_empty());
        assert!(!emoji.is_empty());
        // Different inputs still produce different fallback slugs.
        assert_ne!(cyrillic, emoji);
        // The fallback is stable across repeated calls (deterministic, not
        // e.g. time- or randomness-based).
        assert_eq!(slugify("Модуль"), cyrillic);
    }

    #[test]
    fn note_path_never_collides_for_distinct_all_non_ascii_modules() {
        let root = Path::new("/vault");
        let p1 = note_path(root, NoteType::Readme, "Модуль", "ignored");
        let p2 = note_path(root, NoteType::Readme, "モジュール", "ignored");
        assert_ne!(p1, p2, "distinct non-ASCII module names must not collide on the same path");
        assert!(!p1.to_string_lossy().contains("//"), "must never produce an empty path segment");
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
    fn render_note_frontmatter_escapes_embedded_newlines_and_control_chars() {
        // Cycle 1 review finding: an embedded literal newline (not stripped
        // by .trim(), which only strips leading/trailing whitespace) must
        // not land unescaped inside the quoted YAML scalar -- it would
        // change how a parser folds the value, or in the worst case let a
        // crafted title inject additional-looking frontmatter lines.
        let fm = NoteFrontmatter {
            title: "line one\nmodule: fake\ntype: blog".to_string(),
            module: "x".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(), // pii-test-fixture
            source_commit: "abc".to_string(),
            note_type: NoteType::Wiki,
        };
        let note = render_note(&fm, "body", &[]);
        // The whole title, newlines included, must appear as ONE escaped
        // scalar on the `title:` line -- not as literal newlines that would
        // start what looks like new frontmatter keys.
        let title_line = note.lines().find(|l| l.starts_with("title:")).expect("a title: line");
        assert!(title_line.contains("\\n"), "expected an escaped \\n, got: {title_line}");
        // The injected-looking content is present only as escaped text
        // WITHIN the title's quoted scalar, never as a separate real line
        // of its own (which is what an actual injection would produce).
        assert!(
            !note.lines().any(|l| l == "module: fake"),
            "embedded newline must never produce a standalone 'module: fake' line"
        );
        // Only one frontmatter `type:` line exists, and it's the real one.
        assert_eq!(note.lines().filter(|l| l.starts_with("type:")).count(), 1);
        assert!(note.lines().any(|l| l == "type: wiki"));
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

        let (bare_remote, working_copy) = test_setup_bare_vault(&base);
        let fresh_clone = base.join("fresh-clone");

        let fm = NoteFrontmatter {
            title: "Sundry".to_string(),
            module: "sundry".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(),  // pii-test-fixture
            source_commit: "abc123".to_string(),
            note_type: NoteType::Readme,
        };
        let content = render_note(&fm, "Sundry tools live here.", &[]);
        let note_path_in_vault = note_path(&working_copy, NoteType::Readme, "sundry", "ignored");
        // test_setup_bare_vault already configured a committer identity on
        // the working copy.

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

        let (_bare_remote, working_copy) = test_setup_bare_vault(&base);

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

    #[test]
    fn write_note_and_push_against_a_non_repo_dir_is_a_clean_error_not_panic() {
        // Direct test of write_note_and_push's own defense (cycle 1 review
        // finding: previously relied entirely on the caller's separate
        // `.git`-existence check in mod.rs; this confirms the function
        // itself degrades cleanly -- via git's own "not a git repository"
        // error -- when called directly against a non-repo path.
        let dir = std::env::temp_dir().join(format!("scribe-vault-test-nonrepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let fm = NoteFrontmatter {
            title: "X".to_string(),
            module: "x".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(), // pii-test-fixture
            source_commit: "n/a".to_string(),
            note_type: NoteType::BuildDiary,
        };
        let content = render_note(&fm, "body", &[]);
        let note_path_in_dir = dir.join("build-diaries/x.md");

        let result = write_note_and_push(&dir, &note_path_in_dir, &content, "scribe: test");
        assert!(matches!(result, Err(ToolError::Execution(_))));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_note_and_push_is_a_clean_noop_when_content_is_unchanged() {
        // Cycle 1 review finding: re-running Scribe against unchanged
        // source (byte-identical content) must be a clean success, not a
        // confusing "nothing to commit" Execution error.
        let base = std::env::temp_dir().join(format!("scribe-vault-test-noop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (_bare_remote, working_copy) = test_setup_bare_vault(&base);

        let fm = NoteFrontmatter {
            title: "Sundry".to_string(),
            module: "sundry".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(), // pii-test-fixture
            source_commit: "abc".to_string(),
            note_type: NoteType::Readme,
        };
        let content = render_note(&fm, "Sundry tools.", &[]);
        let note_path_in_vault = note_path(&working_copy, NoteType::Readme, "sundry", "ignored");

        write_note_and_push(&working_copy, &note_path_in_vault, &content, "scribe: first write")
            .expect("first write should succeed");

        // Second call with IDENTICAL content -- must be a clean no-op, not
        // an error.
        write_note_and_push(&working_copy, &note_path_in_vault, &content, "scribe: second write")
            .expect("re-running with unchanged content should be a clean no-op");

        let _ = std::fs::remove_dir_all(&base);
    }
}
