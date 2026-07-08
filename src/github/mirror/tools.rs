//! GHMR-04 — github mirror engine subtools (core registry) + dev-box transport.
//!
//! Exposes the GHMR-01/02/03 mirror engine as four github **core-tool** subtools.
//! They register through [`crate::github::register`], so they land on whatever
//! registry that function is invoked against — the CORE registry in
//! `register_all` and the personal registry in `register_personal` (github is a
//! core tool per the operator's tool taxonomy):
//!
//!   * `github_mirror_status`  — read-only: internal-main divergence vs. the last
//!     approved snapshot, plus the set of `mirror-approved/*` tags.
//!   * `github_mirror_prepare` — sync internal `main`'s content into the clean work
//!     dir → mechanical sweep → PII gate → commit (+ `mirror-approved/<sha>` tag
//!     when gate-clean), via GHMR-03's [`MirrorWorkDir::run`]. Returns residual
//!     violations for GHMR-05 when the tree is not yet clean.
//!   * `github_mirror_approve` — **guarded** operator authorisation of a prepared,
//!     gate-clean snapshot. Requires prepare's `mirror-approved/<sha>` tag for the
//!     CURRENT internal sha (refusing, without bothering the operator, a residual or
//!     un-prepared snapshot); on the operator's grant it records a DISTINCT
//!     `mirror-blessed/<sha>` marker. It never syncs/finalizes here, so it can never
//!     tag a stale work tree under a newer sha.
//!   * `github_mirror_push`    — **guarded**, **fast-forward-only** publish of the
//!     OPERATOR-BLESSED work-dir commit (the `mirror-blessed/<sha>` marker — NOT
//!     prepare's machine tag, so a prepare→push shortcut cannot skip approve) to the
//!     repo's `github_remote`, using `GITHUB_TOKEN`
//!     (via [`crate::github::github_token`], never raw-logged, injected through
//!     `GIT_ASKPASS` — never embedded in the remote URL or argv). Refuses any
//!     non-fast-forward move and points at the GHMR-07 bootstrap; NEVER force-pushes.
//!
//! ## Dev-box-only transport, logic-in-terminus
//! The engine's LOGIC lives here in terminus-rs, but every git operation (the
//! work-dir git ops of GHMR-03 and the `git push` here) RUNS ON THE DEV BOX — the
//! sanctioned git-transport host — because these tools shell out to `git` locally
//! (same `std::process::Command` posture GHMR-03 established). No other host ever
//! holds a GitHub credential: the push reads `GITHUB_TOKEN` from the dev box's own
//! materialised environment and injects it only into the child `git` process.
//!
//! ## Force-push-free
//! Every git argv this module builds is passed through GHMR-03's
//! [`assert_never_force`] guard before execution, so a `--force` / `-f` /
//! `--force-with-lease` can never reach `git` from here. The one sanctioned
//! re-baseline `--force` is GHMR-07's operator-blessed bootstrap, performed
//! outside this engine.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::approval::{self, Gate};
use crate::error::ToolError;
use crate::github::github_token;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::workdir::{assert_never_force, run_git, MirrorWorkDir};

/// Environment variable holding the target GitHub mirror remote URL when a call
/// does not pass one explicitly. Checked per-repo first
/// (`TERMINUS_MIRROR_REMOTE_<REPO_UPPER>`) then as a single fallback
/// (`TERMINUS_MIRROR_REMOTE`). NEVER a literal in code — the remote is infra.
const REMOTE_ENV: &str = "TERMINUS_MIRROR_REMOTE";

/// Tag namespace marking a snapshot the OPERATOR has authorised for push. Created
/// ONLY by `github_mirror_approve` after the approval gate grants — distinct from
/// GHMR-03's `mirror-approved/*` (gate-clean, but machine-created by prepare). Push
/// requires THIS marker, so a prepare→push shortcut cannot skip operator approval.
const BLESSED_TAG_PREFIX: &str = "mirror-blessed/";

// ── Shared argument parsing ────────────────────────────────────────────────

/// Required non-empty string arg.
fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{key}' is required")))
}

/// Reject a `repo` value that is anything but a single safe path component. It is
/// joined onto `TERMINUS_MIRROR_WORKDIR_ROOT` to locate the work dir, and prepare
/// then CLEARS that work dir's tree — so a traversal (`../../checkout`) or absolute
/// path would let the engine wipe an unrelated repository. Allow only
/// `[A-Za-z0-9._-]`, and never `.` / `..` / a path separator / an absolute path.
fn validate_repo(repo: &str) -> Result<(), ToolError> {
    let safe = !repo.is_empty()
        && repo != "."
        && repo != ".."
        && !repo.contains('/')
        && !repo.contains('\\')
        && !repo.contains('\0')
        && !Path::new(repo).is_absolute()
        && repo.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if safe {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "'repo' must be a single safe path component (letters/digits/.-_, no '/', '..', \
             or absolute path): got {repo:?}"
        )))
    }
}

/// Build a [`MirrorWorkDir`] for `(repo, source)` with the work dir resolved from
/// [`WORKDIR_ROOT_ENV`](super::workdir::WORKDIR_ROOT_ENV). `repo` and `source`
/// (the dev-box internal-`main` checkout) are required args on every mirror tool.
fn workdir_from_args(args: &Value) -> Result<MirrorWorkDir, ToolError> {
    let repo = req_str(args, "repo")?;
    validate_repo(repo)?;
    let source = req_str(args, "source")?;
    MirrorWorkDir::from_config(repo, source)
}

/// The operator-blessed marker tag for an internal sha.
fn blessed_tag(internal_sha: &str) -> String {
    format!("{BLESSED_TAG_PREFIX}{internal_sha}")
}

/// The commit the `mirror-blessed/<sha>` marker points at (the operator-authorised
/// commit), or `None` when the snapshot has not been blessed by an approved
/// `github_mirror_approve` call.
fn blessed_commit(work_dir: &Path, internal_sha: &str) -> Result<Option<String>, ToolError> {
    if !work_dir.join(".git").exists() {
        return Ok(None);
    }
    let tag = blessed_tag(internal_sha);
    let listed = run_git(work_dir, &["tag", "-l", &tag])?;
    if !listed.lines().any(|l| l.trim() == tag) {
        return Ok(None);
    }
    let spec = format!("{tag}^{{commit}}");
    let out = run_git(work_dir, &["rev-parse", "--verify", "-q", &spec])?;
    Ok(Some(out.trim().to_string()))
}

/// Create the `mirror-blessed/<sha>` marker at `commit` (idempotent — a no-op if it
/// already exists). A lightweight tag: it needs no committer identity and never
/// moves an existing marker (git refuses to re-tag without force, which the guard
/// forbids anyway).
fn create_blessed_tag(work_dir: &Path, internal_sha: &str, commit: &str) -> Result<(), ToolError> {
    let tag = blessed_tag(internal_sha);
    let listed = run_git(work_dir, &["tag", "-l", &tag])?;
    if listed.lines().any(|l| l.trim() == tag) {
        return Ok(());
    }
    run_git(work_dir, &["tag", &tag, commit])?;
    Ok(())
}

/// Resolve the GitHub mirror remote: explicit `github_remote` arg wins, then
/// `TERMINUS_MIRROR_REMOTE_<REPO_UPPER>`, then `TERMINUS_MIRROR_REMOTE`. The
/// resolved value is validated to NOT look like a git option (see
/// [`validate_remote`]).
fn resolve_remote(args: &Value, repo: &str) -> Result<String, ToolError> {
    if let Some(r) = args.get("github_remote").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        validate_remote(r)?;
        return Ok(r.to_string());
    }
    let per_repo = format!(
        "{REMOTE_ENV}_{}",
        repo.to_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    );
    for key in [per_repo.as_str(), REMOTE_ENV] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                validate_remote(&v)?;
                return Ok(v);
            }
        }
    }
    Err(ToolError::NotConfigured(format!(
        "no GitHub mirror remote for '{repo}': pass 'github_remote' or set {per_repo} / {REMOTE_ENV}"
    )))
}

/// Reject a remote that git would parse as an OPTION rather than a repository. A
/// value beginning with `-` (e.g. `--upload-pack=<cmd>` / `--receive-pack=<cmd>`)
/// would let a caller run an arbitrary command during `ls-remote` / `push`. Every
/// git invocation here ALSO puts `--` before the remote as a second guard, but a
/// clear up-front rejection is better than relying on `--` alone.
fn validate_remote(remote: &str) -> Result<(), ToolError> {
    if remote.starts_with('-') {
        return Err(ToolError::InvalidArgument(format!(
            "refusing an option-like git remote (starts with '-'): {remote:?}"
        )));
    }
    Ok(())
}

/// Verify the internal source checkout's HEAD is actually the tip of its `main`
/// branch (overridable via `TERMINUS_MIRROR_SOURCE_BRANCH`). The mirror publishes
/// a derivative of INTERNAL MAIN; if the dev-box checkout sits on a feature branch,
/// a detached HEAD, or a stale HEAD, `git archive HEAD` would silently mirror THAT
/// tree while every tag/label still claims it is internal main. Refuse before any
/// prepare/approve/push acts on such a checkout.
fn ensure_source_is_main(source: &Path) -> Result<(), ToolError> {
    let branch = std::env::var("TERMINUS_MIRROR_SOURCE_BRANCH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());
    if !source.join(".git").exists() {
        return Err(ToolError::InvalidArgument(format!(
            "source is not a git repo: {}",
            source.display()
        )));
    }
    let head = run_git(source, &["rev-parse", "HEAD"])?.trim().to_string();
    let main_ref = format!("refs/heads/{branch}");
    let main_sha = run_git(source, &["rev-parse", "--verify", "-q", &main_ref]).map_err(|_| {
        ToolError::InvalidArgument(format!(
            "source has no {main_ref} — not an internal-{branch} checkout: {}",
            source.display()
        ))
    })?;
    let main_sha = main_sha.trim();
    if head != main_sha {
        return Err(ToolError::InvalidArgument(format!(
            "source HEAD is not at the {branch} tip (HEAD={head}, {branch}={main_sha}) — refusing \
             to mirror a non-{branch} checkout (feature branch / detached / stale HEAD)"
        )));
    }
    Ok(())
}

// ── github_mirror_status ────────────────────────────────────────────────────

struct GitHubMirrorStatus;

#[async_trait]
impl RustTool for GitHubMirrorStatus {
    fn name(&self) -> &str {
        "github_mirror_status"
    }

    fn description(&self) -> &str {
        "Report the clean mirror work dir's status for a repo: internal-main HEAD, \
         whether that exact commit is already approved (a mirror-approved tag), how \
         far internal main has diverged past the last approved snapshot, the work-dir \
         HEAD, and the full set of mirror-approved tags. Read-only. Requires 'repo' \
         (logical name) and 'source' (the dev-box internal-main checkout path)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Logical repo name (work-dir subdir + commit label)" },
                "source": { "type": "string", "description": "Path to the internal-main checkout on the dev box" }
            },
            "required": ["repo", "source"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let wd = workdir_from_args(&args)?;
        ensure_source_is_main(wd.source())?;
        let initialised = wd.is_initialised();
        let internal_sha = wd.source_head_sha()?;
        let approved = wd.approved_tag_exists(&internal_sha)?;
        let approved_tags = wd.approved_tags()?;
        let work_head = wd.head_sha_opt();

        // Last-approved baseline: the mirror-approved tag sitting on the work-dir
        // HEAD (the tip of the swept derivative is, by construction, the most
        // recently approved snapshot). Its `<sha>` names the internal-main commit
        // that snapshot was taken from — the divergence baseline.
        let last_approved_internal_sha = match &work_head {
            Some(head) => {
                let at_head = run_git(wd.path(), &["tag", "--points-at", head])?;
                at_head
                    .lines()
                    .map(str::trim)
                    .find(|t| t.starts_with("mirror-approved/"))
                    .map(|t| t.trim_start_matches("mirror-approved/").to_string())
            }
            None => None,
        };

        // Divergence: how many internal-main commits have landed since that
        // baseline. 0 when the current sha IS the baseline; `null` when there is no
        // baseline yet or the baseline is not an ancestor of HEAD (history rewrite).
        let commits_since_last_approved: Option<u64> = match &last_approved_internal_sha {
            Some(s) if *s == internal_sha => Some(0),
            Some(s) => {
                // Only a meaningful count when the baseline is genuinely an ANCESTOR
                // of the current tip. After an internal history rewrite it is not,
                // and `rev-list --count old..new` would still return a (misleading)
                // number; report `null` in that case, per the documented contract.
                if git_exit_ok(wd.source(), &["merge-base", "--is-ancestor", s, &internal_sha]) {
                    run_git(wd.source(), &["rev-list", "--count", &format!("{s}..{internal_sha}")])
                        .ok()
                        .and_then(|o| o.trim().parse::<u64>().ok())
                } else {
                    None
                }
            }
            None => None,
        };

        Ok(json!({
            "repo": args.get("repo").and_then(Value::as_str).unwrap_or(""),
            "work_dir": wd.path().display().to_string(),
            "initialised": initialised,
            "internal_sha": internal_sha,
            // Divergence: is the CURRENT internal main already the approved
            // snapshot, or has it advanced past the last approval (needs a
            // prepare/approve/push cycle)?
            "internal_main_approved": approved,
            "needs_prepare": !approved,
            "work_head": work_head,
            // The last approved snapshot (baseline) + how far internal main has
            // diverged past it.
            "last_approved_internal_sha": last_approved_internal_sha,
            "last_approved_tag": last_approved_internal_sha
                .as_ref()
                .map(|s| format!("mirror-approved/{s}")),
            "commits_since_last_approved": commits_since_last_approved,
            "approved_tag_count": approved_tags.len(),
            "approved_tags": approved_tags,
        })
        .to_string())
    }
}

// ── github_mirror_prepare ───────────────────────────────────────────────────

struct GitHubMirrorPrepare;

#[async_trait]
impl RustTool for GitHubMirrorPrepare {
    fn name(&self) -> &str {
        "github_mirror_prepare"
    }

    fn description(&self) -> &str {
        "Prepare the clean mirror work dir for a repo: sync internal main's committed \
         tree content in, run the mechanical PII sweep, run the authoritative PII gate, \
         and commit the swept derivative to the work dir's own linear history — creating \
         a mirror-approved/<internal-sha> tag ONLY when the gate reports 0 residual \
         violations. When residual (non-mechanical) violations remain, nothing is tagged \
         and they are returned for GHMR-05 subagent cleaning. Requires 'repo' and 'source' \
         (the dev-box internal-main checkout). Writes ONLY to the work dir, never the source."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Logical repo name" },
                "source": { "type": "string", "description": "Path to the internal-main checkout on the dev box" }
            },
            "required": ["repo", "source"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let wd = workdir_from_args(&args)?;
        ensure_source_is_main(wd.source())?;
        let report = wd.run()?;
        Ok(report.to_json().to_string())
    }
}

// ── github_mirror_approve (guarded) ─────────────────────────────────────────

struct GitHubMirrorApprove;

#[async_trait]
impl RustTool for GitHubMirrorApprove {
    fn name(&self) -> &str {
        "github_mirror_approve"
    }

    fn description(&self) -> &str {
        "GUARDED. Authorise a prepared, gate-clean mirror snapshot for public push. \
         Refuses (without requesting operator approval) when residual PII violations are \
         still pending — those must be cleaned (GHMR-05) and re-prepared first. On a clean \
         snapshot it idempotently confirms the mirror-approved/<internal-sha> tag and, \
         after the operator approves the one-time code, blesses the snapshot for \
         github_mirror_push. Requires 'repo' and 'source'."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Logical repo name" },
                "source": { "type": "string", "description": "Path to the internal-main checkout on the dev box" },
                "_approval_code": { "type": "string", "description": "One-time approval code (supplied on operator re-dispatch; do not set manually)" }
            },
            "required": ["repo", "source"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let wd = workdir_from_args(&args)?;
        ensure_source_is_main(wd.source())?;
        if !wd.is_initialised() {
            return Err(ToolError::InvalidArgument(
                "work dir not initialised — run github_mirror_prepare first".into(),
            ));
        }
        let repo = req_str(&args, "repo")?.to_string();
        let internal_sha = wd.source_head_sha()?;

        // Approve blesses an ALREADY-PREPARED, gate-clean snapshot for the CURRENT
        // internal sha — it never syncs or finalizes here. Requiring prepare's
        // `mirror-approved/<sha>` tag (a) refuses a residual/un-prepared snapshot
        // without bothering the operator, and (b) avoids the stale-tree hazard of
        // finalizing a work tree that was synced at a DIFFERENT (older) internal sha
        // than the current HEAD: the tag pins a specific committed swept tree to
        // this exact sha, so blessing the commit it points at is always accurate.
        let approved_commit = match wd.approved_commit(&internal_sha)? {
            Some(c) => c,
            None => {
                return Ok(json!({
                    "approved": false,
                    "repo": repo,
                    "internal_sha": internal_sha,
                    "reason": "no gate-clean approved snapshot for this internal sha — run \
                               github_mirror_prepare first (and clean any residual PII violations \
                               via GHMR-05 before it can be approved)",
                })
                .to_string());
            }
        };

        // GUARDED: the operator must bless this snapshot out of band. The gate is
        // content-bound to (repo, source) so a code approved for one repo can't be
        // redeemed against another. The approval code is stripped before matching.
        let summary = format!(
            "Approve mirror snapshot for '{repo}' (internal main {internal_sha}, commit \
             {approved_commit}) for public GitHub push"
        );
        match approval::gate(self.name(), &args, &summary).await {
            Gate::Granted => {
                // Record the operator's authorisation as a distinct marker that ONLY
                // this granted path creates — github_mirror_push requires it, so a
                // prepare→push shortcut can never bypass this approval step.
                create_blessed_tag(wd.path(), &internal_sha, &approved_commit)?;
                Ok(json!({
                    "approved": true,
                    "repo": repo,
                    "internal_sha": internal_sha,
                    "approved_tag": format!("mirror-approved/{internal_sha}"),
                    "blessed_tag": blessed_tag(&internal_sha),
                    "commit_sha": approved_commit,
                    "message": "snapshot blessed — run github_mirror_push to publish (fast-forward only)",
                })
                .to_string())
            }
            Gate::Pending(m) | Gate::Denied(m) => Ok(json!({
                "approved": false,
                "repo": repo,
                "internal_sha": internal_sha,
                "approved_tag": format!("mirror-approved/{internal_sha}"),
                "commit_sha": approved_commit,
                "approval_required": true,
                "message": m,
            })
            .to_string()),
        }
    }
}

// ── github_mirror_push (guarded, fast-forward-only) ─────────────────────────

/// Outcome of the fast-forward analysis against the mirror remote.
#[derive(Debug, PartialEq, Eq)]
enum FfState {
    /// The remote has no `main` yet — the mirror was never bootstrapped.
    NoRemoteBranch,
    /// Remote `main` already equals the approved commit — nothing to push.
    UpToDate,
    /// Remote `main` is a strict ancestor of the approved commit — a clean ff.
    FastForward,
    /// Remote `main` is not an ancestor of the approved commit — refuse.
    NonFastForward { remote_tip: String },
}

struct GitHubMirrorPush;

#[async_trait]
impl RustTool for GitHubMirrorPush {
    fn name(&self) -> &str {
        "github_mirror_push"
    }

    fn description(&self) -> &str {
        "GUARDED. Fast-forward-only publish of a repo's approved mirror snapshot to its \
         GitHub remote. Pushes the commit behind mirror-approved/<internal-sha> to the \
         remote's main using GITHUB_TOKEN (injected via GIT_ASKPASS, never in the URL/argv, \
         never logged). REFUSES any non-fast-forward move (and an un-bootstrapped remote), \
         pointing at the GHMR-07 bootstrap; NEVER force-pushes. Runs on the dev box only. \
         Requires 'repo' and 'source'; the remote comes from 'github_remote' or \
         TERMINUS_MIRROR_REMOTE[_<REPO>]."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":          { "type": "string", "description": "Logical repo name" },
                "source":        { "type": "string", "description": "Path to the internal-main checkout on the dev box" },
                "github_remote": { "type": "string", "description": "Target GitHub mirror remote URL (else TERMINUS_MIRROR_REMOTE[_<REPO>])" },
                "_approval_code": { "type": "string", "description": "One-time approval code (supplied on operator re-dispatch; do not set manually)" }
            },
            "required": ["repo", "source"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let wd = workdir_from_args(&args)?;
        ensure_source_is_main(wd.source())?;
        if !wd.is_initialised() {
            return Err(ToolError::InvalidArgument(
                "work dir not initialised — run github_mirror_prepare first".into(),
            ));
        }
        let repo = req_str(&args, "repo")?.to_string();
        let internal_sha = wd.source_head_sha()?;

        // The SOLE publishable commit is the one the OPERATOR blessed via
        // github_mirror_approve (the `mirror-blessed/<sha>` marker) — NOT prepare's
        // machine-created `mirror-approved` tag. This closes the prepare→push
        // shortcut: without a granted approve there is no blessed marker, so push
        // refuses even on a gate-clean prepared snapshot.
        let approved_commit = blessed_commit(wd.path(), &internal_sha)?.ok_or_else(|| {
            ToolError::Conflict(format!(
                "internal main {internal_sha} is not approved for push — run \
                 github_mirror_approve first (it requires a github_mirror_prepare'd, gate-clean \
                 snapshot and the operator's approval; no mirror-blessed marker present)"
            ))
        })?;

        let remote = resolve_remote(&args, &repo)?;

        // Fast-forward analysis BEFORE the guard, so a non-ff / un-bootstrapped
        // remote is refused without requesting an operator approval that could
        // never legitimately complete.
        match ff_state(wd.path(), &remote, &approved_commit)? {
            FfState::NoRemoteBranch => {
                return Err(ToolError::Conflict(format!(
                    "mirror remote has no 'main' branch — it has not been bootstrapped. \
                     Run the GHMR-07 one-time operator-blessed bootstrap to establish shared \
                     lineage; github_mirror_push is fast-forward-only and never force-pushes."
                )));
            }
            FfState::NonFastForward { remote_tip } => {
                return Err(ToolError::Conflict(format!(
                    "non-fast-forward: mirror 'main' is at {remote_tip}, which is not an ancestor \
                     of the approved commit {approved_commit} (the mirror has diverged / is ahead). \
                     github_mirror_push never force-pushes; reconcile via the GHMR-07 bootstrap."
                )));
            }
            FfState::UpToDate => {
                return Ok(json!({
                    "pushed": false,
                    "up_to_date": true,
                    "repo": repo,
                    "internal_sha": internal_sha,
                    "commit_sha": approved_commit,
                    "branch": "main",
                    "message": "mirror 'main' already at the approved commit — nothing to push",
                })
                .to_string());
            }
            FfState::FastForward => {}
        }

        // GUARDED: the actual mutation of public state requires an operator blessing.
        // The summary names the RESOLVED remote so the operator authorises the exact
        // destination (the remote is caller-selectable) — not a generic "GitHub".
        let summary = format!(
            "Fast-forward push approved mirror commit {approved_commit} (internal main \
             {internal_sha}) for '{repo}' to remote: {remote}"
        );
        match approval::gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(m) | Gate::Denied(m) => {
                return Ok(json!({
                    "pushed": false,
                    "approval_required": true,
                    "repo": repo,
                    "internal_sha": internal_sha,
                    "commit_sha": approved_commit,
                    "message": m,
                })
                .to_string());
            }
        }

        // Token resolved ONLY now, immediately before the push, and injected via
        // GIT_ASKPASS — never in the remote URL, never in argv, never logged.
        let token = github_token()?;
        perform_ff_push(wd.path(), &remote, &approved_commit, &token)?;

        Ok(json!({
            "pushed": true,
            "repo": repo,
            "internal_sha": internal_sha,
            "commit_sha": approved_commit,
            "branch": "main",
            "message": "fast-forward push complete",
        })
        .to_string())
    }
}

// ── Fast-forward analysis + transport (dev-box git) ─────────────────────────

/// The remote `main` tip sha, or `None` when the remote has no `main` branch.
/// Uses `git ls-remote` (read-only) — no working checkout, no token needed for a
/// local test remote (a real https remote resolves credentials via the same
/// GIT_ASKPASS path the push uses, but ls-remote of a public mirror is anonymous).
fn remote_main_tip(remote: &str) -> Result<Option<String>, ToolError> {
    // `--` guards against a remote value that looks like an option.
    let out = run_git_plain(&["ls-remote", "--heads", "--", remote, "refs/heads/main"])?;
    let sha = out
        .lines()
        .find_map(|l| l.split_whitespace().next().map(str::to_string))
        .filter(|s| !s.is_empty());
    Ok(sha)
}

/// Classify the push as up-to-date / fast-forward / non-fast-forward / no-remote-branch.
fn ff_state(work_dir: &Path, remote: &str, approved_commit: &str) -> Result<FfState, ToolError> {
    let remote_tip = match remote_main_tip(remote)? {
        None => return Ok(FfState::NoRemoteBranch),
        Some(t) => t,
    };
    if remote_tip == approved_commit {
        return Ok(FfState::UpToDate);
    }
    // A clean fast-forward requires the remote tip to be an ANCESTOR of the
    // approved commit, and that ancestor must be resolvable in the work dir's own
    // object DB (it is, under the GHMR-07 shared-lineage model — the mirror's
    // history IS this work dir's history). If the remote tip is unknown here (a
    // diverged mirror with no shared ancestor) the merge-base check errors and we
    // conservatively treat it as non-fast-forward — never force.
    let is_ancestor = git_exit_ok(
        work_dir,
        &["merge-base", "--is-ancestor", &remote_tip, approved_commit],
    );
    if is_ancestor {
        Ok(FfState::FastForward)
    } else {
        Ok(FfState::NonFastForward { remote_tip })
    }
}

/// Fast-forward push the approved commit to the remote's `main`, injecting the
/// token via GIT_ASKPASS. NEVER force (`assert_never_force` guards the argv), and
/// the refspec has no leading `+`, so git itself server-side-rejects a non-ff.
fn perform_ff_push(
    work_dir: &Path,
    remote: &str,
    approved_commit: &str,
    token: &str,
) -> Result<(), ToolError> {
    // Refspec `<sha>:refs/heads/main` with NO leading `+` — git refuses a
    // non-fast-forward update, a second safety net beneath our ff pre-check.
    let refspec = format!("{approved_commit}:refs/heads/main");
    let argv = ["push", "--", remote, &refspec];
    assert_never_force(&argv);

    // GIT_ASKPASS script reads the token from a private env var passed only to
    // this child process — the token is never written to disk in the script body,
    // never placed in the remote URL, and never in argv. For a local (path/file://)
    // test remote git never invokes askpass, so the token is simply unused there.
    let askpass = write_askpass_script()?;
    let result = (|| {
        let output = Command::new("git")
            .current_dir(work_dir)
            .args(argv)
            .env("GIT_ASKPASS", askpass.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_MIRROR_TOKEN", token)
            // A tokenless username in the URL is not used here (the remote URL is
            // passed verbatim); askpass supplies the password (the token).
            .output()
            .map_err(|e| ToolError::Execution(format!("failed to spawn git push: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            // stderr cannot contain the token: it is only ever in GIT_MIRROR_TOKEN
            // (child env) and echoed by askpass to git's credential channel, not to
            // stderr. Still, redact defensively before surfacing.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let redacted = stderr.replace(token, "<redacted>");
            Err(ToolError::Execution(format!(
                "git push to mirror failed: {}",
                redacted.trim()
            )))
        }
    })();
    // Best-effort cleanup of the askpass script regardless of push outcome.
    drop(askpass);
    result
}

/// Write a minimal GIT_ASKPASS helper that echoes `$GIT_MIRROR_TOKEN`. The script
/// body carries NO secret; the token lives only in the child process environment.
fn write_askpass_script() -> Result<AskpassScript, ToolError> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "ghmr04-askpass-{}-{}.sh",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let mut f = std::fs::File::create(&path)
        .map_err(|e| ToolError::Execution(format!("failed creating askpass script: {e}")))?;
    // Echo the token for whatever git prompts (username or password); GitHub
    // token auth accepts the token as the password.
    f.write_all(b"#!/bin/sh\nprintf '%s\\n' \"$GIT_MIRROR_TOKEN\"\n")
        .map_err(|e| ToolError::Execution(format!("failed writing askpass script: {e}")))?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ToolError::Execution(format!("failed chmod askpass script: {e}")))?;
    }
    Ok(AskpassScript { path })
}

/// RAII wrapper that removes the askpass script when dropped.
struct AskpassScript {
    path: std::path::PathBuf,
}

impl AskpassScript {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for AskpassScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Run a git command with NO cwd (for `ls-remote`, which needs no repo), returning
/// stdout on success. Force-guarded like every other git argv in the engine.
fn run_git_plain(args: &[&str]) -> Result<String, ToolError> {
    assert_never_force(args);
    let output = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ToolError::Execution(format!("failed to spawn git {}: {e}", args.join(" "))))?;
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

/// Run a git command in `work_dir` purely for its exit status (0 = true). Used for
/// `merge-base --is-ancestor`, where a non-zero exit is a meaningful "not an
/// ancestor" answer, not a spawn failure. Reuses GHMR-03's `run` only for the
/// no-op-on-error shape; force-guarded.
fn git_exit_ok(work_dir: &Path, args: &[&str]) -> bool {
    assert_never_force(args);
    Command::new("git")
        .current_dir(work_dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Registration ────────────────────────────────────────────────────────────

/// Register all four GHMR-04 mirror subtools. Called from
/// [`crate::github::register`], so they attach to whichever registry github is
/// registered against (the CORE registry via `register_all`, the personal
/// registry via `register_personal`). Unconditional: no GitHub credential is
/// needed to construct them; `github_mirror_push` reads the token lazily at call
/// time and returns `NotConfigured` if it is absent.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(GitHubMirrorStatus));
    registry.register_or_replace(Box::new(GitHubMirrorPrepare));
    registry.register_or_replace(Box::new(GitHubMirrorApprove));
    registry.register_or_replace(Box::new(GitHubMirrorPush));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `run_git` is already in scope via `use super::*` (imported at module level);
    // pull in only `git_ok` for the reachability check.
    use super::super::workdir::git_ok;

    // ── local git repo fixtures (mirror the GHMR-03 test helpers) ────────────

    fn unique(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "ghmr04-{tag}-{}-{}",
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

    fn init_source(files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = unique("src");
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q", "-b", "main"]).unwrap();
        for (rel, content) in files {
            write_file(&dir, rel, content);
        }
        commit_all(&dir, "initial");
        dir
    }

    fn commit_all(dir: &Path, msg: &str) {
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

    /// A bare repo standing in for the public GitHub mirror.
    fn init_bare() -> std::path::PathBuf {
        let dir = unique("bare");
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q", "--bare", "-b", "main"]).unwrap();
        dir
    }

    fn clear_env() {
        std::env::remove_var("TERMINUS_MIRROR_PLACEHOLDERS");
        std::env::remove_var("TERMINUS_PII_CONFIG");
        std::env::remove_var("GITHUB_ALLOWED_AUTHORS");
        std::env::remove_var("TERMINUS_MIRROR_WORKDIR_ROOT");
        std::env::remove_var("TERMINUS_MIRROR_REMOTE");
        std::env::remove_var("DATABASE_URL");
    }

    fn cleanup(paths: &[&Path]) {
        for p in paths {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    fn set_root(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        std::env::set_var("TERMINUS_MIRROR_WORKDIR_ROOT", root);
    }

    /// Stand in for a granted `github_mirror_approve`: bless the current internal
    /// sha's approved commit (what the guarded grant path does after the operator
    /// approves). Requires TERMINUS_MIRROR_WORKDIR_ROOT already set.
    fn bless(repo: &str, src: &Path) {
        let wd = MirrorWorkDir::from_config(repo, src).unwrap();
        let sha = wd.source_head_sha().unwrap();
        let commit = wd.approved_commit(&sha).unwrap().unwrap();
        create_blessed_tag(wd.path(), &sha, &commit).unwrap();
    }

    // ── schema / naming ──────────────────────────────────────────────────────

    #[test]
    fn tool_names_and_schema_are_stable() {
        assert_eq!(GitHubMirrorStatus.name(), "github_mirror_status");
        assert_eq!(GitHubMirrorPrepare.name(), "github_mirror_prepare");
        assert_eq!(GitHubMirrorApprove.name(), "github_mirror_approve");
        assert_eq!(GitHubMirrorPush.name(), "github_mirror_push");
        for t in [
            GitHubMirrorStatus.parameters(),
            GitHubMirrorPrepare.parameters(),
            GitHubMirrorApprove.parameters(),
            GitHubMirrorPush.parameters(),
        ] {
            assert_eq!(t["type"], "object");
            let req = t["required"].as_array().unwrap();
            assert!(req.iter().any(|v| v == "repo"));
            assert!(req.iter().any(|v| v == "source"));
        }
    }

    #[test]
    #[serial]
    fn register_adds_four_mirror_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("github_mirror_status"));
        assert!(reg.contains("github_mirror_prepare"));
        assert!(reg.contains("github_mirror_approve"));
        assert!(reg.contains("github_mirror_push"));
    }

    #[test]
    #[serial]
    fn github_register_places_mirror_tools_on_the_registry() {
        // github is a CORE tool: its register() must attach the mirror subtools to
        // whatever registry it is handed, regardless of GITHUB_TOKEN presence.
        let mut reg = ToolRegistry::new();
        let backup = std::env::var("GITHUB_TOKEN").ok();
        std::env::remove_var("GITHUB_TOKEN");
        crate::github::register(&mut reg);
        if let Some(v) = backup {
            std::env::set_var("GITHUB_TOKEN", v);
        }
        assert!(reg.contains("github_mirror_status"));
        assert!(reg.contains("github_mirror_push"));
    }

    // ── missing args ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn status_requires_repo_and_source() {
        assert!(matches!(
            GitHubMirrorStatus.execute(json!({})).await,
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            GitHubMirrorStatus.execute(json!({ "repo": "R" })).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    // ── status / prepare state machine ──────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn prepare_then_status_reports_approved_clean_snapshot() {
        clear_env();
        let src = init_source(&[("README.md", "host <internal-ip> in lab\n")]); // pii-test-fixture
        let root = unique("root");
        set_root(&root);

        let prep = GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let pv: Value = serde_json::from_str(&prep).unwrap();
        assert_eq!(pv["approved"], true, "mechanical IP sweep → clean → approved");
        assert_eq!(pv["tagged"], true);
        assert_eq!(pv["residual_count"], 0);

        let st = GitHubMirrorStatus
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let sv: Value = serde_json::from_str(&st).unwrap();
        assert_eq!(sv["initialised"], true);
        assert_eq!(sv["internal_main_approved"], true);
        assert_eq!(sv["needs_prepare"], false);
        assert_eq!(sv["approved_tag_count"], 1);
        assert_eq!(sv["internal_sha"], run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim());
        // Current sha IS the baseline → 0 divergence.
        assert_eq!(sv["commits_since_last_approved"], 0);
        assert_eq!(sv["last_approved_internal_sha"], sv["internal_sha"]);

        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn status_reports_divergence_since_last_approved() {
        clear_env();
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let c1 = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        // Advance internal main by two commits WITHOUT re-preparing.
        write_file(&src, "a.txt", "v2 clean\n");
        commit_all(&src, "v2");
        write_file(&src, "a.txt", "v3 clean\n");
        commit_all(&src, "v3");

        let st = GitHubMirrorStatus
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let sv: Value = serde_json::from_str(&st).unwrap();
        assert_eq!(sv["internal_main_approved"], false);
        assert_eq!(sv["needs_prepare"], true);
        assert_eq!(sv["last_approved_internal_sha"], c1, "baseline is the first approved snapshot");
        assert_eq!(sv["commits_since_last_approved"], 2, "internal main advanced two commits");

        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn status_divergence_is_null_after_history_rewrite() {
        clear_env();
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let c1 = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        // Rewrite internal history: a fresh root commit with NO ancestry to c1.
        run_git(&src, &["checkout", "-q", "--orphan", "rewritten"]).unwrap();
        write_file(&src, "a.txt", "rewritten clean\n");
        run_git(&src, &["add", "-A"]).unwrap();
        run_git(&src, &["-c", "user.name=src", "-c", "user.email=<email>", "commit", "-q", "-m", "rewrite"]).unwrap(); // pii-test-fixture
        // Make it the new main so ensure_source_is_main passes.
        run_git(&src, &["branch", "-M", "main"]).unwrap();
        let new_head = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        assert_ne!(new_head, c1);

        let st = GitHubMirrorStatus
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let sv: Value = serde_json::from_str(&st).unwrap();
        // Baseline is still recorded, but c1 is no longer an ancestor → null count.
        assert_eq!(sv["last_approved_internal_sha"], c1);
        assert!(sv["commits_since_last_approved"].is_null(), "rewritten history → null divergence");

        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn status_before_prepare_flags_needs_prepare() {
        clear_env();
        let src = init_source(&[("a.txt", "clean\n")]);
        let root = unique("root");
        set_root(&root);

        let st = GitHubMirrorStatus
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let sv: Value = serde_json::from_str(&st).unwrap();
        assert_eq!(sv["initialised"], false);
        assert_eq!(sv["internal_main_approved"], false);
        assert_eq!(sv["needs_prepare"], true);
        assert_eq!(sv["approved_tag_count"], 0);
        assert!(sv["work_head"].is_null());

        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn prepare_with_residual_does_not_tag() {
        clear_env();
        // A raw token-shaped secret is NOT mechanically sweepable → residual.
        let src = init_source(&[(
            "c.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let root = unique("root");
        set_root(&root);

        let prep = GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let pv: Value = serde_json::from_str(&prep).unwrap();
        assert_eq!(pv["approved"], false);
        assert_eq!(pv["tagged"], false);
        assert!(pv["residual_count"].as_u64().unwrap() >= 1);

        cleanup(&[&src, &root]);
    }

    // ── approve: refuses residuals without touching the operator gate ────────

    #[tokio::test]
    #[serial]
    async fn approve_refuses_when_residuals_pending() {
        clear_env();
        let src = init_source(&[(
            "c.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let root = unique("root");
        set_root(&root);
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let out = GitHubMirrorApprove
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["approved"], false);
        assert!(v["reason"].as_str().unwrap().contains("residual"));
        // No approval was requested (no approval_required flag) — the gate was
        // never reached because the snapshot is un-pushable.
        assert!(v.get("approval_required").is_none());

        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn approve_clean_snapshot_reaches_the_guard() {
        clear_env(); // DATABASE_URL unset → gate denies gracefully
        let src = init_source(&[("a.txt", "clean content\n")]);
        let root = unique("root");
        set_root(&root);
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let out = GitHubMirrorApprove
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        // Clean snapshot → the guard is reached; without a DB it is not granted,
        // so approval is required (not an outright residual refusal).
        assert_eq!(v["approved"], false);
        assert_eq!(v["approval_required"], true);
        assert!(v["approved_tag"].as_str().unwrap().starts_with("mirror-approved/"));

        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn approve_requires_initialised_workdir() {
        clear_env();
        let src = init_source(&[("a.txt", "clean\n")]);
        let root = unique("root");
        set_root(&root);
        assert!(matches!(
            GitHubMirrorApprove
                .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
                .await,
            Err(ToolError::InvalidArgument(_))
        ));
        cleanup(&[&src, &root]);
    }

    // ── push: not approved → refuse ─────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn push_refuses_when_not_approved() {
        clear_env();
        // Residual snapshot → prepared but never approved (no tag).
        let src = init_source(&[(
            "c.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let root = unique("root");
        set_root(&root);
        let bare = init_bare();
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let res = GitHubMirrorPush
            .execute(json!({
                "repo": "Terminus",
                "source": src.display().to_string(),
                "github_remote": bare.display().to_string()
            }))
            .await;
        assert!(matches!(res, Err(ToolError::Conflict(_))), "not-approved must be a Conflict");

        cleanup(&[&src, &root, &bare]);
    }

    #[tokio::test]
    #[serial]
    async fn push_refuses_when_prepared_but_not_blessed() {
        // P1-3 regression: a gate-clean prepared snapshot with a bootstrapped,
        // fast-forwardable remote must STILL refuse push until github_mirror_approve
        // has blessed it — prepare's machine tag alone is not push authorisation.
        clear_env();
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        // Bootstrap a remote so ff would otherwise be fine, and advance so there is
        // a real ff to do — none of which should matter without a blessing.
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        let c1 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        let bare = init_bare();
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c1}:refs/heads/main")]).unwrap();
        write_file(&src, "a.txt", "v2 clean\n");
        commit_all(&src, "v2");
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let res = GitHubMirrorPush
            .execute(json!({
                "repo": "Terminus",
                "source": src.display().to_string(),
                "github_remote": bare.display().to_string()
            }))
            .await;
        match res {
            Err(ToolError::Conflict(m)) => assert!(
                m.contains("github_mirror_approve"),
                "unblessed push must point at approve: {m}"
            ),
            other => panic!("expected Conflict requiring approve, got {other:?}"),
        }
        cleanup(&[&src, &root, &bare]);
    }

    #[tokio::test]
    #[serial]
    async fn push_refuses_unbootstrapped_remote() {
        clear_env();
        let src = init_source(&[("a.txt", "clean content\n")]);
        let root = unique("root");
        set_root(&root);
        let bare = init_bare(); // empty — no main branch
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src); // operator-approve stand-in

        let res = GitHubMirrorPush
            .execute(json!({
                "repo": "Terminus",
                "source": src.display().to_string(),
                "github_remote": bare.display().to_string()
            }))
            .await;
        match res {
            Err(ToolError::Conflict(m)) => assert!(m.contains("bootstrap"), "must point at GHMR-07: {m}"),
            other => panic!("expected Conflict pointing at bootstrap, got {other:?}"),
        }

        cleanup(&[&src, &root, &bare]);
    }

    #[tokio::test]
    #[serial]
    async fn push_missing_remote_is_not_configured() {
        clear_env();
        let src = init_source(&[("a.txt", "clean content\n")]);
        let root = unique("root");
        set_root(&root);
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src); // operator-approve stand-in
        // blessed, but no github_remote arg and no env → NotConfigured.
        let res = GitHubMirrorPush
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await;
        assert!(matches!(res, Err(ToolError::NotConfigured(_))));
        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn push_blessed_and_fast_forwardable_reaches_the_guard() {
        // Blessed + a real fast-forward available → validation passes and the
        // GUARDED gate is reached (no DB → approval_required, real push withheld).
        clear_env();
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        let c1 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        let bare = init_bare();
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c1}:refs/heads/main")]).unwrap();
        // Advance + prepare + bless c2 (a genuine ff over the remote's c1).
        write_file(&src, "a.txt", "v2 clean\n");
        commit_all(&src, "v2");
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src);

        let out = GitHubMirrorPush
            .execute(json!({
                "repo": "Terminus",
                "source": src.display().to_string(),
                "github_remote": bare.display().to_string()
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["pushed"], false);
        assert_eq!(v["approval_required"], true);
        // The remote must NOT have advanced — the real push was gated.
        let tip = run_git(&bare, &["rev-parse", "refs/heads/main"]).unwrap().trim().to_string();
        assert_eq!(tip, c1, "unapproved push must not move the mirror");
        cleanup(&[&src, &root, &bare]);
    }

    #[test]
    #[serial]
    fn blessed_tag_round_trips_and_is_idempotent() {
        clear_env();
        let src = init_source(&[("a.txt", "clean\n")]);
        let root = unique("root");
        set_root(&root);
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        wd.run().unwrap();
        let sha = wd.source_head_sha().unwrap();
        let commit = wd.approved_commit(&sha).unwrap().unwrap();
        assert!(blessed_commit(wd.path(), &sha).unwrap().is_none(), "not blessed initially");
        create_blessed_tag(wd.path(), &sha, &commit).unwrap();
        create_blessed_tag(wd.path(), &sha, &commit).unwrap(); // idempotent
        assert_eq!(blessed_commit(wd.path(), &sha).unwrap().as_deref(), Some(commit.as_str()));
        cleanup(&[&src, &root]);
    }

    // ── repo path-component validation (traversal guard) ─────────────────────

    #[tokio::test]
    #[serial]
    async fn tools_reject_unsafe_repo_component() {
        clear_env();
        let root = unique("root");
        set_root(&root);
        for bad in ["../escape", "a/b", "..", ".", "/abs", "a\\b"] {
            let res = GitHubMirrorStatus
                .execute(json!({ "repo": bad, "source": "/tmp/whatever" }))
                .await;
            assert!(
                matches!(res, Err(ToolError::InvalidArgument(_))),
                "unsafe repo {bad:?} must be rejected"
            );
        }
        cleanup(&[&root]);
    }

    #[test]
    fn validate_repo_accepts_safe_and_rejects_traversal() {
        for ok in ["Terminus", "lumina-constellation", "Chord", "a.b_c-1"] {
            assert!(validate_repo(ok).is_ok(), "{ok} should be valid");
        }
        for bad in ["..", ".", "a/b", "../x", "/abs", "a\\b", "", "a b"] {
            assert!(validate_repo(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    // ── option-injection guard on the remote ─────────────────────────────────

    #[test]
    fn validate_remote_rejects_option_like_values() {
        assert!(validate_remote("https://github.com/moosenet-io/Terminus.git").is_ok());
        assert!(validate_remote("/srv/mirrors/Terminus.git").is_ok());
        for bad in ["--upload-pack=evil", "--receive-pack=evil", "-oProxyCommand=x"] {
            assert!(validate_remote(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[tokio::test]
    #[serial]
    async fn push_rejects_option_like_remote() {
        clear_env();
        let src = init_source(&[("a.txt", "clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src);
        let res = GitHubMirrorPush
            .execute(json!({
                "repo": "Terminus",
                "source": src.display().to_string(),
                "github_remote": "--upload-pack=touch /tmp/pwned"
            }))
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArgument(_))));
        cleanup(&[&src, &root]);
    }

    // ── source-HEAD-must-be-main guard ───────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn prepare_and_status_refuse_source_not_on_main() {
        clear_env();
        let src = init_source(&[("a.txt", "on main\n")]);
        let root = unique("root");
        set_root(&root);
        // Move source onto a feature branch whose tip differs from main's tip.
        run_git(&src, &["checkout", "-q", "-b", "feature"]).unwrap();
        write_file(&src, "a.txt", "on feature\n");
        commit_all(&src, "feature commit");

        for res in [
            GitHubMirrorStatus
                .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
                .await,
            GitHubMirrorPrepare
                .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
                .await,
        ] {
            assert!(
                matches!(res, Err(ToolError::InvalidArgument(_))),
                "a non-main source HEAD must be refused: {res:?}"
            );
        }

        // Back on main → prepare succeeds.
        run_git(&src, &["checkout", "-q", "main"]).unwrap();
        let ok = GitHubMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await;
        assert!(ok.is_ok(), "main-tip source must be accepted: {ok:?}");
        cleanup(&[&src, &root]);
    }

    #[test]
    #[serial]
    fn ensure_source_is_main_accepts_detached_at_main_tip() {
        clear_env();
        let src = init_source(&[("a.txt", "clean\n")]);
        // Detach HEAD exactly at main's tip — same commit, so it IS internal main.
        let sha = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        run_git(&src, &["checkout", "-q", &sha]).unwrap();
        assert!(ensure_source_is_main(&src).is_ok(), "detached at main tip is fine");
        cleanup(&[&src]);
    }

    // ── ff_state classification (the core safety logic) ──────────────────────

    #[test]
    #[serial]
    fn ff_state_no_remote_branch_when_bare_empty() {
        clear_env();
        let src = init_source(&[("a.txt", "clean\n")]);
        let root = unique("root");
        set_root(&root);
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        wd.run().unwrap();
        let commit = wd
            .approved_commit(&wd.source_head_sha().unwrap())
            .unwrap()
            .unwrap();
        let bare = init_bare();
        assert_eq!(
            ff_state(wd.path(), &bare.display().to_string(), &commit).unwrap(),
            FfState::NoRemoteBranch
        );
        cleanup(&[&src, &root, &bare]);
    }

    #[test]
    #[serial]
    fn ff_state_up_to_date_fast_forward_and_non_ff() {
        clear_env();
        let src = init_source(&[("a.txt", "clean 1\n")]);
        let root = unique("root");
        set_root(&root);
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        wd.run().unwrap();
        let c1 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();

        // Bootstrap the bare mirror to c1 (the sanctioned one-time seed).
        let bare = init_bare();
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c1}:refs/heads/main")]).unwrap();

        // Remote main == c1 == approved → UpToDate.
        assert_eq!(
            ff_state(wd.path(), &bare.display().to_string(), &c1).unwrap(),
            FfState::UpToDate
        );

        // Advance internal main → c2; remote (c1) is an ancestor of c2 → FastForward.
        write_file(&src, "a.txt", "clean 2\n");
        commit_all(&src, "update2");
        wd.run().unwrap();
        let c2 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        assert_ne!(c1, c2);
        assert_eq!(
            ff_state(wd.path(), &bare.display().to_string(), &c2).unwrap(),
            FfState::FastForward
        );

        // Push c2 to the mirror, then ask to publish the OLDER approved commit c1:
        // remote (c2) is NOT an ancestor of c1 → NonFastForward (mirror ahead).
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c2}:refs/heads/main")]).unwrap();
        match ff_state(wd.path(), &bare.display().to_string(), &c1).unwrap() {
            FfState::NonFastForward { remote_tip } => assert_eq!(remote_tip, c2),
            other => panic!("expected NonFastForward, got {other:?}"),
        }

        cleanup(&[&src, &root, &bare]);
    }

    #[test]
    #[serial]
    fn ff_state_diverged_no_shared_ancestor_is_non_ff() {
        clear_env();
        // Two independent work-dir histories with NO common ancestor. The mirror
        // carries history A; we try to publish history B's approved commit.
        let src_a = init_source(&[("a.txt", "history A clean\n")]);
        let root_a = unique("root-a");
        set_root(&root_a);
        let wd_a = MirrorWorkDir::from_config("RepoA", &src_a).unwrap();
        wd_a.run().unwrap();
        let ca = wd_a.approved_commit(&wd_a.source_head_sha().unwrap()).unwrap().unwrap();
        let bare = init_bare();
        run_git(wd_a.path(), &["push", &bare.display().to_string(), &format!("{ca}:refs/heads/main")]).unwrap();

        // Independent history B (different work-dir root → no shared objects).
        let src_b = init_source(&[("b.txt", "history B clean\n")]);
        let root_b = unique("root-b");
        std::fs::create_dir_all(&root_b).unwrap();
        std::env::set_var("TERMINUS_MIRROR_WORKDIR_ROOT", &root_b);
        let wd_b = MirrorWorkDir::from_config("RepoB", &src_b).unwrap();
        wd_b.run().unwrap();
        let cb = wd_b.approved_commit(&wd_b.source_head_sha().unwrap()).unwrap().unwrap();

        // Remote tip (ca) is unknown in wd_b's object DB → merge-base errors →
        // conservatively classified NonFastForward (never force).
        match ff_state(wd_b.path(), &bare.display().to_string(), &cb).unwrap() {
            FfState::NonFastForward { .. } => {}
            other => panic!("diverged histories must be NonFastForward, got {other:?}"),
        }

        cleanup(&[&src_a, &root_a, &src_b, &root_b, &bare]);
    }

    // ── perform_ff_push: ff succeeds, non-ff refused, token never leaks ──────

    #[test]
    #[serial]
    fn perform_ff_push_succeeds_and_advances_mirror() {
        clear_env();
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        wd.run().unwrap();
        let c1 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        let bare = init_bare();
        // Seed the mirror at c1 (bootstrap), then ff to c2 via perform_ff_push.
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c1}:refs/heads/main")]).unwrap();
        write_file(&src, "a.txt", "v2 clean\n");
        commit_all(&src, "v2");
        wd.run().unwrap();
        let c2 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();

        perform_ff_push(wd.path(), &bare.display().to_string(), &c2, "UNUSED-LOCAL").unwrap();

        // The bare mirror's main now points at c2.
        let tip = run_git(&bare, &["rev-parse", "refs/heads/main"]).unwrap().trim().to_string();
        assert_eq!(tip, c2);

        cleanup(&[&src, &root, &bare]);
    }

    #[test]
    #[serial]
    fn perform_ff_push_refuses_non_fast_forward() {
        clear_env();
        // Two divergent commits on the mirror vs. the pushed commit: git itself
        // must reject the non-`+` refspec as non-fast-forward.
        let src = init_source(&[("a.txt", "v1\n")]);
        let root = unique("root");
        set_root(&root);
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        wd.run().unwrap();
        let c1 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        write_file(&src, "a.txt", "v2\n");
        commit_all(&src, "v2");
        wd.run().unwrap();
        let c2 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();

        let bare = init_bare();
        // Mirror is at c2; pushing the older c1 with no `+` must be refused by git.
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c2}:refs/heads/main")]).unwrap();
        let res = perform_ff_push(wd.path(), &bare.display().to_string(), &c1, "UNUSED-LOCAL");
        assert!(res.is_err(), "git must reject the non-fast-forward push");

        cleanup(&[&src, &root, &bare]);
    }

    #[test]
    #[serial]
    fn perform_ff_push_error_never_contains_the_token() {
        clear_env();
        // Force a push failure (non-ff) and assert the very-recognisable token
        // never appears in the surfaced error string.
        let src = init_source(&[("a.txt", "v1\n")]);
        let root = unique("root");
        set_root(&root);
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        wd.run().unwrap();
        let c1 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        write_file(&src, "a.txt", "v2\n");
        commit_all(&src, "v2");
        wd.run().unwrap();
        let c2 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        let bare = init_bare();
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c2}:refs/heads/main")]).unwrap();

        let token = "<REDACTED-SECRET>"; // pii-test-fixture
        let err = perform_ff_push(wd.path(), &bare.display().to_string(), &c1, token).unwrap_err();
        let msg = format!("{err}");
        assert!(!msg.contains(token), "token must never appear in error output: {msg}");

        cleanup(&[&src, &root, &bare]);
    }

    #[test]
    fn askpass_script_body_carries_no_secret() {
        // The GIT_ASKPASS helper must never embed the token — it reads it from the
        // child process environment instead.
        let script = write_askpass_script().unwrap();
        let body = std::fs::read_to_string(script.path()).unwrap();
        assert!(body.contains("GIT_MIRROR_TOKEN"));
        assert!(!body.contains("ghp_")); // pii-test-fixture
    }

    // ── force is structurally unreachable ────────────────────────────────────

    #[test]
    #[should_panic(expected = "force/hard token")]
    fn push_argv_with_force_would_panic() {
        // Defense in depth: the push argv passes through GHMR-03's force guard.
        assert_never_force(&["push", "--force", "origin", "main"]);
    }

    // silence unused import warning in configurations where git_ok is not used
    #[test]
    fn git_ok_helper_is_reachable() {
        let _ = git_ok as fn(&Path, &[&str]) -> bool;
    }
}
