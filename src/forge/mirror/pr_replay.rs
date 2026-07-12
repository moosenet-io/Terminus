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

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::error::ToolError;
use crate::forge::capability::ForgeEndpoint;
use crate::forge::provider::ForgeRequest;
use crate::forge::registry::{ForgePool, ForgeRegistry};

use super::history::{last_pushed_sha, replay_pr_slice, set_pushed_sha, IdentityMap, ReplayOpts};
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

fn pr_base_sha(pr: &Value) -> Option<String> {
    pr.pointer("/base/sha")
        .and_then(Value::as_str)
        .or_else(|| pr.pointer("/diff_refs/base_sha").and_then(Value::as_str))
        .map(String::from)
}

fn pr_head_sha(pr: &Value) -> Option<String> {
    pr.pointer("/head/sha")
        .and_then(Value::as_str)
        .or_else(|| pr.pointer("/diff_refs/head_sha").and_then(Value::as_str))
        .or_else(|| pr.get("sha").and_then(Value::as_str))
        .map(String::from)
}

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

    // Idempotency: a recorded PR is never re-created.
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
    let base_ref =
        pr_base_sha(&pr).ok_or_else(|| ToolError::Execution("internal PR has no base sha".into()))?;
    let head_sha =
        pr_head_sha(&pr).ok_or_else(|| ToolError::Execution("internal PR has no head sha".into()))?;
    // The PR object's `base.sha` is the base BRANCH tip, which — for an already-merged
    // PR — already contains the PR. The true pre-PR fork point is the merge-base of the
    // base branch and the PR head; replaying base_sha..head_sha from that point yields
    // exactly the PR's own commits. (Assumes the internal merge preserved the head
    // commits — i.e. a merge/rebase, not a squash that discards them; a squash makes
    // the head unreachable and merge-base fails with a clear error.)
    let base_sha = git(&cfg.source, &["merge-base", &base_ref, &head_sha])
        .map_err(|e| {
            ToolError::Execution(format!(
                "cannot resolve the PR fork point (merge-base of base {base_ref} and head \
                 {head_sha}) in the source — the PR head may be unreachable (squash-merged?): {e}"
            ))
        })?
        .trim()
        .to_string();
    let title = scrub_text(pr.get("title").and_then(Value::as_str).unwrap_or("mirrored pull request"));
    let body = scrub_text(pr.get("body").and_then(Value::as_str).unwrap_or(""));

    // 2. Read + scrub the discussion thread.
    let mut c_params = json!({ "repo": priv_repo, "index": internal_pr, "number": internal_pr });
    if let Some(o) = &cfg.private_owner {
        c_params["owner"] = json!(o);
    }
    let comments_body =
        read_private(reg, cfg.private_provider.as_deref(), ForgeEndpoint::PullRequestsListComments, c_params).await?;
    let scrubbed_comments: Vec<String> = comments_body
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("body").and_then(Value::as_str))
                .filter(|b| !b.trim().is_empty())
                .map(scrub_text)
                .collect()
        })
        .unwrap_or_default();

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

    // 3. Fetch the current public main, then replay the PR range onto it as `branch`.
    let public_base = fetch_public_main(&cfg.work_dir, &cfg.remote, token)?;
    let opts = ReplayOpts::with_author_map(cfg.author_map.clone());
    let slice = replay_pr_slice(&cfg.source, &cfg.work_dir, &base_sha, &head_sha, &public_base, &branch, &opts)?;
    base_outcome.commits = slice.commits;

    // 4. Push the scrubbed feature branch to the public remote (NOT force — new ref).
    git_transport(
        &cfg.work_dir,
        &["push", "--", &cfg.remote, &format!("{}:refs/heads/{branch}", slice.branch_tip)],
        token,
    )?;

    // 5. Open the public PR with the scrubbed title/body.
    let mut create = json!({ "repo": pub_repo, "title": title, "head": branch, "base": "main", "body": body });
    if let Some(o) = &cfg.public_owner {
        create["owner"] = json!(o);
    }
    let created = call_public(reg, cfg.public_provider.as_deref(), ForgeEndpoint::PullRequestsCreate, create).await?;
    let public_pr = pr_number(&created)
        .ok_or_else(|| ToolError::Execution("public PR create returned no PR number/iid".into()))?;
    base_outcome.public_pr = Some(public_pr);

    // 6. Mirror each scrubbed comment onto the public PR.
    for c in &scrubbed_comments {
        let mut cp = json!({ "repo": pub_repo, "index": public_pr, "number": public_pr, "comment": c, "body": c });
        if let Some(o) = &cfg.public_owner {
            cp["owner"] = json!(o);
        }
        call_public(reg, cfg.public_provider.as_deref(), ForgeEndpoint::PullRequestsComment, cp).await?;
        base_outcome.comments_mirrored += 1;
    }

    // 7. Merge the public PR — this is what lands the commits on public main.
    let mut mp = json!({
        "repo": pub_repo, "index": public_pr, "number": public_pr,
        "style": cfg.merge_method, "merge_method": cfg.merge_method,
    });
    if let Some(o) = &cfg.public_owner {
        mp["owner"] = json!(o);
    }
    call_public(reg, cfg.public_provider.as_deref(), ForgeEndpoint::PullRequestsMerge, mp).await?;

    // 8. Reconcile: fetch the new public main and record it as the published boundary
    //    so the next PR replays onto it, and tag the internal PR as mirrored.
    let new_public = fetch_public_main(&cfg.work_dir, &cfg.remote, token)?;
    set_pushed_sha(&cfg.work_dir, &new_public)?;
    let _ = last_pushed_sha(&cfg.work_dir); // (read-back is harmless; boundary now set)
    git(&cfg.work_dir, &["tag", "-f", &pr_tag(internal_pr), &new_public])?;

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
