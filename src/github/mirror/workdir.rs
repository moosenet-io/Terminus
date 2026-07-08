//! GHMR-03 — clean mirror work-dir manager + `mirror-approved` tag.
//!
//! Per `mirror_ready` repo, the mirror engine maintains a dedicated **clean work
//! dir**: a PII-swept *derivative* of internal `main` that keeps its OWN linear
//! git history and shares ancestry with the public `moosenet-io/*` mirror (the
//! lineage bridge). This module owns that work dir's lifecycle — init, content
//! sync, sweep+gate, commit, and the `mirror-approved/<internal-sha>` tag.
//!
//! ## What one run does ([`MirrorWorkDir::run`])
//!   1. **Init** (first run only): create the work dir and `git init` a fresh
//!      repo with its own linear history. This history is INDEPENDENT of internal
//!      `main`'s — the two never share a merge (they have no common ancestor by
//!      design), only content flows across.
//!   2. **Sync**: mirror internal `main`'s tree CONTENT into the work dir —
//!      clear the tracked tree (everything but `.git`) and copy the source tree
//!      in. This is a content sync, NOT a merge of the divergent histories, and
//!      it structurally reflects deletions (a file gone from internal `main`
//!      disappears from the work dir).
//!   3. **Sweep + gate**: run GHMR-02's mechanical [`sweep`](super::sweep)
//!      (real → placeholder) which itself re-runs GHMR-01's authoritative gate to
//!      compute the *residual* (non-mechanical) violations.
//!   4. **Commit**: `git add -A` + commit the swept state, the message referencing
//!      the internal sha. An unchanged swept tree makes no empty commit.
//!   5. **Tag**: IFF the gate reports **0 residual violations**, create
//!      `mirror-approved/<internal-sha>` marking that swept commit as vetted for
//!      public push. Residual violations → do NOT tag; they are returned (for
//!      GHMR-05's subagent cleaning).
//!
//! ## Force-push-free, like the vault
//! Every git invocation goes through [`WorkDirGitOp`] + [`assert_never_force`],
//! mirroring `scribe::vault`'s guardrail: no operation here can force-overwrite
//! history. The work dir only ever grows its own linear history forward; the one
//! sanctioned re-baseline `--force` is GHMR-07's operator-blessed bootstrap step,
//! not anything this module performs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;

use crate::error::ToolError;
use crate::github::pii::{active_gate_config_relpaths, TreeViolation};

use super::sweep::{active_config_relpath, sweep_tree_with_resolved_config, SweepReport};

/// Environment variable naming the parent directory that holds every repo's
/// clean mirror work dir (one subdirectory per repo). The per-repo work dir is
/// `<TERMINUS_MIRROR_WORKDIR_ROOT>/<repo>`.
pub const WORKDIR_ROOT_ENV: &str = "TERMINUS_MIRROR_WORKDIR_ROOT";

/// Tag namespace marking a swept commit as gate-clean and vetted for public push.
const APPROVED_TAG_PREFIX: &str = "mirror-approved/";

/// Commit identity for the work-dir history. The work dir is a machine-managed
/// derivative, never authored by a human, so a fixed bot identity keeps its
/// linear history reproducible and independent of the dev box's global git config
/// (which may be unset). The address is an explicit no-reply placeholder — it is
/// on GHMR-01's allowed-emails posture and carries no real PII.
const BOT_NAME: &str = "mirror-bot";
const BOT_EMAIL: &str = "<email>"; // pii-test-fixture

// ── Config ────────────────────────────────────────────────────────────────

/// Locates a single repo's clean mirror work dir and its internal source.
#[derive(Debug, Clone)]
pub struct MirrorWorkDir {
    /// Logical repo name (e.g. `Terminus`) — the work-dir subdirectory name and
    /// the label used in commit messages.
    repo: String,
    /// The internal `main` checkout whose tree content is mirrored in. Its HEAD
    /// sha stamps the commit message and the `mirror-approved/<sha>` tag.
    source: PathBuf,
    /// The per-repo clean work dir (a PII-swept derivative with its own history).
    work_dir: PathBuf,
}

impl MirrorWorkDir {
    /// Construct from explicit paths (the form GHMR-04's tools use once they have
    /// resolved config).
    pub fn new(repo: impl Into<String>, source: impl Into<PathBuf>, work_dir: impl Into<PathBuf>) -> Self {
        Self { repo: repo.into(), source: source.into(), work_dir: work_dir.into() }
    }

    /// Resolve the work dir from config: `<`[`WORKDIR_ROOT_ENV`]`>/<repo>`. The
    /// source is the internal `main` checkout passed by the caller (the dev-box
    /// clone). Errors if the root env var is unset/empty — the caller surfaces it
    /// as a "work-dir location not configured" blocker rather than defaulting to
    /// some ambient path.
    pub fn from_config(repo: impl Into<String>, source: impl Into<PathBuf>) -> Result<Self, ToolError> {
        let repo = repo.into();
        let root = std::env::var(WORKDIR_ROOT_ENV).ok().filter(|s| !s.is_empty()).ok_or_else(|| {
            ToolError::NotConfigured(format!(
                "{WORKDIR_ROOT_ENV} is not set — cannot locate the mirror work dir for {repo}"
            ))
        })?;
        let work_dir = Path::new(&root).join(&repo);
        Ok(Self { repo, source: source.into(), work_dir })
    }

    /// The per-repo clean work dir path.
    pub fn path(&self) -> &Path {
        &self.work_dir
    }

    /// The logical repo name (the work-dir subdirectory + commit label). Exposed
    /// for GHMR-05's cleaning orchestration, which stamps escalation payloads.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// The internal source checkout path.
    pub fn source(&self) -> &Path {
        &self.source
    }

    // ── Public lifecycle ───────────────────────────────────────────────────

    /// Whether the work dir has been initialised (exists and holds a git repo).
    pub fn is_initialised(&self) -> bool {
        self.work_dir.join(".git").exists()
    }

    /// Run one full cycle: init (first run) → sync content → sweep+gate → commit
    /// → tag-if-clean. Idempotent with respect to internal `main`: an unchanged
    /// internal `main` (its sha already carries a `mirror-approved` tag) short-
    /// circuits to a no-op that keeps the existing tag.
    pub fn run(&self) -> Result<WorkDirRunReport, ToolError> {
        self.ensure_disjoint_paths()?;
        let internal_sha = self.internal_head_sha()?;

        // Unchanged internal main → no-op, keep the existing approval tag. We key
        // off the tag rather than a stored state file: the tag IS the durable
        // record that this exact internal sha was already swept clean and vetted.
        let first_run = !self.is_initialised();
        if !first_run && self.approved_tag_exists(&internal_sha)? {
            return Ok(WorkDirRunReport {
                repo: self.repo.clone(),
                internal_sha: internal_sha.clone(),
                first_run: false,
                synced: false,
                committed: false,
                commit_sha: Some(self.work_head_sha()?),
                tagged: false,
                tag: Some(approved_tag(&internal_sha)),
                residual_violations: Vec::new(),
                noop_reason: Some("internal main unchanged since last approval".into()),
            });
        }

        if first_run {
            self.init_work_dir()?;
        }

        // Mirror internal main's tree content in — export the CAPTURED sha's tree
        // (not a live `HEAD`, which could advance under us), content sync only.
        self.sync_content(&internal_sha)?;

        // Sweep → gate → commit/tag the just-synced tree. `synced`/`first_run` are
        // stamped here since `finalize` (also used by GHMR-05 with no sync) can't
        // know them.
        let mut report = self.finalize(&internal_sha)?;
        report.first_run = first_run;
        report.synced = true;
        Ok(report)
    }

    /// Process the CURRENT work-dir tree (NO source sync): mechanical sweep →
    /// GHMR-01 gate → iff clean, commit the swept state + tag
    /// `mirror-approved/<internal_sha>`; else return the residual violations with
    /// nothing committed/tagged.
    ///
    /// This is the primitive GHMR-05 calls AFTER cleaning residual spots in the
    /// work dir: because it never re-syncs from source, a follow-up finalize turns
    /// the operator/agent-cleaned tree into a clean approved commit — whereas
    /// calling [`run`](Self::run) again would clear+re-archive the source and
    /// discard that cleanup. `run()` = sync + `finalize`.
    ///
    /// The returned report's `synced` is `false` and `first_run` is `false`;
    /// [`run`](Self::run) overrides both when it drives the sync itself.
    pub fn finalize(&self, internal_sha: &str) -> Result<WorkDirRunReport, ToolError> {
        self.ensure_disjoint_paths()?;
        // Mechanical sweep → residual detection via GHMR-01's gate.
        let sweep: SweepReport = sweep_tree_with_resolved_config(&self.work_dir)?;
        let residual = sweep.residual_violations.clone();
        let clean = sweep.is_clean();

        // SAFETY INVARIANT (publication safety, force-free): a swept tree with
        // residual PII is NEVER committed. Committing it would make it a permanent
        // ANCESTOR of a later clean approved commit, and — because pushes are
        // ff-only / force-free — that dirty ancestor could never be excised, so
        // pushing the `mirror-approved` tag would leak the residual secret into
        // public history forever. Instead the residual case leaves the swept
        // working tree in place (uncommitted) and returns the violations for
        // GHMR-05 to clean IN THE WORK DIR before a clean commit is ever made.
        // This deliberately refines the spec's "swept+committed" wording in favor
        // of the unconditional PII hard-block the spec also mandates.
        if !clean {
            return Ok(WorkDirRunReport {
                repo: self.repo.clone(),
                internal_sha: internal_sha.to_string(),
                first_run: false,
                synced: false,
                committed: false,
                // None when no commit exists yet; the prior clean commit otherwise
                // (the dirty tree is uncommitted on top of it).
                commit_sha: self.work_head_sha().ok(),
                tagged: false,
                tag: None,
                residual_violations: residual,
                noop_reason: None,
            });
        }

        // Clean: commit the swept state (no-op if byte-identical to the current
        // work-dir HEAD — e.g. only excluded files changed upstream), then tag.
        let committed = self.commit_swept(internal_sha, &sweep)?;
        let commit_sha = self.work_head_sha()?;
        let tagged = self.tag_approved(internal_sha)?;

        Ok(WorkDirRunReport {
            repo: self.repo.clone(),
            internal_sha: internal_sha.to_string(),
            first_run: false,
            synced: false,
            committed,
            commit_sha: Some(commit_sha),
            tagged,
            tag: Some(approved_tag(internal_sha)),
            residual_violations: residual,
            noop_reason: None,
        })
    }

    // ── Steps ──────────────────────────────────────────────────────────────

    /// Reject a source / work-dir configuration where the two paths equal or
    /// contain one another. This is a hard SOURCE-ISOLATION guardrail: `sync_content`
    /// starts by CLEARING the work-dir tree, so if the work dir equaled or contained
    /// the source checkout that clear would delete internal `main`; and an equal path
    /// would also make the mirror reuse the source's own history instead of the
    /// promised independent lineage. [`MirrorWorkDir::new`] accepts arbitrary explicit
    /// paths, so this is validated before ANY mutation (at the top of [`Self::run`] and
    /// [`Self::finalize`]).
    fn ensure_disjoint_paths(&self) -> Result<(), ToolError> {
        // Canonicalize the longest existing prefix of each path so a not-yet-created
        // work dir (first run) still resolves symlinks/`..` on its existing parent.
        fn resolve(p: &Path) -> PathBuf {
            if let Ok(c) = p.canonicalize() {
                return c;
            }
            match (p.parent(), p.file_name()) {
                (Some(parent), Some(name)) => match parent.canonicalize() {
                    Ok(pc) => pc.join(name),
                    Err(_) => p.to_path_buf(),
                },
                _ => p.to_path_buf(),
            }
        }
        let src = resolve(&self.source);
        let wd = resolve(&self.work_dir);
        // `starts_with` is component-wise, so `/a/bc` does not "contain" `/a/b`.
        if src == wd || src.starts_with(&wd) || wd.starts_with(&src) {
            return Err(ToolError::InvalidArgument(format!(
                "mirror work dir and internal source must be disjoint paths (neither equal \
                 nor nested): source={}, work_dir={}",
                src.display(),
                wd.display()
            )));
        }
        Ok(())
    }

    /// The internal source's current `main` HEAD sha (full 40-char).
    fn internal_head_sha(&self) -> Result<String, ToolError> {
        if !self.source.join(".git").exists() {
            return Err(ToolError::InvalidArgument(format!(
                "internal source is not a git repo: {}",
                self.source.display()
            )));
        }
        let out = run_git(&self.source, &["rev-parse", "HEAD"])?;
        Ok(out.trim().to_string())
    }

    /// The work dir's current HEAD sha.
    fn work_head_sha(&self) -> Result<String, ToolError> {
        let out = run_git(&self.work_dir, &["rev-parse", "HEAD"])?;
        Ok(out.trim().to_string())
    }

    /// Create the work dir + a fresh git repo with its own linear history.
    fn init_work_dir(&self) -> Result<(), ToolError> {
        std::fs::create_dir_all(&self.work_dir).map_err(|e| {
            ToolError::Execution(format!("failed creating work dir {}: {e}", self.work_dir.display()))
        })?;
        // `-b main`: a deterministic default branch, independent of the dev box's
        // `init.defaultBranch`. This history has no common ancestor with internal
        // main by design — only content is synced across, never a merge.
        run_git(&self.work_dir, &["init", "-q", "-b", "main"])?;
        Ok(())
    }

    /// Mirror internal `main`'s tree CONTENT into the work dir: clear the tracked
    /// tree (everything but `.git`) then export the source's committed tree at the
    /// captured `internal_sha`. This is a content sync — the work dir keeps its
    /// own history — and structurally reflects deletions.
    ///
    /// The tree comes from `git archive <internal_sha>` (the committed tree at the
    /// exact sha `run()` captured), NOT `HEAD` and NOT the working checkout. That
    /// is deliberate and load-bearing for mirror safety:
    ///   * archiving the captured sha (not live `HEAD`) means that even if the
    ///     source checkout advances between the `rev-parse` and here, the swept
    ///     tree, the commit message, and the `mirror-approved/<sha>` tag all refer
    ///     to the SAME internal commit — the tag genuinely proves that named tree
    ///     passed the gate; and
    ///   * untracked / uncommitted / `.gitignore`d files are NEVER copied, so the
    ///     mirror can never publish local checkout artifacts; and
    ///   * tracked symlinks ARE preserved (git archive emits them as symlink tar
    ///     entries), rather than being silently dropped.
    fn sync_content(&self, internal_sha: &str) -> Result<(), ToolError> {
        clear_tree_except_git(&self.work_dir)?;
        export_tree(&self.source, &self.work_dir, internal_sha)?;
        Ok(())
    }

    /// Stage everything and commit the swept state. Returns `false` (no error)
    /// when there is nothing to commit AND a HEAD already exists — the swept tree
    /// equals the current HEAD, so the internal change touched only excluded
    /// content. First run with an empty (or all-excluded) source tree is the one
    /// exception: with no HEAD yet, an initial `--allow-empty` commit is made so a
    /// valid empty mirror snapshot still gets a committable, taggable HEAD.
    fn commit_swept(&self, internal_sha: &str, sweep: &SweepReport) -> Result<bool, ToolError> {
        // PUBLICATION SAFETY: never commit a matcher-config file into the approved
        // mirror tree. Both the placeholder config (`mirror-placeholders.toml`) and
        // the PII gate config (`pii-gate.toml`) exist to CATALOG the real infra
        // values the sweep/gate map, and each surface deliberately excludes its own
        // config from rewriting + residual detection. An otherwise-clean tree would
        // therefore still ship those raw literals inside the config file, leaking
        // PII into public history under an approved tag. They are build-time inputs,
        // not mirror content; drop each (that resolves INSIDE the work dir) before
        // staging. A no-op when env-pointed outside the work dir; re-synced from
        // source next run, so removal here is never destructive. Only these two
        // catalog files are dropped — other gate-excluded files (source, Cargo.lock,
        // images) are legitimate mirror content and must still ship.
        let mirror_excluded: Vec<String> = active_config_relpath(&self.work_dir)
            .into_iter()
            .chain(active_gate_config_relpaths(&self.work_dir))
            .collect();
        for rel in &mirror_excluded {
            let cfg_path = self.work_dir.join(rel);
            if cfg_path.exists() {
                std::fs::remove_file(&cfg_path).map_err(|e| {
                    ToolError::Execution(format!(
                        "failed excluding matcher config {} from mirror commit: {e}",
                        cfg_path.display()
                    ))
                })?;
            }
        }

        run_git(&self.work_dir, &["add", "-A"])?;

        // `--cached --quiet` exits non-zero iff there is something staged.
        let has_staged = !git_ok(&self.work_dir, &["diff", "--cached", "--quiet"]);
        // Whether the work dir already has any commit (a resolvable HEAD).
        let has_head = git_ok(&self.work_dir, &["rev-parse", "--verify", "-q", "HEAD"]);
        if !has_staged && has_head {
            return Ok(false);
        }
        // Nothing staged AND no HEAD → first run over an empty/all-excluded source
        // tree: fall through and make an initial empty commit so HEAD exists.
        let allow_empty = !has_staged;

        let short = internal_sha.get(..12).unwrap_or(internal_sha);
        let message = format!(
            "mirror: sync {repo} internal main {short}\n\n\
             PII-swept derivative of internal main {internal_sha}.\n\
             files_rewritten={fr} replacements={rp} residual={rc}\n\n\
             This commit belongs to the clean mirror work dir's own linear history;\n\
             it is NOT a merge of internal main (no shared ancestor by design).",
            repo = self.repo,
            fr = sweep.files_rewritten,
            rp = sweep.replacements,
            rc = sweep.residual_violations.len(),
        );

        let mut args: Vec<String> = vec![
            "-c".into(),
            format!("user.name={BOT_NAME}"),
            "-c".into(),
            format!("user.email={BOT_EMAIL}"),
            "commit".into(),
            "-q".into(),
            "-m".into(),
            message,
        ];
        if allow_empty {
            args.push("--allow-empty".into());
        }
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_git(&self.work_dir, &argv)?;
        Ok(true)
    }

    /// Create the `mirror-approved/<internal-sha>` tag on the current work-dir
    /// HEAD. Idempotent: if the tag already exists (same internal sha re-run) it
    /// is kept, not recreated, and `false` is returned. Never moves an existing
    /// tag (that would be a history rewrite).
    fn tag_approved(&self, internal_sha: &str) -> Result<bool, ToolError> {
        if self.approved_tag_exists(internal_sha)? {
            return Ok(false);
        }
        let tag = approved_tag(internal_sha);
        let message = format!(
            "mirror-approved: {repo} internal main {internal_sha}\n\n\
             Gate-clean (0 residual PII violations) swept snapshot, vetted for\n\
             public mirror push.",
            repo = self.repo,
        );
        run_git(
            &self.work_dir,
            &[
                "-c",
                &format!("user.name={BOT_NAME}"),
                "-c",
                &format!("user.email={BOT_EMAIL}"),
                "tag",
                "-a",
                &tag,
                "-m",
                &message,
            ],
        )?;
        Ok(true)
    }

    /// Whether `mirror-approved/<internal-sha>` already exists in the work dir.
    pub fn approved_tag_exists(&self, internal_sha: &str) -> Result<bool, ToolError> {
        if !self.is_initialised() {
            return Ok(false);
        }
        let tag = approved_tag(internal_sha);
        let out = run_git(&self.work_dir, &["tag", "-l", &tag])?;
        Ok(out.lines().any(|l| l.trim() == tag))
    }

    // ── Read-only queries for GHMR-04's mirror subtools ──────────────────────
    //
    // These expose the existing private git reads (HEAD shas, tag resolution)
    // as a public, side-effect-free surface so `mirror::tools` can build the
    // `github_mirror_{status,push}` reports without duplicating the force-guarded
    // git runner. All are pure reads — none mutate the work dir or source.

    /// The internal source's current `main` HEAD sha (full 40-char).
    pub fn source_head_sha(&self) -> Result<String, ToolError> {
        self.internal_head_sha()
    }

    /// The work dir's current HEAD sha, or `None` when the work dir has no commit
    /// yet (uninitialised, or a residual-only first run that never committed).
    pub fn head_sha_opt(&self) -> Option<String> {
        if !self.is_initialised() {
            return None;
        }
        self.work_head_sha().ok()
    }

    /// The work-dir commit the `mirror-approved/<internal_sha>` tag points at, or
    /// `None` when that internal sha has not been approved (no tag). This is the
    /// exact commit GHMR-04's `github_mirror_push` publishes.
    pub fn approved_commit(&self, internal_sha: &str) -> Result<Option<String>, ToolError> {
        if !self.approved_tag_exists(internal_sha)? {
            return Ok(None);
        }
        // `<tag>^{commit}` dereferences the annotated tag object to the commit it
        // marks (a plain `rev-parse <tag>` would yield the tag object's own sha).
        let spec = format!("{}^{{commit}}", approved_tag(internal_sha));
        let out = run_git(&self.work_dir, &["rev-parse", "--verify", "-q", &spec])?;
        Ok(Some(out.trim().to_string()))
    }

    /// Every `mirror-approved/*` tag currently in the work dir (the full set of
    /// vetted snapshots), newest git-tag-order last. Empty when none exist.
    pub fn approved_tags(&self) -> Result<Vec<String>, ToolError> {
        if !self.is_initialised() {
            return Ok(Vec::new());
        }
        let pattern = format!("{APPROVED_TAG_PREFIX}*");
        let out = run_git(&self.work_dir, &["tag", "-l", &pattern])?;
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }
}

/// The approval tag name for an internal sha.
pub(crate) fn approved_tag(internal_sha: &str) -> String {
    format!("{APPROVED_TAG_PREFIX}{internal_sha}")
}

// ── Run report ──────────────────────────────────────────────────────────────

/// Structured outcome of one [`MirrorWorkDir::run`], surfaced by GHMR-04's tools.
#[derive(Debug, Clone)]
pub struct WorkDirRunReport {
    /// Repo name.
    pub repo: String,
    /// Internal `main` HEAD sha this run mirrored.
    pub internal_sha: String,
    /// Whether this run initialised the work dir.
    pub first_run: bool,
    /// Whether content was synced this run (false only on the unchanged no-op).
    pub synced: bool,
    /// Whether a new swept commit was made (false = swept tree unchanged).
    pub committed: bool,
    /// The work-dir HEAD sha after the run.
    pub commit_sha: Option<String>,
    /// Whether a new `mirror-approved` tag was created this run.
    pub tagged: bool,
    /// The approval tag name when the run ended clean (created OR pre-existing);
    /// `None` when residual violations blocked approval.
    pub tag: Option<String>,
    /// Residual (non-mechanical) violations left after the sweep — empty iff the
    /// run was gate-clean. Non-empty means NO tag; hand these to GHMR-05.
    pub residual_violations: Vec<TreeViolation>,
    /// Set when the run short-circuited to a no-op (e.g. unchanged internal main).
    pub noop_reason: Option<String>,
}

impl WorkDirRunReport {
    /// Whether the run ended gate-clean and vetted (tag present, no residuals).
    pub fn is_approved(&self) -> bool {
        self.residual_violations.is_empty() && self.tag.is_some()
    }

    /// Stable machine-readable JSON (for the GHMR-04 mirror subtools).
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "repo": self.repo,
            "internal_sha": self.internal_sha,
            "first_run": self.first_run,
            "synced": self.synced,
            "committed": self.committed,
            "commit_sha": self.commit_sha,
            "tagged": self.tagged,
            "tag": self.tag,
            "approved": self.is_approved(),
            "residual_count": self.residual_violations.len(),
            "residual_violations": self.residual_violations.iter().map(|v| json!({
                "file": v.file,
                "line": v.line,
                "pattern_kind": v.pattern_kind,
                "context": v.context,
            })).collect::<Vec<_>>(),
            "noop_reason": self.noop_reason,
        })
    }
}

// ── Tree content sync (skip .git) ──────────────────────────────────────────

/// Remove every top-level entry of `dir` except `.git`, so the subsequent copy
/// mirrors the source exactly (deletions included). Never touches the git history.
fn clear_tree_except_git(dir: &Path) -> Result<(), ToolError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ToolError::Execution(format!("failed reading work dir {}: {e}", dir.display()))
    })?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let res = if ft.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        res.map_err(|e| {
            ToolError::Execution(format!("failed clearing {}: {e}", path.display()))
        })?;
    }
    Ok(())
}

/// Export the source repo's committed tree AT `sha` into `dst` by streaming
/// `git archive --format=tar <sha>` (run in `src`) into `tar -x` (run in `dst`).
///
/// Archiving the exact captured `sha` (not live `HEAD`) closes a TOCTOU race: the
/// swept tree always corresponds to the same commit the report/tag name, so a
/// concurrent advance of the source checkout can never make the approval tag
/// point at a tree that never passed the gate. Using the committed tree — not a
/// filesystem copy of the checkout — also guarantees only tracked, committed
/// content lands in the mirror derivative: untracked / `.gitignore`d files are
/// excluded, and tracked symlinks are preserved (tar carries them as symlink
/// entries). Both `git` and `tar` are present on the dev box, the sanctioned
/// git-transport host where this runs.
fn export_tree(src: &Path, dst: &Path, sha: &str) -> Result<(), ToolError> {
    let mut archive = Command::new("git")
        .current_dir(src)
        .args(["archive", "--format=tar", sha])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::Execution(format!("failed to spawn git archive: {e}")))?;

    let archive_out = archive
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("git archive produced no stdout pipe".into()))?;

    // `tar -x -f -` reads the archive from stdin and extracts into `dst`.
    let untar = Command::new("tar")
        .current_dir(dst)
        .args(["-x", "-f", "-"])
        .stdin(Stdio::from(archive_out))
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| ToolError::Execution(format!("failed to spawn tar: {e}")))?;

    // Reap git archive and surface either side's failure (a failed archive can
    // still leave tar succeeding on a truncated stream, so check both).
    let archive_status = archive
        .wait()
        .map_err(|e| ToolError::Execution(format!("failed waiting on git archive: {e}")))?;
    if !archive_status.success() {
        let mut err = String::new();
        if let Some(mut s) = archive.stderr.take() {
            use std::io::Read;
            let _ = s.read_to_string(&mut err);
        }
        return Err(ToolError::Execution(format!(
            "git archive {sha} (in {}) failed: {}",
            src.display(),
            err.trim()
        )));
    }
    if !untar.status.success() {
        return Err(ToolError::Execution(format!(
            "tar extract (in {}) failed: {}",
            dst.display(),
            String::from_utf8_lossy(&untar.stderr).trim()
        )));
    }
    Ok(())
}

// ── Git invocation (force-push-free, like scribe::vault) ───────────────────

const BANNED_FORCE_TOKENS: &[&str] = &["--force", "-f", "--force-with-lease", "--hard"];

/// Panics if any argv element is a force / hard-reset token. Mirrors
/// `scribe::vault::assert_never_force_push`: this module's git ops only ever move
/// the work dir's own linear history FORWARD; the one sanctioned re-baseline
/// `--force` is GHMR-07's operator-blessed bootstrap, never performed here.
pub(crate) fn assert_never_force(argv: &[&str]) {
    for token in argv {
        let lower = token.to_lowercase();
        assert!(
            !BANNED_FORCE_TOKENS.contains(&lower.as_str()),
            "mirror work-dir git argv contained a force/hard token '{token}': {argv:?}"
        );
    }
}

/// Command-line flags injected before EVERY git subcommand in the mirror engine to
/// DISABLE repo hooks. The work dir is populated from internal main's tree AND
/// edited by an (operator-configured, but not fully sandboxed) cleaning subagent
/// (GHMR-05), so it could contain a hostile `.git/hooks/pre-commit`. Without this,
/// `finalize`'s `git commit` would execute that hook — running arbitrary code in
/// the parent's process tree and defeating the cleaner's env-isolation. Passing
/// `core.hooksPath` on the command line overrides any repo-config value the cleaner
/// might plant, so no hook under `.git/hooks` (or a redirected path) ever runs.
/// `/dev/null` is a non-directory, so git finds no hook there and silently skips
/// them (the dev box — the sole host these git ops run on — is Linux).
const HOOKS_OFF: &[&str] = &["-c", "core.hooksPath=/dev/null"];

/// Run a git command in `cwd`, returning stdout on success or an `Execution`
/// error carrying stderr on failure. Hooks are disabled (see [`HOOKS_OFF`]).
pub(crate) fn run_git(cwd: &Path, args: &[&str]) -> Result<String, ToolError> {
    assert_never_force(args);
    let output = Command::new("git")
        .current_dir(cwd)
        .args(HOOKS_OFF)
        .args(args)
        .output()
        .map_err(|e| ToolError::Execution(format!("failed to spawn git {}: {e}", args.join(" "))))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(ToolError::Execution(format!(
            "git {} (in {}) failed: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// Run a git command purely for its exit status (used for `diff --cached
/// --quiet`, where non-zero is a meaningful "there are staged changes" signal,
/// not a failure). Returns `true` iff git exited 0.
pub(crate) fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    assert_never_force(args);
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;

    fn unique(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ghmr03-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    /// Build an internal-main-style source git repo with an initial commit.
    fn init_source(files: &[(&str, &str)]) -> PathBuf {
        let dir = unique("src");
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q", "-b", "main"]).unwrap();
        for (rel, content) in files {
            write_file(&dir, rel, content);
        }
        run_git(&dir, &["add", "-A"]).unwrap();
        run_git(
            &dir,
            &[
                "-c",
                "user.name=src",
                "-c",
                "user.email=<email>", // pii-test-fixture
                "commit",
                "-q",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        dir
    }

    /// Add/modify/delete files in the source and make a new commit.
    fn commit_source(dir: &Path, adds: &[(&str, &str)], deletes: &[&str], msg: &str) {
        for (rel, content) in adds {
            write_file(dir, rel, content);
        }
        for rel in deletes {
            let _ = std::fs::remove_file(dir.join(rel));
        }
        run_git(dir, &["add", "-A"]).unwrap();
        run_git(
            dir,
            &[
                "-c",
                "user.name=src",
                "-c",
                "user.email=<email>", // pii-test-fixture
                "commit",
                "-q",
                "-m",
                msg,
            ],
        )
        .unwrap();
    }

    fn clear_env() {
        std::env::remove_var("TERMINUS_MIRROR_PLACEHOLDERS");
        std::env::remove_var("TERMINUS_PII_CONFIG");
        std::env::remove_var("GITHUB_ALLOWED_AUTHORS");
        std::env::remove_var(WORKDIR_ROOT_ENV);
    }

    fn tag_list(dir: &Path) -> Vec<String> {
        run_git(dir, &["tag", "-l"])
            .unwrap()
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn commit_count(dir: &Path) -> usize {
        run_git(dir, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn cleanup(paths: &[&Path]) {
        for p in paths {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    // ── clean tree → swept, committed, tagged ────────────────────────────────
    #[test]
    #[serial]
    fn clean_source_is_swept_committed_and_tagged() {
        clear_env();
        // A private IP is MECHANICALLY swept to a placeholder → 0 residual → clean.
        let src = init_source(&[
            ("README.md", "The host is <internal-ip> in the lab.\n"), // pii-test-fixture
            ("src/lib.rs", "pub fn ok() {}\n"),
        ]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);

        let report = mgr.run().unwrap();
        assert!(report.first_run);
        assert!(report.synced);
        assert!(report.committed);
        assert!(report.tagged, "clean run must create the approval tag");
        assert!(report.residual_violations.is_empty());
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        assert_eq!(report.tag.as_deref(), Some(approved_tag(&sha).as_str()));
        assert!(tag_list(&wd).contains(&approved_tag(&sha)));

        // The swept work dir must no longer contain the raw private IP.
        let readme = std::fs::read_to_string(wd.join("README.md")).unwrap();
        assert!(!readme.contains("<internal-ip>"), "IP must be swept"); // pii-test-fixture
        assert!(readme.contains("REDACTED"), "placeholder must be present");

        cleanup(&[&src, &wd]);
    }

    // ── dirty (non-mechanical residual) → NOT committed, NOT tagged ───────────
    #[test]
    #[serial]
    fn residual_violation_blocks_commit_and_tag() {
        clear_env();
        // A raw API-key-shaped secret is NOT mechanically placeholderable → it
        // stays as a residual violation. The run must NOT tag AND must NOT commit
        // (a dirty commit would become a permanent ancestor of a later approved
        // commit — publication-unsafe under force-free pushes).
        let src = init_source(&[(
            "config.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);

        let report = mgr.run().unwrap();
        assert!(!report.committed, "dirty swept state must NOT be committed");
        assert!(!report.tagged, "residual must block the approval tag");
        assert!(report.tag.is_none());
        assert!(!report.residual_violations.is_empty(), "residual returned for GHMR-05");
        assert!(tag_list(&wd).is_empty(), "no approval tag on a dirty run");
        // The work dir has NO commits — the dirty tree is never in history.
        assert!(
            run_git(&wd, &["rev-parse", "--verify", "HEAD"]).is_err(),
            "no commit must exist after a residual-only first run"
        );
        // The swept tree IS present on disk (uncommitted) for GHMR-05 to clean.
        assert!(wd.join("config.txt").exists());

        cleanup(&[&src, &wd]);
    }

    // ── a later clean run has NO dirty ancestor (P1 safety) ───────────────────
    #[test]
    #[serial]
    fn clean_run_after_residual_has_no_dirty_ancestor() {
        clear_env();
        // Run 1: internal main is dirty (residual secret) → no commit.
        let src = init_source(&[(
            "config.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let r1 = mgr.run().unwrap();
        assert!(!r1.committed && !r1.tagged);

        // Internal main is fixed (secret removed) → run 2 is clean.
        commit_source(&src, &[("config.txt", "just clean config content\n")], &[], "scrub");
        let r2 = mgr.run().unwrap();
        assert!(r2.committed && r2.tagged, "clean run commits+tags");
        // The approved history is exactly ONE commit — no dirty ancestor exists.
        assert_eq!(commit_count(&wd), 1, "no dirty ancestor in approved history");

        cleanup(&[&src, &wd]);
    }

    // ── linear history across two syncs (no divergent-ancestor merge) ─────────
    #[test]
    #[serial]
    fn two_syncs_keep_linear_history() {
        clear_env();
        let src = init_source(&[("a.txt", "clean content 1\n")]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);

        let r1 = mgr.run().unwrap();
        assert!(r1.tagged);
        assert_eq!(commit_count(&wd), 1);

        // A genuine content change upstream → second sync → second commit.
        commit_source(&src, &[("a.txt", "clean content 2\n")], &[], "update a");
        let r2 = mgr.run().unwrap();
        assert!(r2.committed);
        assert!(r2.tagged);
        assert_eq!(commit_count(&wd), 2, "linear: exactly one new commit");

        // History is strictly linear — every commit has at most one parent (no
        // merge of divergent histories).
        let parents = run_git(&wd, &["log", "--pretty=%P"]).unwrap();
        for line in parents.lines() {
            let n = line.split_whitespace().count();
            assert!(n <= 1, "commit has {n} parents — history must stay linear");
        }
        // Both internal shas are tagged.
        assert_eq!(tag_list(&wd).len(), 2);

        cleanup(&[&src, &wd]);
    }

    // ── unchanged internal main → no-op, keep existing tag ───────────────────
    #[test]
    #[serial]
    fn unchanged_internal_main_is_noop_keeps_tag() {
        clear_env();
        let src = init_source(&[("a.txt", "clean content\n")]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);

        let r1 = mgr.run().unwrap();
        assert!(r1.tagged);
        let after_first = commit_count(&wd);
        let tags_after_first = tag_list(&wd);

        // Re-run with NO upstream change → no-op path.
        let r2 = mgr.run().unwrap();
        assert!(!r2.synced, "unchanged internal main must not re-sync");
        assert!(!r2.committed);
        assert!(!r2.tagged, "no new tag");
        assert!(r2.tag.is_some(), "existing tag reported");
        assert!(r2.noop_reason.is_some());
        assert_eq!(commit_count(&wd), after_first, "no new commit");
        assert_eq!(tag_list(&wd), tags_after_first, "tag set unchanged");

        cleanup(&[&src, &wd]);
    }

    // ── deletions in internal main reflected in the work dir ─────────────────
    #[test]
    #[serial]
    fn deleted_files_reflected_in_work_dir() {
        clear_env();
        let src = init_source(&[
            ("keep.txt", "keep me clean\n"),
            ("gone.txt", "delete me clean\n"),
            ("nested/also_gone.txt", "nested clean\n"),
        ]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);

        mgr.run().unwrap();
        assert!(wd.join("gone.txt").exists());
        assert!(wd.join("nested/also_gone.txt").exists());

        // Delete upstream, re-sync.
        commit_source(
            &src,
            &[],
            &["gone.txt", "nested/also_gone.txt"],
            "remove files",
        );
        let r2 = mgr.run().unwrap();
        assert!(r2.committed);
        assert!(!wd.join("gone.txt").exists(), "deleted file must be gone");
        assert!(
            !wd.join("nested/also_gone.txt").exists(),
            "deleted nested file must be gone"
        );
        assert!(wd.join("keep.txt").exists(), "kept file remains");
        // The deletion is captured in the work-dir git history.
        let show = run_git(&wd, &["show", "--stat", "HEAD"]).unwrap();
        assert!(show.contains("gone.txt"), "deletion recorded in history");

        cleanup(&[&src, &wd]);
    }

    // ── from_config resolves <root>/<repo> and errors when unset ──────────────
    #[test]
    #[serial]
    fn from_config_resolves_and_requires_root() {
        clear_env();
        let src = init_source(&[("a.txt", "clean\n")]);
        assert!(
            MirrorWorkDir::from_config("Terminus", &src).is_err(),
            "must error when root env unset"
        );

        let root = unique("root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var(WORKDIR_ROOT_ENV, &root);
        let mgr = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        assert_eq!(mgr.path(), root.join("Terminus"));
        std::env::remove_var(WORKDIR_ROOT_ENV);

        cleanup(&[&src, &root]);
    }

    // ── incremental sweep of a newly-added file is committed & re-tagged ──────
    #[test]
    #[serial]
    fn incremental_added_file_is_swept_and_committed() {
        clear_env();
        let src = init_source(&[("a.txt", "clean 1\n")]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        mgr.run().unwrap();

        // Add a NEW file carrying a mechanically-sweepable IP.
        commit_source(&src, &[("b.txt", "see <internal-ip> here\n")], &[], "add b"); // pii-test-fixture
        let r2 = mgr.run().unwrap();
        assert!(r2.committed);
        assert!(r2.tagged);
        let b = std::fs::read_to_string(wd.join("b.txt")).unwrap();
        assert!(!b.contains("<internal-ip>"), "new file's IP swept"); // pii-test-fixture
        assert_eq!(commit_count(&wd), 2);

        cleanup(&[&src, &wd]);
    }

    // ── finalize after residual cleanup commits without re-syncing (P2) ───────
    #[test]
    #[serial]
    fn finalize_after_cleanup_does_not_resync() {
        clear_env();
        // Run 1: internal main is dirty (residual secret) → uncommitted, no tag.
        let src = init_source(&[(
            "config.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let r1 = mgr.run().unwrap();
        assert!(!r1.committed && !r1.tagged && !r1.residual_violations.is_empty());

        // Simulate GHMR-05 cleaning the residual spot IN THE WORK DIR (source is
        // untouched — internal main still carries the secret).
        write_file(&wd, "config.txt", "token cleaned by ghmr-05\n");

        // finalize() must NOT re-sync (which would re-archive the still-dirty
        // source and clobber the cleanup); it processes the current work-dir tree.
        let r2 = mgr.finalize(&sha).unwrap();
        assert!(r2.committed, "cleaned tree is committed");
        assert!(r2.tagged, "cleaned tree is tagged for the same internal sha");
        assert!(r2.residual_violations.is_empty());
        assert_eq!(r2.tag.as_deref(), Some(approved_tag(&sha).as_str()));
        // The committed work-dir content is the CLEANED version, not the source's.
        assert_eq!(std::fs::read_to_string(wd.join("config.txt")).unwrap(), "token cleaned by ghmr-05\n");
        // Source repo was never modified by the cleaning.
        let src_cfg = std::fs::read_to_string(src.join("config.txt")).unwrap();
        assert!(src_cfg.contains("ghp_"), "source repo untouched"); // pii-test-fixture

        cleanup(&[&src, &wd]);
    }

    // ── active placeholder config is NOT committed into the approved tree ─────
    // (codex P1) The sweep exempts the active `mirror-placeholders.toml` from
    // rewriting + residual because it legitimately holds the REAL infra values its
    // matchers map. If that exempt file were committed, an otherwise-clean approval
    // would ship those raw literals in public history. It must be dropped from the
    // approved mirror tree.
    #[test]
    #[serial]
    fn active_placeholder_config_excluded_from_approved_commit() {
        clear_env();
        // Internal main ships a placeholder config whose matcher embeds a real LAN
        // IP, plus a doc that references it (mechanically swept to the token).
        let src = init_source(&[
            (
                "mirror-placeholders.toml",
                "[[placeholder]]\npattern = '10\\.10\\.0\\.9'\ntoken = \"<REDACTED_LAN_IP>\"\n",
            ),
            ("doc.txt", "reaches <internal-ip> internally\n"), // pii-test-fixture
        ]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let report = mgr.run().unwrap();

        // Tree is clean (config exempt, doc swept) → committed + tagged.
        assert!(report.committed, "clean tree committed");
        assert!(report.tagged, "clean tree tagged");
        assert!(report.residual_violations.is_empty());

        // The config file must NOT be present in the committed work-dir tree (nor
        // on disk after the run) — otherwise its raw private-IP matcher would ship.
        let tracked = run_git(&wd, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(
            !tracked.lines().any(|l| l.trim() == "mirror-placeholders.toml"),
            "active placeholder config must not be in the approved tree: {tracked}"
        );
        assert!(
            !wd.join("mirror-placeholders.toml").exists(),
            "config dropped from the work dir on commit"
        );
        // The real value never reaches the committed HEAD via any file.
        let head_blob = run_git(&wd, &["grep", "-l", "<internal-ip>", "HEAD"]); // pii-test-fixture
        assert!(
            head_blob.is_err() || head_blob.as_deref().map(str::trim).unwrap_or("").is_empty(),
            "no committed file may contain the raw config value: {head_blob:?}"
        );
        // The doc IS mirrored, with its IP swept to the token.
        assert!(tracked.lines().any(|l| l.trim() == "doc.txt"), "doc mirrored");
        assert!(!wd.join("doc.txt").exists() || {
            let d = std::fs::read_to_string(wd.join("doc.txt")).unwrap();
            !d.contains("<internal-ip>") // pii-test-fixture
        });
        // Source repo keeps its config untouched.
        assert!(src.join("mirror-placeholders.toml").exists(), "source config untouched");

        cleanup(&[&src, &wd]);
    }

    // ── active PII gate config is NOT committed into the approved tree ────────
    // (codex round 2 P1) `pii-gate.toml` catalogs the REAL private matcher values
    // (extra_terms/extra_patterns) and the gate excludes its OWN config from
    // scanning, so — like the placeholder config — it must be dropped from the
    // approved mirror tree or its raw literals ship into public history.
    #[test]
    #[serial]
    fn active_gate_config_excluded_from_approved_commit() {
        clear_env();
        // Internal main ships a pii-gate.toml cataloging a private internal host,
        // plus otherwise-clean content.
        let src = init_source(&[
            ("pii-gate.toml", "extra_terms = [\"acme-internal-vault-01\"]\n"),
            ("doc.txt", "nothing sensitive here\n"),
        ]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let report = mgr.run().unwrap();

        assert!(report.committed && report.tagged, "clean tree approved");
        assert!(report.residual_violations.is_empty());

        let tracked = run_git(&wd, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(
            !tracked.lines().any(|l| l.trim() == "pii-gate.toml"),
            "gate config must not be in the approved tree: {tracked}"
        );
        assert!(!wd.join("pii-gate.toml").exists(), "gate config dropped from the work dir");
        // The private term never reaches any committed blob.
        let head_grep = run_git(&wd, &["grep", "-l", "acme-internal-vault-01", "HEAD"]);
        assert!(
            head_grep.is_err() || head_grep.as_deref().map(str::trim).unwrap_or("").is_empty(),
            "no committed file may contain the raw gate-config term: {head_grep:?}"
        );
        // The clean doc IS still mirrored (a gate-excluded matcher config is dropped,
        // ordinary content is not).
        assert!(tracked.lines().any(|l| l.trim() == "doc.txt"), "doc mirrored");
        assert!(src.join("pii-gate.toml").exists(), "source gate config untouched");

        cleanup(&[&src, &wd]);
    }

    // ── empty TERMINUS_PII_CONFIG still drops the local gate config (round 4 P1)
    // A set-but-empty env value makes `ruleset_from_config` fall back to defaults,
    // which still base-name-exclude `pii-gate.toml` from scanning — so the drop
    // logic must catch the local file even when the env resolver contributes
    // nothing.
    #[test]
    #[serial]
    fn empty_pii_config_env_still_drops_local_gate_config() {
        clear_env();
        std::env::set_var("TERMINUS_PII_CONFIG", ""); // set-but-empty
        let src = init_source(&[
            ("pii-gate.toml", "extra_terms = [\"acme-internal-vault-02\"]\n"),
            ("doc.txt", "clean\n"),
        ]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let report = mgr.run().unwrap();
        std::env::remove_var("TERMINUS_PII_CONFIG");

        assert!(report.committed && report.tagged, "clean tree approved");
        let tracked = run_git(&wd, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(
            !tracked.lines().any(|l| l.trim() == "pii-gate.toml"),
            "empty TERMINUS_PII_CONFIG must still drop the local gate config: {tracked}"
        );
        assert!(!wd.join("pii-gate.toml").exists(), "local gate config dropped from work dir");
        let head_grep = run_git(&wd, &["grep", "-l", "acme-internal-vault-02", "HEAD"]);
        assert!(
            head_grep.is_err() || head_grep.as_deref().map(str::trim).unwrap_or("").is_empty(),
            "no committed blob may carry the gate-config term: {head_grep:?}"
        );

        cleanup(&[&src, &wd]);
    }

    // ── a NESTED pii-gate.toml is also dropped from the approved tree (round 5 P1)
    // is_excluded matches by base-name at ANY depth, so a nested gate config is
    // unscanned too and must not ship.
    #[test]
    #[serial]
    fn nested_gate_config_excluded_from_approved_commit() {
        clear_env();
        let src = init_source(&[
            ("sub/pii-gate.toml", "extra_terms = [\"acme-nested-secret-host\"]\n"),
            ("readme.md", "ordinary content\n"),
        ]);
        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let report = mgr.run().unwrap();

        assert!(report.committed && report.tagged, "clean tree approved");
        let tracked = run_git(&wd, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(
            !tracked.lines().any(|l| l.trim() == "sub/pii-gate.toml"),
            "nested gate config must not be in the approved tree: {tracked}"
        );
        assert!(!wd.join("sub/pii-gate.toml").exists(), "nested gate config dropped from work dir");
        let head_grep = run_git(&wd, &["grep", "-l", "acme-nested-secret-host", "HEAD"]);
        assert!(
            head_grep.is_err() || head_grep.as_deref().map(str::trim).unwrap_or("").is_empty(),
            "no committed blob may carry the nested gate-config term: {head_grep:?}"
        );
        assert!(tracked.lines().any(|l| l.trim() == "readme.md"), "ordinary content mirrored");

        cleanup(&[&src, &wd]);
    }

    // ── source / work-dir path overlap is rejected before any mutation (round 5 P1)
    #[test]
    #[serial]
    fn overlapping_source_and_workdir_paths_are_rejected() {
        clear_env();
        let src = init_source(&[("f.txt", "content\n")]);

        // work_dir == source: clearing the work dir would delete the source checkout.
        let same = MirrorWorkDir::new("Terminus", &src, &src);
        assert!(
            matches!(same.run(), Err(ToolError::InvalidArgument(_))),
            "equal source/work_dir must be rejected"
        );
        // Source still intact — no mutation happened.
        assert!(src.join(".git").exists() && src.join("f.txt").exists(), "source untouched");

        // work_dir nested INSIDE source.
        let nested_wd = src.join("mirror-out");
        let wd_in_src = MirrorWorkDir::new("Terminus", &src, &nested_wd);
        assert!(
            matches!(wd_in_src.run(), Err(ToolError::InvalidArgument(_))),
            "work_dir nested in source must be rejected"
        );

        // source nested INSIDE work_dir.
        let outer_wd = unique("outer");
        std::fs::create_dir_all(&outer_wd).unwrap();
        let inner_src = init_source(&[("g.txt", "content\n")]);
        // Move inner_src under outer_wd to make source a descendant of work_dir.
        let src_under_wd = outer_wd.join("inner-src");
        std::fs::rename(&inner_src, &src_under_wd).unwrap();
        let src_in_wd = MirrorWorkDir::new("Terminus", &src_under_wd, &outer_wd);
        assert!(
            matches!(src_in_wd.run(), Err(ToolError::InvalidArgument(_))),
            "source nested in work_dir must be rejected"
        );

        cleanup(&[&src, &outer_wd]);
    }

    // ── empty source repo → valid approved empty snapshot (P3) ────────────────
    #[test]
    #[serial]
    fn empty_source_repo_produces_approved_empty_snapshot() {
        clear_env();
        // A valid repo with a commit but NO files at HEAD.
        let src = unique("src");
        std::fs::create_dir_all(&src).unwrap();
        run_git(&src, &["init", "-q", "-b", "main"]).unwrap();
        run_git(
            &src,
            &[
                "-c",
                "user.name=src",
                "-c",
                "user.email=<email>", // pii-test-fixture
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "empty",
            ],
        )
        .unwrap();

        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let report = mgr.run().unwrap();
        assert!(report.committed, "empty first sync still yields a commit");
        assert!(report.tagged, "empty clean snapshot is approved");
        assert!(report.commit_sha.is_some(), "HEAD exists");
        // The work dir has a HEAD and exactly one commit.
        assert_eq!(commit_count(&wd), 1);

        cleanup(&[&src, &wd]);
    }

    // ── untracked / uncommitted source files are NOT mirrored (P1) ────────────
    #[test]
    #[serial]
    fn untracked_source_files_are_not_mirrored() {
        clear_env();
        let src = init_source(&[("tracked.txt", "committed clean\n")]);
        // Commit a .gitignore first (before any untracked file exists, so the
        // add -A in commit_source cannot accidentally stage them).
        write_file(&src, ".gitignore", "ignored.txt\n");
        commit_source(&src, &[], &[], "add gitignore");
        // NOW create an untracked file + a .gitignore'd file, left UNcommitted —
        // present in the checkout but not in HEAD, so they must never reach the
        // mirror derivative (which is built from `git archive <sha>`).
        write_file(&src, "UNTRACKED.txt", "should not be mirrored\n");
        write_file(&src, "ignored.txt", "also not mirrored\n");

        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let report = mgr.run().unwrap();
        assert!(report.tagged);
        assert!(wd.join("tracked.txt").exists(), "committed file mirrored");
        assert!(
            !wd.join("UNTRACKED.txt").exists(),
            "untracked file must NOT be mirrored"
        );
        assert!(
            !wd.join("ignored.txt").exists(),
            "gitignored file must NOT be mirrored"
        );
        cleanup(&[&src, &wd]);
    }

    // ── tracked symlinks are preserved through the sync (P2) ──────────────────
    #[test]
    #[serial]
    #[cfg(unix)]
    fn tracked_symlink_is_preserved() {
        clear_env();
        let src = init_source(&[("target.txt", "link target clean\n")]);
        // Add a tracked symlink and commit it.
        std::os::unix::fs::symlink("target.txt", src.join("link.txt")).unwrap();
        commit_source(&src, &[], &[], "add symlink");

        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        mgr.run().unwrap();

        let link = wd.join("link.txt");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "tracked symlink must survive the sync as a symlink"
        );
        // git records it as a symlink (mode 120000).
        let ls = run_git(&wd, &["ls-files", "-s", "link.txt"]).unwrap();
        assert!(ls.starts_with("120000"), "symlink stored with mode 120000: {ls}");
        cleanup(&[&src, &wd]);
    }

    // ── a symlink whose target embeds PII blocks approval (codex round 2 P1) ──
    // git archive preserves a tracked symlink as a blob whose content IS the raw
    // target path; the sweep + gate skip symlink bodies, so a PII-bearing target
    // must be caught as residual, or it ships unscanned into an approved commit.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn symlink_target_with_pii_blocks_approval() {
        clear_env();
        let src = init_source(&[("readme.txt", "clean content\n")]);
        // Tracked symlink whose TARGET embeds a private infra IP.
        std::os::unix::fs::symlink("/mnt/<internal-ip>/share", src.join("data")).unwrap(); // pii-test-fixture
        commit_source(&src, &[], &[], "add pii symlink");

        let wd = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd);
        let report = mgr.run().unwrap();

        assert!(
            report.residual_violations.iter().any(|v| v.file == "data"),
            "symlink target PII must surface as residual: {:?}",
            report.residual_violations
        );
        assert!(!report.committed, "dirty symlink target blocks the approval commit");
        assert!(!report.tagged, "no mirror-approved tag while a symlink target leaks PII");
        assert!(tag_list(&wd).is_empty(), "no approval tag created");
        cleanup(&[&src, &wd]);
    }

    // ── force/hard tokens are structurally rejected ──────────────────────────
    #[test]
    #[should_panic(expected = "force/hard token")]
    fn force_token_panics() {
        assert_never_force(&["push", "--force"]);
    }

    #[test]
    #[should_panic(expected = "force/hard token")]
    fn hard_reset_token_panics() {
        assert_never_force(&["reset", "--hard", "HEAD~1"]);
    }
}
