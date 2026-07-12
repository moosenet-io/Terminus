//! GHIST-01 — full-history scrubbed replay for the git-public mirror engine.
//!
//! The per-sync [`MirrorWorkDir::run`](super::workdir::MirrorWorkDir) publishes ONE
//! swept commit per sync — a clean SNAPSHOT, which cannot reproduce a repo's real
//! commit history. This module is the backfill counterpart: it reproduces a source
//! repo's ENTIRE commit graph as a scrubbed derivative, so the public mirror can
//! carry the operator's real contribution history (dates, messages, graph shape)
//! with every historical blob run through the native [`DeterministicCleaner`].
//!
//! ## Mechanism — git-native, streaming, no external tool
//! `git fast-export` the source → transform the stream in-process → `git fast-import`
//! into a fresh work-dir. The transform rewrites only `blob` payloads (and inline
//! `M … inline` blob data) through [`DeterministicCleaner::scrub_bytes`]; commit
//! messages, author/committer idents (remapped in GHIST-03), marks, and the graph
//! structure (`from`/`merge`) pass through unchanged, so commit COUNT and author
//! DATES — what the GitHub contribution calendar keys on — are byte-preserved.
//!
//! The source repo is READ-ONLY throughout (`fast-export` only reads); the scrubbed
//! history lands in a separate work-dir. A full-history PII GATE (GHIST-02) scans
//! every replayed commit's tree before anything is pushed — a secret hidden in an
//! old, later-"removed" commit is caught there, never shipped.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::ToolError;
use crate::github::pii::{ruleset_from_config, TreeViolation};

use super::native_clean::DeterministicCleaner;
use super::workdir::run_git;

/// `core.hooksPath=/dev/null` before every git subcommand — the source repo and the
/// fresh work-dir must never run a repo hook during replay (same posture as the rest
/// of the mirror engine).
const HOOKS_OFF: &[&str] = &["-c", "core.hooksPath=/dev/null"];

/// Outcome of a full-history replay (metrics only — no PII, no shas).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HistoryReport {
    /// `commit` records reproduced.
    pub commits: usize,
    /// blob payloads seen (separate `blob` records + inline `M … inline` data).
    pub blobs_total: usize,
    /// blob payloads the cleaner actually changed.
    pub blobs_rewritten: usize,
    /// author/committer ident lines seen.
    pub idents_seen: usize,
    /// author/committer idents actually rewritten by the GHIST-03 attribution map.
    pub idents_remapped: usize,
}

/// Replay options.
#[derive(Default)]
pub struct ReplayOpts {
    /// GHIST-03 author/committer identity remap. `None` passes idents through
    /// unchanged (GHIST-01 behavior).
    pub author_map: Option<IdentityMap>,
}

impl ReplayOpts {
    pub fn new() -> Self {
        Self::default()
    }

    /// With the given identity remap applied to every author/committer.
    pub fn with_author_map(map: IdentityMap) -> Self {
        Self { author_map: Some(map) }
    }
}

// ── GHIST-03: contribution attribution remap ────────────────────────────────

/// One remap rule: match an internal ident by exact email, email DOMAIN suffix,
/// or exact display name (all case-insensitive), and rewrite it to a public
/// identity. The FIRST matching rule (in file order) wins.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IdentityRule {
    #[serde(default)]
    pub match_email: Option<String>,
    /// Bare domain, e.g. `example.com` — matches an email ending `@example.com`.  // pii-test-fixture
    #[serde(default)]
    pub match_email_domain: Option<String>,
    #[serde(default)]
    pub match_name: Option<String>,
    pub public_name: String,
    pub public_email: String,
}

/// Author-identity remap for the history replay. Loaded from a deployment-config
/// TOML file (path in `TERMINUS_MIRROR_AUTHOR_MAP`) so NO email literal ever lives
/// in source. Rewriting only the ident's name+email (not the timestamp) both
/// attributes commits to the right public account AND scrubs the internal author
/// emails (`*.local`/`*.online`/personal) for free. An email matched by no rule
/// falls through to `default_*` — never left as the raw internal address.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IdentityMap {
    #[serde(default)]
    pub rules: Vec<IdentityRule>,
    pub default_name: String,
    pub default_email: String,
}

impl IdentityMap {
    /// Load from a TOML file. Errors if the file is missing/unreadable/malformed
    /// (a backfill must NOT silently run with no attribution map).
    pub fn from_toml_file(path: &Path) -> Result<Self, ToolError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ToolError::Execution(format!("read author-map {}: {e}", path.display()))
        })?;
        toml::from_str(&text)
            .map_err(|e| ToolError::Execution(format!("parse author-map {}: {e}", path.display())))
    }

    /// Load from `TERMINUS_MIRROR_AUTHOR_MAP` if set, else `None` (no remap).
    pub fn from_env() -> Result<Option<Self>, ToolError> {
        match std::env::var("TERMINUS_MIRROR_AUTHOR_MAP") {
            Ok(p) if !p.trim().is_empty() => Ok(Some(Self::from_toml_file(Path::new(p.trim()))?)),
            _ => Ok(None),
        }
    }

    /// Remap `(name, email)` → the public `(name, email)`. Case-insensitive
    /// matching; first matching rule wins; unmatched → the configured default.
    pub fn remap(&self, name: &str, email: &str) -> (String, String) {
        let email_lc = email.to_ascii_lowercase();
        let name_lc = name.to_ascii_lowercase();
        for r in &self.rules {
            let hit = r
                .match_email
                .as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case(email))
                || r.match_email_domain
                    .as_deref()
                    .is_some_and(|d| email_lc.ends_with(&format!("@{}", d.to_ascii_lowercase())))
                || r.match_name
                    .as_deref()
                    .is_some_and(|n| n.to_ascii_lowercase() == name_lc);
            if hit {
                return (r.public_name.clone(), r.public_email.clone());
            }
        }
        (self.default_name.clone(), self.default_email.clone())
    }
}

/// Run a git subcommand in `cwd` with hooks disabled, capturing stdout. Unlike
/// [`run_git`] this does NOT assert-against `-f`/`--hard` — the replay legitimately
/// needs `reset --hard` to populate a FRESH throwaway work-dir from the just-imported
/// HEAD (nothing to clobber). Never used for a push and never on the source repo's
/// work-tree.
fn git_capture(cwd: &Path, args: &[&str]) -> Result<String, ToolError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(HOOKS_OFF)
        .args(args)
        .output()
        .map_err(|e| ToolError::Execution(format!("git {args:?}: {e}")))?;
    if !out.status.success() {
        return Err(ToolError::Execution(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether `haystack` contains the contiguous byte slice `needle`.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Rewrite a fast-export `author`/`committer` ident line's NAME + EMAIL per the
/// attribution map, PRESERVING the trailing ` <timestamp> <tz>` (so the
/// contribution date is unchanged). Line format: `author NAME <EMAIL> TS TZ\n`.
/// Returns the original bytes on any parse failure — a malformed ident is passed
/// through unchanged rather than dropped or panicking.
fn rewrite_ident_line(line: &[u8], map: &IdentityMap) -> Vec<u8> {
    let prefix_len = if line.starts_with(b"author ") {
        "author ".len()
    } else if line.starts_with(b"committer ") {
        "committer ".len()
    } else {
        return line.to_vec();
    };
    let (prefix, rest) = line.split_at(prefix_len);
    // git idents put the email in an angle-bracket pair; names cannot contain
    // '<'/'>', so the first '<' opens the email.
    let lt = match rest.iter().position(|&b| b == b'<') {
        Some(i) => i,
        None => return line.to_vec(),
    };
    let gt = match rest[lt..].iter().position(|&b| b == b'>') {
        Some(i) => lt + i,
        None => return line.to_vec(),
    };
    let name = match std::str::from_utf8(&rest[..lt]) {
        Ok(s) => s.trim(),
        Err(_) => return line.to_vec(),
    };
    let email = match std::str::from_utf8(&rest[lt + 1..gt]) {
        Ok(s) => s.trim(),
        Err(_) => return line.to_vec(),
    };
    let suffix = &rest[gt + 1..]; // ` <TS> <TZ>\n` — preserved verbatim
    let (public_name, public_email) = map.remap(name, email);
    let mut out = Vec::with_capacity(prefix.len() + public_name.len() + public_email.len() + suffix.len() + 4);
    out.extend_from_slice(prefix);
    out.extend_from_slice(public_name.as_bytes());
    out.extend_from_slice(b" <");
    out.extend_from_slice(public_email.as_bytes());
    out.push(b'>');
    out.extend_from_slice(suffix);
    out
}

/// Reproduce `source`'s entire commit history into `work_dir` as a PII-scrubbed
/// derivative. `work_dir` MUST be empty/non-existent (a fresh backfill target); the
/// source repo is never modified. Returns metrics on what was replayed.
pub fn replay_full_history(
    source: &Path,
    work_dir: &Path,
    opts: &ReplayOpts,
) -> Result<HistoryReport, ToolError> {
    // Confirm the source is a git repo before doing anything.
    run_git(source, &["rev-parse", "--git-dir"])
        .map_err(|e| ToolError::Execution(format!("source is not a git repo ({}): {e}", source.display())))?;
    let default_branch = git_capture(source, &["symbolic-ref", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());

    // Fresh work-dir git repo (must be empty — a backfill starts a new lineage).
    std::fs::create_dir_all(work_dir)
        .map_err(|e| ToolError::Execution(format!("create work_dir {}: {e}", work_dir.display())))?;
    git_capture(work_dir, &["init", "-q", "-b", &default_branch])?;

    // Full backfill: export ALL refs, no import-marks (this is a fresh lineage).
    let report = run_export_import(source, work_dir, &["--all"], false, opts)?;
    // Record the internal HEAD we just mirrored so an incremental run knows where
    // to resume from (GHIST-04).
    if let Ok(head) = git_capture(source, &["rev-parse", "HEAD"]) {
        let head = head.trim();
        if !head.is_empty() {
            set_mirrored_sha(work_dir, head)?;
        }
    }
    Ok(report)
}

// ── GHIST-04: going-forward per-commit (incremental) replay ──────────────────

/// Marks-file paths (kept under `.git/ghist/`, OUTSIDE the work-tree so they are
/// never scanned or committed). `src-marks` records SOURCE commit → mark from
/// `git fast-export`; `import-marks` records mark → WORK-DIR commit from
/// `git fast-import`. Persisting both across runs is what lets a later
/// incremental export reference already-mirrored commits by mark and append onto
/// the existing scrubbed history instead of re-squashing it.
struct MarksPaths {
    src: PathBuf,
    import: PathBuf,
}

fn marks_paths(work_dir: &Path) -> MarksPaths {
    let d = work_dir.join(".git").join("ghist");
    MarksPaths { src: d.join("src-marks"), import: d.join("import-marks") }
}

/// The internal sha last mirrored into `work_dir`, if any (persisted in
/// `.git/ghist/internal-head`).
pub fn last_mirrored_sha(work_dir: &Path) -> Option<String> {
    std::fs::read_to_string(work_dir.join(".git").join("ghist").join("internal-head"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn set_mirrored_sha(work_dir: &Path, sha: &str) -> Result<(), ToolError> {
    let d = work_dir.join(".git").join("ghist");
    std::fs::create_dir_all(&d)
        .map_err(|e| ToolError::Execution(format!("create .git/ghist: {e}")))?;
    std::fs::write(d.join("internal-head"), format!("{sha}\n"))
        .map_err(|e| ToolError::Execution(format!("write internal-head: {e}")))
}

/// The WORK-DIR commit last successfully PUSHED to the public remote (persisted in
/// `.git/ghist/pushed-head`), if any. This is the true "what is published" boundary
/// — distinct from `internal-head` (the SOURCE sha last replayed) and from the local
/// work-dir HEAD (which may sit AHEAD of the remote after a withheld / refused sync).
/// The going-forward runner (GHIST-08) gates and pushes the range
/// `pushed-head..HEAD`, so any commit that was replayed but not yet published — e.g.
/// one whose PII gate failed — stays in the gated range on every subsequent run
/// until it is gate-clean AND pushed. Anchoring on the local HEAD instead would let a
/// later remediation-only re-gate skip a previously-withheld PII commit while a
/// fast-forward push still published it.
pub fn last_pushed_sha(work_dir: &Path) -> Option<String> {
    std::fs::read_to_string(work_dir.join(".git").join("ghist").join("pushed-head"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Record the work-dir commit now published on the public remote. Call ONLY after a
/// confirmed push (or when initialising the boundary to the established baseline).
pub fn set_pushed_sha(work_dir: &Path, sha: &str) -> Result<(), ToolError> {
    let d = work_dir.join(".git").join("ghist");
    std::fs::create_dir_all(&d)
        .map_err(|e| ToolError::Execution(format!("create .git/ghist: {e}")))?;
    std::fs::write(d.join("pushed-head"), format!("{sha}\n"))
        .map_err(|e| ToolError::Execution(format!("write pushed-head: {e}")))
}

/// Shared fast-export → transform → fast-import core for both the full backfill and
/// the incremental range. `rev_args` selects what to export (`--all`, or a
/// `<from>..<to>` range). When `incremental` is true, both git ends read+write the
/// persisted marks files, so exported commits reference already-mirrored parents by
/// mark and are appended onto the existing history (a fast-forward), rather than
/// starting a new root.
fn run_export_import(
    source: &Path,
    work_dir: &Path,
    rev_args: &[&str],
    incremental: bool,
    opts: &ReplayOpts,
) -> Result<HistoryReport, ToolError> {
    let marks = marks_paths(work_dir);
    std::fs::create_dir_all(marks.src.parent().expect("has parent"))
        .map_err(|e| ToolError::Execution(format!("create marks dir: {e}")))?;
    let src_marks = marks.src.display().to_string();
    let import_marks = marks.import.display().to_string();

    // fast-export args (READ-ONLY on source): rev selector + normalization + marks.
    let mut export_args: Vec<String> = vec!["fast-export".into()];
    export_args.extend(rev_args.iter().map(|s| s.to_string()));
    export_args.push("--reencode=yes".into());
    export_args.push("--signed-tags=strip".into());
    export_args.push("--tag-of-filtered-object=drop".into());
    export_args.push(format!("--export-marks={src_marks}"));
    if incremental {
        export_args.push(format!("--import-marks={src_marks}"));
    }

    // fast-import args. --force = allow ref updates into this repo (NOT a push-force).
    let mut import_args: Vec<String> =
        vec!["fast-import".into(), "--quiet".into(), "--force".into()];
    import_args.push(format!("--export-marks={import_marks}"));
    if incremental {
        import_args.push(format!("--import-marks={import_marks}"));
    }

    let mut exporter = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(HOOKS_OFF)
        .args(&export_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn git fast-export: {e}")))?;
    let mut importer = Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(HOOKS_OFF)
        .args(&import_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn git fast-import: {e}")))?;

    let export_out = exporter.stdout.take().expect("piped");
    let export_err = exporter.stderr.take().expect("piped");
    let import_in = importer.stdin.take().expect("piped");
    let import_err = importer.stderr.take().expect("piped");

    let e_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = BufReader::new(export_err).read_to_string(&mut s);
        s
    });
    let i_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = BufReader::new(import_err).read_to_string(&mut s);
        s
    });

    let pump = transform_stream(
        BufReader::new(export_out),
        BufWriter::new(import_in),
        opts.author_map.as_ref(),
    );

    let e_status = exporter
        .wait()
        .map_err(|e| ToolError::Execution(format!("wait fast-export: {e}")))?;
    let i_status = importer
        .wait()
        .map_err(|e| ToolError::Execution(format!("wait fast-import: {e}")))?;
    let e_stderr = e_err.join().unwrap_or_default();
    let i_stderr = i_err.join().unwrap_or_default();

    let report = pump?;
    if !e_status.success() {
        return Err(ToolError::Execution(format!(
            "git fast-export exited non-zero: {}",
            e_stderr.trim()
        )));
    }
    if !i_status.success() {
        return Err(ToolError::Execution(format!(
            "git fast-import exited non-zero: {}",
            i_stderr.trim()
        )));
    }

    // Sync the work-tree to the (new) imported HEAD.
    git_capture(work_dir, &["reset", "--hard"])?;
    Ok(report)
}

/// Append the internal commits in `from_sha..to_sha` onto an ALREADY-backfilled
/// mirror `work_dir`, each scrubbed + attributed, chaining onto the existing
/// scrubbed history via the persisted marks (a fast-forward, not a re-squash).
/// `from_sha` must be an ancestor of `to_sha` in the source (else the internal
/// history was rewritten — we refuse rather than silently diverge).
pub fn replay_range(
    source: &Path,
    work_dir: &Path,
    from_sha: &str,
    to_sha: &str,
    opts: &ReplayOpts,
) -> Result<HistoryReport, ToolError> {
    run_git(source, &["rev-parse", "--git-dir"])
        .map_err(|e| ToolError::Execution(format!("source is not a git repo: {e}")))?;
    if !marks_paths(work_dir).src.exists() {
        return Err(ToolError::Execution(
            "no backfill marks present — run replay_full_history before an incremental range".into(),
        ));
    }
    // Ancestry guard: from must be an ancestor of to (fast-forward only).
    let anc = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(HOOKS_OFF)
        .args(["merge-base", "--is-ancestor", from_sha, to_sha])
        .status()
        .map_err(|e| ToolError::Execution(format!("merge-base: {e}")))?;
    if !anc.success() {
        return Err(ToolError::Execution(format!(
            "internal history is not a fast-forward: {from_sha}..{to_sha} — {to_sha} does not descend from {from_sha}; refusing to diverge the mirror"
        )));
    }
    // Export `--all` WITH `--import-marks`: git skips every commit already recorded
    // in the marks (everything up to the last mirror) and emits ONLY the new ones,
    // updating `refs/heads/*` correctly. (A `<from>..<to>` range instead emits an
    // anonymous per-commit ref and never advances the branch — verified.) The
    // ancestry guard above is what bounds this to a fast-forward from `from_sha`.
    let report = run_export_import(source, work_dir, &["--all"], true, opts)?;
    set_mirrored_sha(work_dir, to_sha)?;
    Ok(report)
}

/// Bring `work_dir` up to `source`'s current HEAD: a full backfill on first run
/// (no prior lineage), else an incremental append of the new commits. Returns
/// `(is_full, report)`. A no-op (already at HEAD) returns a zero report.
pub fn replay_incremental_or_full(
    source: &Path,
    work_dir: &Path,
    opts: &ReplayOpts,
) -> Result<(bool, HistoryReport), ToolError> {
    let head = git_capture(source, &["rev-parse", "HEAD"])?.trim().to_string();
    match last_mirrored_sha(work_dir) {
        Some(last) if last == head => Ok((false, HistoryReport::default())), // nothing new
        Some(last) => Ok((false, replay_range(source, work_dir, &last, &head, opts)?)),
        None => Ok((true, replay_full_history(source, work_dir, opts)?)),
    }
}

// ── GHIST-05: per-PR scrubbed feature-branch replay ──────────────────────────

/// One PR's scrubbed feature branch, produced by [`replay_pr_slice`].
#[derive(Debug, Clone)]
pub struct PrSliceReport {
    /// Canonical scrubbed tip BEFORE this slice (its tree equals `public_base`'s).
    pub canonical_base: String,
    /// Canonical scrubbed tip AFTER this slice.
    pub canonical_head: String,
    /// The PR feature-branch tip: the slice rebased onto `public_base`.
    pub branch_tip: String,
    /// Number of commits in the slice (0 = an empty PR range).
    pub commits: usize,
}

/// The committer identity to stamp on rebased PR-branch commits: the author map's
/// default (the public bot identity), so a rebase never leaks the build host's git
/// user. Falls back to a neutral local identity when no map is configured (tests).
fn committer_identity(opts: &ReplayOpts) -> (String, String) {
    opts.author_map
        .as_ref()
        .map(|m| (m.default_name.clone(), m.default_email.clone()))
        .unwrap_or_else(|| ("mirror-bot".to_string(), "mirror-bot@localhost".to_string()))
}

/// `git rebase --onto <newbase> <upstream> <branch>` with hooks off, a pinned
/// committer identity, and author-dated committers. Aborts a partial rebase so the
/// work-dir is never left mid-rebase on failure.
fn rebase_slice_onto(
    work_dir: &Path,
    newbase: &str,
    upstream: &str,
    branch: &str,
    committer_name: &str,
    committer_email: &str,
) -> Result<(), ToolError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(HOOKS_OFF)
        .env("GIT_COMMITTER_NAME", committer_name)
        .env("GIT_COMMITTER_EMAIL", committer_email)
        .args([
            "rebase",
            "--committer-date-is-author-date",
            "--onto",
            newbase,
            upstream,
            branch,
        ])
        .output()
        .map_err(|e| ToolError::Execution(format!("spawn git rebase: {e}")))?;
    if !out.status.success() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(work_dir)
            .args(HOOKS_OFF)
            .args(["rebase", "--abort"])
            .status();
        return Err(ToolError::Execution(format!(
            "git rebase --onto {newbase} {upstream} {branch} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Replay one internal PR's commit range `base_int..head_int` as a scrubbed feature
/// branch rebased onto `public_base` (the current public-main tip, which MUST already
/// be present in `work_dir`'s object DB — the caller fetches public main first).
///
/// The work-dir's canonical marks-based scrubbed lineage must be exactly at `base_int`
/// (PRs replay in merge ORDER onto the canonical lineage). The range is replayed onto
/// the canonical lineage (scrubbed + attributed via the marks), then the resulting
/// slice is rebased onto `public_base` and `branch_name` is pointed at it — ready to
/// push as the PR's head branch. Because `public_base`'s tree equals the canonical
/// base's tree (the public tip is the merged/squashed equivalent of everything up to
/// `base_int`), the rebase applies with no conflicts. The canonical branch is left at
/// its advanced tip; only `branch_name` is based on `public_base`.
pub fn replay_pr_slice(
    source: &Path,
    work_dir: &Path,
    base_int: &str,
    head_int: &str,
    public_base: &str,
    branch_name: &str,
    opts: &ReplayOpts,
) -> Result<PrSliceReport, ToolError> {
    // Ordering guard: the canonical lineage must be exactly at base_int.
    match last_mirrored_sha(work_dir) {
        Some(l) if l == base_int => {}
        other => {
            return Err(ToolError::Execution(format!(
                "PR replay out of order: canonical scrubbed lineage is at {other:?}, expected \
                 base {base_int}. PRs must replay in merge order onto the canonical lineage."
            )));
        }
    }
    // public_base must be a known commit object (the caller fetched public main).
    if git_capture(work_dir, &["cat-file", "-e", &format!("{public_base}^{{commit}}")]).is_err() {
        return Err(ToolError::Execution(format!(
            "public base {public_base} is not present in the mirror work-dir — fetch public main \
             into the work-dir before replaying a PR slice onto it"
        )));
    }

    let canonical_branch = git_capture(work_dir, &["symbolic-ref", "--short", "HEAD"])?
        .trim()
        .to_string();
    let canonical_base = git_capture(work_dir, &["rev-parse", "HEAD"])?.trim().to_string();

    // Advance the canonical scrubbed lineage over the PR range (marks-chained).
    replay_range(source, work_dir, base_int, head_int, opts)?;
    let canonical_head = git_capture(work_dir, &["rev-parse", "HEAD"])?.trim().to_string();
    let commits: usize = git_capture(
        work_dir,
        &["rev-list", "--count", &format!("{canonical_base}..{canonical_head}")],
    )?
    .trim()
    .parse()
    .unwrap_or(0);

    // Rebase the slice canonical_base..canonical_head onto public_base → the PR head branch.
    git_capture(work_dir, &["branch", "-f", branch_name, &canonical_head])?;
    let (cname, cemail) = committer_identity(opts);
    rebase_slice_onto(work_dir, public_base, &canonical_base, branch_name, &cname, &cemail)?;
    let branch_tip = git_capture(work_dir, &["rev-parse", branch_name])?.trim().to_string();

    // Restore the work-tree to the canonical lineage (the rebase left HEAD on branch_name).
    git_capture(work_dir, &["checkout", "-f", &canonical_branch])?;
    git_capture(work_dir, &["reset", "--hard", &canonical_head])?;

    Ok(PrSliceReport { canonical_base, canonical_head, branch_tip, commits })
}

// ── GHIST-02: full-history PII gate ──────────────────────────────────────────

/// One residual PII violation found in a HISTORICAL commit's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryViolation {
    /// A commit whose tree carries the violation (the representative commit for
    /// the tree — many commits can share one tree; see [`gate_full_history`]).
    pub commit: String,
    pub file: String,
    pub line: usize,
    pub pattern_kind: String,
    /// Redacted context (the gate never stores the full secret).
    pub context: String,
}

/// Result of scanning EVERY commit's tree in a replayed history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullHistoryGateReport {
    /// True iff zero residual violations across all commits.
    pub clean: bool,
    pub commits_scanned: usize,
    /// Distinct tree objects actually scanned (commits sharing a tree scan once).
    pub unique_trees: usize,
    pub violations: Vec<HistoryViolation>,
}

/// Scan the tree of EVERY commit reachable in `work_dir` (not just the tip) with
/// the authoritative PII gate, and report any residual `commit:file:line`. This is
/// the safety spine of the backfill: a secret committed once and later "removed"
/// still lives in that historical commit's tree, and a full-history push would ship
/// it — so the caller MUST refuse to push when `clean` is false.
///
/// Cost is bounded by de-duplicating on the TREE object: commits that share a tree
/// (a no-op commit, a revert to an identical tree) are scanned once. Each unique
/// tree is materialized read-only via `git archive | tar -x` into a throwaway temp
/// dir, scanned, and removed. Progress is logged for large histories.
pub fn gate_full_history(work_dir: &Path) -> Result<FullHistoryGateReport, ToolError> {
    let rev_list = git_capture(work_dir, &["rev-list", "--all"])?;
    let commits: Vec<String> = rev_list.split_whitespace().map(|s| s.to_string()).collect();
    gate_commit_list(work_dir, &commits)
}

/// Gate ONLY the given commits' trees (GHIST-08 — the cheap incremental path for the
/// going-forward runner). A full re-gate of the whole history is wasteful when a sync
/// appended only a handful of new commits; this scans just those (still tree-deduped,
/// still `commit:file:line`, still a hard block on any residual). `commits` is
/// typically `rev-list <last-mirrored>..HEAD` on the work dir.
pub fn gate_commits(work_dir: &Path, commits: &[String]) -> Result<FullHistoryGateReport, ToolError> {
    gate_commit_list(work_dir, commits)
}

/// Shared gate core: scan each commit's tree (tree-deduped) with the authoritative
/// gate, accumulate `commit:file:line` residuals. Used by both the full-history and
/// the incremental gates.
fn gate_commit_list(work_dir: &Path, commits: &[String]) -> Result<FullHistoryGateReport, ToolError> {
    // Same ruleset resolution every other gate surface uses (repo pii-gate.toml /
    // TERMINUS_PII_CONFIG / built-in default), so history is gated identically to
    // the tip.
    let ruleset = ruleset_from_config(Some(work_dir));

    let scan_root = temp_dir_unique("ghist02-gate");
    std::fs::create_dir_all(&scan_root)
        .map_err(|e| ToolError::Execution(format!("create gate scan root: {e}")))?;

    let mut seen_trees: HashSet<String> = HashSet::new();
    let mut violations: Vec<HistoryViolation> = Vec::new();
    let total = commits.len();

    for (i, commit) in commits.iter().enumerate() {
        let tree = git_capture(work_dir, &["rev-parse", &format!("{commit}^{{tree}}")])?
            .trim()
            .to_string();
        if !seen_trees.insert(tree.clone()) {
            continue; // tree already scanned via an earlier (representative) commit
        }
        let dir = scan_root.join(&tree);
        if let Err(e) = extract_tree(work_dir, &tree, &dir) {
            let _ = std::fs::remove_dir_all(&scan_root);
            return Err(e);
        }
        for TreeViolation { file, line, pattern_kind, context } in ruleset.scan_tree(&dir) {
            violations.push(HistoryViolation {
                commit: commit.clone(),
                file,
                line,
                pattern_kind,
                context,
            });
        }
        let _ = std::fs::remove_dir_all(&dir);
        if i > 0 && i % 200 == 0 {
            tracing::info!(
                target: "forge.mirror",
                scanned = i,
                total,
                unique_trees = seen_trees.len(),
                residuals = violations.len(),
                "full-history gate progress"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&scan_root);

    Ok(FullHistoryGateReport {
        clean: violations.is_empty(),
        commits_scanned: total,
        unique_trees: seen_trees.len(),
        violations,
    })
}

/// Materialize a git TREE object read-only into `dest` via `git archive | tar -x`.
/// The two children are connected by an OS pipe (git stdout → tar stdin), so there
/// is no manual pump and no deadlock. Neither touches the source work-tree.
fn extract_tree(repo: &Path, tree: &str, dest: &Path) -> Result<(), ToolError> {
    std::fs::create_dir_all(dest)
        .map_err(|e| ToolError::Execution(format!("create tree dir {}: {e}", dest.display())))?;
    let mut archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(HOOKS_OFF)
        .args(["archive", "--format=tar", tree])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn git archive: {e}")))?;
    let archive_out: Stdio = archive.stdout.take().expect("piped").into();
    let tar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(archive_out)
        .output()
        .map_err(|e| ToolError::Execution(format!("run tar -x: {e}")))?;
    let a_status = archive
        .wait()
        .map_err(|e| ToolError::Execution(format!("wait git archive: {e}")))?;
    let mut a_err = String::new();
    if let Some(mut es) = archive.stderr.take() {
        let _ = es.read_to_string(&mut a_err);
    }
    if !a_status.success() {
        return Err(ToolError::Execution(format!("git archive {tree} failed: {}", a_err.trim())));
    }
    if !tar.status.success() {
        return Err(ToolError::Execution(format!(
            "tar -x failed for {tree}: {}",
            String::from_utf8_lossy(&tar.stderr).trim()
        )));
    }
    Ok(())
}

/// A process-unique temp dir path (no `Date`/`rand` — uses pid + monotonic-ish
/// system time, unique enough for a serialized backfill).
fn temp_dir_unique(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

/// What the NEXT `data <n>` block belongs to, set by the command line preceding it.
#[derive(PartialEq)]
enum Pending {
    None,
    /// A `blob` record or an inline `M … inline` payload — scrub it.
    Blob,
    /// A `commit` message — pass through unchanged.
    Message,
}

/// Stream-transform a `git fast-export` byte stream into a `git fast-import` stream,
/// scrubbing only blob payloads. Command lines are read with `read_until(b'\n')`;
/// a `data <count>` header is followed by EXACTLY `count` raw bytes (which may
/// contain newlines/binary), read with `read_exact`. Only blob payloads are passed
/// through [`DeterministicCleaner::scrub_bytes`]; commit messages and every
/// structural line (`mark`/`from`/`merge`/`M`/`D`/`author`/…) pass through verbatim,
/// preserving the graph, dates, and commit count.
fn transform_stream<R: BufRead, W: Write>(
    mut r: R,
    mut w: W,
    author_map: Option<&IdentityMap>,
) -> Result<HistoryReport, ToolError> {
    let mut report = HistoryReport::default();
    let mut pending = Pending::None;
    let ioerr = |e: std::io::Error| ToolError::Execution(format!("history stream io: {e}"));

    loop {
        let mut line: Vec<u8> = Vec::new();
        let n = r.read_until(b'\n', &mut line).map_err(ioerr)?;
        if n == 0 {
            break; // EOF
        }

        if line.starts_with(b"data ") {
            // Counted-data header: `data <n>\n` then exactly n raw bytes.
            let hdr = std::str::from_utf8(&line[5..])
                .map_err(|_| ToolError::Execution("non-utf8 fast-export data header".into()))?
                .trim();
            if hdr.starts_with("<<") {
                return Err(ToolError::Execution(
                    "fast-export emitted delimited data (data <<EOF); expected counted data".into(),
                ));
            }
            let len: usize = hdr
                .parse()
                .map_err(|_| ToolError::Execution(format!("bad fast-export data length: {hdr:?}")))?;
            let mut data = vec![0u8; len];
            r.read_exact(&mut data).map_err(ioerr)?;

            let out = match pending {
                Pending::Blob => {
                    report.blobs_total += 1;
                    let scrubbed = DeterministicCleaner::scrub_bytes(&data);
                    if scrubbed != data {
                        report.blobs_rewritten += 1;
                    }
                    scrubbed
                }
                // Commit message (or a stray data with no preceding blob/commit) —
                // never scrubbed here (messages are prose; graph fidelity first).
                _ => data,
            };
            pending = Pending::None;
            w.write_all(format!("data {}\n", out.len()).as_bytes())
                .map_err(ioerr)?;
            w.write_all(&out).map_err(ioerr)?;
            continue;
        }

        // Classify a command line to know what its upcoming `data` belongs to.
        if line == b"blob\n" || line == b"blob" {
            pending = Pending::Blob;
        } else if line.starts_with(b"commit ") {
            pending = Pending::Message;
            report.commits += 1;
        } else if line.starts_with(b"M ") && contains_subslice(&line, b" inline ") {
            pending = Pending::Blob;
        } else if line.starts_with(b"author ") || line.starts_with(b"committer ") {
            report.idents_seen += 1;
            // GHIST-03: remap the ident's name+email to the public identity,
            // preserving the timestamp. Only when a map is configured; on any
            // parse failure the original line is written unchanged.
            if let Some(map) = author_map {
                let rewritten = rewrite_ident_line(&line, map);
                if rewritten != line {
                    report.idents_remapped += 1;
                }
                w.write_all(&rewritten).map_err(ioerr)?;
                continue;
            }
        }

        w.write_all(&line).map_err(ioerr)?;
    }

    w.flush().map_err(ioerr)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn unique(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ghist01-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        git_capture(dir, args).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    fn write(dir: &Path, rel: &str, bytes: &[u8]) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, bytes).unwrap();
    }

    /// A source repo with three commits: an IP in text, a binary blob, and a plain
    /// change — with a fixed author/date so we can assert date fidelity.
    fn init_source() -> PathBuf {
        let dir = unique("src");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        let commit = |dir: &Path, msg: &str, date: &str| {
            git(dir, &["add", "-A"]);
            // Fixed author + date so the replay's date-preservation is checkable.
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(HOOKS_OFF)
                .args([
                    "-c",
                    "user.name=Src Author",
                    "-c",
                    "user.email=<email>", // pii-test-fixture
                    "commit",
                    "-q",
                    "-m",
                    msg,
                    "--date",
                    date,
                ])
                .env("GIT_COMMITTER_DATE", date)
                .env("GIT_COMMITTER_NAME", "Src Author")
                .env("GIT_COMMITTER_EMAIL", "<email>") // pii-test-fixture
                .output()
                .unwrap();
            assert!(out.status.success(), "commit: {}", String::from_utf8_lossy(&out.stderr));
        };
        write(&dir, "config.txt", b"internal host <internal-ip> here\n"); // pii-test-fixture
        commit(&dir, "add config with an internal ip", "2021-01-02T03:04:05");
        // A binary blob (contains a NUL) that must pass through untouched.
        write(&dir, "asset.bin", &[0u8, 1, 2, 3, 0xff, b'1', b'9', b'2', 0, 0xfe]);
        commit(&dir, "add a binary asset", "2021-06-07T08:09:10");
        write(&dir, "notes.md", b"a plain note, nothing sensitive\n");
        commit(&dir, "add plain notes", "2021-12-24T11:12:13");
        dir
    }

    #[test]
    fn replay_preserves_graph_dates_and_scrubs_history() {
        let src = init_source();
        let wd = unique("wd");

        let src_commits = git(&src, &["rev-list", "--count", "--all"]).trim().to_string();
        let src_tree = git(&src, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
        let src_dates: String = git(&src, &["log", "--all", "--format=%ad", "--date=iso-strict"]);

        let report = replay_full_history(&src, &wd, &ReplayOpts::new()).unwrap();

        // Commit count preserved.
        assert_eq!(report.commits, 3, "three commits replayed: {report:?}");
        assert_eq!(
            git(&wd, &["rev-list", "--count", "--all"]).trim(),
            src_commits,
            "commit count preserved"
        );
        // Author DATES preserved (contribution-calendar fidelity).
        assert_eq!(
            git(&wd, &["log", "--all", "--format=%ad", "--date=iso-strict"]),
            src_dates,
            "author dates preserved"
        );
        // The internal IP is scrubbed in the FIRST (historical) commit's blob.
        let first_config = git(&wd, &["show", "HEAD~2:config.txt"]);
        assert!(first_config.contains("<internal-ip>"), "historical IP scrubbed: {first_config:?}");
        assert!(!first_config.contains("<internal-ip>"), "raw IP gone"); // pii-test-fixture
        // The binary blob passed through byte-identical.
        let wd_bin = std::fs::read(wd.join("asset.bin")).unwrap();
        assert_eq!(wd_bin, vec![0u8, 1, 2, 3, 0xff, b'1', b'9', b'2', 0, 0xfe], "binary untouched");
        assert!(report.blobs_rewritten >= 1 && report.blobs_total >= 3, "metrics: {report:?}");

        // SOURCE untouched (read-only guarantee).
        assert_eq!(git(&src, &["rev-parse", "HEAD^{tree}"]).trim(), src_tree, "source tree unchanged");
        assert!(git(&src, &["status", "--porcelain"]).trim().is_empty(), "source clean");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn empty_source_replays_cleanly() {
        let src = unique("empty-src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        let wd = unique("empty-wd");
        let report = replay_full_history(&src, &wd, &ReplayOpts::new()).unwrap();
        assert_eq!(report.commits, 0, "no commits");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn contains_subslice_works() {
        assert!(contains_subslice(b"M 100644 inline path", b" inline "));
        assert!(!contains_subslice(b"M 100644 :5 path", b" inline "));
        assert!(contains_subslice(b"anything", b""));
    }

    /// Commit `dir` with a fixed identity (helper for the gate tests).
    fn commit_all(dir: &Path, msg: &str) {
        git(dir, &["add", "-A"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(HOOKS_OFF)
            .args([
                "-c",
                "user.name=Gate Test",
                "-c",
                "user.email=<email>", // pii-test-fixture
                "commit",
                "-q",
                "-m",
                msg,
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "commit: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn bot_map() -> IdentityMap {
        IdentityMap {
            rules: vec![],
            default_name: "MoosenetBot".into(),
            default_email: "<email>".into(), // pii-test-fixture
        }
    }

    // ── GHIST-05: a PR slice is scrubbed, rebased onto the public base, dated ──
    #[test]
    fn replay_pr_slice_rebases_scrubbed_range_onto_public_base() {
        // Source: C1 only at first (the canonical baseline), then a PR adds C2, C3.
        let src = unique("prsrc");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        write(&src, "a.txt", b"baseline\n");
        commit_all(&src, "C1 baseline");
        let c1 = git(&src, &["rev-parse", "HEAD"]).trim().to_string();

        let wd = unique("prwd");
        let opts = ReplayOpts::with_author_map(bot_map());
        // Backfill C1 → canonical lineage at scrubbed(C1).
        replay_full_history(&src, &wd, &opts).unwrap();
        let canon_base = git(&wd, &["rev-parse", "HEAD"]).trim().to_string();
        let base_tree = git(&wd, &["rev-parse", &format!("{canon_base}^{{tree}}")]).trim().to_string();

        // The PR: two new internal commits, one carrying an internal IP to prove scrubbing.
        write(&src, "feature.txt", b"new host <internal-ip> wired in\n"); // pii-test-fixture
        commit_all(&src, "C2 feature");
        write(&src, "more.txt", b"second feature commit\n");
        commit_all(&src, "C3 feature");
        let c3 = git(&src, &["rev-parse", "HEAD"]).trim().to_string();

        // Simulate the PUBLIC main tip: a distinct commit with the SAME tree as the
        // canonical base (as a squash/merge of everything up to C1 would be).
        let pub_base = {
            let out = Command::new("git")
                .arg("-C")
                .arg(&wd)
                .args(HOOKS_OFF)
                .args(["-c", "user.name=Pub", "-c", "user.email=<email>", // pii-test-fixture
                       "commit-tree", &base_tree, "-m", "public squash of C1"])
                .env("GIT_COMMITTER_NAME", "Pub")
                .env("GIT_COMMITTER_EMAIL", "<email>") // pii-test-fixture
                .env("GIT_AUTHOR_NAME", "Pub")
                .env("GIT_AUTHOR_EMAIL", "<email>") // pii-test-fixture
                .output()
                .unwrap();
            assert!(out.status.success(), "commit-tree: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let rep = replay_pr_slice(&src, &wd, &c1, &c3, &pub_base, "pr-mirror/7", &opts).unwrap();
        assert_eq!(rep.commits, 2, "the slice has both feature commits: {rep:?}");

        // The PR branch is based on the public base (not the canonical lineage).
        let is_anc = Command::new("git")
            .arg("-C").arg(&wd).args(HOOKS_OFF)
            .args(["merge-base", "--is-ancestor", &pub_base, "pr-mirror/7"])
            .status().unwrap().success();
        assert!(is_anc, "pr branch must descend from the public base");
        // …and carries exactly the two commits on top of it.
        let n = git(&wd, &["rev-list", "--count", &format!("{pub_base}..pr-mirror/7")]).trim().to_string();
        assert_eq!(n, "2");

        // The branch tip has the SAME tree as the canonical head (content-identical).
        let branch_tree = git(&wd, &["rev-parse", "pr-mirror/7^{tree}"]).trim().to_string();
        let canon_tree = git(&wd, &["rev-parse", &format!("{}^{{tree}}", rep.canonical_head)]).trim().to_string();
        assert_eq!(branch_tree, canon_tree, "rebase preserved content");

        // The internal IP was scrubbed everywhere in the PR branch.
        let grep = git_capture(&wd, &["grep", "-I", "<internal-ip>", "pr-mirror/7"]); // pii-test-fixture
        assert!(grep.is_err() || grep.unwrap().trim().is_empty(), "internal IP scrubbed in PR branch");

        // The author is remapped to the bot on the PR-branch commits.
        let author = git(&wd, &["log", "-1", "--format=%an <%ae>", "pr-mirror/7"]).trim().to_string();
        assert!(author.contains("MoosenetBot"), "author remapped: {author}");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    // ── GHIST-05: refuse a PR replay when the canonical lineage isn't at the base ──
    #[test]
    fn replay_pr_slice_refuses_out_of_order() {
        let src = unique("prsrc2");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        write(&src, "a.txt", b"one\n");
        commit_all(&src, "C1");
        write(&src, "b.txt", b"two\n");
        commit_all(&src, "C2");
        let c2 = git(&src, &["rev-parse", "HEAD"]).trim().to_string();

        let wd = unique("prwd2");
        let opts = ReplayOpts::with_author_map(bot_map());
        // Backfill BOTH commits → canonical lineage is at C2, not C1.
        replay_full_history(&src, &wd, &opts).unwrap();
        let canon = git(&wd, &["rev-parse", "HEAD"]).trim().to_string();

        // Asking to replay a slice whose base is C2's parent (an earlier commit) must
        // refuse — the canonical lineage is already past it.
        let c1 = git(&src, &["rev-parse", "HEAD~1"]).trim().to_string();
        let err = replay_pr_slice(&src, &wd, &c1, &c2, &canon, "pr-mirror/9", &opts);
        assert!(err.is_err(), "must refuse an out-of-order PR replay: {err:?}");
        assert!(format!("{err:?}").contains("out of order"), "{err:?}");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    // ── GHIST-02: a secret present ONLY in a historical (non-tip) commit is caught ──
    #[test]
    fn gate_flags_secret_in_historical_commit() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let dir = unique("gate-hist");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        // Commit 1 introduces an internal IP...
        write(&dir, "leak.txt", b"internal host <internal-ip> in an old commit\n"); // pii-test-fixture
        commit_all(&dir, "add a file with an internal ip");
        // Commit 2 DELETES the file — so the TIP tree is clean, but the IP still
        // lives in commit 1's tree.
        std::fs::remove_file(dir.join("leak.txt")).unwrap();
        write(&dir, "readme.txt", b"nothing sensitive now\n");
        commit_all(&dir, "remove the leaky file");

        // Tip is clean...
        let tip_dir = unique("gate-tip");
        let tip_tree = git(&dir, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
        extract_tree(&dir, &tip_tree, &tip_dir).unwrap();
        assert!(
            crate::github::pii::ruleset_from_config(None).scan_tree(&tip_dir).is_empty(),
            "tip tree is clean"
        );

        // ...but the FULL-HISTORY gate flags commit 1.
        let report = gate_full_history(&dir).unwrap();
        assert!(!report.clean, "gate must flag the historical secret: {report:?}");
        assert!(report.commits_scanned >= 2);
        assert!(
            report.violations.iter().any(|v| v.file == "leak.txt" && v.pattern_kind == "private_ip"),
            "the historical leak.txt private_ip is flagged: {:?}",
            report.violations
        );
        // Context is redacted — the raw IP is never echoed in the report.
        assert!(
            report.violations.iter().all(|v| !v.context.contains("<internal-ip>")), // pii-test-fixture
            "context must be redacted"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&tip_dir);
    }

    // ── GHIST-02: a fully-clean history passes the gate ──
    #[test]
    fn gate_passes_clean_history() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let dir = unique("gate-clean");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        write(&dir, "a.txt", b"just some ordinary content\n");
        commit_all(&dir, "c1");
        write(&dir, "b.txt", b"more ordinary content, no secrets\n");
        commit_all(&dir, "c2");

        let report = gate_full_history(&dir).unwrap();
        assert!(report.clean, "clean history passes: {report:?}");
        assert_eq!(report.violations.len(), 0);
        assert!(report.commits_scanned >= 2 && report.unique_trees >= 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── GHIST-01 + GHIST-02 together: replay scrubs history so the gate then passes ──
    #[test]
    fn replayed_history_passes_the_full_gate() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let src = init_source(); // has an internal IP in its first commit
        let wd = unique("replay-gate-wd");
        replay_full_history(&src, &wd, &ReplayOpts::new()).unwrap();
        // After GHIST-01 scrubbed every blob, the full-history gate is clean.
        let report = gate_full_history(&wd).unwrap();
        assert!(report.clean, "replayed (scrubbed) history passes the full gate: {report:?}");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    // ── GHIST-03: attribution remap ──
    fn rule(email: Option<&str>, domain: Option<&str>, name: Option<&str>, pn: &str, pe: &str) -> IdentityRule {
        IdentityRule {
            match_email: email.map(String::from),
            match_email_domain: domain.map(String::from),
            match_name: name.map(String::from),
            public_name: pn.into(),
            public_email: pe.into(),
        }
    }

    #[test]
    fn identity_map_remaps_by_email_domain_and_name() {
        let map = IdentityMap {
            rules: vec![
                rule(Some("<email>"), None, None, "PubMe", "<email>"), // pii-test-fixture
                rule(None, Some("agents.example"), None, "MoosenetBot", "<email>"),  // pii-test-fixture
                rule(None, None, Some("Legacy Human"), "PubMe", "<email>"),  // pii-test-fixture
            ],
            default_name: "fallback-bot".into(),
            default_email: "<email>".into(),  // pii-test-fixture
        };
        // exact email (case-insensitive)
        assert_eq!(map.remap("Whatever", "<email>"), ("PubMe".into(), "<email>".into())); // pii-test-fixture
        // domain suffix
        assert_eq!(map.remap("Agent", "<email>"), ("MoosenetBot".into(), "<email>".into()));  // pii-test-fixture
        // display name
        assert_eq!(map.remap("Legacy Human", "<email>"), ("PubMe".into(), "<email>".into()));  // pii-test-fixture
        // unmatched → default (never the raw internal email)
        assert_eq!(map.remap("Nobody", "<email>"), ("fallback-bot".into(), "<email>".into()));  // pii-test-fixture
    }

    #[test]
    fn rewrite_ident_preserves_timestamp_and_scrubs_email() {
        let map = IdentityMap {
            rules: vec![rule(Some("<email>"), None, None, "PubMe", "<email>")], // pii-test-fixture
            default_name: "bot".into(),
            default_email: "<email>".into(),  // pii-test-fixture
        };
        let line = b"author Real Name <<email>> 1609556645 +0000\n"; // pii-test-fixture
        let out = rewrite_ident_line(line, &map);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "author PubMe <<email>> 1609556645 +0000\n", "remapped + ts preserved: {s}");  // pii-test-fixture
        assert!(!s.contains("<email>"), "internal email scrubbed"); // pii-test-fixture
        // committer prefix handled too; a malformed line is passed through.
        assert_eq!(rewrite_ident_line(b"committer X <<email>> 1 +0000\n", &map), // pii-test-fixture
                   b"committer PubMe <<email>> 1 +0000\n".to_vec());  // pii-test-fixture
        assert_eq!(rewrite_ident_line(b"author malformed line\n", &map), b"author malformed line\n".to_vec());
    }

    #[test]
    fn replay_with_author_map_remaps_all_idents_and_preserves_dates() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let src = init_source(); // authored by <email>  // pii-test-fixture
        let wd = unique("replay-attr-wd");
        let src_dates: String = git(&src, &["log", "--all", "--format=%ad", "--date=iso-strict"]);
        let map = IdentityMap {
            rules: vec![],
            default_name: "MoosenetBot".into(),
            default_email: "<email>".into(),  // pii-test-fixture
        };
        let report = replay_full_history(&src, &wd, &ReplayOpts::with_author_map(map)).unwrap();
        assert!(report.idents_remapped >= 2, "author+committer remapped: {report:?}");
        // NO internal author email survives; all attributed to the mapped identity.
        let authors = git(&wd, &["log", "--all", "--format=%ae|%ce"]);
        assert!(!authors.contains("<email>"), "internal email gone: {authors}"); // pii-test-fixture
        assert!(authors.contains("<email>"), "remapped: {authors}");  // pii-test-fixture
        // Dates unchanged (contribution fidelity).
        assert_eq!(git(&wd, &["log", "--all", "--format=%ad", "--date=iso-strict"]), src_dates, "dates preserved");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    // ── GHIST-04: incremental replay appends new commits 1:1 (no re-squash) ──
    #[test]
    fn incremental_replay_appends_new_commits() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let src = init_source(); // 3 commits
        let wd = unique("inc-wd");

        // First run: full backfill.
        let (full, r1) = replay_incremental_or_full(&src, &wd, &ReplayOpts::new()).unwrap();
        assert!(full && r1.commits == 3, "first run is a 3-commit backfill: {r1:?}");
        assert_eq!(git(&wd, &["rev-list", "--count", "--all"]).trim(), "3");
        let backfill_head = last_mirrored_sha(&wd).unwrap();
        assert_eq!(backfill_head, git(&src, &["rev-parse", "HEAD"]).trim());

        // Add two more internal commits (one with an IP to scrub).
        write(&src, "new1.txt", b"host <internal-ip> added later\n"); // pii-test-fixture
        commit_all(&src, "add new1");
        write(&src, "new2.txt", b"plain new content\n");
        commit_all(&src, "add new2");

        // Second run: incremental append of exactly the 2 new commits.
        let (full2, r2) = replay_incremental_or_full(&src, &wd, &ReplayOpts::new()).unwrap();
        assert!(!full2, "second run is incremental");
        assert_eq!(r2.commits, 2, "exactly 2 new commits replayed: {r2:?}");
        assert_eq!(
            git(&wd, &["rev-list", "--count", "--all"]).trim(),
            "5",
            "appended onto history (5), not re-squashed"
        );
        // Linear history (each commit has <=1 parent here).
        let tip_files = git(&wd, &["ls-tree", "-r", "--name-only", "HEAD"]);
        assert!(tip_files.contains("new1.txt") && tip_files.contains("new2.txt"), "new files present: {tip_files}");
        // The new commit's IP is scrubbed.
        let n1 = git(&wd, &["show", "HEAD:new1.txt"]);
        assert!(n1.contains("<internal-ip>") && !n1.contains("<internal-ip>"), "new IP scrubbed: {n1:?}"); // pii-test-fixture
        // The whole (backfilled + appended) history passes the gate.
        assert!(gate_full_history(&wd).unwrap().clean, "appended history gate-clean");
        assert_eq!(last_mirrored_sha(&wd).unwrap(), git(&src, &["rev-parse", "HEAD"]).trim());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    // ── GHIST-04: incremental is a no-op when the mirror is already at HEAD ──
    #[test]
    fn incremental_is_noop_at_head() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let src = init_source();
        let wd = unique("noop-wd");
        replay_incremental_or_full(&src, &wd, &ReplayOpts::new()).unwrap();
        let (full, r) = replay_incremental_or_full(&src, &wd, &ReplayOpts::new()).unwrap();
        assert!(!full && r.commits == 0, "no new commits → no-op: {r:?}");
        assert_eq!(git(&wd, &["rev-list", "--count", "--all"]).trim(), "3", "history unchanged");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    // ── GHIST-04: a non-fast-forward internal history is REFUSED, not diverged ──
    #[test]
    fn incremental_refuses_non_fastforward() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let src = init_source();
        let wd = unique("nonff-wd");
        replay_incremental_or_full(&src, &wd, &ReplayOpts::new()).unwrap();
        let c1 = git(&src, &["rev-list", "--max-parents=0", "HEAD"]).trim().to_string(); // root commit

        // Rewrite the source: reset the branch back to the root and commit a
        // DIVERGENT change — the new HEAD does not descend from the mirrored sha.
        git(&src, &["reset", "--hard", &c1]);
        write(&src, "divergent.txt", b"a rewritten branch\n");
        commit_all(&src, "divergent commit");

        let err = replay_incremental_or_full(&src, &wd, &ReplayOpts::new());
        assert!(err.is_err(), "non-ff internal history must be refused: {err:?}");
        assert!(
            format!("{err:?}").contains("fast-forward") || format!("{err:?}").contains("descend"),
            "error explains the non-ff: {err:?}"
        );
        // The mirror is untouched (still 3 commits).
        assert_eq!(git(&wd, &["rev-list", "--count", "--all"]).trim(), "3", "mirror not diverged");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }
}
