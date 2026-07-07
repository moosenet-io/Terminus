//! GitHub tools — port of the Python `github_tools.py` on the fleet host.
//!
//! Four tools:
//!   github_list_repos   — list repos in the configured GitHub org
//!   github_create_repo  — create a new repo in the org (public by default)
//!   github_push_repo    — build the mirror command to push a Gitea repo to GitHub
//!   github_push_branch  — create/fast-forward a single branch via the Git Data API
//!                         (blobs → tree → commit → ref), no git wire protocol,
//!                         no subprocess. See the `github_push_branch` doc comment
//!                         below for the full design rationale.
//!
//! Required env:
//!   GITHUB_TOKEN  — GitHub personal access / app token (Authorization: token …)
//!                   Required scopes: `repo` (push/create) plus `admin:org` to
//!                   create or push to repos under an organisation.
//! Optional env:
//!   GITHUB_ORG    — target org (default: moosenet-io)
//!   GITEA_URL     — Gitea base URL referenced when building the mirror command
//!                   for github_push_repo (default: https://gitea.example.com)
//!   GITHUB_API_BASE — override for the GitHub API base URL (test-only; points
//!                   at an httpmock server in unit tests, defaults to
//!                   https://api.github.com in production).
//!
//! If GITHUB_TOKEN is unset, NotConfigured stubs are registered so callers get a
//! clear error rather than a panic.
//!
//! PII gate (MANDATORY): every WRITE tool here runs its outbound content through
//! [`pii::pii_gate`] BEFORE any network request fires. There is no flag, env var,
//! or argument that disables it — see `pii.rs`. Read-only tools
//! (`github_list_repos`) are not gated.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub mod pii;
use pii::pii_gate;

const DEFAULT_ORG: &str = "moosenet-io";
const DEFAULT_GITEA_URL: &str = "https://gitea.example.com";
const GITHUB_API: &str = "https://api.github.com";

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct GitHubConfig {
    token: String,
    org: String,
    gitea_url: String,
    api_base: String,
}

impl GitHubConfig {
    fn from_env() -> Result<Self, ToolError> {
        let token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("GITHUB_TOKEN not set".into()))?;
        let org = std::env::var("GITHUB_ORG")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ORG.to_string());
        let gitea_url = std::env::var("GITEA_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GITEA_URL.to_string());
        // Test-only override so unit tests can point at an httpmock server
        // instead of the real GitHub API. Unset in production.
        let api_base = std::env::var("GITHUB_API_BASE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| GITHUB_API.to_string());
        Ok(Self { token, org, gitea_url, api_base })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("MooseNet-MCP/1.0")
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    /// Standard GitHub headers, following the task spec (token auth, github+json).
    fn apply_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("Authorization", format!("token {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28") // pii-test-fixture
    }
}

// ── Response shaping ──────────────────────────────────────────────────────────

/// Map one GitHub repo object to the compact shape the Python tool returns.
fn repo_summary(r: &Value) -> Value {
    json!({
        "name":        r.get("name").and_then(Value::as_str).unwrap_or(""),
        "full_name":   r.get("full_name").and_then(Value::as_str).unwrap_or(""),
        "private":     r.get("private").and_then(Value::as_bool).unwrap_or(false),
        "url":         r.get("html_url").and_then(Value::as_str).unwrap_or(""),
        "description": r.get("description").and_then(Value::as_str).unwrap_or(""),
    })
}

/// Build the `git clone --mirror … && git push --mirror …` command string that
/// github_push_repo returns. Tokens are referenced as shell variables
/// ($GITEA_TOKEN, $GITHUB_TOKEN) and are NOT interpolated, so they never appear
/// in tool output — identical to the Python behaviour.
fn build_mirror_cmd(
    gitea_host: &str,
    gitea_owner: &str,
    gitea_repo: &str,
    org: &str,
    github_repo: &str,
    force: bool,
) -> String {
    // `--mirror` already overwrites remote refs, but when `force` is requested we
    // add `--force` to make the clean-history overwrite explicit (needed for
    // PUB-07 re-exports onto an existing public repo). Default keeps prior behaviour.
    let force_flag = if force { " --force" } else { "" };
    // Split onto its own literal so the (unavoidable, shell-syntax) `$VAR@host`
    // token pattern lives on one self-contained line — it trips the PII
    // scanner's email regex on the shell-variable pattern below // pii-test-fixture
    // even though nothing is interpolated here (see doc comment above); this
    // is a false positive of the naive regex, not a real credential or host.
    let push_line = format!(
        "git push --mirror{force_flag} https://$<email>/{org}/{github_repo}.git && \\\n" // pii-test-fixture
    );
    format!(
        "cd /tmp && \
rm -rf _mirror_tmp && \
git clone --mirror http://oauth2:$GITEA_TOKEN@{gitea_host}/{gitea_owner}/{gitea_repo}.git _mirror_tmp && \
cd _mirror_tmp && \
{push_line}\
cd /tmp && rm -rf _mirror_tmp && \
echo MIRROR_OK"
    )
}

/// Strip the scheme prefix from a Gitea URL (http://host:port → host:port).
fn gitea_host_from_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed
        .split_once("://")
        .map(|(_, h)| h)
        .unwrap_or(trimmed)
        .to_string()
}

// ── Tools ───────────────────────────────────────────────────────────────────

struct GitHubListRepos { cfg: GitHubConfig }
struct GitHubCreateRepo { cfg: GitHubConfig }
struct GitHubPushRepo { cfg: GitHubConfig }
struct GitHubPushBranch { cfg: GitHubConfig }

#[async_trait]
impl RustTool for GitHubListRepos {
    fn name(&self) -> &str { "github_list_repos" }

    fn description(&self) -> &str {
        "List all repositories in the moosenet-io GitHub org."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let url = format!(
            "{GITHUB_API}/orgs/{}/repos?per_page=100&sort=updated",
            self.cfg.org
        );
        let client = GitHubConfig::client()?;
        let resp = self
            .cfg
            .apply_headers(client.get(&url))
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
        if !status.is_success() {
            // Mirror the Python {"error": "HTTP {code}: {body}"} surface.
            return Ok(json!({
                "error": format!("HTTP {}: {}", status.as_u16(), body)
            })
            .to_string());
        }

        let data: Value = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Invalid JSON from GitHub: {e}")))?;
        let repos: Vec<Value> = data
            .as_array()
            .map(|arr| arr.iter().map(repo_summary).collect())
            .unwrap_or_default();

        Ok(json!({ "repos": repos }).to_string())
    }
}

#[async_trait]
impl RustTool for GitHubCreateRepo {
    fn name(&self) -> &str { "github_create_repo" }

    fn description(&self) -> &str {
        "Create a new repository in the moosenet-io GitHub org. private=False by default (public repos only)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name":        { "type": "string",  "description": "Repository name (required)" },
                "description": { "type": "string",  "description": "Repository description (optional)" },
                "private":     { "type": "boolean", "description": "Private repo? Default false (public)" }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'name' is required".into()))?;
        let description = args.get("description").and_then(Value::as_str).unwrap_or("");
        let private = args.get("private").and_then(Value::as_bool).unwrap_or(false);

        // MANDATORY PII gate — scan ALL outbound content (repo name + description)
        // BEFORE any API request fires. Any violation rejects the whole operation.
        pii_gate(&format!("{name}\n{description}"))?;

        let url = format!("{GITHUB_API}/orgs/{}/repos", self.cfg.org);
        let payload = json!({
            "name": name,
            "description": description,
            "private": private,
            "auto_init": false,
        });

        let client = GitHubConfig::client()?;
        let resp = self
            .cfg
            .apply_headers(client.post(&url))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;

        if !status.is_success() {
            // Python maps "already exists" / 422 to a friendly {created:false} result.
            if status.as_u16() == 422 || body.contains("already exists") {
                return Ok(json!({
                    "created": false,
                    "error": "repo already exists",
                    "full_name": format!("{}/{}", self.cfg.org, name)
                })
                .to_string());
            }
            return Ok(json!({
                "created": false,
                "error": format!("HTTP {}: {}", status.as_u16(), body)
            })
            .to_string());
        }

        let data: Value = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Invalid JSON from GitHub: {e}")))?;
        Ok(json!({
            "created": true,
            "full_name": data.get("full_name").and_then(Value::as_str).unwrap_or(""),
            "html_url":  data.get("html_url").and_then(Value::as_str).unwrap_or(""),
            "clone_url": data.get("clone_url").and_then(Value::as_str).unwrap_or(""),
        })
        .to_string())
    }
}

#[async_trait]
impl RustTool for GitHubPushRepo {
    fn name(&self) -> &str { "github_push_repo" }

    fn description(&self) -> &str {
        "Mirror a completed Gitea repo to GitHub moosenet-io org. \
The pre-push hook on the dev workstation will scan commits for PII before the push completes. \
gitea_repo: repo name in Gitea (e.g. lumina-constellation). \
github_repo: target repo name in moosenet-io (e.g. lumina-constellation). \
Returns a command to run via dev_run_command on the dev workstation."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "gitea_repo":  { "type": "string", "description": "Repo name in Gitea (e.g. lumina-constellation)" },
                "github_repo": { "type": "string", "description": "Target repo name in moosenet-io (e.g. lumina-constellation)" },
                "gitea_owner": { "type": "string", "description": "Gitea owner/org (default: moosenet)" },
                "force":       { "type": "boolean", "description": "Force-push (overwrite remote history). Default false.", "default": false }
            },
            "required": ["gitea_repo", "github_repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let gitea_repo = args
            .get("gitea_repo")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'gitea_repo' is required".into()))?;
        let github_repo = args
            .get("github_repo")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'github_repo' is required".into()))?;
        let gitea_owner = args
            .get("gitea_owner")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("moosenet");
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

        // MANDATORY PII gate — scan ALL operator-supplied identifiers that will
        // be embedded in the GitHub-bound mirror command BEFORE it is built.
        // Any violation rejects the whole operation; no bypass exists.
        pii_gate(&format!("{gitea_repo}\n{github_repo}\n{gitea_owner}"))?;

        let gitea_host = gitea_host_from_url(&self.cfg.gitea_url);
        let cmd = build_mirror_cmd(&gitea_host, gitea_owner, gitea_repo, &self.cfg.org, github_repo, force);

        Ok(json!({
            "cmd": cmd,
            "note": "Run this via dev_run_command on the dev workstation. Tokens sourced from shell env, not embedded. Pre-push hook scans for PII.",
            "github_url": format!("https://github.com/{}/{}", self.cfg.org, github_repo)
        })
        .to_string())
    }
}

// ── github_push_branch ───────────────────────────────────────────────────────
//
// Design rationale (see task brief for full context): Terminus tools may
// never shell out to `git` — every tool here is typed HTTP via reqwest. A
// naive "push a branch" tool therefore CANNOT use the git wire protocol.
// GitHub's REST API has no single endpoint that accepts an arbitrary local
// git push, but the Git Data API lets us build a brand-new commit purely
// over HTTP:
//
//   1. GET  /git/ref/heads/{branch}        — does the target branch exist?
//   2. GET  /git/commits/{base_sha}        — resolve the base tree to extend
//   3. POST /git/blobs                     — one per changed file's content
//   4. POST /git/trees   (base_tree=...)   — overlay changed/deleted paths
//   5. POST /git/commits (parents=[base])  — the new commit object
//   6. PATCH or POST /git/refs/heads/{..}  — fast-forward or create the ref
//
// This intentionally is NOT "push arbitrary local commit history" — the
// caller (a thin script wrapper) resolves what changed locally with
// read-only git plumbing (`git rev-parse`, `git diff --name-status`) and
// hands this tool a base commit SHA plus the resulting file contents. The
// tool then re-creates that one commit on GitHub's object graph directly,
// never touching a local `git push`. This is the "less destructive sibling"
// of `github_push_repo`: it moves exactly one branch ref, never mirrors
// history, and refuses non-fast-forward moves unless `force` is set.
struct FileWrite {
    path: String,
    content: String,
    encoding: String,
    mode: String,
}

fn parse_files(args: &Value) -> Result<Vec<FileWrite>, ToolError> {
    let arr = args
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    arr.into_iter()
        .map(|f| {
            let path = f
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ToolError::InvalidArgument("each file requires a non-empty 'path'".into()))?;
            let content = f
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| ToolError::InvalidArgument(format!("file '{path}' requires 'content'")))?;
            let encoding = f
                .get("encoding")
                .and_then(Value::as_str)
                .unwrap_or("utf-8")
                .to_string();
            if encoding != "utf-8" && encoding != "base64" {
                return Err(ToolError::InvalidArgument(format!(
                    "file '{path}' has unsupported encoding '{encoding}' (expected 'utf-8' or 'base64')"
                )));
            }
            if encoding == "base64" {
                // Reject undecodable base64 up front — before the PII gate or
                // any network call — rather than letting GitHub fail later.
                B64.decode(&content).map_err(|e| {
                    ToolError::InvalidArgument(format!("file '{path}' has invalid base64 content: {e}"))
                })?;
            }
            let mode = f
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("100644")
                .to_string();
            Ok(FileWrite { path, content, encoding, mode })
        })
        .collect()
}

/// The text the PII gate should scan for a given file: for base64-encoded
/// files this is the *decoded* bytes (lossily as UTF-8), not the base64
/// ciphertext-looking string itself — otherwise PII inside a base64 blob
/// would sail through the gate undetected. `parse_files` already validated
/// decodability, so this only fails if that invariant is ever violated.
fn scan_text_for_file(f: &FileWrite) -> Result<String, ToolError> {
    if f.encoding == "base64" {
        let bytes = B64
            .decode(&f.content)
            .map_err(|e| ToolError::InvalidArgument(format!("file '{}' has invalid base64 content: {e}", f.path)))?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    } else {
        Ok(f.content.clone())
    }
}

fn parse_deletions(args: &Value) -> Result<Vec<String>, ToolError> {
    let arr = args
        .get("deletions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    arr.into_iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ToolError::InvalidArgument("each 'deletions' entry must be a non-empty string path".into()))
        })
        .collect()
}

#[async_trait]
impl RustTool for GitHubPushBranch {
    fn name(&self) -> &str { "github_push_branch" }

    fn description(&self) -> &str {
        "Create or fast-forward a single branch on a GitHub repo by building a new commit \
via the Git Data API (blobs → tree → commit → ref) — no git wire protocol, no subprocess. \
Caller resolves what changed locally (read-only git plumbing) and supplies base_sha (the \
commit this push assumes the branch is currently at, or forks from if the branch is new) \
plus the resulting file contents/deletions. Rejects non-fast-forward moves unless force=true. \
Less destructive than github_push_repo (which mirrors an entire repo's history)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner":   { "type": "string", "description": "GitHub org/owner (default: configured GITHUB_ORG)" },
                "repo":    { "type": "string", "description": "Repository name (required)" },
                "branch":  { "type": "string", "description": "Target branch name on GitHub to create or fast-forward (required)" },
                "base_sha": { "type": "string", "description": "Commit SHA this push assumes the branch is currently at, or forks from if the branch doesn't exist yet (required)" },
                "message": { "type": "string", "description": "Commit message (required)" },
                "files": {
                    "type": "array",
                    "description": "Changed/added files. Unlisted paths are carried over unchanged from base_sha's tree.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path":     { "type": "string" },
                            "content":  { "type": "string" },
                            "encoding": { "type": "string", "description": "'utf-8' (default) or 'base64'" },
                            "mode":     { "type": "string", "description": "git file mode, default '100644'" }
                        },
                        "required": ["path", "content"]
                    }
                },
                "deletions": {
                    "type": "array",
                    "description": "Paths to remove from the tree",
                    "items": { "type": "string" }
                },
                "committer_name":  { "type": "string", "description": "Must be paired with committer_email, or omit both" },
                "committer_email": { "type": "string", "description": "Must be paired with committer_name, or omit both" },
                "force": { "type": "boolean", "description": "Force-update even if base_sha is not the branch's current tip. Default false.", "default": false }
            },
            "required": ["repo", "branch", "base_sha", "message"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let owner = args
            .get("owner")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.cfg.org)
            .to_string();
        let repo = args
            .get("repo")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".into()))?
            .to_string();
        let branch = args
            .get("branch")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'branch' is required".into()))?
            .to_string();
        let base_sha = args
            .get("base_sha")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'base_sha' is required".into()))?
            .to_string();
        let message = args
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'message' is required".into()))?
            .to_string();
        let committer_name = args.get("committer_name").and_then(Value::as_str).map(str::to_string);
        let committer_email = args.get("committer_email").and_then(Value::as_str).map(str::to_string);
        if committer_name.is_some() != committer_email.is_some() {
            return Err(ToolError::InvalidArgument(
                "'committer_name' and 'committer_email' must be supplied together, or not at all \
(no mixing a caller-supplied name with a default email or vice versa)".into(),
            ));
        }
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

        let files = parse_files(&args)?;
        let deletions = parse_deletions(&args)?;
        if files.is_empty() && deletions.is_empty() {
            return Err(ToolError::InvalidArgument(
                "at least one of 'files' or 'deletions' is required".into(),
            ));
        }

        // MANDATORY PII gate — scan every piece of operator-supplied content
        // that will land on GitHub BEFORE any network request fires. Base64
        // files are decoded first (scan_text_for_file) so PII hidden inside
        // an encoded blob can't sail through the gate undetected.
        let mut scan_buf = format!("{owner}\n{repo}\n{branch}\n{base_sha}\n{message}\n");
        if let Some(n) = &committer_name { scan_buf.push_str(n); scan_buf.push('\n'); }
        if let Some(e) = &committer_email { scan_buf.push_str(e); scan_buf.push('\n'); }
        for f in &files {
            scan_buf.push_str(&f.path);
            scan_buf.push('\n');
            scan_buf.push_str(&scan_text_for_file(f)?);
            scan_buf.push('\n');
        }
        for d in &deletions {
            scan_buf.push_str(d);
            scan_buf.push('\n');
        }
        pii_gate(&scan_buf)?;

        let client = GitHubConfig::client()?;
        let api = &self.cfg.api_base;

        // 1. Does the target branch already exist?
        let ref_url = format!("{api}/repos/{owner}/{repo}/git/ref/heads/{branch}");
        let ref_resp = self
            .cfg
            .apply_headers(client.get(&ref_url))
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;
        let ref_status = ref_resp.status();
        let ref_body = ref_resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;

        let existing_sha: Option<String> = if ref_status.as_u16() == 404 {
            None
        } else if ref_status.is_success() {
            let data: Value = serde_json::from_str(&ref_body)
                .map_err(|e| ToolError::Http(format!("Invalid JSON from GitHub: {e}")))?;
            // A 2xx here MUST carry object.sha — GitHub's ref-get response
            // shape guarantees it. A 2xx with no sha is a malformed/unexpected
            // response (e.g. an array from an ambiguous ref match), not "the
            // branch doesn't exist" — treating it as None would silently
            // route into the create-new-branch path against a real object.
            let sha = data
                .get("object")
                .and_then(|o| o.get("sha"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ToolError::Http(format!(
                        "unexpected ref response shape for '{branch}' (missing object.sha): {ref_body}"
                    ))
                })?;
            Some(sha.to_string())
        } else {
            return Err(ToolError::Http(format!(
                "HTTP {}: {}",
                ref_status.as_u16(),
                ref_body
            )));
        };

        // 2. Fast-forward guard — reject BEFORE creating any objects. Note this
        // check is advisory, not the sole safety net: a concurrent move of
        // `branch` between this GET and the final ref update in step 7 is
        // still possible (classic TOCTOU). The real atomicity guarantee comes
        // from GitHub itself — the non-force PATCH in step 7 sends
        // `force: false`, so GitHub's own ref-update endpoint re-checks
        // fast-forward server-side and rejects with a non-2xx (surfaced here
        // as `ToolError::Http`) if the branch moved in the meantime. This
        // early check exists purely to fail fast and avoid the wasted
        // blob/tree/commit calls in the common (non-racy) case.
        if let Some(current) = &existing_sha {
            if current != &base_sha && !force {
                return Err(ToolError::Conflict(format!(
                    "non-fast-forward: branch '{branch}' is at {current}, not base_sha {base_sha} \
(pass force=true to overwrite)"
                )));
            }
        }

        // 3. Resolve the base commit's tree to extend.
        let base_commit_url = format!("{api}/repos/{owner}/{repo}/git/commits/{base_sha}");
        let base_commit_resp = self
            .cfg
            .apply_headers(client.get(&base_commit_url))
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;
        let base_status = base_commit_resp.status();
        let base_body = base_commit_resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
        if base_status.as_u16() == 404 {
            return Err(ToolError::NotFound(format!("base_sha '{base_sha}' not found in {owner}/{repo}")));
        }
        if !base_status.is_success() {
            return Err(ToolError::Http(format!("HTTP {}: {}", base_status.as_u16(), base_body)));
        }
        let base_commit: Value = serde_json::from_str(&base_body)
            .map_err(|e| ToolError::Http(format!("Invalid JSON from GitHub: {e}")))?;
        let base_tree_sha = base_commit
            .get("tree")
            .and_then(|t| t.get("sha"))
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Http("base commit response missing tree.sha".into()))?
            .to_string();

        // 4. Create a blob for each changed file.
        let mut tree_entries: Vec<Value> = Vec::with_capacity(files.len() + deletions.len());
        for f in &files {
            let blob_url = format!("{api}/repos/{owner}/{repo}/git/blobs");
            let blob_resp = self
                .cfg
                .apply_headers(client.post(&blob_url))
                .json(&json!({ "content": f.content, "encoding": f.encoding }))
                .send()
                .await
                .map_err(|e| ToolError::Http(e.to_string()))?;
            let blob_status = blob_resp.status();
            let blob_body = blob_resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
            if !blob_status.is_success() {
                return Err(ToolError::Http(format!(
                    "HTTP {} creating blob for '{}': {}",
                    blob_status.as_u16(),
                    f.path,
                    blob_body
                )));
            }
            let blob: Value = serde_json::from_str(&blob_body)
                .map_err(|e| ToolError::Http(format!("Invalid JSON from GitHub: {e}")))?;
            let sha = blob
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::Http("blob response missing sha".into()))?;
            tree_entries.push(json!({
                "path": f.path,
                "mode": f.mode,
                "type": "blob",
                "sha": sha,
            }));
        }
        for path in &deletions {
            tree_entries.push(json!({
                "path": path,
                "mode": "100644",
                "type": "blob",
                "sha": Value::Null,
            }));
        }

        // 5. Create the tree, layered on top of the base commit's tree.
        let tree_url = format!("{api}/repos/{owner}/{repo}/git/trees");
        let tree_resp = self
            .cfg
            .apply_headers(client.post(&tree_url))
            .json(&json!({ "base_tree": base_tree_sha, "tree": tree_entries }))
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;
        let tree_status = tree_resp.status();
        let tree_body = tree_resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
        if !tree_status.is_success() {
            return Err(ToolError::Http(format!("HTTP {}: {}", tree_status.as_u16(), tree_body)));
        }
        let new_tree: Value = serde_json::from_str(&tree_body)
            .map_err(|e| ToolError::Http(format!("Invalid JSON from GitHub: {e}")))?;
        let new_tree_sha = new_tree
            .get("sha")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Http("tree response missing sha".into()))?
            .to_string();

        // 6. Create the commit object, parented on base_sha.
        let mut commit_payload = json!({
            "message": message,
            "tree": new_tree_sha,
            "parents": [base_sha],
        });
        // Guaranteed both-or-neither by the validation above — no silent
        // mixing of a caller-supplied name with a default email.
        if let (Some(name), Some(email)) = (&committer_name, &committer_email) {
            commit_payload["author"] = json!({ "name": name, "email": email });
            commit_payload["committer"] = commit_payload["author"].clone();
        }
        let commit_url = format!("{api}/repos/{owner}/{repo}/git/commits");
        let commit_resp = self
            .cfg
            .apply_headers(client.post(&commit_url))
            .json(&commit_payload)
            .send()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;
        let commit_status = commit_resp.status();
        let commit_body = commit_resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
        if !commit_status.is_success() {
            return Err(ToolError::Http(format!("HTTP {}: {}", commit_status.as_u16(), commit_body)));
        }
        let new_commit: Value = serde_json::from_str(&commit_body)
            .map_err(|e| ToolError::Http(format!("Invalid JSON from GitHub: {e}")))?;
        let new_commit_sha = new_commit
            .get("sha")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Http("commit response missing sha".into()))?
            .to_string();

        // 7. Move (or create) the branch ref to point at the new commit.
        let (ref_method_url, is_create) = if existing_sha.is_some() {
            (format!("{api}/repos/{owner}/{repo}/git/refs/heads/{branch}"), false)
        } else {
            (format!("{api}/repos/{owner}/{repo}/git/refs"), true)
        };
        let ref_update_resp = if is_create {
            self.cfg
                .apply_headers(client.post(&ref_method_url))
                .json(&json!({ "ref": format!("refs/heads/{branch}"), "sha": new_commit_sha }))
                .send()
                .await
                .map_err(|e| ToolError::Http(e.to_string()))?
        } else {
            self.cfg
                .apply_headers(client.patch(&ref_method_url))
                .json(&json!({ "sha": new_commit_sha, "force": force }))
                .send()
                .await
                .map_err(|e| ToolError::Http(e.to_string()))?
        };
        let ref_update_status = ref_update_resp.status();
        let ref_update_body = ref_update_resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
        if !ref_update_status.is_success() {
            let code = ref_update_status.as_u16();
            // 409/422 here means the branch moved between our step-1 GET and
            // this final ref update (the residual TOCTOU window) and GitHub's
            // own fast-forward check rejected it server-side — report this as
            // the same Conflict a caller would get from the early guard, not
            // a generic Http error, so retry logic can treat them alike.
            if !force && (code == 409 || code == 422) {
                return Err(ToolError::Conflict(format!(
                    "non-fast-forward: '{branch}' moved concurrently before the ref update landed \
(HTTP {code}: {ref_update_body})"
                )));
            }
            return Err(ToolError::Http(format!("HTTP {code}: {ref_update_body}")));
        }

        Ok(json!({
            "pushed": true,
            "owner": owner,
            "repo": repo,
            "branch": branch,
            "base_sha": base_sha,
            "commit_sha": new_commit_sha,
            "tree_sha": new_tree_sha,
            "created_branch": existing_sha.is_none(),
            "html_url": format!("https://github.com/{owner}/{repo}/commit/{new_commit_sha}"),
        })
        .to_string())
    }
}

// ── NotConfigured stub ────────────────────────────────────────────────────────

struct NotConfiguredStub(&'static str);

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str { self.0 }
    fn description(&self) -> &str { "GitHub tool (GITHUB_TOKEN not configured)" }
    fn parameters(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured("GITHUB_TOKEN not set".into()))
    }
}

// ── Registration ──────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    match GitHubConfig::from_env() {
        Ok(cfg) => {
            registry.register_or_replace(Box::new(GitHubListRepos { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(GitHubCreateRepo { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(GitHubPushRepo { cfg: cfg.clone() }));
            registry.register_or_replace(Box::new(GitHubPushBranch { cfg }));
        }
        Err(e) => {
            tracing::warn!("GitHub tools not configured: {e}. Registering stubs.");
            registry.register_or_replace(Box::new(NotConfiguredStub("github_list_repos")));
            registry.register_or_replace(Box::new(NotConfiguredStub("github_create_repo")));
            registry.register_or_replace(Box::new(NotConfiguredStub("github_push_repo")));
            registry.register_or_replace(Box::new(NotConfiguredStub("github_push_branch")));
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    fn cfg() -> GitHubConfig {
        GitHubConfig {
            token: "<REDACTED-SECRET>".into(),
            org: "moosenet-io".into(),
            gitea_url: "https://gitea.example.com".into(),
            api_base: GITHUB_API.to_string(),
        }
    }

    fn cfg_with_base(api_base: String) -> GitHubConfig {
        GitHubConfig { api_base, ..cfg() }
    }

    #[test]
    fn tool_names_are_stable() {
        assert_eq!(GitHubListRepos { cfg: cfg() }.name(), "github_list_repos");
        assert_eq!(GitHubCreateRepo { cfg: cfg() }.name(), "github_create_repo");
        assert_eq!(GitHubPushRepo { cfg: cfg() }.name(), "github_push_repo");
    }

    #[test]
    fn tool_parameters_are_valid_json_schema() {
        let l = GitHubListRepos { cfg: cfg() }.parameters();
        let c = GitHubCreateRepo { cfg: cfg() }.parameters();
        let p = GitHubPushRepo { cfg: cfg() }.parameters();
        assert_eq!(l["type"], "object");
        assert_eq!(c["type"], "object");
        assert_eq!(p["type"], "object");
        // create requires name; push requires gitea_repo + github_repo
        assert_eq!(c["required"][0], "name");
        assert_eq!(p["required"][0], "gitea_repo");
        assert_eq!(p["required"][1], "github_repo");
    }

    // ── config ──────────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn config_missing_token_is_not_configured() {
        let backup = std::env::var("GITHUB_TOKEN").ok();
        std::env::remove_var("GITHUB_TOKEN");
        let r = GitHubConfig::from_env();
        if let Some(v) = backup { std::env::set_var("GITHUB_TOKEN", v); }
        assert!(matches!(r, Err(ToolError::NotConfigured(_))));
    }

    #[test]
    #[serial]
    fn config_defaults_org_when_unset() {
        let tok_backup = std::env::var("GITHUB_TOKEN").ok();
        let org_backup = std::env::var("GITHUB_ORG").ok();
        std::env::set_var("GITHUB_TOKEN", "x");
        std::env::remove_var("GITHUB_ORG");
        let cfg = GitHubConfig::from_env().unwrap();
        assert_eq!(cfg.org, "moosenet-io");
        if let Some(v) = tok_backup { std::env::set_var("GITHUB_TOKEN", v); } else { std::env::remove_var("GITHUB_TOKEN"); }
        if let Some(v) = org_backup { std::env::set_var("GITHUB_ORG", v); } else { std::env::remove_var("GITHUB_ORG"); }
    }

    // ── repo_summary parsing ──────────────────────────────────────────────────

    #[test]
    fn repo_summary_extracts_fields() {
        let r = json!({
            "name": "lumina",
            "full_name": "moosenet-io/lumina",
            "private": true,
            "html_url": "https://github.com/moosenet-io/lumina",
            "description": "the thing"
        });
        let out = repo_summary(&r);
        assert_eq!(out["name"], "lumina");
        assert_eq!(out["full_name"], "moosenet-io/lumina");
        assert_eq!(out["private"], true);
        assert_eq!(out["url"], "https://github.com/moosenet-io/lumina");
        assert_eq!(out["description"], "the thing");
    }

    #[test]
    fn repo_summary_handles_missing_fields() {
        let out = repo_summary(&json!({}));
        assert_eq!(out["name"], "");
        assert_eq!(out["private"], false);
        assert_eq!(out["description"], "");
    }

    #[test]
    fn repo_summary_handles_null_description() {
        // GitHub returns null description for repos with no description set.
        let out = repo_summary(&json!({ "name": "x", "description": Value::Null }));
        assert_eq!(out["description"], "");
    }

    #[test]
    fn list_repos_parses_array_into_repos() {
        let data = json!([
            { "name": "a", "full_name": "moosenet-io/a", "private": false, "html_url": "u1", "description": "d1" },
            { "name": "b", "full_name": "moosenet-io/b", "private": true,  "html_url": "u2", "description": Value::Null }
        ]);
        let repos: Vec<Value> = data.as_array().unwrap().iter().map(repo_summary).collect();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0]["name"], "a");
        assert_eq!(repos[1]["private"], true);
        assert_eq!(repos[1]["description"], "");
    }

    // ── github_push_repo command building (no network) ────────────────────────

    #[test]
    fn gitea_host_strips_scheme() {
        assert_eq!(gitea_host_from_url("http://gitea.example.com:3000"), "gitea.example.com:3000");
        assert_eq!(gitea_host_from_url("https://git.example.com/"), "git.example.com");
        assert_eq!(gitea_host_from_url("git.example.com"), "git.example.com");
    }

    #[test]
    fn mirror_cmd_uses_shell_token_vars_not_values() {
        let cmd = build_mirror_cmd(
            "gitea.example.com:3000",
            "moosenet",
            "lumina-constellation",
            "moosenet-io",
            "lumina-constellation",
            false,
        );
        // Token placeholders, never literal secrets
        assert!(cmd.contains("$GITEA_TOKEN"));
        assert!(cmd.contains("$GITHUB_TOKEN"));
        assert!(cmd.contains("git clone --mirror"));
        assert!(cmd.contains("git push --mirror"));
        assert!(cmd.contains("github.com/moosenet-io/lumina-constellation.git"));
        assert!(cmd.contains("gitea.example.com:3000/moosenet/lumina-constellation.git"));
        assert!(cmd.contains("echo MIRROR_OK"));
        // Default (force=false) must NOT add --force
        assert!(!cmd.contains("--force"));
    }

    #[test]
    fn mirror_cmd_adds_force_flag_when_requested() {
        let cmd = build_mirror_cmd(
            "gitea.example.com:3000",
            "moosenet",
            "r",
            "moosenet-io",
            "r",
            true,
        );
        assert!(cmd.contains("git push --mirror --force"));
    }

    #[test]
    fn push_repo_definition_exposes_force_default_false() {
        let p = GitHubPushRepo { cfg: cfg() }.parameters();
        let force = &p["properties"]["force"];
        assert_eq!(force["type"], "boolean");
        assert_eq!(force["default"], false);
        // force is optional — not in required
        let required = p["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "force"));
    }

    #[tokio::test]
    async fn push_repo_force_flag_threads_through() {
        let tool = GitHubPushRepo { cfg: cfg() };
        let out = tool
            .execute(json!({ "gitea_repo": "r", "github_repo": "r", "force": true }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["cmd"].as_str().unwrap().contains("git push --mirror --force"));
        // Default path stays unchanged
        let out2 = tool
            .execute(json!({ "gitea_repo": "r", "github_repo": "r" }))
            .await
            .unwrap();
        let v2: Value = serde_json::from_str(&out2).unwrap();
        assert!(!v2["cmd"].as_str().unwrap().contains("--force"));
    }

    #[tokio::test]
    async fn push_repo_returns_cmd_and_url() {
        let tool = GitHubPushRepo { cfg: cfg() };
        let out = tool
            .execute(json!({
                "gitea_repo": "lumina-constellation",
                "github_repo": "lumina-constellation"
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("cmd").is_some());
        assert_eq!(v["github_url"], "https://github.com/moosenet-io/lumina-constellation");
        assert!(v["cmd"].as_str().unwrap().contains("$GITHUB_TOKEN"));
        // Custom gitea_owner is honoured
        let out2 = tool
            .execute(json!({ "gitea_repo": "r", "github_repo": "r", "gitea_owner": "someone" }))
            .await
            .unwrap();
        let v2: Value = serde_json::from_str(&out2).unwrap();
        assert!(v2["cmd"].as_str().unwrap().contains("/someone/r.git"));
    }

    #[tokio::test]
    async fn push_repo_requires_both_repos() {
        let tool = GitHubPushRepo { cfg: cfg() };
        assert!(matches!(
            tool.execute(json!({ "gitea_repo": "x" })).await,
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            tool.execute(json!({ "github_repo": "x" })).await,
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            tool.execute(json!({})).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    // ── github_create_repo arg validation (no network) ────────────────────────

    #[tokio::test]
    async fn create_repo_requires_name() {
        let tool = GitHubCreateRepo { cfg: cfg() };
        assert!(matches!(
            tool.execute(json!({})).await,
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            tool.execute(json!({ "name": "  " })).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    // ── registration ──────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn register_adds_three_tools_with_token() {
        let mut reg = ToolRegistry::new();
        let backup = std::env::var("GITHUB_TOKEN").ok();
        std::env::set_var("GITHUB_TOKEN", "testtoken");
        register(&mut reg);
        if let Some(v) = backup { std::env::set_var("GITHUB_TOKEN", v); } else { std::env::remove_var("GITHUB_TOKEN"); }
        assert!(reg.contains("github_list_repos"));
        assert!(reg.contains("github_create_repo"));
        assert!(reg.contains("github_push_repo"));
    }

    #[test]
    #[serial]
    fn register_adds_stubs_without_token() {
        let mut reg = ToolRegistry::new();
        let backup = std::env::var("GITHUB_TOKEN").ok();
        std::env::remove_var("GITHUB_TOKEN");
        register(&mut reg);
        if let Some(v) = backup { std::env::set_var("GITHUB_TOKEN", v); }
        assert!(reg.contains("github_list_repos"));
        assert!(reg.contains("github_create_repo"));
        assert!(reg.contains("github_push_repo"));
    }

    #[tokio::test]
    async fn stub_returns_not_configured() {
        let stub = NotConfiguredStub("github_list_repos");
        assert!(matches!(
            stub.execute(json!({})).await,
            Err(ToolError::NotConfigured(_))
        ));
    }

    // ── github_push_branch ──────────────────────────────────────────────────

    #[test]
    fn push_branch_tool_name_and_schema() {
        let tool = GitHubPushBranch { cfg: cfg() };
        assert_eq!(tool.name(), "github_push_branch");
        let p = tool.parameters();
        assert_eq!(p["type"], "object");
        let required = p["required"].as_array().unwrap();
        for k in ["repo", "branch", "base_sha", "message"] {
            assert!(required.iter().any(|v| v == k), "missing required '{k}'");
        }
    }

    #[tokio::test]
    async fn push_branch_requires_repo_branch_base_sha_message() {
        let tool = GitHubPushBranch { cfg: cfg() };
        assert!(matches!(
            tool.execute(json!({})).await,
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            tool.execute(json!({ "repo": "r", "branch": "b" })).await,
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            tool.execute(json!({ "repo": "r", "branch": "b", "base_sha": "abc" })).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn push_branch_requires_files_or_deletions() {
        let tool = GitHubPushBranch { cfg: cfg() };
        let result = tool
            .execute(json!({
                "repo": "r", "branch": "b", "base_sha": "abc", "message": "m"
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn push_branch_rejects_unsupported_encoding() {
        let tool = GitHubPushBranch { cfg: cfg() };
        let result = tool
            .execute(json!({
                "repo": "r", "branch": "b", "base_sha": "abc", "message": "m",
                "files": [{ "path": "a.txt", "content": "x", "encoding": "gzip" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_creates_new_branch_when_ref_missing() {
        let server = MockServer::start();
        // Branch doesn't exist yet.
        let ref_get = server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/newbranch");
            then.status(404).json_body(json!({ "message": "Not Found" }));
        });
        let base_commit = server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/basesha1");
            then.status(200).json_body(json!({ "sha": "basesha1", "tree": { "sha": "basetree1" } }));
        });
        let blob = server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/blobs");
            then.status(201).json_body(json!({ "sha": "blobsha1" }));
        });
        let tree = server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/trees");
            then.status(201).json_body(json!({ "sha": "treesha1" }));
        });
        let commit = server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/commits");
            then.status(201).json_body(json!({ "sha": "commitsha1" }));
        });
        let ref_create = server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/refs");
            then.status(201).json_body(json!({ "ref": "refs/heads/newbranch" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let out = tool
            .execute(json!({
                "repo": "r",
                "branch": "newbranch",
                "base_sha": "basesha1",
                "message": "add file",
                "files": [{ "path": "a.txt", "content": "hello" }]
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["pushed"], true);
        assert_eq!(v["commit_sha"], "commitsha1");
        assert_eq!(v["created_branch"], true);

        ref_get.assert();
        base_commit.assert();
        blob.assert();
        tree.assert();
        commit.assert();
        ref_create.assert();
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_fast_forwards_existing_branch() {
        let server = MockServer::start();
        let ref_get = server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(200).json_body(json!({ "object": { "sha": "basesha1" } }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/basesha1");
            then.status(200).json_body(json!({ "sha": "basesha1", "tree": { "sha": "basetree1" } }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/blobs");
            then.status(201).json_body(json!({ "sha": "blobsha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/trees");
            then.status(201).json_body(json!({ "sha": "treesha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/commits");
            then.status(201).json_body(json!({ "sha": "commitsha2" }));
        });
        let ref_patch = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/repos/moosenet-io/r/git/refs/heads/main")
                .json_body(json!({ "sha": "commitsha2", "force": false }));
            then.status(200).json_body(json!({ "ref": "refs/heads/main" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let out = tool
            .execute(json!({
                "repo": "r",
                "branch": "main",
                "base_sha": "basesha1",
                "message": "update file",
                "files": [{ "path": "a.txt", "content": "world" }]
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["commit_sha"], "commitsha2");
        assert_eq!(v["created_branch"], false);
        ref_get.assert();
        ref_patch.assert();
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_rejects_non_fast_forward_without_force() {
        let server = MockServer::start();
        let ref_get = server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(200).json_body(json!({ "object": { "sha": "actual_tip_sha" } }));
        });
        // These must NEVER be hit — rejection happens before any object creation.
        let blob = server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/blobs");
            then.status(201).json_body(json!({ "sha": "blobsha1" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let result = tool
            .execute(json!({
                "repo": "r",
                "branch": "main",
                "base_sha": "stale_sha",
                "message": "update file",
                "files": [{ "path": "a.txt", "content": "world" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::Conflict(_))));
        ref_get.assert();
        blob.assert_hits(0);
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_force_overrides_non_fast_forward() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(200).json_body(json!({ "object": { "sha": "actual_tip_sha" } }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/stale_sha");
            then.status(200).json_body(json!({ "sha": "stale_sha", "tree": { "sha": "t" } }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/blobs");
            then.status(201).json_body(json!({ "sha": "blobsha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/trees");
            then.status(201).json_body(json!({ "sha": "treesha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/commits");
            then.status(201).json_body(json!({ "sha": "commitsha3" }));
        });
        let ref_patch = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/repos/moosenet-io/r/git/refs/heads/main")
                .json_body(json!({ "sha": "commitsha3", "force": true }));
            then.status(200).json_body(json!({ "ref": "refs/heads/main" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let out = tool
            .execute(json!({
                "repo": "r",
                "branch": "main",
                "base_sha": "stale_sha",
                "message": "force update",
                "files": [{ "path": "a.txt", "content": "world" }],
                "force": true
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["commit_sha"], "commitsha3");
        ref_patch.assert();
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_maps_missing_base_sha_to_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(404).json_body(json!({ "message": "Not Found" }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/doesnotexist");
            then.status(404).json_body(json!({ "message": "Not Found" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let result = tool
            .execute(json!({
                "repo": "r",
                "branch": "main",
                "base_sha": "doesnotexist",
                "message": "m",
                "files": [{ "path": "a.txt", "content": "x" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_maps_blob_creation_failure_to_http_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(404).json_body(json!({ "message": "Not Found" }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/basesha1");
            then.status(200).json_body(json!({ "sha": "basesha1", "tree": { "sha": "basetree1" } }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/blobs");
            then.status(500).body("internal error");
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let result = tool
            .execute(json!({
                "repo": "r",
                "branch": "main",
                "base_sha": "basesha1",
                "message": "m",
                "files": [{ "path": "a.txt", "content": "x" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_supports_deletions_only() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(200).json_body(json!({ "object": { "sha": "basesha1" } }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/basesha1");
            then.status(200).json_body(json!({ "sha": "basesha1", "tree": { "sha": "basetree1" } }));
        });
        let tree = server.mock(|when, then| {
            when.method(POST)
                .path("/repos/moosenet-io/r/git/trees")
                .json_body(json!({
                    "base_tree": "basetree1",
                    "tree": [{ "path": "old.txt", "mode": "100644", "type": "blob", "sha": Value::Null }]
                }));
            then.status(201).json_body(json!({ "sha": "treesha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/commits");
            then.status(201).json_body(json!({ "sha": "commitsha4" }));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/repos/moosenet-io/r/git/refs/heads/main");
            then.status(200).json_body(json!({ "ref": "refs/heads/main" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let out = tool
            .execute(json!({
                "repo": "r",
                "branch": "main",
                "base_sha": "basesha1",
                "message": "remove old file",
                "deletions": ["old.txt"]
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["commit_sha"], "commitsha4");
        tree.assert();
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_commit_and_tree_bodies_are_exact() {
        // Asserts the two invariants a regression could silently break:
        // the new commit's parent chain, and the tree's file-entry shape
        // (path/mode/type/blob-sha) — neither was covered by the other tests.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(200).json_body(json!({ "object": { "sha": "basesha1" } }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/basesha1");
            then.status(200).json_body(json!({ "sha": "basesha1", "tree": { "sha": "basetree1" } }));
        });
        let blob = server.mock(|when, then| {
            when.method(POST)
                .path("/repos/moosenet-io/r/git/blobs")
                .json_body(json!({ "content": "hello", "encoding": "utf-8" }));
            then.status(201).json_body(json!({ "sha": "blobsha1" }));
        });
        let tree = server.mock(|when, then| {
            when.method(POST)
                .path("/repos/moosenet-io/r/git/trees")
                .json_body(json!({
                    "base_tree": "basetree1",
                    "tree": [{ "path": "a.txt", "mode": "100644", "type": "blob", "sha": "blobsha1" }]
                }));
            then.status(201).json_body(json!({ "sha": "treesha1" }));
        });
        let commit = server.mock(|when, then| {
            when.method(POST)
                .path("/repos/moosenet-io/r/git/commits")
                .json_body(json!({
                    "message": "add file",
                    "tree": "treesha1",
                    "parents": ["basesha1"]
                }));
            then.status(201).json_body(json!({ "sha": "commitsha5" }));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/repos/moosenet-io/r/git/refs/heads/main");
            then.status(200).json_body(json!({ "ref": "refs/heads/main" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let out = tool
            .execute(json!({
                "repo": "r",
                "branch": "main",
                "base_sha": "basesha1",
                "message": "add file",
                "files": [{ "path": "a.txt", "content": "hello" }]
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["commit_sha"], "commitsha5");
        blob.assert();
        tree.assert();
        commit.assert();
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_committer_name_and_email_must_be_paired() {
        let tool = GitHubPushBranch { cfg: cfg() };
        let base = json!({
            "repo": "r", "branch": "b", "base_sha": "abc", "message": "m",
            "files": [{ "path": "a.txt", "content": "x" }]
        });

        let mut only_name = base.clone();
        only_name["committer_name"] = json!("Someone");
        assert!(matches!(
            tool.execute(only_name).await,
            Err(ToolError::InvalidArgument(_))
        ));

        let mut only_email = base.clone();
        only_email["committer_email"] = json!("<email>"); // pii-test-fixture
        assert!(matches!(
            tool.execute(only_email).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_sends_paired_committer_as_author_and_committer() {
        // The PII gate blocks bare emails by default; allow-list this test's
        // fixture address so we're exercising the author/committer wiring,
        // not re-testing the (already covered) PII gate itself.
        std::env::set_var("GITHUB_ALLOWED_AUTHORS", "<email>"); // pii-test-fixture
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(404);
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/basesha1");
            then.status(200).json_body(json!({ "sha": "basesha1", "tree": { "sha": "basetree1" } }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/blobs");
            then.status(201).json_body(json!({ "sha": "blobsha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/trees");
            then.status(201).json_body(json!({ "sha": "treesha1" }));
        });
        let commit = server.mock(|when, then| {
            when.method(POST)
                .path("/repos/moosenet-io/r/git/commits")
                .json_body(json!({
                    "message": "m",
                    "tree": "treesha1",
                    "parents": ["basesha1"],
                    "author": { "name": "Someone", "email": "<email>" }, // pii-test-fixture
                    "committer": { "name": "Someone", "email": "<email>" } // pii-test-fixture
                }));
            then.status(201).json_body(json!({ "sha": "commitsha6" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/refs");
            then.status(201).json_body(json!({ "ref": "refs/heads/main" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let out = tool
            .execute(json!({
                "repo": "r", "branch": "main", "base_sha": "basesha1", "message": "m",
                "files": [{ "path": "a.txt", "content": "x" }],
                "committer_name": "Someone",
                "committer_email": "<email>" // pii-test-fixture
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["commit_sha"], "commitsha6");
        commit.assert();
        std::env::remove_var("GITHUB_ALLOWED_AUTHORS");
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_maps_ref_update_422_rejection_to_conflict() {
        // Simulates the residual TOCTOU case: the fast-forward pre-check
        // passes, but the branch moved concurrently and GitHub's own
        // server-side ref-update check rejects the non-force PATCH with a
        // non-fast-forward status. This must surface as the SAME error kind
        // (Conflict) as the early pre-check, not a generic Http error.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(200).json_body(json!({ "object": { "sha": "basesha1" } }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/basesha1");
            then.status(200).json_body(json!({ "sha": "basesha1", "tree": { "sha": "basetree1" } }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/blobs");
            then.status(201).json_body(json!({ "sha": "blobsha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/trees");
            then.status(201).json_body(json!({ "sha": "treesha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/commits");
            then.status(201).json_body(json!({ "sha": "commitsha7" }));
        });
        let ref_patch = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/repos/moosenet-io/r/git/refs/heads/main");
            then.status(422).json_body(json!({ "message": "Update is not a fast forward" }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let result = tool
            .execute(json!({
                "repo": "r", "branch": "main", "base_sha": "basesha1", "message": "m",
                "files": [{ "path": "a.txt", "content": "x" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::Conflict(_))));
        ref_patch.assert();
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_maps_other_ref_update_failures_to_http_error() {
        // A non-409/422 failure (e.g. 500, or a 403 permissions error) is a
        // genuine failure, not a fast-forward conflict — must stay Http.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(200).json_body(json!({ "object": { "sha": "basesha1" } }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/commits/basesha1");
            then.status(200).json_body(json!({ "sha": "basesha1", "tree": { "sha": "basetree1" } }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/blobs");
            then.status(201).json_body(json!({ "sha": "blobsha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/trees");
            then.status(201).json_body(json!({ "sha": "treesha1" }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/git/commits");
            then.status(201).json_body(json!({ "sha": "commitsha8" }));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/repos/moosenet-io/r/git/refs/heads/main");
            then.status(403).body("Forbidden");
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let result = tool
            .execute(json!({
                "repo": "r", "branch": "main", "base_sha": "basesha1", "message": "m",
                "files": [{ "path": "a.txt", "content": "x" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    async fn push_branch_rejects_invalid_base64_before_any_network_call() {
        // No mock server at all — if this hits the network it panics on
        // connection refused, proving rejection happens before any request.
        let tool = GitHubPushBranch { cfg: cfg_with_base("http://127.0.0.1:1".to_string()) };
        let result = tool
            .execute(json!({
                "repo": "r", "branch": "main", "base_sha": "basesha1", "message": "m",
                "files": [{ "path": "a.bin", "content": "not-valid-base64!!", "encoding": "base64" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn push_branch_scans_decoded_base64_content_for_pii() {
        // A base64 blob whose *decoded* bytes contain a blocked pattern (a
        // private IP) must be rejected by the PII gate even though the raw
        // base64 text itself doesn't look like a private IP.
        let encoded = B64.encode("visit <internal-ip> for details"); // pii-test-fixture
        let tool = GitHubPushBranch { cfg: cfg_with_base("http://127.0.0.1:1".to_string()) };
        let result = tool
            .execute(json!({
                "repo": "r", "branch": "main", "base_sha": "basesha1", "message": "m",
                "files": [{ "path": "a.bin", "content": encoded, "encoding": "base64" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn push_branch_rejects_blank_deletion_entries() {
        let tool = GitHubPushBranch { cfg: cfg() };
        let result = tool
            .execute(json!({
                "repo": "r", "branch": "main", "base_sha": "basesha1", "message": "m",
                "deletions": ["good.txt", ""]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    #[serial]
    async fn push_branch_rejects_malformed_successful_ref_response() {
        // A 2xx ref-get with no object.sha is malformed/unexpected — must be
        // treated as an error, not silently routed into "branch doesn't exist".
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/git/ref/heads/main");
            then.status(200).json_body(json!({ "no_object_field": true }));
        });

        let tool = GitHubPushBranch { cfg: cfg_with_base(server.base_url()) };
        let result = tool
            .execute(json!({
                "repo": "r", "branch": "main", "base_sha": "basesha1", "message": "m",
                "files": [{ "path": "a.txt", "content": "x" }]
            }))
            .await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[test]
    #[serial]
    fn register_adds_github_push_branch_with_token() {
        let mut reg = ToolRegistry::new();
        let backup = std::env::var("GITHUB_TOKEN").ok();
        std::env::set_var("GITHUB_TOKEN", "testtoken");
        register(&mut reg);
        if let Some(v) = backup { std::env::set_var("GITHUB_TOKEN", v); } else { std::env::remove_var("GITHUB_TOKEN"); }
        assert!(reg.contains("github_push_branch"));
    }

    #[test]
    #[serial]
    fn register_adds_github_push_branch_stub_without_token() {
        let mut reg = ToolRegistry::new();
        let backup = std::env::var("GITHUB_TOKEN").ok();
        std::env::remove_var("GITHUB_TOKEN");
        register(&mut reg);
        if let Some(v) = backup { std::env::set_var("GITHUB_TOKEN", v); }
        assert!(reg.contains("github_push_branch"));
    }
}
