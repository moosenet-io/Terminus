//! GHMR-04 / GITX-08 — git-public mirror engine subtools (core registry) +
//! dev-box transport.
//!
//! Exposes the GHMR-01/02/03 mirror engine as five **core-tool** subtools
//! (moved from the github module to `crate::forge::mirror` and renamed from
//! `github_mirror_*` to `git_public_mirror_*` at GITX-08 — the engine has
//! been behaviorally provider-agnostic since GITX-05; only the naming still
//! said "GitHub"). They register through [`crate::github::register`], so
//! they land on whatever registry that function is invoked against — the
//! CORE registry in `register_all` and the personal registry in
//! `register_personal` (github is a core tool per the operator's tool
//! taxonomy). GitHub remains the only currently-configured public mirror
//! target; Gitea is the internal source of truth.
//!
//!   * `git_public_mirror_status`  — read-only: internal-main divergence vs. the last
//!     approved snapshot, plus the set of `mirror-approved/*` tags.
//!   * `git_public_mirror_sync_source` (S111E/MIRR-04) — clone/fetch the internal-main
//!     "parking lot" checkout directly from Gitea (git-protocol transport: clone if
//!     absent, else fetch + checkout + hard-reset to `origin/<branch>`), using the
//!     resolved `GITEA_PAT_<NAME>` credential via `GIT_ASKPASS`. This is what feeds
//!     `source` for the other four tools — see the operator-decision note below.
//!   * `git_public_mirror_prepare` — sync internal `main`'s content into the clean work
//!     dir → mechanical sweep → PII gate → commit (+ `mirror-approved/<sha>` tag
//!     when gate-clean), via GHMR-03's [`MirrorWorkDir::run`]. Returns residual
//!     violations for GHMR-05 when the tree is not yet clean.
//!   * `git_public_mirror_approve` — **guarded** operator authorisation of a prepared,
//!     gate-clean snapshot. Requires prepare's `mirror-approved/<sha>` tag for the
//!     CURRENT internal sha (refusing, without bothering the operator, a residual or
//!     un-prepared snapshot); on the operator's grant it records a DISTINCT
//!     `mirror-blessed/<sha>` marker. It never syncs/finalizes here, so it can never
//!     tag a stale work tree under a newer sha.
//!   * `git_public_mirror_push`    — **guarded**, **fast-forward-only** publish of the
//!     OPERATOR-BLESSED work-dir commit (the `mirror-blessed/<sha>` marker — NOT
//!     prepare's machine tag, so a prepare→push shortcut cannot skip approve) to the
//!     repo's `github_remote`, using the resolved GitHub credential
//!     (`GITHUB_PAT_<NAME>` for the default identity — `GITHUB_PAT_MOOSE` — with
//!     legacy `GITHUB_TOKEN` as a fallback; via [`crate::github::github_token`],
//!     never raw-logged, injected through `GIT_ASKPASS` — never embedded in the
//!     remote URL or argv). Refuses any
//!     non-fast-forward move and points at the GHMR-07 bootstrap; NEVER force-pushes.
//!
//! ## Git-protocol transport ownership (S111E, 2026-07-10, operator decision)
//! As of moosenet-spec skill v3.14, the Terminus git tool (this module, plus
//! [`crate::github`]/[`crate::gitea`]) is the SANCTIONED OWNER of git-protocol
//! transport (clone/fetch/merge/push/source-sync), holding both the Gitea and
//! GitHub credentials via `GIT_ASKPASS`. This SUPERSEDES the former
//! dev-box-only git-transport rule for these operations — the one sanctioned
//! door for git transport is now this engine, not a specific host. Other hosts
//! still never get their own ad hoc credentials.
//!
//! ## Dev-box-only transport, logic-in-terminus
//! The engine's LOGIC lives here in terminus-rs, but every git operation (the
//! work-dir git ops of GHMR-03 and the `git push` here) RUNS ON THE DEV BOX — the
//! sanctioned git-transport host — because these tools shell out to `git` locally
//! (same `std::process::Command` posture GHMR-03 established). No other host ever
//! holds a GitHub credential: the push resolves the default identity's
//! `GITHUB_PAT_<NAME>` (`GITHUB_PAT_MOOSE`, legacy `GITHUB_TOKEN` fallback) from
//! the dev box's own materialised environment and injects it only into the child
//! `git` process.
//!
//! ## Force-push-free
//! Every git argv this module builds is passed through GHMR-03's
//! [`assert_never_force`] guard before execution, so a `--force` / `-f` /
//! `--force-with-lease` can never reach `git` from here. The one sanctioned
//! re-baseline `--force` is GHMR-07's operator-blessed bootstrap, performed
//! outside this engine.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::approval::{self, Gate};
use crate::error::ToolError;
use crate::github::github_token;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::clean::dispatch_cleaning;
use super::workdir::{assert_never_force, run_git, MirrorWorkDir};

/// Environment variable holding the target GitHub mirror remote URL when a call
/// does not pass one explicitly. Checked per-repo first
/// (`TERMINUS_MIRROR_REMOTE_<REPO_UPPER>`) then as a single fallback
/// (`TERMINUS_MIRROR_REMOTE`). NEVER a literal in code — the remote is infra.
const REMOTE_ENV: &str = "TERMINUS_MIRROR_REMOTE";

/// Resolve the transport token for a mirror-push TARGET provider. The engine's
/// logic is provider-agnostic by construction (S106 / GITX-05): `github` is
/// the only wired resolver today because it is the only configured public
/// mirror target, but this is a routing table, not a hardcoded assumption —
/// adding a second target (e.g. once a `codeberg`/`gitlab_saas` mirror is
/// configured) is one more match arm here, not a rewrite of the push/prepare
/// transport. An unrouted provider is a clean, honest [`ToolError::NotConfigured`],
/// never a silent fallback to GitHub's credential.
fn mirror_provider_token(provider: &str) -> Result<String, ToolError> {
    match provider {
        "github" => github_token(),
        other => Err(ToolError::NotConfigured(format!(
            "mirror engine has no transport credential resolver configured for provider \
             '{other}' yet (only 'github' is wired) — the engine is provider-routable, but \
             this target has not been configured"
        ))),
    }
}

/// The mirror-push target provider for a call: explicit `provider` arg, else
/// `github` (today's only configured target). Kept as its own accessor so the
/// "not hardcoded, just currently mono-configured" distinction is visible at
/// every call site.
fn mirror_provider(args: &Value) -> String {
    args.get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("github")
        .to_string()
}

/// Environment variable enabling autonomous approve/push on a verified-clean,
/// fast-forward-eligible snapshot (MIRR-02 / S111E): when set to `"true"`
/// (case-insensitive), `git_public_mirror_approve` skips the operator one-time code
/// once the machine `mirror-approved/<sha>` tag (prepare's 0-residual PII proof)
/// is present for the current internal sha, and `git_public_mirror_push` skips it
/// once the snapshot is blessed AND the fast-forward analysis is clean. Default
/// FALSE (unset/anything else) — the operator code is still required. This flag
/// NEVER touches the hard PII block: a dirty/residual sweep never creates a
/// `mirror-approved` tag in the first place, so auto-approve cannot fire on it; a
/// non-fast-forward / un-bootstrapped remote refuses unconditionally regardless
/// of this flag. Every auto-approve/auto-push is logged loudly (see
/// `auto_approve_enabled`'s call sites).
const AUTO_APPROVE_ENV: &str = "TERMINUS_MIRROR_AUTO_APPROVE";

/// Whether [`AUTO_APPROVE_ENV`] is enabled. Matches the codebase's existing
/// boolean-env convention (see e.g. `scribe::mod`'s
/// `SCRIBE_ALLOW_SUBPROCESS_VAULT_WRITE`): case-insensitive `"true"`, anything
/// else (including unset) is `false`.
fn auto_approve_enabled() -> bool {
    std::env::var(AUTO_APPROVE_ENV)
        .ok()
        .map(|s| s.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Tag namespace marking a snapshot the OPERATOR has authorised for push. Created
/// ONLY by `git_public_mirror_approve` after the approval gate grants — distinct from
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

/// Environment variable holding a configurable "parking lot" root directory
/// containing one internal-`main` checkout per repo
/// (`<TERMINUS_MIRROR_SOURCE_ROOT>/<repo>`), used to derive `source` when a
/// caller does not pass it explicitly — e.g. a shared NFS location updated by
/// the dev box on merge and read by whichever host runs the mirror tools
/// (MIRR-01 / S111E). Unset by default: with no root configured, `source`
/// remains a required explicit arg (back-compat with pre-MIRR-01 callers).
const SOURCE_ROOT_ENV: &str = "TERMINUS_MIRROR_SOURCE_ROOT";

/// Resolve the `source` path for a call: an explicit `source` arg always wins
/// (even when a root is configured); otherwise, when
/// [`SOURCE_ROOT_ENV`] is set, derive `<root>/<repo>`; otherwise a clear
/// [`ToolError::NotConfigured`] — there is nothing to fall back to. `repo` must
/// already be validated by [`validate_repo`] before this is called (it is
/// joined onto the root exactly like [`MirrorWorkDir::from_config`] joins the
/// work-dir root, so the same traversal guard applies).
fn resolve_source(args: &Value, repo: &str) -> Result<PathBuf, ToolError> {
    if let Some(s) = args.get("source").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(s));
    }
    let root = std::env::var(SOURCE_ROOT_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ToolError::NotConfigured(format!(
                "'source' was not given and {SOURCE_ROOT_ENV} is not set — pass 'source' \
                 explicitly (the internal-main checkout path) or configure {SOURCE_ROOT_ENV} so \
                 it can be derived as {SOURCE_ROOT_ENV}/{repo}"
            ))
        })?;
    Ok(Path::new(&root).join(repo))
}

/// Build a [`MirrorWorkDir`] for `(repo, source)` with the work dir resolved from
/// [`WORKDIR_ROOT_ENV`](super::workdir::WORKDIR_ROOT_ENV). `repo` is a required
/// arg on every mirror tool; `source` (the internal-`main` checkout) is either
/// passed explicitly or derived from [`SOURCE_ROOT_ENV`] (MIRR-01) — see
/// [`resolve_source`].
fn workdir_from_args(args: &Value) -> Result<MirrorWorkDir, ToolError> {
    let repo = req_str(args, "repo")?;
    validate_repo(repo)?;
    let source = resolve_source(args, repo)?;
    MirrorWorkDir::from_config(repo, source)
}

/// Build the value handed to [`approval::gate`] so the approval code is bound to
/// the FRESHLY-RESOLVED snapshot, not just the caller's `repo`/`source`. The gate
/// content-binds on the whole args object (minus the approval code); injecting the
/// recomputed `internal_sha` (and, for push, `approved_commit`) means a code
/// approved while main was at commit A cannot be redeemed once the tool recomputes
/// a different identity at commit B — the pending row no longer matches.
fn gate_content_binding(args: &Value, internal_sha: &str, approved_commit: Option<&str>) -> Value {
    let mut a = args.clone();
    if let Some(obj) = a.as_object_mut() {
        obj.insert("internal_sha".into(), json!(internal_sha));
        if let Some(c) = approved_commit {
            obj.insert("approved_commit".into(), json!(c));
        }
    }
    a
}

/// The operator-blessed marker tag for an internal sha.
fn blessed_tag(internal_sha: &str) -> String {
    format!("{BLESSED_TAG_PREFIX}{internal_sha}")
}

/// The commit the `mirror-blessed/<sha>` marker points at (the operator-authorised
/// commit), or `None` when the snapshot has not been blessed by an approved
/// `git_public_mirror_approve` call.
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

// ── git_public_mirror_status ────────────────────────────────────────────────────

struct GitPublicMirrorStatus;

#[async_trait]
impl RustTool for GitPublicMirrorStatus {
    fn name(&self) -> &str {
        "git_public_mirror_status"
    }

    fn description(&self) -> &str {
        "Report the clean mirror work dir's status for a repo: internal-main HEAD, \
         whether that exact commit is already approved (a mirror-approved tag), how \
         far internal main has diverged past the last approved snapshot, the work-dir \
         HEAD, and the full set of mirror-approved tags. Read-only. Requires 'repo' \
         (logical name); 'source' (the internal-main checkout path) is required UNLESS \
         TERMINUS_MIRROR_SOURCE_ROOT is configured, in which case it defaults to \
         TERMINUS_MIRROR_SOURCE_ROOT/<repo> (an explicit 'source' always overrides)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Logical repo name (work-dir subdir + commit label)" },
                "source": { "type": "string", "description": "Path to the internal-main checkout (optional when TERMINUS_MIRROR_SOURCE_ROOT is set — defaults to <root>/<repo>)" }
            },
            "required": ["repo"]
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

        // Last-approved baseline + divergence. The baseline is the approved internal
        // sha CLOSEST to the current tip — the most recent approved snapshot that is
        // still an ancestor of internal main. Computed over EVERY `mirror-approved`
        // tag (not just those on the work-dir HEAD): when several internal commits
        // produce byte-identical swept content, `commit_swept` reuses one work commit
        // and stacks multiple tags on it, so a name-sorted `--points-at` pick could
        // return an arbitrary (older) sha. Ranking every candidate by ancestor
        // distance instead always lands on the true latest baseline, and yields
        // `null` when NO approved sha is an ancestor (a history rewrite).
        let mut baseline: Option<(u64, String)> = None;
        for tag in &approved_tags {
            let sha = tag.trim_start_matches("mirror-approved/").to_string();
            let dist = if sha == internal_sha {
                Some(0u64)
            } else if git_exit_ok(wd.source(), &["merge-base", "--is-ancestor", &sha, &internal_sha]) {
                run_git(wd.source(), &["rev-list", "--count", &format!("{sha}..{internal_sha}")])
                    .ok()
                    .and_then(|o| o.trim().parse::<u64>().ok())
            } else {
                None
            };
            if let Some(d) = dist {
                if baseline.as_ref().map_or(true, |(bd, _)| d < *bd) {
                    baseline = Some((d, sha));
                }
            }
        }
        let last_approved_internal_sha = baseline.as_ref().map(|(_, s)| s.clone());
        let commits_since_last_approved: Option<u64> = baseline.as_ref().map(|(d, _)| *d);

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

// ── git_public_mirror_prepare ───────────────────────────────────────────────────

struct GitPublicMirrorPrepare;

#[async_trait]
impl RustTool for GitPublicMirrorPrepare {
    fn name(&self) -> &str {
        "git_public_mirror_prepare"
    }

    fn description(&self) -> &str {
        "Prepare the clean mirror work dir for a repo: sync internal main's committed \
         tree content in, run the mechanical PII sweep, run the authoritative PII gate, \
         and commit the swept derivative to the work dir's own linear history — creating \
         a mirror-approved/<internal-sha> tag ONLY when the gate reports 0 residual \
         violations. When residual (non-mechanical) violations remain, it runs the \
         operationalized bounded cleaning pass (GHMR-05): a configured cleaning subagent \
         remediates the flagged spots in the work dir and re-gates, up to 3 rounds — driving \
         the gate to 0 (then committed + tagged) or escalating the exact file:line spots to \
         the operator. Requires 'repo'; 'source' (the internal-main checkout) is required \
         UNLESS TERMINUS_MIRROR_SOURCE_ROOT is configured, in which case it defaults to \
         TERMINUS_MIRROR_SOURCE_ROOT/<repo>. Writes ONLY to the work dir, never the source."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Logical repo name" },
                "source": { "type": "string", "description": "Path to the internal-main checkout (optional when TERMINUS_MIRROR_SOURCE_ROOT is set — defaults to <root>/<repo>)" }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let wd = workdir_from_args(&args)?;
        ensure_source_is_main(wd.source())?;
        let report = wd.run()?;

        // GHMR-05: when the mechanical sweep leaves residual (non-mechanical)
        // violations, run the operationalized, bounded cleaning pass (a configured
        // cleaning subagent editing the work dir only) instead of just returning the
        // residuals. It drives the gate to 0 (→ committed + tagged) or escalates the
        // exact spots to the operator. No cleaner configured → immediate escalation,
        // never a silent pass-through.
        if !report.residual_violations.is_empty() {
            let outcome = dispatch_cleaning(&wd, &report)?;
            return Ok(outcome.to_json().to_string());
        }
        Ok(report.to_json().to_string())
    }
}

// ── git_public_mirror_approve (guarded) ─────────────────────────────────────────

struct GitPublicMirrorApprove;

#[async_trait]
impl RustTool for GitPublicMirrorApprove {
    fn name(&self) -> &str {
        "git_public_mirror_approve"
    }

    fn description(&self) -> &str {
        "GUARDED. Authorise a prepared, gate-clean mirror snapshot for public push. \
         Refuses (without requesting operator approval) when residual PII violations are \
         still pending — those must be cleaned (GHMR-05) and re-prepared first. On a clean \
         snapshot it idempotently confirms the mirror-approved/<internal-sha> tag and, \
         after the operator approves the one-time code, blesses the snapshot for \
         git_public_mirror_push. When TERMINUS_MIRROR_AUTO_APPROVE is true AND the \
         mirror-approved/<sha> tag (the 0-residual PII proof) is present, the operator \
         code is skipped and the snapshot is blessed automatically. Requires 'repo'; \
         'source' defaults to TERMINUS_MIRROR_SOURCE_ROOT/<repo> when that root is \
         configured, else it is required."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Logical repo name" },
                "source": { "type": "string", "description": "Path to the internal-main checkout (optional when TERMINUS_MIRROR_SOURCE_ROOT is set — defaults to <root>/<repo>)" },
                "_approval_code": { "type": "string", "description": "One-time approval code (supplied on operator re-dispatch; do not set manually)" }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let wd = workdir_from_args(&args)?;
        ensure_source_is_main(wd.source())?;
        if !wd.is_initialised() {
            return Err(ToolError::InvalidArgument(
                "work dir not initialised — run git_public_mirror_prepare first".into(),
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
                               git_public_mirror_prepare first (and clean any residual PII violations \
                               via GHMR-05 before it can be approved)",
                })
                .to_string());
            }
        };

        // MIRR-02: TERMINUS_MIRROR_AUTO_APPROVE removes the human convenience gate
        // ONLY on an already-verified-clean snapshot — the match arm above already
        // proved `approved_commit` exists, which happens IFF prepare's PII gate
        // reported 0 residual violations for this exact internal sha (see GHMR-03's
        // `MirrorWorkDir::run`). A dirty/residual/un-prepared sha never reaches this
        // point (it returns early above), so auto-approve can never bless unswept
        // content — the hard PII block is untouched. Every auto-approval is logged
        // loudly to the audit log (repo + sha + commit).
        if auto_approve_enabled() {
            create_blessed_tag(wd.path(), &internal_sha, &approved_commit)?;
            tracing::warn!(
                target: "mirror_audit",
                event = "auto_approve",
                repo = %repo,
                internal_sha = %internal_sha,
                commit_sha = %approved_commit,
                "AUTO-APPROVE (TERMINUS_MIRROR_AUTO_APPROVE): mirror snapshot blessed \
                 without an operator code — verified 0-residual PII sweep"
            );
            return Ok(json!({
                "approved": true,
                "repo": repo,
                "internal_sha": internal_sha,
                "approved_tag": format!("mirror-approved/{internal_sha}"),
                "blessed_tag": blessed_tag(&internal_sha),
                "commit_sha": approved_commit,
                "auto_approved": true,
                "message": "snapshot auto-blessed (TERMINUS_MIRROR_AUTO_APPROVE, verified clean \
                             sweep) — run git_public_mirror_push to publish (fast-forward only)",
            })
            .to_string());
        }

        // GUARDED: the operator must bless this snapshot out of band. The gate is
        // content-bound to the FRESHLY-RESOLVED identity — repo, source, AND the
        // recomputed internal_sha + approved_commit — so a code approved for one
        // snapshot can never bless a different one: if internal main advances (or
        // the resolved commit changes) between request and redemption, the tool
        // recomputes a different internal_sha here, the gate content no longer
        // matches the pending row, and the stale code is refused.
        let gate_args = gate_content_binding(&args, &internal_sha, Some(&approved_commit));
        let summary = format!(
            "Approve mirror snapshot for '{repo}' (internal main {internal_sha}, commit \
             {approved_commit}) for public GitHub push"
        );
        match approval::gate(self.name(), &gate_args, &summary).await {
            Gate::Granted => {
                // Record the operator's authorisation as a distinct marker that ONLY
                // this granted path creates — git_public_mirror_push requires it, so a
                // prepare→push shortcut can never bypass this approval step.
                create_blessed_tag(wd.path(), &internal_sha, &approved_commit)?;
                Ok(json!({
                    "approved": true,
                    "repo": repo,
                    "internal_sha": internal_sha,
                    "approved_tag": format!("mirror-approved/{internal_sha}"),
                    "blessed_tag": blessed_tag(&internal_sha),
                    "commit_sha": approved_commit,
                    "message": "snapshot blessed — run git_public_mirror_push to publish (fast-forward only)",
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

// ── git_public_mirror_push (guarded, fast-forward-only) ─────────────────────────

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

struct GitPublicMirrorPush;

#[async_trait]
impl RustTool for GitPublicMirrorPush {
    fn name(&self) -> &str {
        "git_public_mirror_push"
    }

    fn description(&self) -> &str {
        "GUARDED. Fast-forward-only publish of a repo's approved mirror snapshot to its \
         GitHub remote. Pushes the commit behind mirror-approved/<internal-sha> to the \
         remote's main using the resolved GitHub credential (GITHUB_PAT_<NAME> for the \
         default identity, i.e. GITHUB_PAT_MOOSE, falling back to legacy GITHUB_TOKEN; \
         injected via GIT_ASKPASS, never in the URL/argv, never logged). REFUSES any \
         non-fast-forward move (and an un-bootstrapped remote), \
         pointing at the GHMR-07 bootstrap; NEVER force-pushes. Runs on the dev box only. \
         When TERMINUS_MIRROR_AUTO_APPROVE is true AND the snapshot is blessed AND the \
         fast-forward analysis is clean, the operator code is skipped and the push \
         proceeds automatically — a non-fast-forward / un-bootstrapped remote still \
         refuses unconditionally. Requires 'repo'; 'source' defaults to \
         TERMINUS_MIRROR_SOURCE_ROOT/<repo> when that root is configured, else it is \
         required; the remote comes from 'github_remote' or TERMINUS_MIRROR_REMOTE[_<REPO>]."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":          { "type": "string", "description": "Logical repo name" },
                "source":        { "type": "string", "description": "Path to the internal-main checkout (optional when TERMINUS_MIRROR_SOURCE_ROOT is set — defaults to <root>/<repo>)" },
                "github_remote": { "type": "string", "description": "Target mirror remote URL (else TERMINUS_MIRROR_REMOTE[_<REPO>])" },
                "provider":      { "type": "string", "description": "Mirror-push target provider (default 'github'; the engine is provider-routable — see mirror_provider_token)" },
                "_approval_code": { "type": "string", "description": "One-time approval code (supplied on operator re-dispatch; do not set manually)" }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let wd = workdir_from_args(&args)?;
        ensure_source_is_main(wd.source())?;
        if !wd.is_initialised() {
            return Err(ToolError::InvalidArgument(
                "work dir not initialised — run git_public_mirror_prepare first".into(),
            ));
        }
        let repo = req_str(&args, "repo")?.to_string();
        let internal_sha = wd.source_head_sha()?;

        // The SOLE publishable commit is the one the OPERATOR blessed via
        // git_public_mirror_approve (the `mirror-blessed/<sha>` marker) — NOT prepare's
        // machine-created `mirror-approved` tag. This closes the prepare→push
        // shortcut: without a granted approve there is no blessed marker, so push
        // refuses even on a gate-clean prepared snapshot.
        let approved_commit = blessed_commit(wd.path(), &internal_sha)?.ok_or_else(|| {
            ToolError::Conflict(format!(
                "internal main {internal_sha} is not approved for push — run \
                 git_public_mirror_approve first (it requires a git_public_mirror_prepare'd, gate-clean \
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
                     lineage; git_public_mirror_push is fast-forward-only and never force-pushes."
                )));
            }
            FfState::NonFastForward { remote_tip } => {
                return Err(ToolError::Conflict(format!(
                    "non-fast-forward: mirror 'main' is at {remote_tip}, which is not an ancestor \
                     of the approved commit {approved_commit} (the mirror has diverged / is ahead). \
                     git_public_mirror_push never force-pushes; reconcile via the GHMR-07 bootstrap."
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

        // MIRR-02: TERMINUS_MIRROR_AUTO_APPROVE removes the human convenience gate
        // ONLY once BOTH operator-independent preconditions already hold: a
        // `mirror-blessed/<sha>` marker (checked above — the operator must have
        // approved this exact snapshot at some point, auto or manual) AND a clean
        // FastForward analysis (checked immediately above — NoRemoteBranch and
        // NonFastForward both return/refuse before reaching here, unconditionally,
        // regardless of this flag). Every auto-push is logged loudly to the audit
        // log (repo + sha + remote).
        if auto_approve_enabled() {
            tracing::warn!(
                target: "mirror_audit",
                event = "auto_push",
                repo = %repo,
                internal_sha = %internal_sha,
                commit_sha = %approved_commit,
                remote = %remote,
                "AUTO-PUSH (TERMINUS_MIRROR_AUTO_APPROVE): fast-forward mirror push \
                 proceeding without an operator code — blessed + fast-forward verified"
            );
            let token = mirror_provider_token(&mirror_provider(&args))?;
            perform_ff_push(wd.path(), &remote, &approved_commit, &token)?;
            return Ok(json!({
                "pushed": true,
                "repo": repo,
                "internal_sha": internal_sha,
                "commit_sha": approved_commit,
                "branch": "main",
                "auto_pushed": true,
                "message": "fast-forward push complete (auto, TERMINUS_MIRROR_AUTO_APPROVE)",
            })
            .to_string());
        }

        // GUARDED: the actual mutation of public state requires an operator blessing.
        // The summary names the RESOLVED remote so the operator authorises the exact
        // destination (the remote is caller-selectable) — not a generic "GitHub".
        // The gate content is bound to the freshly-resolved internal_sha,
        // approved_commit AND remote, so a pending code cannot authorise a different
        // commit or a different destination if state changes before redemption.
        let mut gate_args = gate_content_binding(&args, &internal_sha, Some(&approved_commit));
        gate_args["github_remote"] = json!(remote);
        let summary = format!(
            "Fast-forward push approved mirror commit {approved_commit} (internal main \
             {internal_sha}) for '{repo}' to remote: {remote}"
        );
        match approval::gate(self.name(), &gate_args, &summary).await {
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
        // Routed by target provider (default 'github'; see `mirror_provider_token`
        // for why this is a routing table, not a hardcoded assumption).
        let token = mirror_provider_token(&mirror_provider(&args))?;
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
    // `-c core.hooksPath=/dev/null` disables hooks (e.g. a planted pre-push) for the
    // same reason GHMR-03's run_git does: the work dir is cleaner-writable and must
    // never execute a hook under the parent's environment during transport.
    let argv = ["-c", "core.hooksPath=/dev/null", "push", "--", remote, &refspec];
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

// ── git_public_mirror_sync_source (S111E / MIRR-04) ─────────────────────────────

/// Environment variable holding the internal Gitea git-transport remote for a
/// repo's parking-lot checkout, when a call does not pass `internal_remote`
/// explicitly. Checked per-repo first (`TERMINUS_MIRROR_INTERNAL_REMOTE_<REPO_UPPER>`)
/// then as a single fallback (`TERMINUS_MIRROR_INTERNAL_REMOTE`) — same shape as
/// [`REMOTE_ENV`] for the public mirror target. NEVER a literal in code, and
/// NEVER carries an embedded token (auth is injected via `GIT_ASKPASS` at call
/// time — see [`run_git_askpass_plain`] / [`run_git_askpass_in`]).
const INTERNAL_REMOTE_ENV: &str = "TERMINUS_MIRROR_INTERNAL_REMOTE";

/// Resolve the internal Gitea remote for `(args, repo)`: explicit
/// `internal_remote` arg wins, then `TERMINUS_MIRROR_INTERNAL_REMOTE_<REPO_UPPER>`,
/// then `TERMINUS_MIRROR_INTERNAL_REMOTE`. Reuses [`validate_remote`] so an
/// option-like remote (`-`-prefixed) is refused the same way the public mirror
/// remote is.
fn resolve_internal_remote(args: &Value, repo: &str) -> Result<String, ToolError> {
    if let Some(r) = args
        .get("internal_remote")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        validate_remote(r)?;
        return Ok(r.to_string());
    }
    let per_repo = format!(
        "{INTERNAL_REMOTE_ENV}_{}",
        repo.to_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    );
    for key in [per_repo.as_str(), INTERNAL_REMOTE_ENV] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                validate_remote(&v)?;
                return Ok(v);
            }
        }
    }
    Err(ToolError::NotConfigured(format!(
        "no internal Gitea remote for '{repo}': pass 'internal_remote' or set {per_repo} / \
         {INTERNAL_REMOTE_ENV} (e.g. http://<gitea-host>/moosenet/{repo}.git — no embedded token; \
         auth is injected via GIT_ASKPASS)"
    )))
}

/// The parking-lot checkout branch: `TERMINUS_MIRROR_SOURCE_BRANCH` (same env
/// var [`ensure_source_is_main`] honours), default `"main"`.
fn source_branch() -> String {
    std::env::var("TERMINUS_MIRROR_SOURCE_BRANCH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string())
}

/// Force-guard for the sync-source parking-lot checkout. Unlike
/// [`assert_never_force`] (the clean mirror WORK DIR's guard, which bans
/// `--hard` unconditionally because that tree's history only ever moves
/// forward), this checkout IS the internal-main parking lot itself — making it
/// exactly match internal main via `reset --hard origin/<branch>` is the
/// intended, sanctioned sync-source operation (S111E/MIRR-04), not a
/// force-push of anything public. `--force`/`-f`/`--force-with-lease` remain
/// unconditionally banned; `--hard` is tolerated ONLY in the exact
/// `["reset", "--hard", "origin/<branch>"]` shape `fetch_and_reset` below
/// builds — any other appearance of `--hard` is refused just as loudly.
fn assert_source_sync_safe(argv: &[&str]) {
    const BANNED: &[&str] = &["--force", "-f", "--force-with-lease"];
    for token in argv {
        let lower = token.to_lowercase();
        assert!(
            !BANNED.contains(&lower.as_str()),
            "sync-source git argv contained a force token '{token}': {argv:?}"
        );
    }
    if argv.iter().any(|a| *a == "--hard") {
        let sanctioned = argv.first() == Some(&"reset")
            && argv.get(1) == Some(&"--hard")
            && argv.len() == 3
            && argv.get(2).map(|s| s.starts_with("origin/")).unwrap_or(false);
        assert!(
            sanctioned,
            "sync-source git argv used --hard outside the sanctioned \
             'reset --hard origin/<branch>' shape: {argv:?}"
        );
    }
}

/// Run a git command in `cwd` with no credential injection (local ops: checkout,
/// rev-parse, the hard reset itself). Force-guarded via
/// [`assert_source_sync_safe`], NOT [`assert_never_force`] — this checkout
/// tolerates the one sanctioned `reset --hard origin/<branch>` shape.
fn run_source_git(cwd: &Path, args: &[&str]) -> Result<String, ToolError> {
    assert_source_sync_safe(args);
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
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

/// Run a git command with no `cwd` (for `clone`, which creates its own target
/// dir), injecting `token` via `GIT_ASKPASS` exactly like [`perform_ff_push`]'s
/// transport — the token never appears in argv, the remote URL, or logs (a
/// failure's stderr is defensively redacted of the token before surfacing).
fn run_git_askpass_plain(args: &[&str], token: &str) -> Result<String, ToolError> {
    assert_source_sync_safe(args);
    let askpass = write_askpass_script()?;
    let output = Command::new("git")
        .args(args)
        .env("GIT_ASKPASS", askpass.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_MIRROR_TOKEN", token)
        .output()
        .map_err(|e| ToolError::Execution(format!("failed to spawn git {}: {e}", args.join(" "))))?;
    drop(askpass);
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let redacted = stderr.replace(token, "<redacted>");
        Err(ToolError::Execution(format!(
            "git {} failed: {}",
            args.join(" "),
            redacted.trim()
        )))
    }
}

/// Like [`run_git_askpass_plain`] but in an existing checkout (`fetch`), with
/// hooks disabled the same way [`run_git`] disables them.
fn run_git_askpass_in(cwd: &Path, args: &[&str], token: &str) -> Result<String, ToolError> {
    assert_source_sync_safe(args);
    let askpass = write_askpass_script()?;
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_ASKPASS", askpass.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_MIRROR_TOKEN", token)
        .output()
        .map_err(|e| ToolError::Execution(format!("failed to spawn git {}: {e}", args.join(" "))))?;
    drop(askpass);
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let redacted = stderr.replace(token, "<redacted>");
        Err(ToolError::Execution(format!(
            "git {} (in {}) failed: {}",
            args.join(" "),
            cwd.display(),
            redacted.trim()
        )))
    }
}

/// `<source>/.git` absent → clone. The token is injected via `GIT_ASKPASS`; the
/// remote URL (stored verbatim in the resulting `.git/config`'s `origin`) never
/// carries the token, so subsequent `fetch`es re-resolve auth per-call the same
/// way.
fn clone_source(dest: &Path, remote: &str, token: &str) -> Result<(), ToolError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ToolError::Execution(format!(
                "failed to create source parent dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let dest_str = dest.to_string_lossy().to_string();
    let argv = ["clone", "--", remote, dest_str.as_str()];
    run_git_askpass_plain(&argv, token)?;
    Ok(())
}

/// `<source>/.git` present → `fetch origin` + `checkout <branch>` +
/// `reset --hard origin/<branch>`, making the checkout exactly match internal
/// main (this is the sanctioned `--hard`; see [`assert_source_sync_safe`]).
fn fetch_and_reset(source: &Path, branch: &str, token: &str) -> Result<(), ToolError> {
    run_git_askpass_in(source, &["fetch", "--", "origin", branch], token)?;
    run_source_git(source, &["checkout", branch])?;
    let reset_target = format!("origin/{branch}");
    run_source_git(source, &["reset", "--hard", reset_target.as_str()])?;
    Ok(())
}

struct GitPublicMirrorSyncSource;

#[async_trait]
impl RustTool for GitPublicMirrorSyncSource {
    fn name(&self) -> &str {
        "git_public_mirror_sync_source"
    }

    fn description(&self) -> &str {
        "S111E/MIRR-04. Sync a repo's internal-main 'parking lot' checkout (the \
         source the mirror engine's git_public_mirror_prepare reads) directly from \
         Gitea, using the resolved GITEA_PAT_<NAME> credential (default identity \
         GITEA_PAT_MOOSE) injected via GIT_ASKPASS — never in argv/URL/logs. If the \
         checkout doesn't exist yet (<source>/.git absent) it is cloned; otherwise it \
         is fetched and hard-reset to origin/<branch>, making the parking lot exactly \
         match internal main. 'source' defaults to TERMINUS_MIRROR_SOURCE_ROOT/<repo> \
         (same resolution as the other mirror tools); 'internal_remote' defaults to \
         TERMINUS_MIRROR_INTERNAL_REMOTE_<REPO_UPPER> then TERMINUS_MIRROR_INTERNAL_REMOTE \
         (no embedded token — an http(s) Gitea remote URL only). Returns the resulting \
         HEAD sha and branch. This is the git-protocol transport the operator (S111E, \
         2026-07-10, moosenet-spec skill v3.14) designated the Terminus git tool to own; \
         it supersedes the former dev-box-only git-clone/fetch rule for this transport."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":            { "type": "string", "description": "Logical repo name (source-root subdir)" },
                "source":          { "type": "string", "description": "Path to the parking-lot checkout (optional when TERMINUS_MIRROR_SOURCE_ROOT is set — defaults to <root>/<repo>)" },
                "internal_remote": { "type": "string", "description": "Internal Gitea git remote (else TERMINUS_MIRROR_INTERNAL_REMOTE[_<REPO>])" },
                "identity":        { "type": "string", "description": "Named GITEA_PAT_<NAME> identity to authenticate as (default: GITEA_IDENTITY_NAME, i.e. moose)" }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = req_str(&args, "repo")?.to_string();
        validate_repo(&repo)?;
        let source = resolve_source(&args, &repo)?;
        let remote = resolve_internal_remote(&args, &repo)?;
        let identity = args.get("identity").and_then(Value::as_str).map(str::to_string);
        // Resolved ONLY here, immediately before use, and injected via
        // GIT_ASKPASS — never in the remote URL, never in argv, never logged.
        let token = crate::gitea::gitea_token(identity.as_deref())?;
        let branch = source_branch();

        let cloned = if !source.join(".git").exists() {
            clone_source(&source, &remote, &token)?;
            // A fresh clone may check out the remote's default branch under a
            // different local name than `branch` (rare, but not guaranteed) —
            // make sure the parking lot lands on the configured branch.
            run_source_git(&source, &["checkout", &branch]).map_err(|e| {
                ToolError::Execution(format!(
                    "cloned '{repo}' but could not check out branch '{branch}': {e}"
                ))
            })?;
            true
        } else {
            fetch_and_reset(&source, &branch, &token)?;
            false
        };

        let head_sha = run_source_git(&source, &["rev-parse", "HEAD"])?.trim().to_string();
        let current_branch = run_source_git(&source, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string();

        Ok(json!({
            "repo": repo,
            "source": source.display().to_string(),
            "cloned": cloned,
            "remote": remote,
            "head_sha": head_sha,
            "branch": current_branch,
        })
        .to_string())
    }
}

// ── git-public integration (S106 / GITX-05) ─────────────────────────────────

/// Forward a mirror action to the underlying GHMR subtool. This is how the
/// `git_public` MCP tool (`crate::forge::git_public`) integrates the mirror
/// engine as its swept-clean-tree write path for a FULL repo mirror sync,
/// without duplicating any of the engine's PII-gate / fast-forward-only /
/// no-force logic: it simply calls the exact same [`RustTool::execute`] these
/// four core tools already run when dispatched by name via the registry.
/// `action` is one of `"status" | "prepare" | "approve" | "push" | "sync-source"`;
/// anything else is a clean invalid-argument error. `sync-source` (S111E/MIRR-04)
/// is the Gitea-side transport (clone/fetch the internal-main parking lot) —
/// distinct from the other four, which operate on the swept work-dir derivative
/// and its GitHub-side transport.
pub(crate) async fn dispatch_mirror_action(action: &str, args: Value) -> Result<String, ToolError> {
    match action {
        "status" => GitPublicMirrorStatus.execute(args).await,
        "prepare" => GitPublicMirrorPrepare.execute(args).await,
        "approve" => GitPublicMirrorApprove.execute(args).await,
        "push" => GitPublicMirrorPush.execute(args).await,
        "sync-source" => GitPublicMirrorSyncSource.execute(args).await,
        other => Err(ToolError::InvalidArgument(format!(
            "unknown mirror_action '{other}'; expected one of status/prepare/approve/push/sync-source"
        ))),
    }
}

// ── Registration ────────────────────────────────────────────────────────────

/// Register all four GHMR-04 mirror subtools. Called from
/// [`crate::github::register`], so they attach to whichever registry github is
/// registered against (the CORE registry via `register_all`, the personal
/// registry via `register_personal`). Unconditional: no GitHub credential is
/// needed to construct them; `git_public_mirror_push` reads the token lazily at call
/// time and returns `NotConfigured` if it is absent.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(GitPublicMirrorStatus));
    registry.register_or_replace(Box::new(GitPublicMirrorPrepare));
    registry.register_or_replace(Box::new(GitPublicMirrorApprove));
    registry.register_or_replace(Box::new(GitPublicMirrorPush));
    registry.register_or_replace(Box::new(GitPublicMirrorSyncSource));
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
        init_source_at(&dir, files);
        dir
    }

    /// Like [`init_source`] but at a caller-chosen path (MIRR-01: used to build a
    /// `<source_root>/<repo>` parking-lot checkout at an exact location).
    fn init_source_at(dir: &Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        run_git(dir, &["init", "-q", "-b", "main"]).unwrap();
        for (rel, content) in files {
            write_file(dir, rel, content);
        }
        commit_all(dir, "initial");
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
        std::env::remove_var("TERMINUS_MIRROR_SOURCE_ROOT");
        std::env::remove_var("TERMINUS_MIRROR_AUTO_APPROVE");
        std::env::remove_var("TERMINUS_MIRROR_INTERNAL_REMOTE");
        std::env::remove_var("TERMINUS_MIRROR_INTERNAL_REMOTE_DEMO");
        std::env::remove_var("TERMINUS_MIRROR_SOURCE_BRANCH");
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

    /// Stand in for a granted `git_public_mirror_approve`: bless the current internal
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
        assert_eq!(GitPublicMirrorStatus.name(), "git_public_mirror_status");
        assert_eq!(GitPublicMirrorPrepare.name(), "git_public_mirror_prepare");
        assert_eq!(GitPublicMirrorApprove.name(), "git_public_mirror_approve");
        assert_eq!(GitPublicMirrorPush.name(), "git_public_mirror_push");
        for t in [
            GitPublicMirrorStatus.parameters(),
            GitPublicMirrorPrepare.parameters(),
            GitPublicMirrorApprove.parameters(),
            GitPublicMirrorPush.parameters(),
        ] {
            assert_eq!(t["type"], "object");
            let req = t["required"].as_array().unwrap();
            assert!(req.iter().any(|v| v == "repo"));
            // MIRR-01: 'source' is no longer schema-required — it is derivable from
            // TERMINUS_MIRROR_SOURCE_ROOT/<repo> when that root is configured (an
            // explicit 'source' still always overrides). It remains a documented
            // property either way.
            assert!(!req.iter().any(|v| v == "source"));
            assert!(t["properties"].get("source").is_some());
        }
    }

    #[test]
    #[serial]
    fn register_adds_four_mirror_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("git_public_mirror_status"));
        assert!(reg.contains("git_public_mirror_prepare"));
        assert!(reg.contains("git_public_mirror_approve"));
        assert!(reg.contains("git_public_mirror_push"));
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
        assert!(reg.contains("git_public_mirror_status"));
        assert!(reg.contains("git_public_mirror_push"));
    }

    // ── missing args ─────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn status_requires_repo_and_source() {
        clear_env();
        assert!(matches!(
            GitPublicMirrorStatus.execute(json!({})).await,
            Err(ToolError::InvalidArgument(_))
        ));
        // MIRR-01: with no explicit 'source' AND no TERMINUS_MIRROR_SOURCE_ROOT
        // configured, there is nothing to derive from — a clear NotConfigured,
        // not the old "missing required arg" InvalidArgument.
        assert!(matches!(
            GitPublicMirrorStatus.execute(json!({ "repo": "R" })).await,
            Err(ToolError::NotConfigured(_))
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

        let prep = GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let pv: Value = serde_json::from_str(&prep).unwrap();
        assert_eq!(pv["approved"], true, "mechanical IP sweep → clean → approved");
        assert_eq!(pv["tagged"], true);
        assert_eq!(pv["residual_count"], 0);

        let st = GitPublicMirrorStatus
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let c1 = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        // Advance internal main by two commits WITHOUT re-preparing.
        write_file(&src, "a.txt", "v2 clean\n");
        commit_all(&src, "v2");
        write_file(&src, "a.txt", "v3 clean\n");
        commit_all(&src, "v3");

        let st = GitPublicMirrorStatus
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
        GitPublicMirrorPrepare
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

        let st = GitPublicMirrorStatus
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let sv: Value = serde_json::from_str(&st).unwrap();
        // c1 is no longer an ancestor of the rewritten HEAD → no valid baseline.
        let _ = c1;
        assert!(sv["last_approved_internal_sha"].is_null(), "rewritten history → no baseline");
        assert!(sv["commits_since_last_approved"].is_null(), "rewritten history → null divergence");

        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn status_picks_closest_baseline_when_multiple_tags_share_a_commit() {
        // P2 regression: when two internal commits yield byte-identical swept content
        // (here: c2 changes only the dropped pii-gate.toml), both mirror-approved tags
        // land on ONE work commit. Status must rank by ancestor distance, not tag
        // name-sort, so the CLOSEST approved sha is the baseline.
        clear_env();
        let src = init_source(&[
            ("README.md", "clean content\n"),
            ("pii-gate.toml", "extra_terms = [\"host-a\"]\n"),
        ]);
        let root = unique("root");
        set_root(&root);
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        // c2 changes ONLY the gate config (dropped from the mirror commit) → the
        // swept tree is identical → a second mirror-approved tag on the same commit.
        write_file(&src, "pii-gate.toml", "extra_terms = [\"host-b\"]\n");
        commit_all(&src, "config only");
        let c2 = run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        // Two mirror-approved tags now exist on one work commit.
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        assert_eq!(wd.approved_tags().unwrap().len(), 2, "two tags share one work commit");

        // Advance with a REAL content change → c3, unapproved.
        write_file(&src, "README.md", "clean content 2\n");
        commit_all(&src, "readme change");
        let st = GitPublicMirrorStatus
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let sv: Value = serde_json::from_str(&st).unwrap();
        // Baseline must be the CLOSEST approved ancestor (c2, dist 1), not c1 (dist 2).
        assert_eq!(sv["last_approved_internal_sha"], c2, "closest approved baseline wins");
        assert_eq!(sv["commits_since_last_approved"], 1);

        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn status_before_prepare_flags_needs_prepare() {
        clear_env();
        let src = init_source(&[("a.txt", "clean\n")]);
        let root = unique("root");
        set_root(&root);

        let st = GitPublicMirrorStatus
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

        let prep = GitPublicMirrorPrepare
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let out = GitPublicMirrorApprove
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let out = GitPublicMirrorApprove
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
            GitPublicMirrorApprove
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let res = GitPublicMirrorPush
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
        // fast-forwardable remote must STILL refuse push until git_public_mirror_approve
        // has blessed it — prepare's machine tag alone is not push authorisation.
        clear_env();
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitPublicMirrorPrepare
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let res = GitPublicMirrorPush
            .execute(json!({
                "repo": "Terminus",
                "source": src.display().to_string(),
                "github_remote": bare.display().to_string()
            }))
            .await;
        match res {
            Err(ToolError::Conflict(m)) => assert!(
                m.contains("git_public_mirror_approve"),
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src); // operator-approve stand-in

        let res = GitPublicMirrorPush
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src); // operator-approve stand-in
        // blessed, but no github_remote arg and no env → NotConfigured.
        let res = GitPublicMirrorPush
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
        GitPublicMirrorPrepare
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src);

        let out = GitPublicMirrorPush
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
            let res = GitPublicMirrorStatus
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
    fn gate_content_binding_injects_resolved_identity() {
        let args = json!({ "repo": "Terminus", "source": "/x", "_approval_code": "Z" });
        let b = gate_content_binding(&args, "abc123", Some("commitxyz"));
        assert_eq!(b["internal_sha"], "abc123");
        assert_eq!(b["approved_commit"], "commitxyz");
        assert_eq!(b["repo"], "Terminus");
        // A different resolved sha yields different gate content → a stale code
        // (bound to the old sha) cannot match.
        let other = gate_content_binding(&args, "def456", Some("commitxyz"));
        assert_ne!(b["internal_sha"], other["internal_sha"]);
        // approved_commit omitted for the approve-without-commit shape.
        let b2 = gate_content_binding(&args, "abc123", None);
        assert!(b2.get("approved_commit").is_none());
        assert_eq!(b2["internal_sha"], "abc123");
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
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src);
        let res = GitPublicMirrorPush
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
            GitPublicMirrorStatus
                .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
                .await,
            GitPublicMirrorPrepare
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
        let ok = GitPublicMirrorPrepare
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

    // ── MIRR-01: configurable source root ("parking lot") ────────────────────

    #[tokio::test]
    #[serial]
    async fn source_derives_from_configured_root_when_arg_omitted() {
        clear_env();
        let source_root = unique("source-root");
        let repo = "Terminus";
        let src = source_root.join(repo);
        init_source_at(&src, &[("a.txt", "clean\n")]);
        std::env::set_var("TERMINUS_MIRROR_SOURCE_ROOT", &source_root);
        let root = unique("root");
        set_root(&root);

        let st = GitPublicMirrorStatus.execute(json!({ "repo": repo })).await.unwrap();
        let sv: Value = serde_json::from_str(&st).unwrap();
        assert_eq!(
            sv["internal_sha"],
            run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim(),
            "source resolved as TERMINUS_MIRROR_SOURCE_ROOT/<repo>"
        );

        std::env::remove_var("TERMINUS_MIRROR_SOURCE_ROOT");
        cleanup(&[&source_root, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn explicit_source_arg_overrides_configured_root() {
        clear_env();
        // The configured root points at a location with NO 'Terminus' checkout —
        // if it were ever consulted, ensure_source_is_main would fail. It must be
        // ignored entirely because 'source' is passed explicitly.
        let bogus_root = unique("bogus-root");
        std::env::set_var("TERMINUS_MIRROR_SOURCE_ROOT", &bogus_root);
        let src = init_source(&[("a.txt", "clean\n")]);
        let root = unique("root");
        set_root(&root);

        let st = GitPublicMirrorStatus
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let sv: Value = serde_json::from_str(&st).unwrap();
        assert_eq!(sv["internal_sha"], run_git(&src, &["rev-parse", "HEAD"]).unwrap().trim());

        std::env::remove_var("TERMINUS_MIRROR_SOURCE_ROOT");
        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn source_root_unset_and_source_absent_is_not_configured() {
        clear_env();
        let root = unique("root");
        set_root(&root);
        // Neither an explicit 'source' arg nor TERMINUS_MIRROR_SOURCE_ROOT — a
        // clear NotConfigured error, distinct from a residual/blocked state.
        let res = GitPublicMirrorPrepare.execute(json!({ "repo": "Terminus" })).await;
        assert!(matches!(res, Err(ToolError::NotConfigured(_))));
        cleanup(&[&root]);
    }

    // ── MIRR-02: auto-approve / auto-push on a verified-clean sweep ──────────

    #[tokio::test]
    #[serial]
    async fn auto_approve_bypasses_gate_when_snapshot_is_verified_clean() {
        clear_env(); // DATABASE_URL unset — proves the gate is genuinely skipped, not just granted
        let src = init_source(&[("a.txt", "clean content\n")]);
        let root = unique("root");
        set_root(&root);
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        std::env::set_var("TERMINUS_MIRROR_AUTO_APPROVE", "true");

        let out = GitPublicMirrorApprove
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["approved"], true);
        assert_eq!(v["auto_approved"], true);
        assert!(v.get("approval_required").is_none(), "no operator code should be requested");

        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        let sha = wd.source_head_sha().unwrap();
        assert!(blessed_commit(wd.path(), &sha).unwrap().is_some(), "auto-approve must actually bless");

        std::env::remove_var("TERMINUS_MIRROR_AUTO_APPROVE");
        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn auto_push_bypasses_gate_when_blessed_and_fast_forward() {
        clear_env();
        // Auto-push (unlike the guarded path in push_blessed_and_fast_forwardable_
        // reaches_the_guard) actually reaches token resolution, since there is no
        // approval_required stop — the local bare remote never invokes askpass, but
        // mirror_provider_token still needs a resolvable credential.
        std::env::set_var("GITHUB_TOKEN", "unused-local-test-token"); // pii-test-fixture
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        let c1 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        let bare = init_bare();
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c1}:refs/heads/main")]).unwrap();
        // Advance so there is a genuine fast-forward available.
        write_file(&src, "a.txt", "v2 clean\n");
        commit_all(&src, "v2");
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        std::env::set_var("TERMINUS_MIRROR_AUTO_APPROVE", "true");
        let ap = GitPublicMirrorApprove
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let apv: Value = serde_json::from_str(&ap).unwrap();
        assert_eq!(apv["auto_approved"], true);

        let out = GitPublicMirrorPush
            .execute(json!({
                "repo": "Terminus",
                "source": src.display().to_string(),
                "github_remote": bare.display().to_string()
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["pushed"], true);
        assert_eq!(v["auto_pushed"], true);
        assert!(v.get("approval_required").is_none());

        let c2 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        let tip = run_git(&bare, &["rev-parse", "refs/heads/main"]).unwrap().trim().to_string();
        assert_eq!(tip, c2, "auto-push must actually advance the mirror");

        std::env::remove_var("TERMINUS_MIRROR_AUTO_APPROVE");
        std::env::remove_var("GITHUB_TOKEN");
        cleanup(&[&src, &root, &bare]);
    }

    #[tokio::test]
    #[serial]
    async fn auto_approve_off_still_requires_the_operator_code() {
        // Explicit regression companion to approve_clean_snapshot_reaches_the_guard
        // / push_blessed_and_fast_forwardable_reaches_the_guard: with the flag
        // unset (default FALSE), both approve and push must still stop at the
        // guarded gate.
        clear_env();
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();

        let ap = GitPublicMirrorApprove
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let apv: Value = serde_json::from_str(&ap).unwrap();
        assert_eq!(apv["approved"], false);
        assert_eq!(apv["approval_required"], true);
        assert!(apv.get("auto_approved").is_none());

        bless("Terminus", &src); // operator-approve stand-in so push can be reached
        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        let c1 = wd.approved_commit(&wd.source_head_sha().unwrap()).unwrap().unwrap();
        let bare = init_bare();
        run_git(wd.path(), &["push", &bare.display().to_string(), &format!("{c1}:refs/heads/main")]).unwrap();
        write_file(&src, "a.txt", "v2 clean\n");
        commit_all(&src, "v2");
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src);

        let out = GitPublicMirrorPush
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
        assert!(v.get("auto_pushed").is_none());

        cleanup(&[&src, &root, &bare]);
    }

    #[tokio::test]
    #[serial]
    async fn auto_approve_does_not_fire_without_a_clean_approved_tag() {
        clear_env();
        // Residual (non-mechanical) violation → prepare never creates the
        // mirror-approved/<sha> tag, so there is no 0-residual proof to act on.
        let src = init_source(&[(
            "c.txt",
            "token = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        )]);
        let root = unique("root");
        set_root(&root);
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        std::env::set_var("TERMINUS_MIRROR_AUTO_APPROVE", "true");

        let out = GitPublicMirrorApprove
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["approved"], false);
        assert!(v["reason"].as_str().unwrap().contains("residual"));
        assert!(v.get("auto_approved").is_none(), "auto-approve must never fire on a dirty sweep");

        let wd = MirrorWorkDir::from_config("Terminus", &src).unwrap();
        let sha = wd.source_head_sha().unwrap();
        assert!(
            blessed_commit(wd.path(), &sha).unwrap().is_none(),
            "the hard PII block must be untouched: a dirty sweep is never blessed, flag or no flag"
        );

        std::env::remove_var("TERMINUS_MIRROR_AUTO_APPROVE");
        cleanup(&[&src, &root]);
    }

    #[tokio::test]
    #[serial]
    async fn auto_push_still_refuses_non_fast_forward() {
        clear_env();
        let src = init_source(&[("a.txt", "v1 clean\n")]);
        let root = unique("root");
        set_root(&root);
        GitPublicMirrorPrepare
            .execute(json!({ "repo": "Terminus", "source": src.display().to_string() }))
            .await
            .unwrap();
        bless("Terminus", &src);

        // Seed the bare mirror with a commit from a totally independent history
        // (no shared ancestor with the Terminus work dir) — any push to it is
        // structurally non-fast-forward, regardless of the auto-approve flag.
        let other_src = init_source(&[("z.txt", "unrelated\n")]);
        let other_root = unique("other-root");
        std::env::set_var("TERMINUS_MIRROR_WORKDIR_ROOT", &other_root);
        let other_wd = MirrorWorkDir::from_config("Other", &other_src).unwrap();
        other_wd.run().unwrap();
        let other_c = other_wd.approved_commit(&other_wd.source_head_sha().unwrap()).unwrap().unwrap();
        let bare = init_bare();
        run_git(other_wd.path(), &["push", &bare.display().to_string(), &format!("{other_c}:refs/heads/main")])
            .unwrap();
        std::env::set_var("TERMINUS_MIRROR_WORKDIR_ROOT", &root); // restore for Terminus

        std::env::set_var("TERMINUS_MIRROR_AUTO_APPROVE", "true");
        let res = GitPublicMirrorPush
            .execute(json!({
                "repo": "Terminus",
                "source": src.display().to_string(),
                "github_remote": bare.display().to_string()
            }))
            .await;
        assert!(
            matches!(res, Err(ToolError::Conflict(_))),
            "non-fast-forward must refuse even with TERMINUS_MIRROR_AUTO_APPROVE on: {res:?}"
        );
        // The remote must not have moved.
        let tip = run_git(&bare, &["rev-parse", "refs/heads/main"]).unwrap().trim().to_string();
        assert_eq!(tip, other_c);

        std::env::remove_var("TERMINUS_MIRROR_AUTO_APPROVE");
        cleanup(&[&src, &root, &other_src, &other_root, &bare]);
    }

    // ── git_public_mirror_sync_source (S111E / MIRR-04) ─────────────────────

    /// Set the trio of Gitea env vars `sync-source`'s `gitea_token()` call
    /// needs to resolve without hitting the network (GITEA_URL just needs to
    /// be *set*, never actually contacted — the test remotes below are local
    /// filesystem paths, so git never invokes GIT_ASKPASS against them and the
    /// token value itself is never used, only resolved). Returns the prior
    /// values so callers can restore them.
    fn set_dummy_gitea_env(token: &str) -> (Option<String>, Option<String>, Option<String>) {
        let url = std::env::var("GITEA_URL").ok();
        let pat = std::env::var("GITEA_PAT_MOOSE").ok();
        let identity = std::env::var("GITEA_IDENTITY_NAME").ok();
        std::env::set_var("GITEA_URL", "http://example.invalid"); // pii-test-fixture
        std::env::set_var("GITEA_PAT_MOOSE", token);
        std::env::remove_var("GITEA_IDENTITY_NAME");
        (url, pat, identity)
    }

    fn restore_gitea_env(saved: (Option<String>, Option<String>, Option<String>)) {
        let (url, pat, identity) = saved;
        match url { Some(v) => std::env::set_var("GITEA_URL", v), None => std::env::remove_var("GITEA_URL") }
        match pat { Some(v) => std::env::set_var("GITEA_PAT_MOOSE", v), None => std::env::remove_var("GITEA_PAT_MOOSE") }
        match identity { Some(v) => std::env::set_var("GITEA_IDENTITY_NAME", v), None => std::env::remove_var("GITEA_IDENTITY_NAME") }
    }

    #[test]
    #[serial]
    fn sync_source_resolve_internal_remote_prefers_explicit_then_per_repo_then_fallback() {
        clear_env();
        // Nothing configured -> NotConfigured, names the env vars.
        let err = resolve_internal_remote(&json!({}), "demo").unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
        assert!(err.to_string().contains("TERMINUS_MIRROR_INTERNAL_REMOTE"));

        // Generic fallback.
        std::env::set_var("TERMINUS_MIRROR_INTERNAL_REMOTE", "http://gitea.example/moosenet/demo.git"); // pii-test-fixture
        assert_eq!(
            resolve_internal_remote(&json!({}), "demo").unwrap(),
            "http://gitea.example/moosenet/demo.git" // pii-test-fixture
        );

        // Per-repo env wins over the generic fallback.
        std::env::set_var("TERMINUS_MIRROR_INTERNAL_REMOTE_DEMO", "http://gitea.example/moosenet/demo-specific.git"); // pii-test-fixture
        assert_eq!(
            resolve_internal_remote(&json!({}), "demo").unwrap(),
            "http://gitea.example/moosenet/demo-specific.git" // pii-test-fixture
        );

        // An explicit arg wins over everything.
        assert_eq!(
            resolve_internal_remote(&json!({"internal_remote": "http://gitea.example/moosenet/explicit.git"}), "demo").unwrap(), // pii-test-fixture
            "http://gitea.example/moosenet/explicit.git" // pii-test-fixture
        );
        clear_env();
    }

    #[test]
    fn sync_source_assert_source_sync_safe_allows_only_the_sanctioned_hard_reset_shape() {
        // The sanctioned shape must pass.
        assert_source_sync_safe(&["reset", "--hard", "origin/main"]);
        // Non-hard-reset argv (no --hard at all) is always fine.
        assert_source_sync_safe(&["fetch", "--", "origin", "main"]);
        assert_source_sync_safe(&["checkout", "main"]);
    }

    #[test]
    #[should_panic(expected = "force")]
    fn sync_source_assert_source_sync_safe_rejects_force_flag() {
        assert_source_sync_safe(&["push", "--force", "origin", "main"]);
    }

    #[test]
    #[should_panic(expected = "--hard")]
    fn sync_source_assert_source_sync_safe_rejects_hard_outside_sanctioned_shape() {
        // `--hard` against a branch that is NOT `origin/<branch>` (e.g. a
        // caller-supplied ref) must still be refused — only
        // `reset --hard origin/<branch>` is tolerated.
        assert_source_sync_safe(&["reset", "--hard", "some-other-ref"]);
    }

    #[tokio::test]
    #[serial]
    async fn sync_source_missing_root_and_no_explicit_source_is_not_configured() {
        clear_env();
        let err = GitPublicMirrorSyncSource
            .execute(json!({"repo": "demo"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
        assert!(err.to_string().contains("TERMINUS_MIRROR_SOURCE_ROOT"));
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn sync_source_clones_when_absent_then_fetches_and_resets_when_present() {
        clear_env();
        let remote = init_source(&[("f.txt", "v1")]);
        let root = unique("sync-root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("TERMINUS_MIRROR_SOURCE_ROOT", &root);
        std::env::set_var("TERMINUS_MIRROR_INTERNAL_REMOTE", remote.display().to_string());
        let saved = set_dummy_gitea_env("dummy-clone-token"); // pii-test-fixture

        let out = GitPublicMirrorSyncSource.execute(json!({"repo": "demo"})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cloned"], true, "first sync must clone: {v}");
        assert_eq!(v["branch"], "main");
        let source = root.join("demo");
        assert!(source.join(".git").exists());
        let remote_head_after_clone = run_git(&remote, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        assert_eq!(v["head_sha"], remote_head_after_clone);

        // Advance the "remote" (a plain local repo standing in for internal
        // Gitea main) with a second commit, then re-sync — this must fetch +
        // hard-reset the existing checkout, NOT re-clone.
        write_file(&remote, "f.txt", "v2");
        commit_all(&remote, "second");
        let remote_head_after_second = run_git(&remote, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        assert_ne!(remote_head_after_clone, remote_head_after_second);

        let out2 = GitPublicMirrorSyncSource.execute(json!({"repo": "demo"})).await.unwrap();
        let v2: Value = serde_json::from_str(&out2).unwrap();
        assert_eq!(v2["cloned"], false, "second sync must fetch+reset, not re-clone: {v2}");
        assert_eq!(v2["head_sha"], remote_head_after_second);

        // The persisted git config must carry the plain remote path — never a
        // credential embedded in the URL (auth goes only through GIT_ASKPASS).
        let origin_url = run_git(&source, &["config", "--get", "remote.origin.url"]).unwrap().trim().to_string();
        assert_eq!(origin_url, remote.display().to_string());
        assert!(!origin_url.contains("dummy-clone-token"));

        restore_gitea_env(saved);
        cleanup(&[&remote, &root]);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn sync_source_token_never_leaks_into_error_output() {
        clear_env();
        let root = unique("sync-root-err");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("TERMINUS_MIRROR_SOURCE_ROOT", &root);
        // A remote path that does not exist -> clone fails, but the failure
        // must never echo the resolved token.
        let bogus_remote = unique("does-not-exist");
        std::env::set_var("TERMINUS_MIRROR_INTERNAL_REMOTE", bogus_remote.display().to_string());
        let very_distinctive_token = "<REDACTED-SECRET>"; // pii-test-fixture
        let saved = set_dummy_gitea_env(very_distinctive_token);

        let err = GitPublicMirrorSyncSource
            .execute(json!({"repo": "demo"}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(very_distinctive_token),
            "error message must never contain the raw token: {msg}"
        );

        restore_gitea_env(saved);
        cleanup(&[&root]);
        clear_env();
    }

    #[tokio::test]
    #[serial]
    async fn dispatch_mirror_action_routes_sync_source() {
        clear_env();
        // Same NotConfigured failure mode as calling the tool directly —
        // proves the dispatcher actually forwards to GitPublicMirrorSyncSource
        // rather than silently no-op'ing or hitting the wrong tool.
        let err = dispatch_mirror_action("sync-source", json!({"repo": "demo"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
        assert!(err.to_string().contains("TERMINUS_MIRROR_SOURCE_ROOT"));
        clear_env();
    }

    #[test]
    fn sync_source_tool_is_registered() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("git_public_mirror_sync_source"));
    }
}
