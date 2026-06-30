//! Gitea tools: 10 RustTool implementations for the Gitea source-control API.
//!
//! All tools use `reqwest` for typed HTTP calls. Write operations include a PII
//! gate that scans content for private IP ranges and API-key patterns before
//! submitting to Gitea — this was MISSING from the Python gitea_tools.py.
//!
//! ## Configuration (env vars)
//! - `GITEA_URL`   — base URL, e.g. `https://gitea.example.com` (required)
//! - `GITEA_TOKEN` — personal access token (required)
//! - `GITEA_OWNER` — default repo owner/organisation (default: `"moosenet"`)

pub mod types;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::env;
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use types::{
    GiteaBranchInfo, GiteaCreatePrRequest, GiteaDeleteFileRequest, GiteaFileContent,
    GiteaFileRequest, GiteaFileResponse, GiteaPullRequest, GiteaRepo,
};

// ─── PII gate ────────────────────────────────────────────────────────────────

/// Private IP ranges that must not appear in committed content.
///
/// Patterns checked:
/// - `192.168.x.x`
/// - `10.x.x.x`
/// - `172.{16-31}.x.x`
/// - Bare API key patterns: long hex strings (≥32 chars) or `sk-...` tokens
fn pii_check(content: &str) -> Option<String> {
    // Private IP ranges
    let private_ip_patterns: &[(&str, &str)] = &[
        ("192.168.", "RFC-1918 192.168.x.x address"),
        ("10.", "RFC-1918 10.x.x.x address"),
    ];

    for (prefix, label) in private_ip_patterns {
        // Walk through occurrences and verify the next chars look like an IP octet
        let mut pos = 0;
        while let Some(idx) = content[pos..].find(prefix) {
            let abs = pos + idx;
            let after = &content[abs + prefix.len()..];
            // For 10. the following character must be a digit (avoids "10.times" etc.)
            if *prefix == "10." {
                if after.starts_with(|c: char| c.is_ascii_digit()) {
                    return Some(format!("Content contains private infrastructure value: {label}"));
                }
            } else {
                // 192.168. — treat any following content as a match
                return Some(format!("Content contains private infrastructure value: {label}"));
            }
            pos = abs + 1;
        }
    }

    // 172.16–31.x.x
    {
        let mut pos = 0;
        while let Some(idx) = content[pos..].find("172.") {
            let abs = pos + idx;
            let after = &content[abs + 4..];
            // Parse the next number
            let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num_str.parse::<u8>() {
                if (16..=31).contains(&n) {
                    return Some(
                        "Content contains private infrastructure value: RFC-1918 172.16-31.x.x address".to_string(),
                    );
                }
            }
            pos = abs + 1;
        }
    }

    // API key patterns: `sk-` prefixed tokens (OpenAI-style)
    if content.contains("sk-") {
        let sk_idx = content.find("sk-").unwrap();
        let after = &content[sk_idx + 3..];
        let token_len: usize = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .count();
        if token_len >= 20 {
            return Some(
                "Content appears to contain an API key (sk- token)".to_string(),
            );
        }
    }

    // Long hex strings ≥ 32 chars (bearer tokens, secrets)
    {
        let mut run = 0usize;
        for ch in content.chars() {
            if ch.is_ascii_hexdigit() {
                run += 1;
                if run >= 32 {
                    return Some(
                        "Content appears to contain a secret (long hex string)".to_string(),
                    );
                }
            } else {
                run = 0;
            }
        }
    }

    None
}

// ─── GiteaClient ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GiteaClient {
    http: Client,
    base_url: String,
    token: String,
    owner: String,
}

impl GiteaClient {
    /// Build from environment variables.
    ///
    /// Returns `Err(ToolError::NotConfigured)` if `GITEA_URL` is not set.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = env::var("GITEA_URL").map_err(|_| {
            ToolError::NotConfigured("GITEA_URL environment variable is not set".to_string())
        })?;
        let token = env::var("GITEA_TOKEN").unwrap_or_default();
        let owner = env::var("GITEA_OWNER").unwrap_or_else(|_| "moosenet".to_string());

        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;

        Ok(Self { http, base_url, token, owner })
    }

    fn api(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url.trim_end_matches('/'), path)
    }

    fn auth_header(&self) -> String {
        format!("token {}", self.token)
    }

    /// GET request returning parsed JSON or a ToolError.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ToolError> {
        let url = self.api(path);
        debug!("GET {url}");
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound("Resource not found in Gitea".to_string()));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Gitea returned {status}: {body}")));
        }
        resp.json::<T>()
            .await
            .map_err(|e| ToolError::Http(format!("JSON parse error: {e}")))
    }

    /// POST request sending JSON body, returning parsed JSON.
    async fn post<B, T>(&self, path: &str, body: &B) -> Result<T, ToolError>
    where
        B: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let url = self.api(path);
        debug!("POST {url}");
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Gitea returned {status}: {body_text}")));
        }
        resp.json::<T>()
            .await
            .map_err(|e| ToolError::Http(format!("JSON parse error: {e}")))
    }

    /// PUT request sending JSON body, returning parsed JSON.
    async fn put<B, T>(&self, path: &str, body: &B) -> Result<T, ToolError>
    where
        B: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let url = self.api(path);
        debug!("PUT {url}");
        let resp = self
            .http
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        // Gitea returns 422 when trying to PUT (update) a file that doesn't exist yet.
        // Callers should use POST (create) instead — this is surfaced as a clear error.
        if status == StatusCode::UNPROCESSABLE_ENTITY {
            return Err(ToolError::Http(
                "Gitea returned 422: file may not exist yet — use create_file for new files"
                    .to_string(),
            ));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Gitea returned {status}: {body_text}")));
        }
        resp.json::<T>()
            .await
            .map_err(|e| ToolError::Http(format!("JSON parse error: {e}")))
    }

    /// DELETE request sending JSON body; Gitea's delete-file endpoint uses a body.
    async fn delete_with_body<B>(&self, path: &str, body: &B) -> Result<(), ToolError>
    where
        B: serde::Serialize,
    {
        let url = self.api(path);
        debug!("DELETE {url}");
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound("File not found in repo".to_string()));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Gitea returned {status}: {body_text}")));
        }
        Ok(())
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Fetch the current SHA of a file. Needed before any update operation.
    pub async fn get_file_sha(&self, repo: &str, path: &str) -> Result<String, ToolError> {
        let endpoint = format!("/repos/{}/{}/contents/{}", self.owner, repo, path);
        let content: GiteaFileContent = self.get(&endpoint).await?;
        Ok(content.sha)
    }

    /// Resolve `owner` field: use explicit override or fall back to configured default.
    fn resolve_owner<'a>(&'a self, override_owner: Option<&'a str>) -> &'a str {
        override_owner.unwrap_or(&self.owner)
    }
}

// ─── Tool implementations ────────────────────────────────────────────────────

// 1. list_repos
pub struct ListRepos {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for ListRepos {
    fn name(&self) -> &str { "gitea_list_repos" }

    fn description(&self) -> &str {
        "List repositories for the configured Gitea owner/organisation."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max repos to return (default 50, max 50)",
                    "default": 50
                },
                "page": {
                    "type": "integer",
                    "description": "Page number (1-based, default 1)",
                    "default": 1
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let limit = args["limit"].as_u64().unwrap_or(50).min(50);
        let page = args["page"].as_u64().unwrap_or(1).max(1);

        let path = format!(
            "/repos/search?owner={}&limit={}&page={}",
            self.client.owner, limit, page
        );
        let raw: Value = self.client.get(&path).await?;
        // Gitea search returns {"data": [...], "ok": true}
        let repos: Vec<GiteaRepo> = serde_json::from_value(
            raw["data"].clone(),
        )
        .map_err(|e| ToolError::Http(format!("Failed to parse repo list: {e}")))?;

        if repos.is_empty() {
            return Ok(format!("No repositories found for '{}'.", self.client.owner));
        }

        let mut out = format!(
            "Repositories for '{}' (page {}, showing {}):\n\n",
            self.client.owner,
            page,
            repos.len()
        );
        for r in &repos {
            out.push_str(&format!(
                "• {} — {} ({}{})\n",
                r.full_name,
                if r.description.is_empty() { "no description" } else { &r.description },
                if r.private { "private, " } else { "" },
                r.default_branch,
            ));
        }
        Ok(out)
    }
}

// 2. get_repo
pub struct GetRepo {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for GetRepo {
    fn name(&self) -> &str { "gitea_get_repo" }

    fn description(&self) -> &str {
        "Get detailed information about a specific Gitea repository."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "owner": {
                    "type": "string",
                    "description": "Owner (optional — defaults to configured GITEA_OWNER)"
                }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let owner = self.client.resolve_owner(args["owner"].as_str());

        let path = format!("/repos/{}/{}", owner, repo);
        let r: GiteaRepo = self.client.get(&path).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(format!("Repository '{owner}/{repo}' not found")),
            other => other,
        })?;

        Ok(format!(
            "Repository: {}\nDescription: {}\nURL: {}\nDefault branch: {}\nPrivate: {}\nStars: {} | Forks: {} | Open issues: {}\nUpdated: {}",
            r.full_name,
            if r.description.is_empty() { "(none)".to_string() } else { r.description },
            r.html_url,
            r.default_branch,
            r.private,
            r.stars_count,
            r.forks_count,
            r.open_issues_count,
            r.updated.unwrap_or_default(),
        ))
    }
}

// 2b. create_repo
//
// Wraps POST {GITEA_URL}/api/v1/orgs/{org}/repos. Uses a direct reqwest call
// (rather than GiteaClient::post) so we can surface 422 (already exists) and
// 401/403 (auth) as clear, distinct errors. Credentials come from the shared
// GiteaClient (GITEA_URL/GITEA_TOKEN), never std::env::var here or hardcoded.
pub struct CreateRepo {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for CreateRepo {
    fn name(&self) -> &str { "gitea_create_repo" }

    fn description(&self) -> &str {
        "Create a new repository in a Gitea organisation. private=true by default."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "org":         { "type": "string",  "description": "Organisation to create the repo under" },
                "name":        { "type": "string",  "description": "Repository name" },
                "description": { "type": "string",  "description": "Repository description (optional)" },
                "private":     { "type": "boolean", "description": "Private repo? Default true", "default": true }
            },
            "required": ["org", "name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let org = args["org"].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'org' is required".to_string()))?;
        let name = args["name"].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'name' is required".to_string()))?;
        let description = args["description"].as_str().unwrap_or("");
        let private = args["private"].as_bool().unwrap_or(true);

        let payload = json!({
            "name": name,
            "description": description,
            "private": private,
            "auto_init": false,
        });

        let endpoint = format!("/orgs/{}/repos", org);
        let url = self.client.api(&endpoint);
        debug!("POST {url}");
        let resp = self
            .client
            .http
            .post(&url)
            .header("Authorization", self.client.auth_header())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::UNPROCESSABLE_ENTITY {
            return Err(ToolError::InvalidArgument(format!(
                "Repository '{org}/{name}' already exists (Gitea returned 422)."
            )));
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ToolError::Http(format!(
                "Gitea authentication/authorisation failed ({}). Check GITEA_TOKEN scope \
                 (needs write:organization for org repos).",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Gitea returned {status}: {body_text}")));
        }

        // Gitea's repo object includes html_url, clone_url and ssh_url; read them
        // directly from the response so we don't fabricate the SSH form ourselves.
        let repo: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Http(format!("JSON parse error: {e}")))?;

        Ok(json!({
            "full_name": repo.get("full_name").and_then(Value::as_str).unwrap_or(""),
            "html_url":  repo.get("html_url").and_then(Value::as_str).unwrap_or(""),
            "clone_url": repo.get("clone_url").and_then(Value::as_str).unwrap_or(""),
            "ssh_url":   repo.get("ssh_url").and_then(Value::as_str).unwrap_or(""),
        })
        .to_string())
    }
}

// 3. create_file
pub struct CreateFile {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for CreateFile {
    fn name(&self) -> &str { "gitea_create_file" }

    fn description(&self) -> &str {
        "Create a new file in a Gitea repository. Content must not contain private IPs or API keys."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":    { "type": "string", "description": "Repository name" },
                "path":    { "type": "string", "description": "File path within the repo" },
                "content": { "type": "string", "description": "File content (plain text)" },
                "message": { "type": "string", "description": "Commit message" },
                "branch":  { "type": "string", "description": "Branch (optional, defaults to repo default)" },
                "owner":   { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "path", "content", "message"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' is required".to_string()))?;
        let content = args["content"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'content' is required".to_string()))?;
        let message = args["message"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'message' is required".to_string()))?;
        let owner = self.client.resolve_owner(args["owner"].as_str());

        // PII gate
        if let Some(reason) = pii_check(content) {
            warn!("PII gate blocked create_file on {owner}/{repo}/{path}: {reason}");
            return Err(ToolError::InvalidArgument(format!(
                "Content rejected by PII gate: {reason}"
            )));
        }

        let body = GiteaFileRequest {
            message: message.to_string(),
            content: B64.encode(content),
            sha: None, // new file — no SHA
            branch: args["branch"].as_str().map(str::to_string),
            new_branch: None,
        };

        let endpoint = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        let resp: GiteaFileResponse = self.client.post(&endpoint, &body).await?;

        Ok(format!(
            "File created: {}/{}/{}\nCommit: {}",
            owner,
            repo,
            path,
            resp.commit.sha,
        ))
    }
}

// 4. read_file
pub struct ReadFile {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for ReadFile {
    fn name(&self) -> &str { "gitea_read_file" }

    fn description(&self) -> &str {
        "Read the contents of a file from a Gitea repository."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Repository name" },
                "path":   { "type": "string", "description": "File path within the repo" },
                "ref":    { "type": "string", "description": "Branch, tag, or commit SHA (optional)" },
                "owner":  { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' is required".to_string()))?;
        let owner = self.client.resolve_owner(args["owner"].as_str());

        let mut endpoint = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        if let Some(git_ref) = args["ref"].as_str() {
            endpoint.push_str(&format!("?ref={}", git_ref));
        }

        let fc: GiteaFileContent = self.client.get(&endpoint).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(format!("File not found in repo: {owner}/{repo}/{path}")),
            other => other,
        })?;

        // Decode base64 content
        let raw_content = fc.content.unwrap_or_default();
        // Gitea wraps lines with newlines in the base64 — strip them
        let clean = raw_content.replace('\n', "").replace('\r', "");
        let decoded = B64
            .decode(&clean)
            .map_err(|e| ToolError::Http(format!("Failed to decode file content: {e}")))?;
        let text = String::from_utf8_lossy(&decoded).to_string();

        Ok(format!(
            "File: {owner}/{repo}/{path}\nSHA: {}\nSize: {} bytes\n\n---\n{text}",
            fc.sha, fc.size
        ))
    }
}

// 5. update_file
pub struct UpdateFile {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for UpdateFile {
    fn name(&self) -> &str { "gitea_update_file" }

    fn description(&self) -> &str {
        "Update an existing file in a Gitea repository. Fetches current SHA automatically. \
         Content must not contain private IPs or API keys."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":    { "type": "string", "description": "Repository name" },
                "path":    { "type": "string", "description": "File path within the repo" },
                "content": { "type": "string", "description": "New file content (plain text)" },
                "message": { "type": "string", "description": "Commit message" },
                "branch":  { "type": "string", "description": "Branch (optional)" },
                "owner":   { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "path", "content", "message"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' is required".to_string()))?;
        let content = args["content"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'content' is required".to_string()))?;
        let message = args["message"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'message' is required".to_string()))?;
        let owner = self.client.resolve_owner(args["owner"].as_str());

        // PII gate before fetching SHA (fail fast)
        if let Some(reason) = pii_check(content) {
            warn!("PII gate blocked update_file on {owner}/{repo}/{path}: {reason}");
            return Err(ToolError::InvalidArgument(format!(
                "Content rejected by PII gate: {reason}"
            )));
        }

        // Fetch current SHA — required by Gitea for updates
        let sha = self.client.get_file_sha(repo, path).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(
                format!("File not found in repo: {owner}/{repo}/{path}. Use create_file for new files.")
            ),
            other => other,
        })?;

        let body = GiteaFileRequest {
            message: message.to_string(),
            content: B64.encode(content),
            sha: Some(sha),
            branch: args["branch"].as_str().map(str::to_string),
            new_branch: None,
        };

        let endpoint = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        let resp: GiteaFileResponse = self.client.put(&endpoint, &body).await?;

        Ok(format!(
            "File updated: {owner}/{repo}/{path}\nCommit: {}",
            resp.commit.sha,
        ))
    }
}

// 6. delete_file
pub struct DeleteFile {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for DeleteFile {
    fn name(&self) -> &str { "gitea_delete_file" }

    fn description(&self) -> &str {
        "Delete a file from a Gitea repository. Fetches current SHA automatically."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":    { "type": "string", "description": "Repository name" },
                "path":    { "type": "string", "description": "File path within the repo" },
                "message": { "type": "string", "description": "Commit message" },
                "branch":  { "type": "string", "description": "Branch (optional)" },
                "owner":   { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "path", "message"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' is required".to_string()))?;
        let message = args["message"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'message' is required".to_string()))?;
        let owner = self.client.resolve_owner(args["owner"].as_str());

        // Fetch current SHA — required by Gitea
        let sha = self.client.get_file_sha(repo, path).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(format!("File not found in repo: {owner}/{repo}/{path}")),
            other => other,
        })?;

        let body = GiteaDeleteFileRequest {
            message: message.to_string(),
            sha,
            branch: args["branch"].as_str().map(str::to_string),
        };

        let endpoint = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        self.client.delete_with_body(&endpoint, &body).await?;

        Ok(format!("File deleted: {owner}/{repo}/{path}"))
    }
}

// 7. list_prs
pub struct ListPrs {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for ListPrs {
    fn name(&self) -> &str { "gitea_list_prs" }

    fn description(&self) -> &str {
        "List pull requests for a Gitea repository."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "state": { "type": "string", "description": "Filter by state: open | closed | all (default: open)", "enum": ["open", "closed", "all"] },
                "limit": { "type": "integer", "description": "Max results (default 20)", "default": 20 },
                "page":  { "type": "integer", "description": "Page number (default 1)", "default": 1 },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let state = args["state"].as_str().unwrap_or("open");
        let limit = args["limit"].as_u64().unwrap_or(20).min(50);
        let page = args["page"].as_u64().unwrap_or(1).max(1);
        let owner = self.client.resolve_owner(args["owner"].as_str());

        let endpoint = format!(
            "/repos/{}/{}/pulls?state={}&limit={}&page={}",
            owner, repo, state, limit, page
        );
        let prs: Vec<GiteaPullRequest> = self.client.get(&endpoint).await?;

        if prs.is_empty() {
            return Ok(format!("No {} pull requests in {owner}/{repo}.", state));
        }

        let mut out = format!(
            "Pull requests in {owner}/{repo} ({state}, page {page}, showing {}):\n\n",
            prs.len()
        );
        for pr in &prs {
            out.push_str(&format!(
                "• #{} — {} [{}] by {} ({} → {})\n",
                pr.number,
                pr.title,
                pr.state,
                pr.user.login,
                pr.head.ref_name,
                pr.base.ref_name,
            ));
        }
        Ok(out)
    }
}

// 8. create_pr
pub struct CreatePr {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for CreatePr {
    fn name(&self) -> &str { "gitea_create_pr" }

    fn description(&self) -> &str {
        "Create a pull request in a Gitea repository."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "title": { "type": "string", "description": "PR title" },
                "head":  { "type": "string", "description": "Source branch" },
                "base":  { "type": "string", "description": "Target branch (e.g. main)" },
                "body":  { "type": "string", "description": "PR description (optional)" },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "title", "head", "base"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let title = args["title"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'title' is required".to_string()))?;
        let head = args["head"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'head' is required".to_string()))?;
        let base = args["base"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'base' is required".to_string()))?;
        let owner = self.client.resolve_owner(args["owner"].as_str());

        // PII gate on PR body if provided
        if let Some(body_text) = args["body"].as_str() {
            if let Some(reason) = pii_check(body_text) {
                warn!("PII gate blocked create_pr body for {owner}/{repo}: {reason}");
                return Err(ToolError::InvalidArgument(format!(
                    "PR body rejected by PII gate: {reason}"
                )));
            }
        }

        let body = GiteaCreatePrRequest {
            title: title.to_string(),
            head: head.to_string(),
            base: base.to_string(),
            body: args["body"].as_str().map(str::to_string),
        };

        let endpoint = format!("/repos/{}/{}/pulls", owner, repo);
        let pr: GiteaPullRequest = self.client.post(&endpoint, &body).await?;

        Ok(format!(
            "Pull request created: #{} — {}\nURL: {}\n{} → {}",
            pr.number, pr.title, pr.html_url, pr.head.ref_name, pr.base.ref_name,
        ))
    }
}

// 9. merge_pr
pub struct MergePr {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for MergePr {
    fn name(&self) -> &str { "gitea_merge_pr" }

    fn description(&self) -> &str {
        "Merge a pull request in a Gitea repository."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Repository name" },
                "pr":     { "type": "integer", "description": "Pull request number" },
                "style":  { "type": "string", "description": "Merge style: merge | rebase | squash (default: merge)", "enum": ["merge", "rebase", "squash"] },
                "message": { "type": "string", "description": "Merge commit message (optional)" },
                "owner":  { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "pr"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let pr_num = args["pr"].as_u64()
            .ok_or_else(|| ToolError::InvalidArgument("'pr' must be an integer".to_string()))?;
        let style = args["style"].as_str().unwrap_or("merge");
        let owner = self.client.resolve_owner(args["owner"].as_str());

        let mut body = json!({ "Do": style });
        if let Some(msg) = args["message"].as_str() {
            body["MergeMessageField"] = json!(msg);
        }

        let endpoint = format!("/repos/{}/{}/pulls/{}/merge", owner, repo, pr_num);
        // Merge endpoint returns 200 with no body on success
        let url = self.client.api(&endpoint);
        let resp = self
            .client
            .http
            .post(&url)
            .header("Authorization", self.client.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound(format!(
                "Pull request #{pr_num} not found in {owner}/{repo}"
            )));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Merge failed: {status}: {body_text}")));
        }

        Ok(format!("Pull request #{pr_num} merged into {base} in {owner}/{repo}.", base = style))
    }
}

// 10. list_branches
// ─── gitea_list_directory ─────────────────────────────────────────────────────

pub struct ListDirectory {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for ListDirectory {
    fn name(&self) -> &str { "gitea_list_directory" }

    fn description(&self) -> &str {
        "List files and sub-directories at a path in a Gitea repository. \
Returns entries with name, type (file/dir), path, and SHA."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "path":  { "type": "string", "description": "Directory path (empty for root)" },
                "ref":   { "type": "string", "description": "Branch, tag, or commit SHA (optional)" },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo  = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path  = args["path"].as_str().unwrap_or("").trim_matches('/');
        let owner = self.client.resolve_owner(args["owner"].as_str());

        let mut endpoint = if path.is_empty() {
            format!("/repos/{owner}/{repo}/contents/")
        } else {
            format!("/repos/{owner}/{repo}/contents/{path}")
        };
        if let Some(git_ref) = args["ref"].as_str() {
            // percent-encode spaces and special chars that matter in refs
            let encoded: String = git_ref.chars().map(|c| match c {
                ' ' => "%20".to_string(),
                '#' => "%23".to_string(),
                '?' => "%3F".to_string(),
                '&' => "%26".to_string(),
                c   => c.to_string(),
            }).collect();
            endpoint.push_str(&format!("?ref={encoded}"));
        }

        let entries: Vec<Value> = self.client.get(&endpoint).await
            .map_err(|e| match e {
                ToolError::NotFound(_) => ToolError::NotFound(
                    format!("Path not found: {owner}/{repo}/{path}")),
                other => other,
            })?;

        let mut out = format!("Directory: {owner}/{repo}/{}\n{} entries:\n",
            if path.is_empty() { "/" } else { path }, entries.len());
        for e in &entries {
            let kind = e["type"].as_str().unwrap_or("?");
            let name = e["name"].as_str().unwrap_or("?");
            let indicator = if kind == "dir" { "📁" } else { "📄" };
            out.push_str(&format!("  {indicator} {name}\n"));
        }
        Ok(out)
    }
}

pub struct ListBranches {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for ListBranches {
    fn name(&self) -> &str { "gitea_list_branches" }

    fn description(&self) -> &str {
        "List branches in a Gitea repository."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "limit": { "type": "integer", "description": "Max results (default 30)", "default": 30 },
                "page":  { "type": "integer", "description": "Page number (default 1)", "default": 1 },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let limit = args["limit"].as_u64().unwrap_or(30).min(50);
        let page = args["page"].as_u64().unwrap_or(1).max(1);
        let owner = self.client.resolve_owner(args["owner"].as_str());

        let endpoint = format!(
            "/repos/{}/{}/branches?limit={}&page={}",
            owner, repo, limit, page
        );
        let branches: Vec<GiteaBranchInfo> = self.client.get(&endpoint).await?;

        if branches.is_empty() {
            return Ok(format!("No branches found in {owner}/{repo}."));
        }

        let mut out = format!(
            "Branches in {owner}/{repo} (page {page}, showing {}):\n\n",
            branches.len()
        );
        for b in &branches {
            out.push_str(&format!(
                "• {} ({}{})\n",
                b.name,
                b.commit.id.get(..8).unwrap_or(&b.commit.id),
                if b.protected { ", protected" } else { "" },
            ));
        }
        Ok(out)
    }
}

// ─── Registration ────────────────────────────────────────────────────────────

/// Register all Gitea tools into the global ToolRegistry.
///
/// If `GITEA_URL` is not set the tools still register but return
/// `ToolError::NotConfigured` on every call.
pub fn register(registry: &mut ToolRegistry) {
    match GiteaClient::from_env() {
        Ok(client) => {
            let _ = registry.register(Box::new(ListRepos { client: client.clone() }));
            let _ = registry.register(Box::new(GetRepo { client: client.clone() }));
            let _ = registry.register(Box::new(CreateRepo { client: client.clone() }));
            let _ = registry.register(Box::new(CreateFile { client: client.clone() }));
            let _ = registry.register(Box::new(ReadFile { client: client.clone() }));
            let _ = registry.register(Box::new(UpdateFile { client: client.clone() }));
            let _ = registry.register(Box::new(DeleteFile { client: client.clone() }));
            let _ = registry.register(Box::new(ListPrs { client: client.clone() }));
            let _ = registry.register(Box::new(CreatePr { client: client.clone() }));
            let _ = registry.register(Box::new(MergePr { client: client.clone() }));
            let _ = registry.register(Box::new(ListBranches { client: client.clone() }));
            let _ = registry.register(Box::new(ListDirectory { client }));
        }
        Err(e) => {
            tracing::warn!("Gitea tools not configured: {e}. Registering no-op stubs.");
            // Register stubs that return NotConfigured — this way the tools still appear
            // in the catalog and give a useful error message rather than being invisible.
            macro_rules! stub {
                ($name:literal, $desc:literal) => {
                    let _ = registry.register(Box::new(NotConfiguredStub {
                        tool_name: $name,
                        description: $desc,
                    }));
                };
            }
            stub!("gitea_list_repos", "List Gitea repositories (not configured)");
            stub!("gitea_get_repo", "Get Gitea repository details (not configured)");
            stub!("gitea_create_repo", "Create a Gitea repository (not configured)");
            stub!("gitea_create_file", "Create file in Gitea (not configured)");
            stub!("gitea_read_file", "Read file from Gitea (not configured)");
            stub!("gitea_update_file", "Update file in Gitea (not configured)");
            stub!("gitea_delete_file", "Delete file in Gitea (not configured)");
            stub!("gitea_list_prs", "List Gitea pull requests (not configured)");
            stub!("gitea_create_pr", "Create Gitea pull request (not configured)");
            stub!("gitea_merge_pr", "Merge Gitea pull request (not configured)");
            stub!("gitea_list_branches", "List Gitea branches (not configured)");
            stub!("gitea_list_directory", "List directory contents in Gitea (not configured)");
        }
    }
}

struct NotConfiguredStub {
    tool_name: &'static str,
    description: &'static str,
}

#[async_trait]
impl RustTool for NotConfiguredStub {
    fn name(&self) -> &str { self.tool_name }
    fn description(&self) -> &str { self.description }
    fn parameters(&self) -> Value { json!({"type": "object", "properties": {}}) }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::NotConfigured(
            "GITEA_URL environment variable is not set. Configure Gitea integration to use this tool.".to_string(),
        ))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn mock_client(server: &MockServer) -> GiteaClient {
        GiteaClient {
            http: Client::new(),
            base_url: server.base_url(),
            token: "test-token".to_string(),
            owner: "testorg".to_string(),
        }
    }

    // ── PII gate tests ────────────────────────────────────────────────────

    #[test]
    fn test_pii_gate_blocks_192_168() {
        let result = pii_check("Host is at 192.168.x.x");
        assert!(result.is_some(), "Should detect 192.168.x.x address");
        let msg = result.unwrap();
        assert!(msg.contains("192.168"), "Error message should mention the pattern");
    }

    #[test]
    fn test_pii_gate_blocks_10_x() {
        let result = pii_check("Connect to 10.0.0.1 for service");
        assert!(result.is_some(), "Should detect 10.x.x.x address");
    }

    #[test]
    fn test_pii_gate_allows_10_percent() {
        // "10. " — decimal in text like "10. something" should not match a private IP
        // The gate requires the char after "10." to be a digit
        let result = pii_check("10. Conclusion: done.");
        assert!(result.is_none(), "10. followed by a space should not be flagged");
    }

    #[test]
    fn test_pii_gate_blocks_172_16_31() {
        let result = pii_check("Address: 172.20.0.5");
        assert!(result.is_some(), "Should detect 172.16-31.x.x address");
    }

    #[test]
    fn test_pii_gate_allows_172_15() {
        // 172.15 is not in private range
        let result = pii_check("Address: 172.15.0.5");
        assert!(result.is_none(), "172.15.x.x is not a private range");
    }

    #[test]
    fn test_pii_gate_blocks_sk_token() {
        let result = pii_check("key=sk-proj-abcdefghijklmnopqrstuvwxyz123456");
        assert!(result.is_some(), "Should detect sk- API key");
    }

    #[test]
    fn test_pii_gate_blocks_long_hex() {
        let result = pii_check("secret=abcdef1234567890abcdef1234567890ab");
        assert!(result.is_some(), "Should detect long hex secret");
    }

    #[test]
    fn test_pii_gate_allows_clean_content() {
        let result = pii_check("# README\nThis is a normal markdown file with no secrets.");
        assert!(result.is_none(), "Clean content should pass PII gate");
    }

    // ── list_repos ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_repos_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/search")
                .query_param("owner", "testorg");
            then.status(200).json_body(serde_json::json!({
                "data": [
                    {
                        "id": 1,
                        "name": "lumina",
                        "full_name": "testorg/lumina",
                        "description": "Project docs",
                        "private": false,
                        "html_url": "http://example.com/testorg/lumina",
                        "clone_url": "http://example.com/testorg/lumina.git",
                        "default_branch": "main",
                        "stars_count": 0,
                        "forks_count": 0,
                        "open_issues_count": 0,
                        "updated": null
                    }
                ],
                "ok": true
            }));
        });

        let tool = ListRepos { client: mock_client(&server) };
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        mock.assert();
        assert!(result.contains("testorg/lumina"));
        assert!(result.contains("Project docs"));
    }

    // ── get_repo ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_repo_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/repos/testorg/lumina");
            then.status(200).json_body(serde_json::json!({
                "id": 1,
                "name": "lumina",
                "full_name": "testorg/lumina",
                "description": "Main docs",
                "private": false,
                "html_url": "http://example.com/testorg/lumina",
                "clone_url": "http://example.com/testorg/lumina.git",
                "default_branch": "main",
                "stars_count": 3,
                "forks_count": 1,
                "open_issues_count": 2,
                "updated": "2026-06-07T00:00:00Z"
            }));
        });

        let tool = GetRepo { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({"repo": "lumina"}))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("testorg/lumina"));
        assert!(result.contains("main"));
    }

    #[tokio::test]
    async fn test_get_repo_404_returns_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/repos/testorg/missing");
            then.status(404).json_body(serde_json::json!({"message": "Not Found"}));
        });

        let tool = GetRepo { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({"repo": "missing"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    // ── create_file ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_create_file_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/repos/testorg/myrepo/contents/README.md");
            then.status(201).json_body(serde_json::json!({
                "content": null,
                "commit": {
                    "sha": "abc123",
                    "url": "http://example.com",
                    "html_url": "http://example.com",
                    "message": "init"
                }
            }));
        });

        let tool = CreateFile { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({
                "repo": "myrepo",
                "path": "README.md",
                "content": "# Hello world",
                "message": "init"
            }))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("abc123"));
    }

    #[tokio::test]
    async fn test_create_file_pii_blocked() {
        let server = MockServer::start();
        // No mock needed — PII gate should fire before any HTTP call
        let tool = CreateFile { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({
                "repo": "myrepo",
                "path": "config.md",
                "content": "Connect to 192.168.x.x for the service",
                "message": "add config"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        let msg = err.to_string();
        assert!(msg.contains("PII gate") || msg.contains("private infrastructure"));
    }

    // ── read_file ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_read_file_decodes_base64() {
        let server = MockServer::start();
        // "Hello, Gitea!" base64-encoded
        let encoded = base64::engine::general_purpose::STANDARD.encode("Hello, Gitea!");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/contents/hello.txt");
            then.status(200).json_body(serde_json::json!({
                "type": "file",
                "encoding": "base64",
                "size": 13,
                "name": "hello.txt",
                "path": "hello.txt",
                "content": encoded,
                "sha": "deadbeef",
                "url": "http://example.com",
                "html_url": "http://example.com"
            }));
        });

        let tool = ReadFile { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({"repo": "myrepo", "path": "hello.txt"}))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("Hello, Gitea!"));
        assert!(result.contains("deadbeef"));
    }

    #[tokio::test]
    async fn test_read_file_404_returns_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/contents/ghost.txt");
            then.status(404).json_body(serde_json::json!({"message": "Not Found"}));
        });

        let tool = ReadFile { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({"repo": "myrepo", "path": "ghost.txt"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        assert!(err.to_string().contains("ghost.txt"));
    }

    // ── update_file ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_update_file_fetches_sha_before_put() {
        let server = MockServer::start();

        // First: GET to fetch SHA
        let get_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/contents/README.md");
            then.status(200).json_body(serde_json::json!({
                "type": "file",
                "encoding": "base64",
                "size": 5,
                "name": "README.md",
                "path": "README.md",
                "content": base64::engine::general_purpose::STANDARD.encode("hello"),
                "sha": "sha-before-update",
                "url": "http://example.com",
                "html_url": "http://example.com"
            }));
        });

        // Second: PUT to update
        let put_mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/api/v1/repos/testorg/myrepo/contents/README.md");
            then.status(200).json_body(serde_json::json!({
                "content": null,
                "commit": {
                    "sha": "new-sha-after-update",
                    "url": "http://example.com",
                    "html_url": "http://example.com",
                    "message": "update readme"
                }
            }));
        });

        let tool = UpdateFile { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({
                "repo": "myrepo",
                "path": "README.md",
                "content": "# Updated",
                "message": "update readme"
            }))
            .await
            .unwrap();

        get_mock.assert();
        put_mock.assert();
        assert!(result.contains("new-sha-after-update"));
    }

    #[tokio::test]
    async fn test_update_file_pii_blocked_before_sha_fetch() {
        let server = MockServer::start();
        // No mocks should be called — PII gate fires before network access
        let tool = UpdateFile { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({
                "repo": "myrepo",
                "path": "config.txt",
                "content": "SERVER=192.168.x.x",
                "message": "add server"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── list_prs ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_prs_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/pulls")
                .query_param("state", "open");
            then.status(200).json_body(serde_json::json!([
                {
                    "id": 1,
                    "number": 42,
                    "state": "open",
                    "title": "Add Gitea tools",
                    "body": null,
                    "html_url": "http://example.com/pr/42",
                    "user": { "login": "moose", "full_name": "Moose" },
                    "head": { "label": "feature", "ref": "CHORD-07-gitea-tools", "sha": "abc", "repo": null },
                    "base": { "label": "main", "ref": "main", "sha": "def", "repo": null },
                    "mergeable": true,
                    "merged": false,
                    "created_at": "2026-06-07T00:00:00Z",
                    "updated_at": "2026-06-07T00:00:00Z"
                }
            ]));
        });

        let tool = ListPrs { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({"repo": "myrepo"}))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("#42"));
        assert!(result.contains("Add Gitea tools"));
    }

    // ── create_pr ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_create_pr_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/repos/testorg/myrepo/pulls");
            then.status(201).json_body(serde_json::json!({
                "id": 1,
                "number": 7,
                "state": "open",
                "title": "My PR",
                "body": null,
                "html_url": "http://example.com/pr/7",
                "user": { "login": "moose", "full_name": null },
                "head": { "label": "feat", "ref": "feature-branch", "sha": "abc", "repo": null },
                "base": { "label": "main", "ref": "main", "sha": "def", "repo": null },
                "mergeable": null,
                "merged": false,
                "created_at": "2026-06-07T00:00:00Z",
                "updated_at": "2026-06-07T00:00:00Z"
            }));
        });

        let tool = CreatePr { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({
                "repo": "myrepo",
                "title": "My PR",
                "head": "feature-branch",
                "base": "main"
            }))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("#7"));
        assert!(result.contains("feature-branch"));
    }

    // ── list_branches ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_branches_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/branches");
            then.status(200).json_body(serde_json::json!([
                {
                    "name": "main",
                    "commit": { "id": "abcdef1234567890", "message": "init", "timestamp": null },
                    "protected": true
                },
                {
                    "name": "CHORD-07-gitea-tools",
                    "commit": { "id": "deadbeef12345678", "message": null, "timestamp": null },
                    "protected": false
                }
            ]));
        });

        let tool = ListBranches { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({"repo": "myrepo"}))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("main"));
        assert!(result.contains("protected"));
        assert!(result.contains("CHORD-07-gitea-tools"));
    }

    // ── NotConfigured when GITEA_URL not set ──────────────────────────────

    #[tokio::test]
    async fn test_not_configured_stub_returns_error() {
        let stub = NotConfiguredStub {
            tool_name: "gitea_list_repos",
            description: "test",
        };
        let err = stub.execute(serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
        assert!(err.to_string().contains("GITEA_URL"));
    }

    // ── SHA fetch test (explicit) ─────────────────────────────────────────

    #[tokio::test]
    async fn test_get_file_sha_returns_sha_from_api() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/contents/foo.txt");
            then.status(200).json_body(serde_json::json!({
                "type": "file",
                "encoding": "base64",
                "size": 3,
                "name": "foo.txt",
                "path": "foo.txt",
                "content": base64::engine::general_purpose::STANDARD.encode("abc"),
                "sha": "the-expected-sha",
                "url": "http://example.com",
                "html_url": "http://example.com"
            }));
        });

        let client = mock_client(&server);
        let sha = client.get_file_sha("myrepo", "foo.txt").await.unwrap();
        mock.assert();
        assert_eq!(sha, "the-expected-sha");
    }

    // ── delete_file ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_delete_file_fetches_sha_and_deletes() {
        let server = MockServer::start();

        // GET for SHA
        let get_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/contents/old.txt");
            then.status(200).json_body(serde_json::json!({
                "type": "file",
                "encoding": "base64",
                "size": 3,
                "name": "old.txt",
                "path": "old.txt",
                "content": base64::engine::general_purpose::STANDARD.encode("bye"),
                "sha": "sha-to-delete",
                "url": "http://example.com",
                "html_url": "http://example.com"
            }));
        });

        // DELETE
        let del_mock = server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/v1/repos/testorg/myrepo/contents/old.txt");
            then.status(200);
        });

        let tool = DeleteFile { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({
                "repo": "myrepo",
                "path": "old.txt",
                "message": "remove old file"
            }))
            .await
            .unwrap();

        get_mock.assert();
        del_mock.assert();
        assert!(result.contains("old.txt"));
    }

    // ── create_repo ───────────────────────────────────────────────────────

    #[test]
    fn test_create_repo_definition_shape() {
        let server = MockServer::start();
        let tool = CreateRepo { client: mock_client(&server) };
        assert_eq!(tool.name(), "gitea_create_repo");
        let p = tool.parameters();
        assert_eq!(p["type"], "object");
        let required = p["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "org"));
        assert!(required.iter().any(|v| v == "name"));
        // private is optional and defaults to true
        assert_eq!(p["properties"]["private"]["type"], "boolean");
        assert_eq!(p["properties"]["private"]["default"], true);
    }

    #[tokio::test]
    async fn test_create_repo_correct_request_and_output() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/orgs/myorg/repos");
            then.status(201).json_body(serde_json::json!({
                "id": 9,
                "name": "newrepo",
                "full_name": "myorg/newrepo",
                "description": "a new repo",
                "private": true,
                "html_url": "http://example.com/myorg/newrepo",
                "clone_url": "http://example.com/myorg/newrepo.git",
                "ssh_url": "git@example.com:myorg/newrepo.git",
                "default_branch": "main",
                "stars_count": 0,
                "forks_count": 0,
                "open_issues_count": 0,
                "updated": null
            }));
        });

        let tool = CreateRepo { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({ "org": "myorg", "name": "newrepo" }))
            .await
            .unwrap();
        mock.assert();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["full_name"], "myorg/newrepo");
        assert_eq!(v["html_url"], "http://example.com/myorg/newrepo");
        assert_eq!(v["clone_url"], "http://example.com/myorg/newrepo.git");
        assert_eq!(v["ssh_url"], "git@example.com:myorg/newrepo.git");
    }

    #[tokio::test]
    async fn test_create_repo_422_already_exists() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/orgs/myorg/repos");
            then.status(422).json_body(serde_json::json!({ "message": "repo already exists" }));
        });

        let tool = CreateRepo { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({ "org": "myorg", "name": "dup" }))
            .await
            .unwrap_err();
        mock.assert();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_create_repo_401_auth_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/v1/orgs/myorg/repos");
            then.status(401).json_body(serde_json::json!({ "message": "unauthorized" }));
        });

        let tool = CreateRepo { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({ "org": "myorg", "name": "x" }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("authentication") || msg.contains("401"));
        assert!(msg.contains("GITEA_TOKEN"));
    }

    #[tokio::test]
    async fn test_create_repo_requires_org_and_name() {
        let server = MockServer::start();
        let tool = CreateRepo { client: mock_client(&server) };
        assert!(matches!(
            tool.execute(serde_json::json!({ "name": "x" })).await,
            Err(ToolError::InvalidArgument(_))
        ));
        assert!(matches!(
            tool.execute(serde_json::json!({ "org": "myorg" })).await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    // ── registration (env-driven) ──────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn test_register_adds_create_repo_with_url() {
        let url_backup = std::env::var("GITEA_URL").ok();
        std::env::set_var("GITEA_URL", "http://example.com");
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        if let Some(v) = url_backup { std::env::set_var("GITEA_URL", v); } else { std::env::remove_var("GITEA_URL"); }
        assert!(reg.contains("gitea_create_repo"));
    }

    #[test]
    #[serial_test::serial]
    fn test_register_adds_create_repo_stub_without_url() {
        let url_backup = std::env::var("GITEA_URL").ok();
        std::env::remove_var("GITEA_URL");
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        if let Some(v) = url_backup { std::env::set_var("GITEA_URL", v); }
        // Stub registered so the tool still appears and returns NotConfigured.
        assert!(reg.contains("gitea_create_repo"));
    }
}
