//! GHMR-05 — operationalized cleaning of residual (non-mechanical) violations.
//!
//! GHMR-02's mechanical sweep rewrites the deterministically-fixable PII (private
//! IPs, container IDs, config-mapped hosts) into placeholder tokens, but leaves
//! **residual** violations that need judgment — a raw leaked secret, prose that
//! embeds an infra fact, an ambiguous string that must be restructured rather than
//! token-substituted. GHMR-03's [`MirrorWorkDir::run`] therefore refuses to commit
//! or tag such a tree (a dirty commit would become a permanent, force-free-
//! un-excisable ancestor of a later approved commit) and returns those residuals.
//!
//! This module turns "hand the residuals to a subagent" into a **repeatable,
//! bounded harness step** invoked by `git_public_mirror_prepare` (GHMR-04) whenever
//! residuals remain — not an ad hoc one-off:
//!
//!   1. Dispatch a scoped [`ResidualCleaner`] that remediates the flagged spots
//!      **in the clean work dir only** (judgment placeholdering / prose
//!      restructuring). The source repo is never handed to the cleaner and is
//!      never touched.
//!   2. Re-run the sweep + GHMR-01 gate via [`MirrorWorkDir::finalize`] (which
//!      processes the CURRENT work-dir tree — it never re-syncs from source, so it
//!      cannot clobber the cleanup). If the gate is now 0, `finalize` commits +
//!      tags the cleaned tree and the pass returns [`CleaningOutcome::Cleaned`].
//!   3. Otherwise repeat, up to [`MAX_CLEAN_ROUNDS`] (the infinite-loop guard),
//!      also stopping early if a round makes **no progress** (the residual set is
//!      unchanged — the cleaner is stuck). On exhaustion, escalate the exact spots
//!      (`file:line`) to the operator via [`CleaningOutcome::Escalated`]; nothing
//!      is committed or tagged.
//!
//! ## The cleaning-subagent contract ([`ResidualCleaner`])
//! A cleaner is handed **only** the work-dir path and the residual list
//! (`{file, line, pattern_kind, context}` — the context is a redacted snippet, the
//! full secret is never stored). It must edit files under that work dir to remove
//! the flagged PII and return `Ok(())` when its round of edits is done (or an error
//! if it cannot proceed). It must NOT touch anything outside the work dir. The
//! orchestration re-verifies with the authoritative gate after every round, so a
//! cleaner that lies about success simply fails to drive the gate to 0 and is
//! escalated — it can never smuggle residual PII into an approved tag.
//!
//! ## Trust boundary / defense-in-depth
//! The cleaner is OPERATOR-CONFIGURED (via [`CLEAN_CMD_ENV`]), not arbitrary
//! network input, but it is treated as only semi-trusted and confined by several
//! layers: (1) it is handed ONLY the work-dir path + a redacted residual list,
//! never the source; (2) it runs with a **cleared environment** (only `PATH`,
//! `HOME`, and the two `MIRROR_*` handoff vars — no service credentials); and
//! (3) the work dir's `.git` metadata is **snapshotted and restored around every
//! cleaner round** ([`with_protected_git_dir`]), so nothing a cleaner writes under
//! `.git` survives into `finalize` — a planted `.git/hooks/*`, a `.git/config`
//! `core.worktree` redirect that would make `git add`/`commit` approve an UNSCANNED
//! external tree, or clean/smudge filters / `gpg.program` that would run under
//! finalize are all discarded; only work-TREE edits persist, and those are re-
//! scanned by the gate. (All mirror-engine git ops ALSO disable hooks on the
//! command line via `core.hooksPath=/dev/null` in [`workdir`](super::workdir) as a
//! second layer.) A full filesystem sandbox (preventing absolute-path writes
//! OUTSIDE the work dir) is the operator's deployment responsibility for the
//! configured command — out of scope for this in-process orchestration.
//!
//! ## The security boundary is the operator's OS sandbox (authoritative)
//! The in-process measures above (redacted inputs only, cleared env, in-memory
//! `.git` snapshot/restore, command-line hook disabling, own-process-group +
//! group-kill of the cleaner) are **defense-in-depth** — they contain buggy or
//! casually-hostile cleaners. They do NOT, and structurally CANNOT, fully contain a
//! cleaner that executes arbitrary local code: such a cleaner can write to arbitrary
//! ABSOLUTE paths (including the source checkout), or double-fork into a new session
//! to escape its process group and race `.git`/the work tree during finalize. There
//! is no in-process defense against arbitrary local code execution — that is a
//! fundamental property, not a fixable bug here. **The configured cleaning command
//! MUST therefore be run under an OS filesystem/process sandbox (bwrap / nsjail /
//! container with the work dir bind-mounted and nothing else, no network, killed as
//! a unit), which is the operator's deployment responsibility and the real trust
//! boundary.** This module enforces the git-metadata-integrity property to the
//! extent an in-process orchestration can and defers the rest to that sandbox by
//! design.
//!
//! In production the cleaner is dispatched through a config-driven command hook
//! ([`CommandCleaner`], from [`CLEAN_CMD_ENV`]) with that cleared environment.
//! Tests inject a mock. When no
//! command is configured, [`dispatch_cleaning`] escalates immediately (0 rounds)
//! rather than silently passing residuals through.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::error::ToolError;
use crate::github::pii::TreeViolation;

use super::workdir::{MirrorWorkDir, WorkDirRunReport};

/// Maximum cleaning rounds per prepare — the bounded infinite-loop guard. The spec
/// caps this at 3; a cleaner that cannot reach 0 within this many rounds escalates.
pub const MAX_CLEAN_ROUNDS: u32 = 3;

/// Env var naming the command that dispatches the scoped cleaning subagent. The
/// command is run once per round with `MIRROR_WORK_DIR` (the clean work dir it may
/// edit) and `MIRROR_RESIDUALS_FILE` (a JSON file of the residual spots) in its
/// environment, and its cwd set to the work dir. NEVER a literal in code — the
/// dispatch mechanism is operator/harness configuration, and it is what invokes
/// the actual cleaning subagent on the dev box.
pub const CLEAN_CMD_ENV: &str = "TERMINUS_MIRROR_CLEAN_CMD";

// ── The cleaning-subagent contract ──────────────────────────────────────────

/// A scoped remediator for residual (non-mechanical) violations. Implementors edit
/// files **inside `work_dir` only** to remove the flagged PII; the orchestration
/// re-verifies with the authoritative gate after each round, so correctness is
/// enforced regardless of what the cleaner claims.
pub trait ResidualCleaner {
    /// Remediate the flagged spots in `work_dir`. Returns `Ok(())` when this round
    /// of edits is complete (the gate is re-run by the caller afterwards), or an
    /// error if the cleaner cannot proceed at all.
    fn clean_round(&self, work_dir: &Path, residuals: &[TreeViolation]) -> Result<(), ToolError>;

    /// A short label for reports/logging.
    fn label(&self) -> &str {
        "cleaner"
    }
}

/// Production cleaner: dispatch a configured command hook (the scoped cleaning
/// subagent). Config-driven — no hardcoded dispatch in code.
pub struct CommandCleaner {
    cmd: String,
}

impl CommandCleaner {
    /// Build from [`CLEAN_CMD_ENV`], or `None` when it is unset/empty (→ the caller
    /// escalates rather than dispatching).
    pub fn from_env() -> Option<Self> {
        std::env::var(CLEAN_CMD_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|cmd| Self { cmd })
    }
}

impl ResidualCleaner for CommandCleaner {
    fn clean_round(&self, work_dir: &Path, residuals: &[TreeViolation]) -> Result<(), ToolError> {
        // Resolve the work dir to an ABSOLUTE path before handing it to the child.
        // TERMINUS_MIRROR_WORKDIR_ROOT may be relative, so `work_dir` can be too;
        // once it is the child's cwd, a relative `MIRROR_WORK_DIR` would make a
        // contract-following cleaner (`$MIRROR_WORK_DIR/<file>`) resolve BENEATH the
        // work dir (cwd/work_dir/…) rather than at the file. Canonicalising fixes
        // both cwd and the env var (and resolves symlinks). The work dir exists here
        // (prepare's `run` created it before any residual), so canonicalize succeeds.
        let work_dir = work_dir.canonicalize().unwrap_or_else(|_| work_dir.to_path_buf());
        let work_dir = work_dir.as_path();
        // Serialize the residual spots to a temp JSON file the command reads. The
        // context snippets are already redacted by the gate; no full secret is
        // written here.
        let payload = json!({
            "work_dir": work_dir.display().to_string(),
            "residual_violations": residuals.iter().map(|v| json!({
                "file": v.file,
                "line": v.line,
                "pattern_kind": v.pattern_kind,
                "context": v.context,
            })).collect::<Vec<_>>(),
        });
        let residuals_file = std::env::temp_dir().join(format!(
            "ghmr05-residuals-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&residuals_file, payload.to_string()).map_err(|e| {
            ToolError::Execution(format!("failed writing residuals file for cleaning command: {e}"))
        })?;

        // Least-privilege: CLEAR the inherited environment before launching the
        // cleaner. The parent (terminus / the dev-box mirror invocation) holds
        // service credentials — GITHUB_TOKEN, PLANE_PAT_*, DATABASE_URL — that an
        // external cleaning subagent has no business seeing; inheriting them would
        // contradict the scoped-cleaner contract and leak unrelated secrets. Pass
        // only what the hook legitimately needs: PATH + HOME (so the shell resolves
        // binaries / a home-relative toolchain — neither is a secret) and the two
        // MIRROR_* handoff vars.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&self.cmd)
            .current_dir(work_dir)
            .env_clear()
            .env("MIRROR_WORK_DIR", work_dir)
            .env("MIRROR_RESIDUALS_FILE", &residuals_file);
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        if let Ok(home) = std::env::var("HOME") {
            cmd.env("HOME", home);
        }

        // On unix, run the cleaner in its OWN process group and reap any surviving
        // descendants when it returns. A cleaner could otherwise fork a background
        // process that outlives the shell (`Command::wait` reaps only the shell) and
        // re-tamper with `.git` AFTER the caller restores it. Putting the child in a
        // fresh group (pgid == child pid) and `killpg(pgid, SIGKILL)`-ing that group
        // after it exits kills such forked descendants before `.git` is restored and
        // finalize runs. (A cleaner that deliberately double-forks into a NEW SESSION
        // to escape its process group, or writes to arbitrary absolute paths, is
        // beyond any in-process measure and is contained only by the operator's OS
        // sandbox — the documented security boundary on this module.)
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            match cmd.spawn() {
                Ok(mut child) => {
                    let pgid = child.id() as libc::pid_t;
                    let st = child.wait();
                    // Terminate the whole group via a direct killpg(2) syscall — no
                    // reliance on an external `pkill` binary (absent in minimal
                    // containers). The group leader (the shell) has already exited;
                    // this reaches any forked children still alive. ESRCH (empty
                    // group) is a harmless no-op. Safe: killpg takes plain integers
                    // and has no memory effects.
                    unsafe {
                        libc::killpg(pgid, libc::SIGKILL);
                    }
                    st
                }
                Err(e) => Err(e),
            }
        };
        #[cfg(not(unix))]
        let status = cmd.status();

        let _ = std::fs::remove_file(&residuals_file);

        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(ToolError::Execution(format!(
                "cleaning command exited with status {s} (dispatched via {CLEAN_CMD_ENV})"
            ))),
            Err(e) => Err(ToolError::Execution(format!("failed to spawn cleaning command: {e}"))),
        }
    }

    fn label(&self) -> &str {
        "command"
    }
}

// ── Outcome ─────────────────────────────────────────────────────────────────

/// The result of a bounded cleaning pass.
#[derive(Debug)]
pub enum CleaningOutcome {
    /// The gate was driven to 0 residual violations; the cleaned tree was committed
    /// and tagged. Carries GHMR-03's finalize report (now `approved`) + the round
    /// count it took.
    Cleaned {
        report: WorkDirRunReport,
        rounds_used: u32,
    },
    /// Residual violations remained after the bounded rounds (or no cleaner was
    /// configured). NOTHING was committed or tagged; the exact spots are escalated
    /// to the operator.
    Escalated {
        repo: String,
        internal_sha: String,
        rounds_used: u32,
        residual_violations: Vec<TreeViolation>,
        reason: String,
    },
}

impl CleaningOutcome {
    /// Whether the pass ended clean, committed, and tagged.
    pub fn is_cleaned(&self) -> bool {
        matches!(self, CleaningOutcome::Cleaned { .. })
    }

    /// Stable machine-readable JSON for `git_public_mirror_prepare`.
    pub fn to_json(&self) -> Value {
        match self {
            CleaningOutcome::Cleaned { report, rounds_used } => {
                let mut v = report.to_json();
                v["cleaning"] = json!({
                    "attempted": true,
                    "cleaned": true,
                    "escalated": false,
                    "rounds_used": rounds_used,
                });
                v
            }
            CleaningOutcome::Escalated {
                repo,
                internal_sha,
                rounds_used,
                residual_violations,
                reason,
            } => json!({
                "repo": repo,
                "internal_sha": internal_sha,
                "approved": false,
                "tagged": false,
                "residual_count": residual_violations.len(),
                "residual_violations": residual_violations.iter().map(|v| json!({
                    "file": v.file,
                    "line": v.line,
                    "pattern_kind": v.pattern_kind,
                    "context": v.context,
                })).collect::<Vec<_>>(),
                "cleaning": {
                    "attempted": true,
                    "cleaned": false,
                    "escalated": true,
                    "rounds_used": rounds_used,
                    "reason": reason,
                    // Exact spots for the operator to remediate by hand.
                    "escalation_spots": residual_violations.iter()
                        .map(|v| format!("{}:{}", v.file, v.line))
                        .collect::<Vec<_>>(),
                },
            }),
        }
    }
}

// ── Orchestration ───────────────────────────────────────────────────────────

/// Run the bounded cleaning loop over the work dir with an explicit cleaner (the
/// testable core). `initial_residuals` are the violations GHMR-03's `run` returned;
/// `internal_sha` is the sha that `run` synced (so `finalize` tags for the SAME
/// internal commit without re-syncing). `max_rounds` is clamped to at least 1 and
/// at most [`MAX_CLEAN_ROUNDS`].
///
/// Source isolation: this only ever calls into `wd.finalize` (which processes the
/// work-dir tree, never the source) and hands the cleaner only `wd.path()`. The
/// source repo is never a parameter to any step here.
pub fn run_cleaning_pass(
    wd: &MirrorWorkDir,
    internal_sha: &str,
    initial_residuals: Vec<TreeViolation>,
    cleaner: &dyn ResidualCleaner,
    max_rounds: u32,
) -> Result<CleaningOutcome, ToolError> {
    // Nothing to clean → finalize to surface the already-clean approved state.
    if initial_residuals.is_empty() {
        let report = wd.finalize(internal_sha)?;
        if report.residual_violations.is_empty() {
            return Ok(CleaningOutcome::Cleaned { report, rounds_used: 0 });
        }
        // Defensive: a caller said "clean" but the gate disagrees — treat as
        // residual and fall through to cleaning.
        return run_cleaning_pass(
            wd,
            internal_sha,
            report.residual_violations,
            cleaner,
            max_rounds,
        );
    }

    let rounds = max_rounds.clamp(1, MAX_CLEAN_ROUNDS);
    let mut residuals = initial_residuals;

    for round in 1..=rounds {
        // Dispatch one cleaning round INTO THE WORK DIR ONLY, with the `.git`
        // metadata PROTECTED: it is snapshotted before the cleaner runs and
        // restored after, so nothing the cleaner writes under `.git` survives into
        // finalize. This closes the whole class of git-metadata attacks a
        // work-dir-writable cleaner could otherwise mount — a planted
        // `.git/hooks/*`, a `.git/config` `core.worktree` redirect that would make
        // `git add`/`commit` approve an UNSCANNED external tree, or executable
        // clean/smudge filters / `gpg.program` that run under finalize. Only the
        // cleaner's work-TREE edits persist, and those are re-scanned by the gate
        // below, preserving the "nothing is committed that the gate did not scan"
        // guarantee. (`.git` command-line hardening in `run_git` remains a second
        // layer.)
        with_protected_git_dir(wd.path(), || cleaner.clean_round(wd.path(), &residuals))?;

        // Re-sweep + authoritative gate on the current (cleaned) work-dir tree.
        // finalize commits + tags iff the gate is now 0; it never re-syncs from
        // source, so it cannot clobber the cleaner's edits.
        let report = wd.finalize(internal_sha)?;
        if report.residual_violations.is_empty() {
            return Ok(CleaningOutcome::Cleaned { report, rounds_used: round });
        }

        // Infinite-loop / no-progress guard: if a round leaves the residual set
        // byte-identical, the cleaner is stuck — escalate now rather than burn the
        // remaining rounds re-running an unproductive step.
        let next = report.residual_violations;
        if next == residuals {
            return Ok(CleaningOutcome::Escalated {
                repo: wd.repo().to_string(),
                internal_sha: internal_sha.to_string(),
                rounds_used: round,
                residual_violations: next,
                reason: format!(
                    "cleaning made no progress on round {round}/{rounds} \
                     (residual set unchanged) — escalating to the operator"
                ),
            });
        }
        residuals = next;
    }

    // Bounded rounds exhausted with residuals remaining → escalate exact spots.
    Ok(CleaningOutcome::Escalated {
        repo: wd.repo().to_string(),
        internal_sha: internal_sha.to_string(),
        rounds_used: rounds,
        residual_violations: residuals,
        reason: format!(
            "residual PII remained after {rounds} cleaning round(s) — escalating to the operator"
        ),
    })
}

/// Prepare-time entry point: given GHMR-03's `run` report that still carries
/// residual violations, either dispatch the configured cleaning command
/// ([`CommandCleaner`]) through the bounded pass, or — when no command is
/// configured — escalate the residual spots to the operator immediately (0 rounds).
/// Never silently passes residuals through.
pub fn dispatch_cleaning(
    wd: &MirrorWorkDir,
    report: &WorkDirRunReport,
) -> Result<CleaningOutcome, ToolError> {
    let residuals = report.residual_violations.clone();
    match CommandCleaner::from_env() {
        Some(cleaner) => {
            run_cleaning_pass(wd, &report.internal_sha, residuals, &cleaner, MAX_CLEAN_ROUNDS)
        }
        None => Ok(CleaningOutcome::Escalated {
            repo: report.repo.clone(),
            internal_sha: report.internal_sha.clone(),
            rounds_used: 0,
            residual_violations: residuals,
            reason: format!(
                "residual PII violations remain and no cleaning command is configured \
                 ({CLEAN_CMD_ENV} unset). Configure the cleaning-subagent command so the bounded \
                 pass runs INSIDE prepare (its work-dir edits survive via finalize), or remediate \
                 the flagged spots at their source (internal main / the placeholder+gate config) \
                 and re-run git_public_mirror_prepare. NOTE: hand-editing the work dir and re-running \
                 prepare does NOT work — prepare re-syncs internal main's tree and discards those \
                 edits; only the in-prepare cleaning pass (which finalizes without re-syncing) or \
                 a source-side fix is a valid remediation path."
            ),
        }),
    }
}

// ── `.git` protection around a cleaner round ────────────────────────────────

/// One captured `.git` entry, held IN MEMORY (see [`with_protected_git_dir`]).
enum GitEntry {
    Dir,
    File { bytes: Vec<u8>, mode: u32 },
    Symlink { target: PathBuf },
}

/// Run `f` (a cleaner round) with the work dir's `.git` metadata protected: capture
/// it into an **in-memory** snapshot first, then rebuild `.git` from that snapshot
/// afterward regardless of `f`'s outcome. The cleaner can therefore only affect
/// work-TREE files; nothing it writes under `.git` (hooks, a `core.worktree`
/// redirect, clean/smudge filters, `gpg.program`) survives into the later
/// `finalize` that trusts that metadata.
///
/// The snapshot lives in process memory — NOT on disk — precisely because a cleaner
/// is not confined to its cwd: an on-disk snapshot (even a sibling temp dir) could
/// be located and tampered with by a filesystem-writing cleaner before the restore,
/// whereas an in-memory copy is unreachable by any filesystem operation. The work
/// dir's `.git` is a small swept linear history, so holding it in memory briefly is
/// cheap. A missing `.git` (defensive; prepare's `run` always creates it before any
/// residual) skips protection and just runs `f`.
///
/// NOTE: this enforces GIT-METADATA integrity in-process; it does not stop a cleaner
/// from writing to unrelated ABSOLUTE paths (e.g. the source checkout) — that
/// requires an OS filesystem sandbox around the configured command, which is the
/// operator's deployment responsibility (documented on the module).
fn with_protected_git_dir<F>(work_dir: &Path, f: F) -> Result<(), ToolError>
where
    F: FnOnce() -> Result<(), ToolError>,
{
    let git_dir = work_dir.join(".git");
    if !git_dir.exists() {
        return f();
    }
    let snapshot = snapshot_git_dir(&git_dir)
        .map_err(|e| ToolError::Execution(format!("failed snapshotting .git before cleaning: {e}")))?;

    let result = f();

    // Rebuild the pristine `.git` from the in-memory snapshot regardless of the
    // cleaner's result and regardless of what it did to the live `.git` (deleted,
    // replaced with a file, tampered).
    restore_git_dir(&git_dir, &snapshot)
        .map_err(|e| ToolError::Execution(format!("failed restoring pristine .git after cleaning: {e}")))?;

    result
}

/// Read an entire `.git` tree into an in-memory snapshot (relative path → entry).
/// Directories precede their contents (so restore creates parents first). Symlinks
/// are captured as their target, never followed.
fn snapshot_git_dir(git_dir: &Path) -> std::io::Result<Vec<(PathBuf, GitEntry)>> {
    fn walk(base: &Path, cur: &Path, out: &mut Vec<(PathBuf, GitEntry)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(cur)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                let target = std::fs::read_link(&path)?;
                out.push((rel, GitEntry::Symlink { target }));
            } else if ft.is_dir() {
                out.push((rel, GitEntry::Dir));
                walk(base, &path, out)?;
            } else {
                let bytes = std::fs::read(&path)?;
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::MetadataExt;
                    entry.metadata()?.mode()
                };
                #[cfg(not(unix))]
                let mode = 0o644;
                out.push((rel, GitEntry::File { bytes, mode }));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(git_dir, git_dir, &mut out)?;
    Ok(out)
}

/// Rebuild `.git` from an in-memory snapshot. Robustly removes whatever the cleaner
/// left at `git_dir` first — a directory, a file/symlink it was replaced with, or
/// nothing at all (`NotFound` is fine) — then recreates every captured entry.
fn restore_git_dir(git_dir: &Path, snapshot: &[(PathBuf, GitEntry)]) -> std::io::Result<()> {
    match std::fs::symlink_metadata(git_dir) {
        Ok(md) => {
            if md.file_type().is_dir() {
                std::fs::remove_dir_all(git_dir)?;
            } else {
                std::fs::remove_file(git_dir)?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(git_dir)?;
    for (rel, entry) in snapshot {
        let dst = git_dir.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match entry {
            GitEntry::Dir => {
                std::fs::create_dir_all(&dst)?;
            }
            GitEntry::Symlink { target } => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &dst)?;
                #[cfg(not(unix))]
                std::fs::write(&dst, target.to_string_lossy().as_bytes())?;
            }
            GitEntry::File { bytes, mode } => {
                std::fs::write(&dst, bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(*mode))?;
                }
                #[cfg(not(unix))]
                let _ = mode;
            }
        }
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::cell::Cell;
    use std::path::PathBuf;

    use super::super::workdir::{approved_tag, run_git, MirrorWorkDir};

    fn unique(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ghmr05-{tag}-{}-{}",
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
        std::fs::write(&path, content).unwrap();
    }

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

    fn clear_env() {
        std::env::remove_var("TERMINUS_MIRROR_PLACEHOLDERS");
        std::env::remove_var("TERMINUS_PII_CONFIG");
        std::env::remove_var("GITHUB_ALLOWED_AUTHORS");
        std::env::remove_var("TERMINUS_MIRROR_WORKDIR_ROOT");
        std::env::remove_var(CLEAN_CMD_ENV);
    }

    fn cleanup(paths: &[&Path]) {
        for p in paths {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    fn tag_list(dir: &Path) -> Vec<String> {
        run_git(dir, &["tag", "-l"])
            .unwrap()
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// A cleaner that overwrites a target file with clean content — the happy path
    /// (a scoped subagent remediating the flagged spot in the work dir).
    struct FixFileCleaner {
        file: String,
        clean_content: String,
        calls: Cell<u32>,
    }

    impl ResidualCleaner for FixFileCleaner {
        fn clean_round(&self, work_dir: &Path, _residuals: &[TreeViolation]) -> Result<(), ToolError> {
            self.calls.set(self.calls.get() + 1);
            write_file(work_dir, &self.file, &self.clean_content);
            Ok(())
        }
    }

    /// A cleaner that does nothing — stands in for a subagent that cannot resolve
    /// the residual. Drives the no-progress guard.
    struct NoopCleaner;
    impl ResidualCleaner for NoopCleaner {
        fn clean_round(&self, _work_dir: &Path, _residuals: &[TreeViolation]) -> Result<(), ToolError> {
            Ok(())
        }
    }

    /// A cleaner that keeps the residual present but mutates the tree each round
    /// (shifting the line) so the residual SET differs — this defeats the
    /// no-progress guard and exercises the full max-rounds exhaustion path.
    struct ShiftingCleaner {
        file: String,
        rounds: Cell<u32>,
    }
    impl ResidualCleaner for ShiftingCleaner {
        fn clean_round(&self, work_dir: &Path, _residuals: &[TreeViolation]) -> Result<(), ToolError> {
            let n = self.rounds.get() + 1;
            self.rounds.set(n);
            // Prepend `n` blank lines so the still-present secret lands on a new
            // line number each round → the residual set changes but never clears.
            let pad = "\n".repeat(n as usize);
            write_file(
                work_dir,
                &self.file,
                &format!("{pad}token = \"<REDACTED-SECRET>\"\n"), // pii-test-fixture
            );
            Ok(())
        }
    }

    // ── residual → bounded cleaning → gate-0 → tag-able ───────────────────────
    #[test]
    #[serial]
    fn residual_cleaned_by_agent_drives_gate_to_zero_and_tags() {
        clear_env();
        // Internal main carries a raw token-shaped secret — NOT mechanically
        // sweepable, so it is a residual after prepare.
        let src = init_source(&[(
            "config.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // run() leaves an uncommitted, residual work tree (GHMR-03 behavior).
        let report = mgr.run().unwrap();
        assert!(!report.residual_violations.is_empty(), "residual present");
        assert!(!report.tagged);

        // The scoped cleaner remediates the flagged spot IN THE WORK DIR.
        let cleaner = FixFileCleaner {
            file: "config.txt".into(),
            clean_content: "token cleaned by subagent\n".into(),
            calls: Cell::new(0),
        };
        let outcome = run_cleaning_pass(
            &mgr,
            &sha,
            report.residual_violations.clone(),
            &cleaner,
            MAX_CLEAN_ROUNDS,
        )
        .unwrap();

        // Gate driven to 0 → committed + tagged (tag-able state reached).
        match &outcome {
            CleaningOutcome::Cleaned { report, rounds_used } => {
                assert_eq!(*rounds_used, 1, "one round sufficed");
                assert!(report.tagged, "cleaned tree is tagged");
                assert!(report.residual_violations.is_empty());
                assert_eq!(report.tag.as_deref(), Some(approved_tag(&sha).as_str()));
            }
            other => panic!("expected Cleaned, got {other:?}"),
        }
        assert!(outcome.is_cleaned());
        assert_eq!(cleaner.calls.get(), 1);
        assert!(tag_list(&wd_path).contains(&approved_tag(&sha)));

        // JSON payload reports the cleaning metadata.
        let v = outcome.to_json();
        assert_eq!(v["approved"], true);
        assert_eq!(v["cleaning"]["cleaned"], true);
        assert_eq!(v["cleaning"]["escalated"], false);
        assert_eq!(v["cleaning"]["rounds_used"], 1);

        // SOURCE UNTOUCHED — internal main still carries the raw secret.
        let src_cfg = std::fs::read_to_string(src.join("config.txt")).unwrap();
        assert!(src_cfg.contains("ghp_"), "source repo must be untouched"); // pii-test-fixture
        // The committed work-dir content is the CLEANED version.
        assert_eq!(
            std::fs::read_to_string(wd_path.join("config.txt")).unwrap(),
            "token cleaned by subagent\n"
        );

        cleanup(&[&src, &wd_path]);
    }

    // ── unresolvable residual → escalation with exact spots, no tag ───────────
    #[test]
    #[serial]
    fn unresolvable_residual_escalates_with_exact_spots() {
        clear_env();
        let src = init_source(&[(
            "secret.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let report = mgr.run().unwrap();
        assert!(!report.residual_violations.is_empty());

        // A cleaner that cannot fix anything → no-progress guard escalates.
        let outcome =
            run_cleaning_pass(&mgr, &sha, report.residual_violations.clone(), &NoopCleaner, MAX_CLEAN_ROUNDS)
                .unwrap();

        match &outcome {
            CleaningOutcome::Escalated {
                repo,
                internal_sha,
                residual_violations,
                rounds_used,
                ..
            } => {
                assert_eq!(repo, "Terminus");
                assert_eq!(internal_sha, &sha);
                assert!(!residual_violations.is_empty(), "spots escalated");
                assert_eq!(*rounds_used, 1, "no-progress guard stops after the first stuck round");
            }
            other => panic!("expected Escalated, got {other:?}"),
        }
        assert!(!outcome.is_cleaned());

        // No approval tag was created (gate never reached 0).
        assert!(tag_list(&wd_path).is_empty(), "no tag on escalation");

        // JSON escalation payload carries exact file:line spots.
        let v = outcome.to_json();
        assert_eq!(v["approved"], false);
        assert_eq!(v["cleaning"]["escalated"], true);
        let spots = v["cleaning"]["escalation_spots"].as_array().unwrap();
        assert!(!spots.is_empty(), "escalation lists exact spots");
        assert!(
            spots.iter().all(|s| s.as_str().unwrap().contains("secret.txt:")),
            "spots are file:line: {spots:?}"
        );

        cleanup(&[&src, &wd_path]);
    }

    // ── bounded rounds: exhaustion escalates after MAX_CLEAN_ROUNDS ────────────
    #[test]
    #[serial]
    fn exhausts_bounded_rounds_then_escalates() {
        clear_env();
        let src = init_source(&[(
            "s.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let report = mgr.run().unwrap();

        // Residual moves each round (defeating the no-progress guard) but never
        // clears → the bounded loop runs all MAX_CLEAN_ROUNDS then escalates.
        let cleaner = ShiftingCleaner { file: "s.txt".into(), rounds: Cell::new(0) };
        let outcome = run_cleaning_pass(
            &mgr,
            &sha,
            report.residual_violations.clone(),
            &cleaner,
            MAX_CLEAN_ROUNDS,
        )
        .unwrap();

        match &outcome {
            CleaningOutcome::Escalated { rounds_used, residual_violations, reason, .. } => {
                assert_eq!(*rounds_used, MAX_CLEAN_ROUNDS, "ran the full bounded rounds");
                assert!(!residual_violations.is_empty());
                assert!(reason.contains("after 3 cleaning round"), "reason: {reason}");
            }
            other => panic!("expected Escalated, got {other:?}"),
        }
        assert_eq!(cleaner.rounds.get(), MAX_CLEAN_ROUNDS, "cleaner invoked exactly max times");
        assert!(tag_list(&wd_path).is_empty(), "no tag");

        cleanup(&[&src, &wd_path]);
    }

    // ── max_rounds argument is clamped to [1, MAX_CLEAN_ROUNDS] ────────────────
    #[test]
    #[serial]
    fn max_rounds_is_clamped() {
        clear_env();
        let src = init_source(&[(
            "s.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let report = mgr.run().unwrap();

        // Ask for 99 rounds; a shifting (never-clearing) cleaner must still stop at
        // MAX_CLEAN_ROUNDS — the clamp is the real infinite-loop guard.
        let cleaner = ShiftingCleaner { file: "s.txt".into(), rounds: Cell::new(0) };
        let outcome =
            run_cleaning_pass(&mgr, &sha, report.residual_violations.clone(), &cleaner, 99).unwrap();
        match outcome {
            CleaningOutcome::Escalated { rounds_used, .. } => {
                assert_eq!(rounds_used, MAX_CLEAN_ROUNDS, "clamped to the cap");
            }
            other => panic!("expected Escalated, got {other:?}"),
        }
        assert_eq!(cleaner.rounds.get(), MAX_CLEAN_ROUNDS);

        cleanup(&[&src, &wd_path]);
    }

    // ── dispatch_cleaning with no configured command escalates immediately ─────
    #[test]
    #[serial]
    fn dispatch_without_command_escalates_zero_rounds() {
        clear_env(); // CLEAN_CMD_ENV unset
        let src = init_source(&[(
            "s.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let report = mgr.run().unwrap();
        assert!(!report.residual_violations.is_empty());

        let outcome = dispatch_cleaning(&mgr, &report).unwrap();
        match &outcome {
            CleaningOutcome::Escalated { rounds_used, reason, residual_violations, .. } => {
                assert_eq!(*rounds_used, 0, "no command → immediate escalation");
                assert!(reason.contains(CLEAN_CMD_ENV), "reason names the missing config var");
                assert!(!residual_violations.is_empty());
            }
            other => panic!("expected Escalated, got {other:?}"),
        }
        // No silent pass-through: nothing tagged.
        assert!(tag_list(&wd_path).is_empty());

        cleanup(&[&src, &wd_path]);
    }

    // ── dispatch_cleaning drives a configured command cleaner to a clean tag ───
    #[test]
    #[serial]
    fn dispatch_with_command_cleaner_cleans_via_shell() {
        clear_env();
        let src = init_source(&[(
            "config.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let report = mgr.run().unwrap();
        assert!(!report.residual_violations.is_empty());

        // The command hook overwrites the flagged file with clean content, cwd is
        // the work dir (MIRROR_WORK_DIR). A shell one-liner stands in for the
        // dispatched cleaning subagent.
        std::env::set_var(
            CLEAN_CMD_ENV,
            "printf 'cleaned by command\\n' > \"$MIRROR_WORK_DIR/config.txt\"",
        );
        let outcome = dispatch_cleaning(&mgr, &report).unwrap();
        std::env::remove_var(CLEAN_CMD_ENV);

        match &outcome {
            CleaningOutcome::Cleaned { report, rounds_used } => {
                assert_eq!(*rounds_used, 1);
                assert!(report.tagged);
            }
            other => panic!("expected Cleaned, got {other:?}"),
        }
        assert!(tag_list(&wd_path).contains(&approved_tag(&sha)));
        // Source repo untouched by the command cleaner.
        assert!(std::fs::read_to_string(src.join("config.txt")).unwrap().contains("ghp_")); // pii-test-fixture

        cleanup(&[&src, &wd_path]);
    }

    // ── CommandCleaner hands the cleaner an ABSOLUTE, symlink-resolved path ────
    // (codex P2) A relative/symlinked work dir would make a contract-following
    // `$MIRROR_WORK_DIR/<file>` cleaner target the wrong path once cwd is the work
    // dir. clean_round must canonicalize before setting cwd + MIRROR_WORK_DIR.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn command_cleaner_passes_absolute_workdir() {
        clear_env();
        // A real work dir reached through a SYMLINK — canonicalize must resolve it.
        let real = unique("real-wd");
        std::fs::create_dir_all(&real).unwrap();
        let link = unique("link-wd");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The command records the MIRROR_WORK_DIR it received into the work dir.
        std::env::set_var(
            CLEAN_CMD_ENV,
            "printf '%s' \"$MIRROR_WORK_DIR\" > \"$MIRROR_WORK_DIR/seen_path.txt\"",
        );
        let cleaner = CommandCleaner::from_env().unwrap();
        cleaner.clean_round(&link, &[]).unwrap();
        std::env::remove_var(CLEAN_CMD_ENV);

        // The file landed in the REAL dir (not real/real or link/link), and the
        // path the cleaner saw is absolute + symlink-resolved.
        let seen = std::fs::read_to_string(real.join("seen_path.txt")).unwrap();
        assert!(Path::new(&seen).is_absolute(), "MIRROR_WORK_DIR must be absolute: {seen}");
        assert_eq!(
            Path::new(&seen).canonicalize().unwrap(),
            real.canonicalize().unwrap(),
            "cleaner path must resolve to the real work dir"
        );
        assert!(!real.join("real-wd").exists() && !real.join("link-wd").exists(), "no nested dir");

        cleanup(&[&real, &link]);
    }

    // ── a git hook planted by a cleaner does NOT execute during finalize ──────
    // (codex round 3 P1) The work dir is cleaner-writable, so a hostile
    // .git/hooks/pre-commit could run arbitrary code under the parent env when
    // finalize commits. HOOKS_OFF (core.hooksPath=/dev/null) must neutralize it.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn planted_git_hook_does_not_run_during_finalize() {
        use std::os::unix::fs::PermissionsExt;
        clear_env();
        let src = init_source(&[(
            "config.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let report = mgr.run().unwrap(); // work dir now has a .git
        assert!(!report.residual_violations.is_empty());

        // A sentinel path the malicious hook would touch if it ran.
        let sentinel = unique("hook-fired");
        let sentinel_s = sentinel.display().to_string();

        // A cleaner that fixes the residual BUT also plants a pre-commit hook.
        struct HookPlanter {
            sentinel: String,
        }
        impl ResidualCleaner for HookPlanter {
            fn clean_round(&self, work_dir: &Path, _r: &[TreeViolation]) -> Result<(), ToolError> {
                write_file(work_dir, "config.txt", "cleaned\n");
                let hook = work_dir.join(".git/hooks/pre-commit");
                std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
                std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", self.sentinel)).unwrap();
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
                Ok(())
            }
        }

        let outcome = run_cleaning_pass(
            &mgr,
            &sha,
            report.residual_violations.clone(),
            &HookPlanter { sentinel: sentinel_s.clone() },
            MAX_CLEAN_ROUNDS,
        )
        .unwrap();
        assert!(outcome.is_cleaned(), "residual fixed → commit happens (which would fire the hook if enabled)");
        assert!(
            !sentinel.exists(),
            "planted pre-commit hook must NOT have executed during finalize's commit"
        );

        cleanup(&[&src, &wd_path, &sentinel]);
    }

    // ── a cleaner's .git tampering does NOT survive into finalize ─────────────
    // (codex round 4 P1) A cleaner could set core.worktree in .git/config to an
    // EXTERNAL tree so `git add`/commit approve unscanned content, or plant filters
    // / gpg programs. with_protected_git_dir must snapshot+restore .git so only the
    // work-TREE edits (re-scanned by the gate) survive.
    #[test]
    #[serial]
    fn cleaner_git_config_tampering_does_not_survive() {
        clear_env();
        let src = init_source(&[(
            "config.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let report = mgr.run().unwrap();
        assert!(!report.residual_violations.is_empty());

        // An EXTERNAL tree the attacker wants committed instead — it carries PII
        // that the gate would flag if it were ever scanned/committed.
        let external = unique("external-worktree");
        std::fs::create_dir_all(&external).unwrap();
        write_file(&external, "leak.txt", "<internal-ip> secret host\n"); // pii-test-fixture
        let external_s = external.display().to_string();

        // Cleaner: fix the real work-tree file, but ALSO redirect core.worktree and
        // append a hostile config stanza.
        struct ConfigTamperer {
            external: String,
        }
        impl ResidualCleaner for ConfigTamperer {
            fn clean_round(&self, work_dir: &Path, _r: &[TreeViolation]) -> Result<(), ToolError> {
                write_file(work_dir, "config.txt", "cleaned\n");
                // Point core.worktree at the external (PII-bearing) tree.
                run_git(work_dir, &["config", "core.worktree", &self.external]).unwrap();
                Ok(())
            }
        }

        let outcome = run_cleaning_pass(
            &mgr,
            &sha,
            report.residual_violations.clone(),
            &ConfigTamperer { external: external_s },
            MAX_CLEAN_ROUNDS,
        )
        .unwrap();

        // The .git restore drops the core.worktree redirect, so finalize commits the
        // real work dir's CLEANED file and the gate stays satisfied.
        assert!(outcome.is_cleaned(), "cleaned work-tree file → approved: {outcome:?}");

        // The committed tree is the work dir's cleaned file — NOT the external leak.
        let tracked = run_git(&wd_path, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(tracked.lines().any(|l| l.trim() == "config.txt"), "work file committed: {tracked}");
        assert!(!tracked.lines().any(|l| l.trim() == "leak.txt"), "external leak NOT committed: {tracked}");
        // core.worktree tampering did not persist in the restored .git.
        let cfg = run_git(&wd_path, &["config", "--get", "core.worktree"]);
        assert!(cfg.is_err() || cfg.unwrap().trim().is_empty(), "core.worktree redirect must be gone");

        cleanup(&[&src, &wd_path, &external]);
    }

    // ── restore rebuilds .git even when a cleaner DELETES it (codex r5 P2) ────
    #[test]
    #[serial]
    fn restore_rebuilds_git_when_cleaner_removes_it() {
        clear_env();
        let src = init_source(&[(
            "config.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let report = mgr.run().unwrap();
        assert!(!report.residual_violations.is_empty());

        // A cleaner that fixes the file but then blows away .git entirely.
        struct GitDeleter;
        impl ResidualCleaner for GitDeleter {
            fn clean_round(&self, work_dir: &Path, _r: &[TreeViolation]) -> Result<(), ToolError> {
                write_file(work_dir, "config.txt", "cleaned\n");
                std::fs::remove_dir_all(work_dir.join(".git")).unwrap();
                Ok(())
            }
        }

        let outcome =
            run_cleaning_pass(&mgr, &sha, report.residual_violations.clone(), &GitDeleter, MAX_CLEAN_ROUNDS)
                .unwrap();
        // .git was rebuilt from the in-memory snapshot → finalize commits + tags.
        assert!(outcome.is_cleaned(), "restore-regardless must survive .git deletion: {outcome:?}");
        assert!(wd_path.join(".git").is_dir(), ".git rebuilt");
        assert!(tag_list(&wd_path).contains(&approved_tag(&sha)));

        cleanup(&[&src, &wd_path]);
    }

    // ── the cleaner does NOT inherit parent service credentials ───────────────
    // (codex round 2 P1) env_clear + explicit allowlist: a secret-shaped var in the
    // parent must not reach the external cleaning subagent, but the MIRROR_* handoff
    // vars must.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn command_cleaner_does_not_inherit_parent_secrets() {
        clear_env();
        let wd = unique("wd");
        std::fs::create_dir_all(&wd).unwrap();
        // A secret-shaped var set in the PARENT process env.
        std::env::set_var("GITHUB_TOKEN", "<REDACTED-SECRET>"); // pii-test-fixture
        std::env::set_var(
            CLEAN_CMD_ENV,
            // Dump what the child can see for the two vars.
            "printf 'tok=[%s] wd=[%s]' \"$GITHUB_TOKEN\" \"$MIRROR_WORK_DIR\" > \"$MIRROR_WORK_DIR/env.txt\"",
        );
        let cleaner = CommandCleaner::from_env().unwrap();
        cleaner.clean_round(&wd, &[]).unwrap();
        std::env::remove_var(CLEAN_CMD_ENV);
        std::env::remove_var("GITHUB_TOKEN");

        let seen = std::fs::read_to_string(wd.join("env.txt")).unwrap();
        assert!(seen.contains("tok=[]"), "parent secret must NOT reach the cleaner: {seen}");
        assert!(!seen.contains("<REDACTED-SECRET>"), "no secret leaked: {seen}"); // pii-test-fixture
        assert!(seen.contains("wd=[/"), "MIRROR_WORK_DIR must be passed (absolute): {seen}");

        cleanup(&[&wd]);
    }

    // ── cleaning NEVER touches the source repo ────────────────────────────────
    #[test]
    #[serial]
    fn cleaning_never_modifies_source() {
        clear_env();
        let src = init_source(&[
            ("a.txt", "token = \"<REDACTED-SECRET>\"\n"), // pii-test-fixture
            ("b.txt", "second file content\n"),
        ]);
        let wd_path = unique("wd");
        let mgr = MirrorWorkDir::new("Terminus", &src, &wd_path);
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // Snapshot the source tree object hash before cleaning.
        let src_tree_before = run_git(&src, &["rev-parse", "HEAD^{tree}"]).unwrap().trim().to_string();
        let a_before = std::fs::read_to_string(src.join("a.txt")).unwrap();

        let report = mgr.run().unwrap();
        let cleaner = FixFileCleaner {
            file: "a.txt".into(),
            clean_content: "token cleaned\n".into(),
            calls: Cell::new(0),
        };
        let outcome =
            run_cleaning_pass(&mgr, &sha, report.residual_violations.clone(), &cleaner, MAX_CLEAN_ROUNDS)
                .unwrap();
        assert!(outcome.is_cleaned());

        // Source tree hash + file content are byte-for-byte unchanged.
        let src_tree_after = run_git(&src, &["rev-parse", "HEAD^{tree}"]).unwrap().trim().to_string();
        assert_eq!(src_tree_before, src_tree_after, "source tree must be unchanged");
        assert_eq!(a_before, std::fs::read_to_string(src.join("a.txt")).unwrap());
        // And the source working tree is not dirty.
        let status = run_git(&src, &["status", "--porcelain"]).unwrap();
        assert!(status.trim().is_empty(), "source working tree must stay clean: {status:?}");

        cleanup(&[&src, &wd_path]);
    }
}
