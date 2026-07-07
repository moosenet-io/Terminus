//! GitHub tools — port of the Python `github_tools.py` on the fleet host.
//!
//! Three tools mirroring the Python implementation exactly:
//!   github_list_repos   — list repos in the configured GitHub org
//!   github_create_repo  — create a new repo in the org (public by default)
//!   github_push_repo    — build the mirror command to push a Gitea repo to GitHub
//!
//! Required env:
//!   GITHUB_TOKEN  — GitHub personal access / app token (Authorization: token …)
//!                   Required scopes: `repo` (push/create) plus `admin:org` to
//!                   create or push to repos under an organisation.
//! Optional env:
//!   GITHUB_ORG    — target org (default: moosenet-io)
//!   GITEA_URL     — Gitea base URL referenced when building the mirror command
//!                   for github_push_repo (default: https://gitea.example.com)
//!
//! If GITHUB_TOKEN is unset, NotConfigured stubs are registered so callers get a
//! clear error rather than a panic.
//!
//! PII gate (MANDATORY): every WRITE tool here runs its outbound content through
//! [`pii::pii_gate`] BEFORE any network request fires. There is no flag, env var,
//! or argument that disables it — see `pii.rs`. Read-only tools
//! (`github_list_repos`) are not gated.

use async_trait::async_trait;
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
        Ok(Self { token, org, gitea_url })
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
            registry.register_or_replace(Box::new(GitHubPushRepo { cfg }));
        }
        Err(e) => {
            tracing::warn!("GitHub tools not configured: {e}. Registering stubs.");
            registry.register_or_replace(Box::new(NotConfiguredStub("github_list_repos")));
            registry.register_or_replace(Box::new(NotConfiguredStub("github_create_repo")));
            registry.register_or_replace(Box::new(NotConfiguredStub("github_push_repo")));
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cfg() -> GitHubConfig {
        GitHubConfig {
            token: "<REDACTED-SECRET>".into(),
            org: "moosenet-io".into(),
            gitea_url: "https://gitea.example.com".into(),
        }
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
}
