//! `git_private_push` — git-PROTOCOL transport for the git-private domain
//! (TERM-git-transport / HCAT-29 prerequisite).
//!
//! The [`git_private`](crate::forge::git_private) tool speaks the forge REST API
//! (repos / branches / PRs / releases) but has NO git-PROTOCOL transport: it
//! cannot move actual COMMITS. This module adds exactly that one missing
//! capability — pushing a local branch's commits to the self-hosted Gitea — so a
//! caller (Harmony's `git/pr.rs::push_branch`) no longer has to embed a
//! `GITEA_TOKEN` in a remote URL and shell `git push` in its own process. The
//! credential lives HERE, on the terminus-primary; the caller holds none.
//!
//! ## Bundle-based transport (the caller ships commits; the tool holds the token)
//! Harmony's branch lives in its own worktree on a different host, so there is no
//! shared object store to push from. The clean transport is a **git bundle**:
//!   1. Harmony runs `git bundle create <file> <ref>` locally and base64-encodes it.
//!   2. This tool base64-decodes it into a fresh [`tempfile::TempDir`], `git init`s
//!      a throwaway repo there, and imports the ref from the bundle (`git fetch`
//!      from the bundle file — validates the bundle's prerequisites are complete).
//!   3. It resolves the Gitea remote URL + the `GITEA_PAT_<identity>` credential
//!      and `git push`es the imported ref to `refs/heads/<ref>` on the remote.
//!
//! The credential is injected via `GIT_ASKPASS` (reusing the mirror runner's
//! [`write_askpass_script`](crate::forge::mirror::tools::write_askpass_script) —
//! token in `GIT_MIRROR_TOKEN` env only, NEVER in the URL, argv, or on disk, and
//! redacted from any surfaced stderr). Local temp-repo git ops reuse GHMR-03's
//! [`run_git`](crate::forge::mirror::workdir::run_git), which disables hooks and
//! force-guards the argv.
//!
//! ## Safety
//! A normal push is CREATE / fast-forward only (the refspec carries no leading
//! `+` and no `--force`, so git itself server-side-rejects a non-ff). A
//! `force: true` push REWRITES remote history and is destructive: it is gated
//! behind the same `confirm: true` posture the rest of git-private uses
//! ([`requests_force_or_rewrite`]-shaped), and only the destructive+confirm path
//! is allowed to bypass [`assert_never_force`].
//!
//! ## Placement
//! PERSONAL registry only, alongside `git_private` — the operator's own
//! source-of-truth git access, registered from
//! [`crate::forge::register_private`].
//!
//! ## Secrets
//! The token is resolved at call time from the runtime secret-materialised env
//! (`GITEA_PAT_<NAME>`) via [`crate::gitea::gitea_token`] and handed to `git`
//! only through `GIT_ASKPASS`; it is never a literal in source, never placed in a
//! URL, and never logged.

use std::path::Path;
use std::process::Command;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use super::mirror::tools::write_askpass_script;
use super::mirror::workdir::{assert_never_force, run_git};

/// Environment variable the askpass helper reads the token from — the same
/// convention the mirror runner uses, so the reused
/// [`write_askpass_script`] helper works unchanged.
const ASKPASS_TOKEN_ENV: &str = "GIT_MIRROR_TOKEN";

pub struct GitPrivatePush;

impl GitPrivatePush {
    /// Full flow: gate → decode bundle → import ref into a temp repo → resolve
    /// the Gitea remote + credential → push. Factored out of [`RustTool::execute`]
    /// so both `execute` and `execute_structured` share one implementation.
    async fn run(&self, args: Value) -> Result<Value, ToolError> {
        let repo = req_str(&args, "repo")?;
        let ref_name = req_str(&args, "ref")?;
        let bundle_b64 = req_str(&args, "bundle_b64")?;
        let owner_arg = opt_str(&args, "owner");
        let identity = opt_str(&args, "identity");
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        let confirm = args
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // ── Input validation (before any git / network / credential touch) ──
        validate_segment("repo", &repo)?;
        if let Some(o) = &owner_arg {
            validate_segment("owner", o)?;
        }
        validate_ref(&ref_name)?;

        // ── Safety gate: a force push is DESTRUCTIVE (rewrites remote history) ──
        // and requires explicit confirmation, mirroring git_private's posture. A
        // normal push is create / fast-forward only and needs no confirm.
        if force && !confirm {
            return Err(ToolError::InvalidArgument(format!(
                "force-pushing '{ref_name}' to '{repo}' rewrites remote history and is a \
                 destructive git-private operation — retry with 'confirm': true to proceed \
                 (a normal, non-force push is create / fast-forward only and needs no confirm)"
            )));
        }

        // ── Decode + import the bundle into a throwaway repo ──
        let bundle_bytes = decode_bundle(&bundle_b64)?;
        let tmp = tempfile::TempDir::new().map_err(|e| {
            ToolError::Execution(format!("failed creating temp dir for bundle import: {e}"))
        })?;
        let sha = import_bundle_ref(tmp.path(), &bundle_bytes, &ref_name)?;

        // ── Resolve the Gitea remote URL + credential (NEVER embed in the URL) ──
        // Token resolved immediately before use and injected via GIT_ASKPASS.
        let token = crate::gitea::gitea_token(identity.as_deref())?;
        let client = crate::gitea::GiteaClient::from_env()?;
        let owner = owner_arg.unwrap_or_else(|| client.owner().to_string());
        let base = client.base_url().trim_end_matches('/');
        let remote = format!("{base}/{owner}/{repo}.git");

        // ── Push the imported ref ──
        let local_ref = format!("refs/heads/{ref_name}");
        let remote_ref = format!("refs/heads/{ref_name}");
        push_ref(tmp.path(), &remote, &local_ref, &remote_ref, &token, force)?;

        Ok(json!({
            "pushed": true,
            "repo": repo,
            "owner": owner,
            "ref": ref_name,
            "sha": sha,
            "forced": force,
        }))
    }
}

#[async_trait]
impl RustTool for GitPrivatePush {
    fn name(&self) -> &str {
        "git_private_push"
    }

    fn description(&self) -> &str {
        "Push a git branch (shipped as a base64 git bundle) to a self-hosted Gitea repo, \
         using Terminus's own Gitea credential (git-private domain). The caller holds no \
         token. Bundle the branch locally with `git bundle create <file> <ref>`, base64 the \
         file, and pass it as 'bundle_b64' with the target 'repo' and 'ref'. A normal push \
         is create / fast-forward only; a 'force': true push rewrites remote history and \
         requires 'confirm': true. The Gitea URL + credential (GITEA_PAT_<identity>) are \
         resolved from config and injected via GIT_ASKPASS — never embedded in the URL or \
         logged. PERSONAL registry only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "The Gitea repository name (single segment, e.g. 'Harmony')"
                },
                "bundle_b64": {
                    "type": "string",
                    "description": "Base64 of a `git bundle` file containing the branch's commits + the ref"
                },
                "ref": {
                    "type": "string",
                    "description": "The branch/ref name to create or update on the remote (e.g. 'HCAT-99-foo'); a leading 'refs/heads/' is accepted and stripped"
                },
                "owner": {
                    "type": "string",
                    "description": "Repo owner/org (optional; defaults to the identity's configured default owner, normally GITEA_OWNER = 'moosenet')"
                },
                "force": {
                    "type": "boolean",
                    "description": "Force-push (DESTRUCTIVE — rewrites remote history). Default false. Requires 'confirm': true."
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Required 'true' to authorise a destructive force push."
                },
                "identity": {
                    "type": "string",
                    "description": "Optional Gitea identity to act as: a configured GITEA_PAT_<NAME> identity name (e.g. \"moose\", \"harmony\"). Omit to use the active default identity (GITEA_IDENTITY_NAME, default \"moose\")."
                }
            },
            "required": ["repo", "bundle_b64", "ref"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.to_string())
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let structured = self.run(args).await?;
        Ok(ToolOutput::with_structured(
            structured.to_string(),
            structured,
        ))
    }
}

// ── Bundle decode + import ──────────────────────────────────────────────────

/// Base64-decode the shipped bundle, rejecting a malformed or empty payload with
/// a clean [`ToolError::InvalidArgument`] before any git work.
fn decode_bundle(bundle_b64: &str) -> Result<Vec<u8>, ToolError> {
    let bytes = B64.decode(bundle_b64.trim().as_bytes()).map_err(|e| {
        ToolError::InvalidArgument(format!("'bundle_b64' is not valid base64: {e}"))
    })?;
    if bytes.is_empty() {
        return Err(ToolError::InvalidArgument(
            "'bundle_b64' decoded to zero bytes — an empty bundle carries no commits".to_string(),
        ));
    }
    Ok(bytes)
}

/// Write `bundle_bytes` to a file inside `dir`, `git init` a throwaway repo there,
/// import the requested `ref_name` from the bundle into `refs/heads/<ref_name>`,
/// and return the imported commit's full sha.
///
/// This is the pure-ish, git-only step (no network, no credential): given a local
/// bundle it is fully exercisable against a local temp repo in tests.
fn import_bundle_ref(dir: &Path, bundle_bytes: &[u8], ref_name: &str) -> Result<String, ToolError> {
    // A leading `refs/heads/` is accepted on the caller-facing `ref` (validated by
    // `validate_ref`) — normalise it away for the local branch name.
    let short_ref = ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name);

    let bundle_path = dir.join("incoming.bundle");
    std::fs::write(&bundle_path, bundle_bytes)
        .map_err(|e| ToolError::Execution(format!("failed writing bundle to temp file: {e}")))?;

    // Fresh throwaway repo. `run_git` disables hooks and force-guards the argv.
    run_git(dir, &["init", "-q"])?;

    // Verify the bundle is well-formed and locate the ref it carries. A branch
    // bundle stores the ref as `refs/heads/<branch>`; be tolerant of a bundle that
    // stored a bare or otherwise-suffixed name.
    let bundle_path_str = bundle_path.to_string_lossy().into_owned();
    let heads = run_git(dir, &["bundle", "list-heads", &bundle_path_str]).map_err(|e| {
        ToolError::InvalidArgument(format!(
            "'bundle_b64' is not a valid git bundle (git bundle list-heads failed): {e}"
        ))
    })?;
    let bundle_ref = find_bundle_ref(&heads, short_ref).ok_or_else(|| {
        ToolError::InvalidArgument(format!(
            "the bundle does not contain ref '{short_ref}'. Refs present:\n{}",
            heads.trim()
        ))
    })?;

    // Import the objects + ref from the bundle. `git fetch` validates the bundle's
    // prerequisites (a complete branch bundle has none) and imports into a fresh
    // local branch — no force needed (the temp repo has no such ref yet).
    let refspec = format!("{bundle_ref}:refs/heads/{short_ref}");
    run_git(dir, &["fetch", "--no-tags", &bundle_path_str, &refspec]).map_err(|e| {
        ToolError::InvalidArgument(format!(
            "failed importing ref '{short_ref}' from the bundle (incomplete bundle / missing \
             prerequisite commits?): {e}"
        ))
    })?;

    let sha = run_git(dir, &["rev-parse", &format!("refs/heads/{short_ref}")])?
        .trim()
        .to_string();
    if sha.is_empty() {
        return Err(ToolError::Execution(format!(
            "imported ref '{short_ref}' resolved to no commit"
        )));
    }
    Ok(sha)
}

/// Find the full ref name inside a `git bundle list-heads` listing (`<sha> <ref>`
/// per line) that corresponds to the requested short ref: an exact
/// `refs/heads/<ref>` match wins, else a bare `<ref>`, else any ref ending in
/// `/<ref>`.
fn find_bundle_ref(list_heads_output: &str, short_ref: &str) -> Option<String> {
    let target_full = format!("refs/heads/{short_ref}");
    let refs: Vec<&str> = list_heads_output
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    // 1. exact refs/heads/<ref>
    if let Some(r) = refs.iter().find(|r| **r == target_full) {
        return Some((*r).to_string());
    }
    // 2. bare <ref>
    if let Some(r) = refs.iter().find(|r| **r == short_ref) {
        return Some((*r).to_string());
    }
    // 3. anything ending in /<ref>
    let suffix = format!("/{short_ref}");
    refs.iter()
        .find(|r| r.ends_with(&suffix))
        .map(|r| (*r).to_string())
}

// ── Push (credential injected via GIT_ASKPASS) ──────────────────────────────

/// Push `local_ref` to `remote_ref` on `remote`, injecting `token` via
/// `GIT_ASKPASS` (never in the URL / argv / logs). A non-force push uses a
/// refspec with no leading `+` and no `--force`, so git server-side-rejects a
/// non-fast-forward — a second safety net beneath the `confirm` gate. Only the
/// `force` path (already `confirm`-gated by the caller) carries `+`/`--force` and
/// bypasses [`assert_never_force`]; the normal path is force-guarded.
fn push_ref(
    repo_dir: &Path,
    remote: &str,
    local_ref: &str,
    remote_ref: &str,
    token: &str,
    force: bool,
) -> Result<(), ToolError> {
    let refspec = if force {
        format!("+{local_ref}:{remote_ref}")
    } else {
        format!("{local_ref}:{remote_ref}")
    };
    let mut argv: Vec<&str> = vec!["-c", "core.hooksPath=/dev/null", "push"];
    if force {
        argv.push("--force");
    }
    argv.push("--");
    argv.push(remote);
    argv.push(&refspec);
    // Force-guard the NORMAL path (a create / fast-forward push must never carry a
    // force token); the destructive path is confirm-gated by the caller and is the
    // one sanctioned place a force token is allowed.
    if !force {
        assert_never_force(&argv);
    }

    let askpass = write_askpass_script()?;
    let result = (|| {
        let output = Command::new("git")
            .current_dir(repo_dir)
            .args(&argv)
            .env("GIT_ASKPASS", askpass.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(ASKPASS_TOKEN_ENV, token)
            .output()
            .map_err(|e| ToolError::Execution(format!("failed to spawn git push: {e}")))?;
        if output.status.success() {
            return Ok(());
        }
        // stderr should never carry the token (it is only ever in the child env,
        // echoed by askpass to git's credential channel) — redact defensively anyway.
        let stderr = String::from_utf8_lossy(&output.stderr).replace(token, "<redacted>");
        Err(classify_push_error(&stderr, remote, remote_ref))
    })();
    drop(askpass);
    result
}

/// Map a failed `git push`'s stderr onto a clear [`ToolError`]: a non-fast-forward
/// rejection → [`ToolError::Conflict`] pointing at the conflict; an auth failure →
/// a credential error; anything else → a generic execution error.
fn classify_push_error(stderr: &str, remote: &str, remote_ref: &str) -> ToolError {
    let low = stderr.to_lowercase();
    let trimmed = stderr.trim();
    if low.contains("non-fast-forward")
        || low.contains("fetch first")
        || low.contains("[rejected]")
        || low.contains("failed to push some refs")
    {
        ToolError::Conflict(format!(
            "push rejected (non-fast-forward): '{remote_ref}' on the remote has commits not \
             contained in the pushed branch. Reconcile the branch, or retry with 'force': true \
             + 'confirm': true to overwrite remote history. git said: {trimmed}"
        ))
    } else if low.contains("authentication failed")
        || low.contains("could not read password")
        || low.contains(" 403")
        || low.contains(" 401")
        || low.contains("access denied")
        || low.contains("permission denied")
    {
        ToolError::NotConfigured(format!(
            "Gitea authentication/authorization failed pushing to {remote}: the resolved \
             GITEA_PAT credential is missing, invalid, or lacks write access. git said: {trimmed}"
        ))
    } else {
        ToolError::Execution(format!("git push to {remote} failed: {trimmed}"))
    }
}

// ── Input validation ────────────────────────────────────────────────────────

/// A required, non-empty, trimmed string argument.
fn req_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{key}' is required")))
}

/// An optional, non-empty, trimmed string argument.
fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Validate a single URL path segment (`repo` / `owner`) that is interpolated
/// into the remote URL: it must be a plain identifier so it cannot break out of
/// its slot, inject a credential/host, or traverse. Allowed: ASCII
/// alphanumerics plus `-`, `_`, `.` — but never a `..` traversal, never a leading
/// `-` (option-like), never empty.
fn validate_segment(key: &str, v: &str) -> Result<(), ToolError> {
    if v.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "'{key}' must not be empty"
        )));
    }
    if v.starts_with('-') {
        return Err(ToolError::InvalidArgument(format!(
            "'{key}' must not start with '-' (looks like a git option): {v:?}"
        )));
    }
    if v.split(['/', '\\']).any(|seg| matches!(seg, "." | "..")) || v.contains("..") {
        return Err(ToolError::InvalidArgument(format!(
            "'{key}' must not contain a path-traversal segment: {v:?}"
        )));
    }
    if !v
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ToolError::InvalidArgument(format!(
            "'{key}' contains characters not allowed in a repo/owner name (allowed: \
             A-Z a-z 0-9 . _ -): {v:?}"
        )));
    }
    Ok(())
}

/// Validate a branch/ref name. Accepts an optional leading `refs/heads/`. Rejects
/// values git itself would refuse or that are dangerous in a refspec/argv: empty,
/// leading `-`, whitespace, control chars, `..`, `//`, a trailing `/`, and the
/// special characters `~ ^ : ? * [ \` and space.
fn validate_ref(v: &str) -> Result<(), ToolError> {
    let bad = |msg: &str| ToolError::InvalidArgument(format!("invalid 'ref' {v:?}: {msg}"));
    if v.is_empty() {
        return Err(bad("must not be empty"));
    }
    if v.starts_with('-') {
        return Err(bad("must not start with '-' (looks like a git option)"));
    }
    if v.starts_with('/') || v.ends_with('/') {
        return Err(bad("must not start or end with '/'"));
    }
    if v.contains("..") || v.contains("//") {
        return Err(bad("must not contain '..' or '//'"));
    }
    if v.ends_with(".lock") {
        return Err(bad("must not end with '.lock'"));
    }
    for c in v.chars() {
        if c.is_control() || c.is_whitespace() {
            return Err(bad("must not contain whitespace or control characters"));
        }
        if matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return Err(bad("must not contain any of ~ ^ : ? * [ \\"));
        }
    }
    Ok(())
}

// ── Registration ────────────────────────────────────────────────────────────

/// Register `git_private_push` — PERSONAL registry only, alongside `git_private`.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(GitPrivatePush));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn unique(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gitxport-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// `git` in `cwd` for test setup (no force-guard needed — test fixtures).
    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Build a source repo with one commit on `branch`, then `git bundle create`
    /// that branch and return (bundle_bytes, head_sha).
    fn make_branch_bundle(branch: &str) -> (Vec<u8>, String) {
        let src = unique("src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", branch]);
        std::fs::write(src.join("f.txt"), "hello bundle\n").unwrap();
        git(&src, &["add", "-A"]);
        git(
            &src,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=<email>",
                "commit",
                "-q",
                "-m",
                "initial",
            ],
        );
        let sha = git(&src, &["rev-parse", "HEAD"]).trim().to_string();
        let bundle = src.join("out.bundle");
        git(
            &src,
            &["bundle", "create", bundle.to_str().unwrap(), branch],
        );
        let bytes = std::fs::read(&bundle).unwrap();
        let _ = std::fs::remove_dir_all(&src);
        (bytes, sha)
    }

    // (a) A bundle round-trips: decode + unbundle imports the ref/sha.
    #[test]
    fn bundle_round_trips_into_temp_repo() {
        let (bytes, sha) = make_branch_bundle("HCAT-99-foo");
        let b64 = B64.encode(&bytes);

        let decoded = decode_bundle(&b64).expect("decode");
        assert_eq!(decoded, bytes);

        let dir = unique("imp");
        std::fs::create_dir_all(&dir).unwrap();
        let imported = import_bundle_ref(&dir, &decoded, "HCAT-99-foo").expect("import");
        assert_eq!(
            imported, sha,
            "imported sha must match the bundled branch head"
        );
        // The ref exists as a local branch in the temp repo.
        let rev = git(&dir, &["rev-parse", "refs/heads/HCAT-99-foo"])
            .trim()
            .to_string();
        assert_eq!(rev, sha);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (a') A caller passing the fully-qualified ref (refs/heads/<b>) also works.
    #[test]
    fn bundle_round_trips_with_fully_qualified_ref() {
        let (bytes, sha) = make_branch_bundle("feature-x");
        let dir = unique("impq");
        std::fs::create_dir_all(&dir).unwrap();
        let imported = import_bundle_ref(&dir, &bytes, "refs/heads/feature-x").expect("import");
        assert_eq!(imported, sha);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (a'') A ref not present in the bundle is a clean InvalidArgument.
    #[test]
    fn missing_ref_in_bundle_is_clean_error() {
        let (bytes, _sha) = make_branch_bundle("only-branch");
        let dir = unique("impm");
        std::fs::create_dir_all(&dir).unwrap();
        let err = import_bundle_ref(&dir, &bytes, "does-not-exist").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (b) A force push without confirm is rejected — before any git/network.
    #[tokio::test]
    async fn force_without_confirm_is_rejected() {
        let (bytes, _sha) = make_branch_bundle("HCAT-1-x");
        let b64 = B64.encode(&bytes);
        let tool = GitPrivatePush;
        let err = tool
            .execute(json!({
                "repo": "Harmony",
                "ref": "HCAT-1-x",
                "bundle_b64": b64,
                "force": true,
            }))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgument(m) => assert!(m.contains("confirm"), "{m}"),
            other => panic!("expected InvalidArgument mentioning confirm, got {other:?}"),
        }
    }

    // (c) Malformed base64 → clean error.
    #[test]
    fn malformed_base64_is_clean_error() {
        let err = decode_bundle("!!!not base64!!!").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn empty_bundle_bytes_rejected() {
        // Valid base64 of an empty string decodes to zero bytes.
        let err = decode_bundle("").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
    }

    // Validation: repo/owner segment + ref-name guards.
    #[test]
    fn segment_and_ref_validation() {
        assert!(validate_segment("repo", "Harmony").is_ok());
        assert!(validate_segment("repo", "lumina-core").is_ok());
        assert!(validate_segment("repo", "../etc").is_err());
        assert!(validate_segment("repo", "-rf").is_err());
        assert!(validate_segment("repo", "a/b").is_err());
        assert!(validate_segment("repo", "").is_err());

        assert!(validate_ref("HCAT-99-foo").is_ok());
        assert!(validate_ref("feature/nested-branch").is_ok());
        assert!(validate_ref("refs/heads/ok").is_ok());
        assert!(validate_ref("-x").is_err());
        assert!(validate_ref("bad..ref").is_err());
        assert!(validate_ref("has space").is_err());
        assert!(validate_ref("has:colon").is_err());
        assert!(validate_ref("trailing/").is_err());
    }

    // find_bundle_ref matching precedence.
    #[test]
    fn find_bundle_ref_precedence() {
        let listing = "abc123 refs/heads/main\ndef456 refs/heads/HCAT-99\n";
        assert_eq!(
            find_bundle_ref(listing, "HCAT-99").as_deref(),
            Some("refs/heads/HCAT-99")
        );
        // bare ref name
        assert_eq!(
            find_bundle_ref("aaa HCAT-7\n", "HCAT-7").as_deref(),
            Some("HCAT-7")
        );
        // suffix fallback
        assert_eq!(
            find_bundle_ref("aaa refs/remotes/origin/topic\n", "topic").as_deref(),
            Some("refs/remotes/origin/topic")
        );
        assert_eq!(find_bundle_ref("aaa refs/heads/main\n", "nope"), None);
    }

    // classify_push_error routing.
    #[test]
    fn classify_push_error_routes() {
        assert!(matches!(
            classify_push_error(
                "! [rejected] main -> main (non-fast-forward)",
                "r",
                "refs/heads/main"
            ),
            ToolError::Conflict(_)
        ));
        assert!(matches!(
            classify_push_error(
                "fatal: Authentication failed for 'http://x'",
                "r",
                "refs/heads/main"
            ),
            ToolError::NotConfigured(_)
        ));
        assert!(matches!(
            classify_push_error("some other failure", "r", "refs/heads/main"),
            ToolError::Execution(_)
        ));
    }
}
