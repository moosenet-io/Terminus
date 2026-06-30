//! Infisical tools — read-only secret queries against Infisical, ported from the
//! CT322 Python `infisical_tools.py` exactly.
//!
//! Five tools, all GUARDED (operator approval required before any action):
//!   infisical_status            — server health + auth status
//!   infisical_list_projects     — list accessible projects/workspaces
//!   infisical_list_secrets      — list secret KEYS (names only) in an env/path
//!   infisical_get_secret        — retrieve one secret value by key
//!   infisical_get_secrets_batch — retrieve all secrets (keys + values) in an env/path
//!
//! Required env vars (mirrors the Python source):
//!   INFISICAL_URL            — e.g. http://<infisical-host>:8080
//!   INFISICAL_CLIENT_ID      — mcp-query machine identity client id
//!   INFISICAL_CLIENT_SECRET  — mcp-query machine identity client secret
//!
//! Auth uses Infisical Universal Auth: POST clientId/clientSecret to obtain a
//! short-lived bearer token, then call the v2/v3 secret endpoints. Unlike the
//! Python (which caches the token per-process), each call here authenticates
//! fresh — there is no shared mutable state and the token never leaves the call.
//!
//! Security: read-only. Secret VALUES are returned to the caller exactly as the
//! Python returns them, but values are NEVER logged or echoed by this module.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::approval::{gate, Gate};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct InfisicalConfig {
    url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

impl InfisicalConfig {
    fn from_env() -> Self {
        Self {
            url: std::env::var("INFISICAL_URL").ok().filter(|s| !s.is_empty()),
            client_id: std::env::var("INFISICAL_CLIENT_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            client_secret: std::env::var("INFISICAL_CLIENT_SECRET")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }

    fn is_configured(&self) -> bool {
        self.url.is_some() && self.client_id.is_some() && self.client_secret.is_some()
    }

    /// Base URL with any trailing slash removed, or NotConfigured.
    fn base_url(&self) -> Result<String, ToolError> {
        let url = self.url.as_deref().ok_or_else(|| {
            ToolError::NotConfigured("Missing required env var: INFISICAL_URL".into())
        })?;
        Ok(url.trim_end_matches('/').to_string())
    }

    fn client_id(&self) -> Result<&str, ToolError> {
        self.client_id.as_deref().ok_or_else(|| {
            ToolError::NotConfigured("Missing required env var: INFISICAL_CLIENT_ID".into())
        })
    }

    fn client_secret(&self) -> Result<&str, ToolError> {
        self.client_secret.as_deref().ok_or_else(|| {
            ToolError::NotConfigured("Missing required env var: INFISICAL_CLIENT_SECRET".into())
        })
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }
}

// ── Auth ──────────────────────────────────────────────────────────────────────

/// Authenticate with Infisical Universal Auth and return a bearer access token.
async fn get_access_token(
    client: &reqwest::Client,
    cfg: &InfisicalConfig,
) -> Result<String, ToolError> {
    let base = cfg.base_url()?;
    let body = json!({
        "clientId": cfg.client_id()?,
        "clientSecret": cfg.client_secret()?,
    });

    let resp = client
        .post(format!("{base}/api/v1/auth/universal-auth/login"))
        .json(&body)
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("Infisical auth request failed: {e}")))?;

    let status = resp.status();
    let parsed: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Http(format!("Infisical auth response not JSON: {e}")))?;

    if !status.is_success() {
        // Do not echo the credentials; surface only the server status.
        return Err(ToolError::Http(format!("Infisical auth failed: HTTP {status}")));
    }

    parsed
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::Http("Infisical auth response missing accessToken".into()))
}

// ── Response shaping (matches the Python return dicts) ──────────────────────────

/// Shape the projects list response. Mirrors `infisical_list_projects`.
fn shape_projects(body: &Value) -> Value {
    let workspaces = body
        .get("workspaces")
        .cloned()
        .unwrap_or_else(|| body.clone());
    let arr = workspaces.as_array().cloned().unwrap_or_default();
    let projects: Vec<Value> = arr
        .iter()
        .map(|w| {
            json!({
                "id":   w.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": w.get("name").and_then(Value::as_str).unwrap_or(""),
                "slug": w.get("slug").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect();
    json!({ "projects": projects })
}

/// Shape the list-secrets (keys only) response. Mirrors `infisical_list_secrets`.
fn shape_list_secrets(body: &Value, environment: &str, secret_path: &str) -> Value {
    let secrets = body
        .get("secrets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let keys: Vec<&str> = secrets
        .iter()
        .filter_map(|s| s.get("secretKey").and_then(Value::as_str))
        .collect();
    json!({
        "environment": environment,
        "path":        secret_path,
        "count":       keys.len(),
        "keys":        keys,
    })
}

/// Shape a single secret response. Mirrors `infisical_get_secret`.
fn shape_get_secret(body: &Value, secret_key: &str, environment: &str) -> Value {
    let secret = body.get("secret").cloned().unwrap_or_else(|| json!({}));
    json!({
        "key": secret.get("secretKey").and_then(Value::as_str).unwrap_or(secret_key),
        "value": secret.get("secretValue").and_then(Value::as_str).unwrap_or(""),
        "environment": environment,
        "version": secret.get("version").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Shape the batch (keys + values) response. Mirrors `infisical_get_secrets_batch`.
fn shape_get_secrets_batch(body: &Value, environment: &str, secret_path: &str) -> Value {
    let secrets = body
        .get("secrets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut map = serde_json::Map::new();
    for s in &secrets {
        if let Some(k) = s.get("secretKey").and_then(Value::as_str) {
            let v = s.get("secretValue").and_then(Value::as_str).unwrap_or("");
            map.insert(k.to_string(), Value::String(v.to_string()));
        }
    }
    json!({
        "environment": environment,
        "path":        secret_path,
        "count":       secrets.len(),
        "secrets":     Value::Object(map),
    })
}

// ── HTTP helpers ────────────────────────────────────────────────────────────────

/// GET a JSON endpoint (with optional query params + bearer token) using
/// reqwest's query builder. Returns the parsed body. On a non-2xx status,
/// mirrors the Python `_api_request` error dict shape.
async fn get_json(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
    token: Option<&str>,
) -> Result<Value, ToolError> {
    let mut req = client
        .get(url)
        .header("Accept", "application/json")
        .query(query);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !status.is_success() {
        // Match Python: return an error object rather than raising.
        return Ok(json!({
            "error": true,
            "status": status.as_u16(),
            "message": text,
        }));
    }

    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text)
        .map_err(|e| ToolError::Http(format!("Infisical response not JSON: {e}")))
}

/// The three secret-endpoint query params (workspaceId, environment, secretPath).
fn secret_query<'a>(
    project_id: &'a str,
    environment: &'a str,
    secret_path: &'a str,
) -> [(&'static str, &'a str); 3] {
    [
        ("workspaceId", project_id),
        ("environment", environment),
        ("secretPath", secret_path),
    ]
}

/// Percent-encode a path segment (the secret key) — equivalent to Python
/// `urllib.parse.quote(key, safe="")`: every byte that is not an unreserved
/// character (ALPHA / DIGIT / `-` `_` `.` `~`) becomes %XX.
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── Tool structs ────────────────────────────────────────────────────────────────

struct InfisicalStatus {
    config: InfisicalConfig,
}
struct InfisicalListProjects {
    config: InfisicalConfig,
}
struct InfisicalListSecrets {
    config: InfisicalConfig,
}
struct InfisicalGetSecret {
    config: InfisicalConfig,
}
struct InfisicalGetSecretsBatch {
    config: InfisicalConfig,
}

// ── infisical_status ────────────────────────────────────────────────────────────

#[async_trait]
impl RustTool for InfisicalStatus {
    fn name(&self) -> &str {
        "infisical_status"
    }

    fn description(&self) -> &str {
        "Check Infisical server health and authentication status. GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let summary = "Infisical: check server health and authentication status".to_string();
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        let base = self.config.base_url()?;
        let client = InfisicalConfig::client()?;

        let health = get_json(&client, &format!("{base}/api/status"), &[], None).await?;

        let auth: Value = match get_access_token(&client, &self.config).await {
            Ok(_) => Value::Bool(true),
            Err(e) => Value::String(e.to_string()),
        };

        Ok(json!({ "server": health, "auth": auth }).to_string())
    }
}

// ── infisical_list_projects ──────────────────────────────────────────────────────

#[async_trait]
impl RustTool for InfisicalListProjects {
    fn name(&self) -> &str {
        "infisical_list_projects"
    }

    fn description(&self) -> &str {
        "List all projects (workspaces) accessible to the mcp-query identity. GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let summary = "Infisical: list all accessible projects/workspaces".to_string();
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        let base = self.config.base_url()?;
        let client = InfisicalConfig::client()?;
        let token = get_access_token(&client, &self.config).await?;

        let result = get_json(
            &client,
            &format!("{base}/api/v2/organizations/me/workspaces"),
            &[],
            Some(&token),
        )
        .await?;

        if result.get("error").is_some() {
            return Ok(result.to_string());
        }
        Ok(shape_projects(&result).to_string())
    }
}

// ── infisical_list_secrets ───────────────────────────────────────────────────────

#[async_trait]
impl RustTool for InfisicalListSecrets {
    fn name(&self) -> &str {
        "infisical_list_secrets"
    }

    fn description(&self) -> &str {
        "List secret keys (names only, not values) in a project/environment. \
GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id":  { "type": "string", "description": "The workspace/project ID (get from infisical_list_projects)" },
                "environment": { "type": "string", "description": "Environment slug (production, development, staging). Default: prod" },
                "secret_path": { "type": "string", "description": "Folder path within the environment (default: /)" }
            },
            "required": ["project_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = args
            .get("project_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let environment = args
            .get("environment")
            .and_then(Value::as_str)
            .unwrap_or("prod")
            .trim()
            .to_string();
        let secret_path = args
            .get("secret_path")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();

        let summary = format!(
            "Infisical: list secret KEYS (names only) in project '{project_id}' env '{environment}' path '{secret_path}'"
        );
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        if project_id.is_empty() {
            return Err(ToolError::InvalidArgument("project_id is required".into()));
        }

        let base = self.config.base_url()?;
        let client = InfisicalConfig::client()?;
        let token = get_access_token(&client, &self.config).await?;

        let qs = secret_query(&project_id, &environment, &secret_path);
        let result = get_json(
            &client,
            &format!("{base}/api/v3/secrets/raw"),
            &qs,
            Some(&token),
        )
        .await?;

        if result.get("error").is_some() {
            return Ok(result.to_string());
        }
        Ok(shape_list_secrets(&result, &environment, &secret_path).to_string())
    }
}

// ── infisical_get_secret ─────────────────────────────────────────────────────────

#[async_trait]
impl RustTool for InfisicalGetSecret {
    fn name(&self) -> &str {
        "infisical_get_secret"
    }

    fn description(&self) -> &str {
        "Retrieve a specific secret's value by key. Returns the actual secret value. \
Use infisical_list_secrets first to discover key names. GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id":  { "type": "string", "description": "The workspace/project ID" },
                "secret_key":  { "type": "string", "description": "The secret key name (e.g., ANTHROPIC_API_KEY)" },
                "environment": { "type": "string", "description": "Environment slug (production, development, staging). Default: prod" },
                "secret_path": { "type": "string", "description": "Folder path within the environment (default: /)" }
            },
            "required": ["project_id", "secret_key"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = args
            .get("project_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let secret_key = args
            .get("secret_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let environment = args
            .get("environment")
            .and_then(Value::as_str)
            .unwrap_or("prod")
            .trim()
            .to_string();
        let secret_path = args
            .get("secret_path")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();

        // Summary names the key being fetched but NEVER its value.
        let summary = format!(
            "Infisical: retrieve secret VALUE for key '{secret_key}' in project '{project_id}' env '{environment}' path '{secret_path}'"
        );
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        if project_id.is_empty() {
            return Err(ToolError::InvalidArgument("project_id is required".into()));
        }
        if secret_key.is_empty() {
            return Err(ToolError::InvalidArgument("secret_key is required".into()));
        }

        let base = self.config.base_url()?;
        let client = InfisicalConfig::client()?;
        let token = get_access_token(&client, &self.config).await?;

        let qs = secret_query(&project_id, &environment, &secret_path);
        let encoded_key = encode_key(&secret_key);
        let result = get_json(
            &client,
            &format!("{base}/api/v3/secrets/raw/{encoded_key}"),
            &qs,
            Some(&token),
        )
        .await?;

        if result.get("error").is_some() {
            return Ok(result.to_string());
        }
        Ok(shape_get_secret(&result, &secret_key, &environment).to_string())
    }
}

// ── infisical_get_secrets_batch ──────────────────────────────────────────────────

#[async_trait]
impl RustTool for InfisicalGetSecretsBatch {
    fn name(&self) -> &str {
        "infisical_get_secrets_batch"
    }

    fn description(&self) -> &str {
        "Retrieve all secrets (keys + values) in a project/environment. Returns all \
secret values — use for bulk injection, not browsing. GUARDED: requires operator approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id":  { "type": "string", "description": "The workspace/project ID" },
                "environment": { "type": "string", "description": "Environment slug (production, development, staging). Default: prod" },
                "secret_path": { "type": "string", "description": "Folder path within the environment (default: /)" }
            },
            "required": ["project_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = args
            .get("project_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let environment = args
            .get("environment")
            .and_then(Value::as_str)
            .unwrap_or("prod")
            .trim()
            .to_string();
        let secret_path = args
            .get("secret_path")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();

        let summary = format!(
            "Infisical: retrieve ALL secret values (bulk) in project '{project_id}' env '{environment}' path '{secret_path}'"
        );
        match gate(self.name(), &args, &summary).await {
            Gate::Granted => {}
            Gate::Pending(msg) | Gate::Denied(msg) => return Ok(msg),
        }

        if project_id.is_empty() {
            return Err(ToolError::InvalidArgument("project_id is required".into()));
        }

        let base = self.config.base_url()?;
        let client = InfisicalConfig::client()?;
        let token = get_access_token(&client, &self.config).await?;

        let qs = secret_query(&project_id, &environment, &secret_path);
        let result = get_json(
            &client,
            &format!("{base}/api/v3/secrets/raw"),
            &qs,
            Some(&token),
        )
        .await?;

        if result.get("error").is_some() {
            return Ok(result.to_string());
        }
        Ok(shape_get_secrets_batch(&result, &environment, &secret_path).to_string())
    }
}

// ── Registration ────────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    let config = InfisicalConfig::from_env();
    if !config.is_configured() {
        tracing::warn!(
            "Infisical tools not fully configured (INFISICAL_URL / INFISICAL_CLIENT_ID / \
INFISICAL_CLIENT_SECRET). Tools registered; calls will return NotConfigured until set."
        );
    }
    registry.register_or_replace(Box::new(InfisicalStatus {
        config: config.clone(),
    }));
    registry.register_or_replace(Box::new(InfisicalListProjects {
        config: config.clone(),
    }));
    registry.register_or_replace(Box::new(InfisicalListSecrets {
        config: config.clone(),
    }));
    registry.register_or_replace(Box::new(InfisicalGetSecret {
        config: config.clone(),
    }));
    registry.register_or_replace(Box::new(InfisicalGetSecretsBatch { config }));
}

// ── Tests (no network / no SSH / no DB) ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cfg(url: Option<&str>, id: Option<&str>, secret: Option<&str>) -> InfisicalConfig {
        InfisicalConfig {
            url: url.map(str::to_string),
            client_id: id.map(str::to_string),
            client_secret: secret.map(str::to_string),
        }
    }

    fn full_cfg() -> InfisicalConfig {
        cfg(Some("http://infisical.test:8080/"), Some("cid"), Some("csecret"))
    }

    // ── config ────────────────────────────────────────────────────────────────

    #[test]
    fn base_url_strips_trailing_slash() {
        let c = full_cfg();
        assert_eq!(c.base_url().unwrap(), "http://infisical.test:8080");
    }

    #[test]
    fn base_url_missing_is_not_configured() {
        let c = cfg(None, Some("cid"), Some("cs"));
        assert!(matches!(c.base_url(), Err(ToolError::NotConfigured(_))));
    }

    #[test]
    fn is_configured_requires_all_three() {
        assert!(full_cfg().is_configured());
        assert!(!cfg(Some("u"), Some("i"), None).is_configured());
        assert!(!cfg(Some("u"), None, Some("s")).is_configured());
        assert!(!cfg(None, Some("i"), Some("s")).is_configured());
    }

    // ── query / key encoding ────────────────────────────────────────────────────

    #[test]
    fn secret_query_builds_expected_params() {
        let qs = secret_query("ws123", "prod", "/");
        assert_eq!(qs[0], ("workspaceId", "ws123"));
        assert_eq!(qs[1], ("environment", "prod"));
        assert_eq!(qs[2], ("secretPath", "/"));
        // reqwest .query() will percent-encode these at send time.
    }

    #[test]
    fn secret_query_passes_nested_path_verbatim() {
        let qs = secret_query("ws", "dev", "/app/db");
        assert_eq!(qs[2], ("secretPath", "/app/db"));
    }

    #[test]
    fn encode_key_percent_encodes_special_chars() {
        assert_eq!(encode_key("ANTHROPIC_API_KEY"), "ANTHROPIC_API_KEY");
        assert_eq!(encode_key("a b"), "a%20b");
        assert_eq!(encode_key("a/b"), "a%2Fb");
    }

    // ── response shaping ────────────────────────────────────────────────────────

    #[test]
    fn shape_projects_handles_workspaces_wrapper() {
        let body = json!({
            "workspaces": [
                { "id": "p1", "name": "Alpha", "slug": "alpha" },
                { "id": "p2", "name": "Beta" }
            ]
        });
        let out = shape_projects(&body);
        let projects = out["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0]["id"], "p1");
        assert_eq!(projects[0]["slug"], "alpha");
        // missing slug defaults to empty string
        assert_eq!(projects[1]["slug"], "");
    }

    #[test]
    fn shape_projects_handles_bare_array() {
        let body = json!([{ "id": "x", "name": "X", "slug": "x" }]);
        let out = shape_projects(&body);
        assert_eq!(out["projects"].as_array().unwrap().len(), 1);
        assert_eq!(out["projects"][0]["id"], "x");
    }

    #[test]
    fn shape_list_secrets_returns_keys_only() {
        let body = json!({
            "secrets": [
                { "secretKey": "FOO", "secretValue": "supersecret" },
                { "secretKey": "BAR", "secretValue": "anothersecret" }
            ]
        });
        let out = shape_list_secrets(&body, "prod", "/");
        assert_eq!(out["environment"], "prod");
        assert_eq!(out["path"], "/");
        assert_eq!(out["count"], 2);
        let keys = out["keys"].as_array().unwrap();
        assert_eq!(keys[0], "FOO");
        assert_eq!(keys[1], "BAR");
        // CRITICAL: no values must leak in the keys-only listing
        let s = out.to_string();
        assert!(!s.contains("supersecret"));
        assert!(!s.contains("anothersecret"));
    }

    #[test]
    fn shape_list_secrets_empty() {
        let out = shape_list_secrets(&json!({}), "dev", "/sub");
        assert_eq!(out["count"], 0);
        assert_eq!(out["keys"].as_array().unwrap().len(), 0);
        assert_eq!(out["path"], "/sub");
    }

    #[test]
    fn shape_get_secret_extracts_fields() {
        let body = json!({
            "secret": { "secretKey": "API_KEY", "secretValue": "v3", "version": 4 }
        });
        let out = shape_get_secret(&body, "API_KEY", "prod");
        assert_eq!(out["key"], "API_KEY");
        assert_eq!(out["value"], "v3");
        assert_eq!(out["version"], 4);
        assert_eq!(out["environment"], "prod");
    }

    #[test]
    fn shape_get_secret_missing_secret_uses_defaults() {
        let out = shape_get_secret(&json!({}), "WANTED", "staging");
        assert_eq!(out["key"], "WANTED"); // falls back to requested key
        assert_eq!(out["value"], "");
        assert_eq!(out["version"], 0);
    }

    #[test]
    fn shape_get_secrets_batch_maps_key_value() {
        let body = json!({
            "secrets": [
                { "secretKey": "A", "secretValue": "1" },
                { "secretKey": "B", "secretValue": "2" }
            ]
        });
        let out = shape_get_secrets_batch(&body, "prod", "/");
        assert_eq!(out["count"], 2);
        assert_eq!(out["secrets"]["A"], "1");
        assert_eq!(out["secrets"]["B"], "2");
        assert_eq!(out["environment"], "prod");
    }

    #[test]
    fn shape_get_secrets_batch_empty() {
        let out = shape_get_secrets_batch(&json!({"secrets": []}), "prod", "/");
        assert_eq!(out["count"], 0);
        assert_eq!(out["secrets"].as_object().unwrap().len(), 0);
    }

    // ── approval gate is enforced before any action ──────────────────────────────
    //
    // With DATABASE_URL unset the gate cannot reach Postgres, so it must Deny and
    // each tool must return that message verbatim (NOT perform HTTP, NOT return a
    // NotConfigured/InvalidArgument error from the real action path).

    fn assert_gated(out: &str) {
        assert!(
            out.contains("unavailable") || out.contains("DATABASE_URL") || out.contains("APPROVAL"),
            "expected approval-gate message, got: {out}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn status_blocked_by_gate_without_db() {
        std::env::remove_var("DATABASE_URL");
        let tool = InfisicalStatus { config: full_cfg() };
        let out = tool.execute(json!({})).await.unwrap();
        assert_gated(&out);
    }

    #[tokio::test]
    #[serial]
    async fn list_projects_blocked_by_gate_without_db() {
        std::env::remove_var("DATABASE_URL");
        let tool = InfisicalListProjects { config: full_cfg() };
        let out = tool.execute(json!({})).await.unwrap();
        assert_gated(&out);
    }

    #[tokio::test]
    #[serial]
    async fn list_secrets_blocked_by_gate_without_db() {
        std::env::remove_var("DATABASE_URL");
        let tool = InfisicalListSecrets { config: full_cfg() };
        // even with a missing project_id, the gate must fire FIRST (before validation)
        let out = tool.execute(json!({})).await.unwrap();
        assert_gated(&out);
    }

    #[tokio::test]
    #[serial]
    async fn get_secret_blocked_by_gate_without_db() {
        std::env::remove_var("DATABASE_URL");
        let tool = InfisicalGetSecret { config: full_cfg() };
        let out = tool
            .execute(json!({ "project_id": "p", "secret_key": "K" }))
            .await
            .unwrap();
        assert_gated(&out);
    }

    #[tokio::test]
    #[serial]
    async fn get_secrets_batch_blocked_by_gate_without_db() {
        std::env::remove_var("DATABASE_URL");
        let tool = InfisicalGetSecretsBatch { config: full_cfg() };
        let out = tool.execute(json!({ "project_id": "p" })).await.unwrap();
        assert_gated(&out);
    }

    // ── tool metadata ────────────────────────────────────────────────────────────

    #[test]
    fn tool_names_match_python() {
        let c = full_cfg();
        assert_eq!(InfisicalStatus { config: c.clone() }.name(), "infisical_status");
        assert_eq!(
            InfisicalListProjects { config: c.clone() }.name(),
            "infisical_list_projects"
        );
        assert_eq!(
            InfisicalListSecrets { config: c.clone() }.name(),
            "infisical_list_secrets"
        );
        assert_eq!(
            InfisicalGetSecret { config: c.clone() }.name(),
            "infisical_get_secret"
        );
        assert_eq!(
            InfisicalGetSecretsBatch { config: c }.name(),
            "infisical_get_secrets_batch"
        );
    }

    #[test]
    fn tool_parameters_are_valid_schema() {
        let c = full_cfg();
        let ls = InfisicalListSecrets { config: c.clone() }.parameters();
        assert_eq!(ls["type"], "object");
        assert_eq!(ls["required"][0], "project_id");

        let gs = InfisicalGetSecret { config: c.clone() }.parameters();
        let req = gs["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "project_id"));
        assert!(req.iter().any(|v| v == "secret_key"));

        let st = InfisicalStatus { config: c }.parameters();
        assert_eq!(st["type"], "object");
    }

    // ── registration ─────────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn register_adds_five_tools() {
        let mut reg = ToolRegistry::new();
        let url = std::env::var("INFISICAL_URL").ok();
        let id = std::env::var("INFISICAL_CLIENT_ID").ok();
        let secret = std::env::var("INFISICAL_CLIENT_SECRET").ok();
        std::env::remove_var("INFISICAL_URL");
        std::env::remove_var("INFISICAL_CLIENT_ID");
        std::env::remove_var("INFISICAL_CLIENT_SECRET");

        register(&mut reg);

        if let Some(v) = url {
            std::env::set_var("INFISICAL_URL", v);
        }
        if let Some(v) = id {
            std::env::set_var("INFISICAL_CLIENT_ID", v);
        }
        if let Some(v) = secret {
            std::env::set_var("INFISICAL_CLIENT_SECRET", v);
        }

        assert!(reg.contains("infisical_status"));
        assert!(reg.contains("infisical_list_projects"));
        assert!(reg.contains("infisical_list_secrets"));
        assert!(reg.contains("infisical_get_secret"));
        assert!(reg.contains("infisical_get_secrets_batch"));
        assert_eq!(reg.len(), 5);
    }
}
