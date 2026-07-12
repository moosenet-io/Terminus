//! GHIST-05: PR-process replay — reproduce an internal (private-forge) pull request
//! as a scrubbed, attributed pull request on the PUBLIC forge, so the public mirrors
//! show the review PROCESS, not just a commit stream.
//!
//! Provider-agnostic by construction: the internal PR (and its discussion thread) is
//! read through the private-pool `ForgeEndpoint` dispatch, and the public PR is
//! opened / commented / merged through the public-pool dispatch — never a
//! GitHub-specific client. The scrubbed feature branch is produced by
//! [`super::history::replay_pr_slice`] (rebased onto the current public-main tip) and
//! pushed over git transport with the resolved public credential.
//!
//! Safety spine, mirroring the rest of the engine:
//! - Only MERGED internal PRs are replayed (an open PR has no final content).
//! - Title / body / every conversation comment is PII-SCRUBBED (the same
//!   `DeterministicCleaner` the blob replay uses) before any public write. Comment
//!   scope is the PR CONVERSATION thread (issue-style comments) — inline per-file
//!   review-diff comments live on a separate surface and are out of scope.
//! - Provider-agnostic transport: the public fetch/push credential is supplied by the
//!   caller (resolved through the mirror-token seam), NOT a hardcoded GitHub token, so
//!   a GitLab/Codeberg public destination works too. Internal-PR fields are read
//!   tolerantly across gitea/github/gitlab response shapes.
//! - Idempotent: a `mirror-pr/<n>` tag records a completed PR (skip); a leftover
//!   remote feature branch from a partial prior run is refused rather than
//!   double-created.
//! - The public PR's MERGE is what lands the commits on public main; afterwards the
//!   engine fetches public main and records it as the published boundary, so the
//!   next PR replays onto it (PRs replay in merge order — enforced by
//!   `replay_pr_slice`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::error::ToolError;
use crate::forge::capability::ForgeEndpoint;
use crate::forge::provider::ForgeRequest;
use crate::forge::registry::{ForgePool, ForgeRegistry};

use super::history::{
    last_mirrored_sha, replay_pr_slice, restore_canonical, set_pushed_sha, snapshot_canonical,
    IdentityMap, ReplayOpts,
};
use super::native_clean::DeterministicCleaner;

const HOOKS_OFF: &[&str] = &["-c", "core.hooksPath=/dev/null"];

/// Inputs for one PR replay.
pub struct PrReplayConfig {
    /// Logical repo name (also the default private + public repo name).
    pub repo: String,
    /// Internal-main checkout the PR's commits are read from (the replay source).
    pub source: PathBuf,
    /// The full-history mirror work-dir (the canonical scrubbed lineage lives here).
    pub work_dir: PathBuf,
    /// Public remote URL (the mirror) — where the feature branch is pushed.
    pub remote: String,
    /// Optional owner overrides (default to the pool's configured owner env).
    pub public_owner: Option<String>,
    pub private_owner: Option<String>,
    /// Optional repo-name overrides per side (default to `repo`).
    pub public_repo: Option<String>,
    pub private_repo: Option<String>,
    /// Optional explicit provider ids (default to the pool default: gitea / github).
    pub private_provider: Option<String>,
    pub public_provider: Option<String>,
    /// Attribution map (fail-closed at the call site).
    pub author_map: IdentityMap,
    /// Public merge method: "squash" (default) | "merge" | "rebase".
    pub merge_method: String,
    /// The git-transport credential for the PUBLIC remote (fetch/push), resolved by
    /// the caller through the provider-agnostic mirror-token seam (so a GitLab/
    /// Codeberg public destination supplies its own token, not GitHub's). Provider-
    /// agnostic: `pr_replay` only ever sees an opaque token string here.
    pub transport_token: String,
}

/// Outcome of a single PR replay.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PrReplayOutcome {
    pub repo: String,
    pub internal_pr: u64,
    /// True when the PR was already mirrored (idempotent no-op).
    pub skipped: bool,
    /// True when a public PR was opened + merged this run.
    pub replayed: bool,
    pub public_pr: Option<u64>,
    pub branch: String,
    pub commits: usize,
    pub comments_mirrored: usize,
    pub public_head: Option<String>,
    pub note: String,
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, ToolError> {
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

/// Scrub a text field with the same deterministic cleaner the blob replay uses, so
/// public PR text carries no fleet identifiers / bounded secrets. Non-UTF-8 or
/// oversized input is passed through by the cleaner; the result is always UTF-8 here
/// (PR text is small text).
fn scrub_text(s: &str) -> String {
    String::from_utf8_lossy(&DeterministicCleaner::scrub_bytes(s.as_bytes())).into_owned()
}

/// Minimal GIT_ASKPASS helper echoing `$GIT_MIRROR_TOKEN` (no secret in the file).
fn write_askpass() -> Result<(PathBuf, AskpassGuard), ToolError> {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "ghist05-askpass-{}-{}.sh",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let mut f = std::fs::File::create(&path)
        .map_err(|e| ToolError::Execution(format!("create askpass: {e}")))?;
    f.write_all(b"#!/bin/sh\nprintf '%s\\n' \"$GIT_MIRROR_TOKEN\"\n")
        .map_err(|e| ToolError::Execution(format!("write askpass: {e}")))?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ToolError::Execution(format!("chmod askpass: {e}")))?;
    }
    Ok((path.clone(), AskpassGuard { path }))
}

struct AskpassGuard {
    path: PathBuf,
}
impl Drop for AskpassGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A tokened git transport call (fetch / push) with the credential injected via
/// GIT_ASKPASS only — never in argv, the URL, or on disk.
fn git_transport(work_dir: &Path, args: &[&str], token: &str) -> Result<(), ToolError> {
    let (askpass, _guard) = write_askpass()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(HOOKS_OFF)
        .args(args)
        .env("GIT_ASKPASS", &askpass)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_MIRROR_TOKEN", token)
        .output()
        .map_err(|e| ToolError::Execution(format!("git transport {args:?}: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).replace(token, "<redacted>");
        return Err(ToolError::Execution(format!("git {args:?} failed: {}", stderr.trim())));
    }
    Ok(())
}

/// Fetch the public mirror's `main` into the work-dir and return its tip sha.
fn fetch_public_main(work_dir: &Path, remote: &str, token: &str) -> Result<String, ToolError> {
    git_transport(work_dir, &["fetch", "--quiet", remote, "refs/heads/main"], token)?;
    Ok(git(work_dir, &["rev-parse", "FETCH_HEAD"])?.trim().to_string())
}

/// True if the public remote already has a `refs/heads/<branch>` — used to detect a
/// PARTIAL prior run (branch pushed, PR create/merge incomplete) and refuse rather
/// than create a duplicate. ls-remote reads the public repo (no write).
fn remote_branch_exists(work_dir: &Path, remote: &str, branch: &str, token: &str) -> Result<bool, ToolError> {
    let (askpass, _guard) = write_askpass()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(HOOKS_OFF)
        .args(["ls-remote", "--heads", "--", remote, &format!("refs/heads/{branch}")])
        .env("GIT_ASKPASS", &askpass)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_MIRROR_TOKEN", token)
        .output()
        .map_err(|e| ToolError::Execution(format!("git ls-remote: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).replace(token, "<redacted>");
        return Err(ToolError::Execution(format!("git ls-remote failed: {}", stderr.trim())));
    }
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

// ── Provider-tolerant field extraction ───────────────────────────────────────
// Forge adapters return raw provider JSON. gitea/github share a shape (base.sha /
// head.sha / merged:bool / number); GitLab MRs differ (diff_refs.*_sha / sha / iid /
// state=="merged"). These read either shape so the orchestration is truly
// provider-agnostic, not gitea/github-only.

fn pr_is_merged(pr: &Value) -> bool {
    pr.get("merged").and_then(Value::as_bool).unwrap_or(false)
        || pr.get("state").and_then(Value::as_str) == Some("merged")
        || pr.get("merged_at").map(|v| !v.is_null()).unwrap_or(false)
}

fn pr_number(created: &Value) -> Option<u64> {
    created
        .get("number")
        .and_then(Value::as_u64)
        .or_else(|| created.get("iid").and_then(Value::as_u64))
}

/// The internal commit this PR's merge landed at — the internal-main tip after the
/// merge. gitea/github: `merge_commit_sha`; gitlab merged MR: `merge_commit_sha` (or
/// `squash_commit_sha` when squashed). NOTE: gitlab's `sha` is the MR's HEAD diff sha,
/// NOT the merge commit — never use it here.
fn pr_merge_commit(pr: &Value) -> Option<String> {
    pr.get("merge_commit_sha")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| pr.get("squash_commit_sha").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .map(String::from)
}

/// A PR's head branch ref, across gitea/github (`head.ref`) and gitlab (`source_branch`).
fn pr_head_ref(pr: &Value) -> Option<String> {
    pr.pointer("/head/ref")
        .and_then(Value::as_str)
        .or_else(|| pr.get("source_branch").and_then(Value::as_str))
        .map(String::from)
}

/// Read the FULL PR conversation thread across pages, provider-agnostically. gitea
/// returns one page per call (caller-paged); github/gitlab auto-follow pages and
/// return everything on the first call. De-duping by comment `id` and stopping when a
/// page adds no new comment handles BOTH: a single-page provider keeps paging; a
/// return-all provider stops after its second call (all dupes). Returns scrubbed,
/// non-empty comment bodies in order.
async fn read_all_comments(
    reg: &ForgeRegistry,
    provider: Option<&str>,
    repo: &str,
    owner: Option<&str>,
    pr: u64,
) -> Result<Vec<String>, ToolError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut page = 1u64;
    loop {
        let mut params = json!({ "repo": repo, "index": pr, "number": pr, "page": page, "limit": 50 });
        if let Some(o) = owner {
            params["owner"] = json!(o);
        }
        let body = read_private(reg, provider, ForgeEndpoint::PullRequestsListComments, params).await?;
        let arr = body.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            break;
        }
        let mut new_in_page = 0usize;
        for c in &arr {
            // Stable per-comment key: the `id` (any JSON scalar) else the whole object.
            let key = c.get("id").map(|v| v.to_string()).unwrap_or_else(|| c.to_string());
            if seen.insert(key) {
                new_in_page += 1;
                // Skip auto-generated SYSTEM notes (GitLab marks lifecycle events —
                // "changed milestone", "assigned", etc. — with `system: true`); those
                // are not human review discussion and must not be mirrored as comments.
                if c.get("system").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if let Some(b) = c.get("body").and_then(Value::as_str) {
                    if !b.trim().is_empty() {
                        out.push(scrub_text(b));
                    }
                }
            }
        }
        if new_in_page == 0 {
            break; // return-all provider: this page repeated the first — done.
        }
        page += 1;
        if page > 500 {
            break; // safety cap — no realistic PR thread is this long.
        }
    }
    Ok(out)
}

/// Bounded scan of the PUBLIC forge for an existing PR whose head branch is `branch`
/// (any state). Returns the matching PR object so the caller can distinguish a MERGED
/// one (the work is done) from an OPEN/partial one (a prior run stalled — must NOT be
/// treated as done). Used for durable idempotency across a lost work-dir / a crash
/// after the public merge but before the local tag.
async fn public_pr_for_branch(
    reg: &ForgeRegistry,
    cfg: &PrReplayConfig,
    pub_repo: &str,
    branch: &str,
) -> Result<Option<Value>, ToolError> {
    let mut page = 1u64;
    loop {
        let mut params = json!({ "repo": pub_repo, "state": "all", "page": page, "limit": 50 });
        if let Some(o) = &cfg.public_owner {
            params["owner"] = json!(o);
        }
        let body = call_public(reg, cfg.public_provider.as_deref(), ForgeEndpoint::PullRequestsList, params).await?;
        let arr = body.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            break;
        }
        for pr in &arr {
            if pr_head_ref(pr).as_deref() == Some(branch) {
                return Ok(Some(pr.clone()));
            }
        }
        page += 1;
        if page > 10 {
            break; // bounded — a freshly-created mirror PR is on the first pages.
        }
    }
    Ok(None)
}

/// True if a remote URL embeds userinfo (`scheme://user[:pass]@host`). A `file://`
/// path or a bare `https://host/…` URL has no `@` before the first path `/`, so this
/// stays false for legitimate remotes and true for a credential-bearing one.
fn remote_has_userinfo(remote: &str) -> bool {
    let after_scheme = remote.split_once("://").map(|(_, rest)| rest).unwrap_or(remote);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@')
}

fn pr_tag(n: u64) -> String {
    format!("mirror-pr/{n}")
}

fn already_replayed(work_dir: &Path, n: u64) -> bool {
    git(work_dir, &["tag", "-l", &pr_tag(n)])
        .map(|s| s.lines().any(|l| l.trim() == pr_tag(n)))
        .unwrap_or(false)
}

/// A private-pool read: dispatch `endpoint` with `params`, returning the response body.
async fn read_private(
    reg: &ForgeRegistry,
    provider: Option<&str>,
    endpoint: ForgeEndpoint,
    params: Value,
) -> Result<Value, ToolError> {
    let p = reg.resolve(ForgePool::Private, provider)?;
    let resp = p
        .dispatch(endpoint, ForgeRequest::new(params))
        .await
        .map_err(|e| ToolError::Execution(format!("private forge {}: {e}", endpoint.as_str())))?;
    Ok(resp.body)
}

/// A public-pool write/read: dispatch `endpoint` with `params`, returning the body.
async fn call_public(
    reg: &ForgeRegistry,
    provider: Option<&str>,
    endpoint: ForgeEndpoint,
    params: Value,
) -> Result<Value, ToolError> {
    let p = reg.resolve(ForgePool::Public, provider)?;
    let resp = p
        .dispatch(endpoint, ForgeRequest::new(params))
        .await
        .map_err(|e| ToolError::Execution(format!("public forge {}: {e}", endpoint.as_str())))?;
    Ok(resp.body)
}

/// Reproduce internal PR `internal_pr` as a scrubbed public PR. See the module docs
/// for the full contract. Returns a structured outcome (idempotent skip, a "not
/// merged" no-op, or a completed replay).
pub async fn replay_pr(
    reg: &ForgeRegistry,
    cfg: &PrReplayConfig,
    internal_pr: u64,
) -> Result<PrReplayOutcome, ToolError> {
    let branch = format!("pr-mirror/{internal_pr}");
    let priv_repo = cfg.private_repo.clone().unwrap_or_else(|| cfg.repo.clone());
    let pub_repo = cfg.public_repo.clone().unwrap_or_else(|| cfg.repo.clone());

    let mut base_outcome = PrReplayOutcome {
        repo: cfg.repo.clone(),
        internal_pr,
        skipped: false,
        replayed: false,
        public_pr: None,
        branch: branch.clone(),
        commits: 0,
        comments_mirrored: 0,
        public_head: None,
        note: String::new(),
    };

    // The credential is injected only via GIT_ASKPASS — a remote URL must never carry
    // embedded userinfo (`scheme://user:pass@host`), which would leak into process
    // listings / reflogs and bypass the askpass path. Refuse it.
    if remote_has_userinfo(&cfg.remote) {
        return Err(ToolError::InvalidArgument(
            "the public remote URL must not embed credentials (scheme://user:pass@host) — the \
             token is supplied via GIT_ASKPASS only. Configure a bare remote URL."
                .into(),
        ));
    }

    // Idempotency (local): a recorded PR is never re-created.
    if already_replayed(&cfg.work_dir, internal_pr) {
        base_outcome.skipped = true;
        base_outcome.note = "already mirrored (mirror-pr tag present) — skipped".into();
        return Ok(base_outcome);
    }

    // 1. Read the internal PR.
    let mut pr_params = json!({ "repo": priv_repo, "index": internal_pr, "number": internal_pr });
    if let Some(o) = &cfg.private_owner {
        pr_params["owner"] = json!(o);
    }
    let pr = read_private(reg, cfg.private_provider.as_deref(), ForgeEndpoint::PullRequestsGet, pr_params).await?;

    if !pr_is_merged(&pr) {
        base_outcome.note = "internal PR is not merged — nothing to replay yet".into();
        return Ok(base_outcome);
    }
    // The replay range is the internal commits this PR ADDED to internal main:
    // base_int = the canonical scrubbed lineage's current internal position
    // (last_mirrored_sha — i.e. internal main just BEFORE this PR), and head_int = the
    // PR's merge commit (internal main just AFTER it). base_int..head_int is exactly the
    // PR's commits (+ its merge commit). This auto-satisfies replay_pr_slice's order
    // guard (base_int == last_mirrored) and ff guard (head_int descends from base_int),
    // and — unlike the PR object's base.sha (the post-merge base-branch tip) — is never
    // empty for a normal merged PR. It requires PRs to replay in merge ORDER.
    let head_int = pr_merge_commit(&pr).ok_or_else(|| {
        ToolError::Execution("internal PR has no merge commit sha — cannot determine its range".into())
    })?;
    let base_int = last_mirrored_sha(&cfg.work_dir).ok_or_else(|| {
        ToolError::Conflict(
            "no canonical scrubbed lineage — run git_public_history_backfill first".into(),
        )
    })?;

    // Idempotency (DURABLE): scan the public forge for an existing PR on this branch.
    // Now that head_int is known we can verify consistency, not just presence:
    //   - a MERGED public PR AND the canonical lineage already at head_int
    //     (base_int == head_int) → the work is truly done; reconcile the boundary from
    //     public main + tag, then skip.
    //   - a MERGED public PR but the canonical lineage is NOT at head_int → an
    //     inconsistent state (a stale / manual / out-of-band merge): REFUSE
    //     (fail-closed) rather than advance the boundary and desync merge order.
    //   - an OPEN/partial public PR → fall through to the remote-branch guard, which
    //     refuses it as an incomplete prior run.
    let pub_repo_probe = cfg.public_repo.clone().unwrap_or_else(|| cfg.repo.clone());
    if let Some(existing) = public_pr_for_branch(reg, cfg, &pub_repo_probe, &branch).await? {
        if pr_is_merged(&existing) {
            if base_int != head_int {
                return Err(ToolError::Conflict(format!(
                    "public PR for '{branch}' is MERGED but the canonical scrubbed lineage is at \
                     {base_int}, not this PR's merge {head_int} — the mirror is out of sync with \
                     the public repo (a stale / manual / out-of-band merge). Reconcile before \
                     re-running; git_public_mirror_replay_pr will not advance the boundary over an \
                     inconsistent state."
                )));
            }
            let new_public = fetch_public_main(&cfg.work_dir, &cfg.remote, &cfg.transport_token)?;
            set_pushed_sha(&cfg.work_dir, &new_public)?;
            git(&cfg.work_dir, &["tag", "-f", &pr_tag(internal_pr), &new_public])?;
            base_outcome.skipped = true;
            base_outcome.public_head = Some(new_public);
            base_outcome.note = format!(
                "public PR for '{branch}' already MERGED and canonical lineage consistent — \
                 boundary reconciled + tagged, skipped (durable idempotency)"
            );
            return Ok(base_outcome);
        }
        // An OPEN / non-merged public PR exists — a stalled prior run. Fall through to
        // the remote-branch guard below, which refuses it (never silently 'done').
    }

    // ORDER guard: this PR must be the NEXT unreplayed one, or base_int..head_int would
    // span an earlier pending PR and publish its work under THIS PR's title/body/
    // comments (mis-attribution). The only way to VERIFY order from git is via a merge
    // commit's parents: a merge commit's FIRST parent is internal main just before the
    // merge, so it must equal the canonical tip (base_int). A single-parent merge
    // (fast-forward / squash) leaves no such marker — order cannot be guaranteed — so
    // we REFUSE rather than risk a silent mis-attribution (fail-closed). The fleet uses
    // merge-commit PRs, so this is the normal path; a FF/squash-merged repo must
    // publish going-forward via git_public_history_sync instead.
    let parents: Vec<String> = git(&cfg.source, &["rev-list", "--parents", "-n", "1", &head_int])
        .map_err(|e| {
            ToolError::Execution(format!("cannot read parents of {head_int} for PR {internal_pr}: {e}"))
        })?
        .split_whitespace()
        .skip(1) // first token is head_int itself; the rest are its parents
        .map(String::from)
        .collect();
    if parents.len() < 2 {
        return Err(ToolError::Conflict(format!(
            "internal PR {internal_pr} was fast-forward/squash-merged (its merge commit {head_int} \
             has a single parent) — git_public_mirror_replay_pr needs merge-commit-style internal \
             merges to guarantee ordered, correctly-attributed replay. Use merge commits internally, \
             or publish going-forward via git_public_history_sync."
        )));
    }
    if parents[0] != base_int {
        return Err(ToolError::Conflict(format!(
            "internal PR {internal_pr} is not the next unreplayed merged PR — its merge commit's \
             first parent {} does not equal the canonical lineage tip {base_int}. Replay merged \
             PRs in merge order (the immediately-following one first).",
            parents[0]
        )));
    }

    let title = scrub_text(pr.get("title").and_then(Value::as_str).unwrap_or("mirrored pull request"));
    // Body: gitea/github use `body`; GitLab MRs use `description`. Read either so a
    // GitLab internal MR body is reproduced, not silently dropped.
    let body = scrub_text(
        pr.get("body")
            .and_then(Value::as_str)
            .or_else(|| pr.get("description").and_then(Value::as_str))
            .unwrap_or(""),
    );

    // 2. Read + scrub the FULL discussion thread (paged, provider-agnostic).
    let scrubbed_comments = read_all_comments(
        reg,
        cfg.private_provider.as_deref(),
        &priv_repo,
        cfg.private_owner.as_deref(),
        internal_pr,
    )
    .await?;

    // Partial-prior-run guard: if the feature branch already exists on the remote,
    // a previous run pushed it but did not finish (create/comment/merge + the
    // mirror-pr tag). Re-running would open a DUPLICATE PR, so refuse and let the
    // operator reconcile (delete the remote branch / any stale PR) — fail-safe, never
    // double-create. (A fully completed run is caught earlier by the mirror-pr tag.)
    let token = &cfg.transport_token;
    if remote_branch_exists(&cfg.work_dir, &cfg.remote, &branch, token)? {
        return Err(ToolError::Conflict(format!(
            "public remote already has branch '{branch}' but internal PR {internal_pr} is not \
             tagged mirrored — a prior replay did not complete. Reconcile (remove the stale remote \
             branch and any partial PR) before re-running; git_public_mirror_replay_pr never \
             double-creates."
        )));
    }

    // 3. Fetch the current public main (a read — no canonical mutation yet).
    let public_base = fetch_public_main(&cfg.work_dir, &cfg.remote, token)?;
    let opts = ReplayOpts::with_author_map(cfg.author_map.clone());

    // The public MERGE is the point of no return: it advances the public repo
    // irreversibly. So the work is split into two phases around it:
    //   Phase A (replay → push → create PR → comments): fully roll-back-able — nothing
    //     is published yet, so on ANY failure restore the canonical lineage so a retry
    //     starts clean.
    //   Phase B (merge → reconcile boundary → tag): once the merge SUCCEEDS, the
    //     canonical lineage (already advanced to head_int by replay_pr_slice) MUST stay
    //     advanced to match the published state — it is NEVER rolled back after a
    //     successful merge. If a post-merge step (fetch/boundary/tag) fails, the error
    //     is surfaced but canonical stays put; the durable-idempotency path finishes
    //     the reconciliation on retry.
    let snap = snapshot_canonical(&cfg.work_dir)?;

    // ── Phase A (roll-back-able) ────────────────────────────────────────────────
    let phase_a: Result<(usize, u64, usize), ToolError> = async {
        let slice =
            replay_pr_slice(&cfg.source, &cfg.work_dir, &base_int, &head_int, &public_base, &branch, &opts)?;
        git_transport(
            &cfg.work_dir,
            &["push", "--", &cfg.remote, &format!("{}:refs/heads/{branch}", slice.branch_tip)],
            token,
        )?;
        let mut create = json!({ "repo": pub_repo, "title": title, "head": branch, "base": "main", "body": body });
        if let Some(o) = &cfg.public_owner {
            create["owner"] = json!(o);
        }
        let created =
            call_public(reg, cfg.public_provider.as_deref(), ForgeEndpoint::PullRequestsCreate, create).await?;
        let public_pr = pr_number(&created)
            .ok_or_else(|| ToolError::Execution("public PR create returned no PR number/iid".into()))?;
        let mut mirrored = 0usize;
        for c in &scrubbed_comments {
            let mut cp = json!({ "repo": pub_repo, "index": public_pr, "number": public_pr, "comment": c, "body": c });
            if let Some(o) = &cfg.public_owner {
                cp["owner"] = json!(o);
            }
            call_public(reg, cfg.public_provider.as_deref(), ForgeEndpoint::PullRequestsComment, cp).await?;
            mirrored += 1;
        }
        Ok((slice.commits, public_pr, mirrored))
    }
    .await;
    let (commits, public_pr, mirrored) = match phase_a {
        Ok(v) => v,
        Err(e) => {
            restore_canonical(&cfg.work_dir, &snap, &branch)?;
            return Err(e);
        }
    };

    // ── Phase B: merge (point of no return) ─────────────────────────────────────
    let mut mp = json!({
        "repo": pub_repo, "index": public_pr, "number": public_pr,
        "style": cfg.merge_method, "merge_method": cfg.merge_method,
    });
    if let Some(o) = &cfg.public_owner {
        mp["owner"] = json!(o);
    }
    if let Err(e) = call_public(reg, cfg.public_provider.as_deref(), ForgeEndpoint::PullRequestsMerge, mp).await {
        // The dispatch errored — but a timeout / 5xx-after-success / dropped response
        // could mean the merge ACTUALLY LANDED. Rolling back the canonical lineage while
        // public main advanced would corrupt the mirror. Re-read the PR's state and roll
        // back ONLY if it is POSITIVELY still unmerged; if it merged, fall through
        // (canonical stays advanced); if the state can't be confirmed, do NOT roll back.
        match public_pr_for_branch(reg, cfg, &pub_repo, &branch).await {
            Ok(Some(existing)) if pr_is_merged(&existing) => { /* merged despite the error */ }
            Ok(Some(_)) => {
                // Positively still open → the merge did not happen → safe to roll back.
                restore_canonical(&cfg.work_dir, &snap, &branch)?;
                return Err(e);
            }
            _ => {
                // Could not confirm (re-read failed / PR not found) → do NOT roll back.
                return Err(ToolError::Execution(format!(
                    "public merge for '{branch}' errored ({e}) and its state could not be \
                     confirmed — NOT rolling back the canonical lineage (the merge may have \
                     landed). Re-run to reconcile via durable idempotency."
                )));
            }
        }
    }

    // Merge SUCCEEDED. From here the canonical lineage stays at head_int (it matches the
    // now-published state) and is NOT rolled back on failure. Reconcile the boundary +
    // tag; on failure surface an error telling the caller to retry (the durable-
    // idempotency path will finish reconciliation), but leave canonical advanced.
    let new_public = fetch_public_main(&cfg.work_dir, &cfg.remote, token).map_err(|e| {
        ToolError::Execution(format!(
            "public PR for '{branch}' MERGED but reconciling the published boundary failed \
             ({e}); the canonical lineage is correctly advanced — re-run to finish (it will skip \
             via durable idempotency)"
        ))
    })?;
    set_pushed_sha(&cfg.work_dir, &new_public)?;
    git(&cfg.work_dir, &["tag", "-f", &pr_tag(internal_pr), &new_public])?;

    base_outcome.commits = commits;
    base_outcome.public_pr = Some(public_pr);
    base_outcome.comments_mirrored = mirrored;
    base_outcome.replayed = true;
    base_outcome.public_head = Some(new_public);
    base_outcome.note = "public PR opened, commented, and merged — the PR process is mirrored".into();
    Ok(base_outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ghist05-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn bot_map() -> IdentityMap {
        IdentityMap {
            rules: vec![],
            default_name: "MoosenetBot".into(),
            default_email: "<email>".into(), // pii-test-fixture
        }
    }

    fn cfg_for(repo: &str, work_dir: PathBuf) -> PrReplayConfig {
        PrReplayConfig {
            repo: repo.to_string(),
            source: work_dir.clone(),
            work_dir,
            remote: "file:///dev/null".into(),
            public_owner: None,
            private_owner: None,
            public_repo: None,
            private_repo: None,
            private_provider: None,
            public_provider: None,
            author_map: bot_map(),
            merge_method: "squash".into(),
            transport_token: "<REDACTED-SECRET>".into(),
        }
    }

    // A recorded PR (mirror-pr/<n> tag present) is a clean, forge-free idempotent skip.
    #[tokio::test]
    async fn replay_pr_is_idempotent_on_recorded_pr() {
        let wd = unique("idem-wd");
        std::fs::create_dir_all(&wd).unwrap();
        git(&wd, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(wd.join("a.txt"), b"x\n").unwrap();
        git(&wd, &["add", "-A"]).unwrap();
        // A commit (needs an identity) then a mirror-pr tag on it.
        let out = Command::new("git")
            .arg("-C")
            .arg(&wd)
            .args(HOOKS_OFF)
            .args(["-c", "user.name=T", "-c", "user.email=<email>", // pii-test-fixture
                   "commit", "-q", "-m", "seed"])
            .output()
            .unwrap();
        assert!(out.status.success());
        git(&wd, &["tag", &pr_tag(7)]).unwrap();

        // Empty registry — the idempotency check must return BEFORE any forge call.
        let reg = ForgeRegistry::new();
        let cfg = cfg_for("Demo", wd.clone());
        let outcome = replay_pr(&reg, &cfg, 7).await.unwrap();
        assert!(outcome.skipped, "recorded PR must be skipped: {outcome:?}");
        assert!(!outcome.replayed);

        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn pr_field_getters_tolerate_provider_shapes() {
        // gitea/github merged PR
        let gh = json!({"merged": true, "merge_commit_sha": "abc123", "head": {"ref": "pr-mirror/7"}});
        assert!(pr_is_merged(&gh));
        assert_eq!(pr_merge_commit(&gh).as_deref(), Some("abc123"));
        assert_eq!(pr_head_ref(&gh).as_deref(), Some("pr-mirror/7"));
        assert_eq!(pr_number(&json!({"number": 12})), Some(12));
        // gitlab MR merged: merge_commit_sha (NOT `sha`, which is the MR head diff sha)
        let gl = json!({"state": "merged", "sha": "headdiff", "merge_commit_sha": "def456", "source_branch": "pr-mirror/9", "iid": 9});
        assert!(pr_is_merged(&gl));
        assert_eq!(pr_merge_commit(&gl).as_deref(), Some("def456"));
        // gitlab squashed MR
        let gls = json!({"state": "merged", "squash_commit_sha": "sq789", "source_branch": "pr-mirror/9"});
        assert_eq!(pr_merge_commit(&gls).as_deref(), Some("sq789"));
        assert_eq!(pr_head_ref(&gl).as_deref(), Some("pr-mirror/9"));
        assert_eq!(pr_number(&gl), Some(9));
        // an open PR is not merged
        assert!(!pr_is_merged(&json!({"merged": false, "state": "open"})));
    }

    #[test]
    fn remote_userinfo_is_detected() {
        assert!(remote_has_userinfo("https://user:<email>/o/r.git")); // pii-test-fixture
        assert!(remote_has_userinfo("https://<email>/o/r.git")); // pii-test-fixture
        // Bare URLs and file paths have no embedded credentials.
        assert!(!remote_has_userinfo("https://github.com/o/r.git"));
        assert!(!remote_has_userinfo("file:///tmp/mirror-bare"));
        // An `@` in a path/query, not the authority, is not userinfo.
        assert!(!remote_has_userinfo("https://github.com/o/r.git?x=a@b"));
    }

    #[test]
    fn scrub_text_removes_fleet_identifiers() {
        // An internal IP in PR text must be scrubbed before any public write.
        let cleaned = scrub_text("see host <internal-ip> for the staging box"); // pii-test-fixture
        assert!(!cleaned.contains("<internal-ip>"), "IP scrubbed: {cleaned}"); // pii-test-fixture
        // Ordinary prose is untouched.
        assert_eq!(scrub_text("a normal review comment"), "a normal review comment");
    }
}
