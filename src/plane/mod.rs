//! Plane CE tool implementations (CHORD-06, hardened per the plane-helper port).
//!
//! Provides 27 Rust tools that wrap the Plane CE REST API via reqwest.
//! All configuration comes from environment variables — no hardcoded URLs or tokens.
//!
//! ## Configuration
//! - `PLANE_API_URL` — base URL of the Plane CE instance (required at call time)
//! - `PLANE_API_KEY` — default API key/token for authentication (required at call time)
//! - `PLANE_API_KEY_<NAME>` — additional named identities (e.g. `PLANE_API_KEY_AXON`),
//!   see "Multi-identity" below
//! - `PLANE_IDENTITY_NAME` — human name for the default `PLANE_API_KEY` identity
//! - `PLANE_WORKSPACE` — workspace slug (default: "moosenet")
//! - `PLANE_RPM` / `PLANE_RATE_SHARE` — proactive pacing, default 60 RPM / share of 3
//!   (60/3 = 20 effective RPM = 3s minimum interval between requests, shared across
//!   every tool call in this process via a single in-process rate limiter)
//! - `PLANE_CACHE_TTL_SECS` — in-memory GET response cache TTL, default 5s
//!
//! When `PLANE_API_URL` is not set the tools register normally but return
//! `ToolError::NotConfigured` on every call.
//!
//! ## Multi-identity
//! This is a *replacement*, not a port, of the Python `plane_client.py`
//! `whoami()` design, which resolved identity by scanning other agents'
//! plaintext `.env` files for a matching token substring — a credential-sprawl
//! anti-pattern. Instead, named identities are configured explicitly via
//! `PLANE_API_KEY_<NAME>` secrets (injected into this process's environment at
//! start by the operator's secret manager, never read from another process's
//! files at call time). [`PlaneClient::for_identity`]
//! returns a clone of the client scoped to a named identity's token, sharing the
//! HTTP client, rate limiter, and GET cache. [`PlaneWhoami`] (`plane_whoami`)
//! reports the active identity, or resolves whether a named identity is configured.

pub mod types;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use types::*;

/// True if `s` is a canonical 8-4-4-4-12 hyphenated UUID. // pii-test-fixture
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

// ─── In-process rate limiter ─────────────────────────────────────────────────
//
// Replaces the Python client's `fcntl.flock`-guarded `/tmp/plane-helper.lock` +
// `/tmp/plane-helper.last` pacing. This service is a single long-running
// process (not many independent CLI invocations), so a `tokio::sync::Mutex`
// guarding a shared "last request" timestamp is the correct equivalent: every
// call — across every tool, every identity — passes through the same gate.

#[derive(Debug)]
struct RateLimiter {
    last: AsyncMutex<Option<Instant>>,
    min_interval: Duration,
}

impl RateLimiter {
    /// Build from `PLANE_RPM` / `PLANE_RATE_SHARE` (defaults: 60 / 3, i.e. a
    /// 3-second minimum interval), matching the Python client's env-var names.
    fn from_env() -> Self {
        let rpm: f64 = std::env::var("PLANE_RPM").ok().and_then(|v| v.parse().ok()).unwrap_or(60.0);
        let share: f64 = std::env::var("PLANE_RATE_SHARE").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
        Self::new(rpm, share)
    }

    fn new(rpm: f64, share: f64) -> Self {
        let effective_rpm = if share > 0.0 { rpm / share } else { rpm };
        let min_interval = if effective_rpm > 0.0 {
            Duration::from_secs_f64(60.0 / effective_rpm)
        } else {
            Duration::ZERO
        };
        Self { last: AsyncMutex::new(None), min_interval }
    }

    /// Block until at least `min_interval` has elapsed since the previous
    /// call made through this limiter (shared across every clone of the owning
    /// `PlaneClient` and every identity, since it lives behind an `Arc`).
    async fn acquire(&self) {
        let mut last = self.last.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }
}

// ─── In-memory GET cache ──────────────────────────────────────────────────────
//
// Replaces the Python client's shared `/tmp/plane-helper-cache.json` file.
// Keyed by full request URL, TTL-based, in-process only (this service doesn't
// span multiple OS processes the way the CLI-invocation Python client did).

#[derive(Debug)]
struct GetCache {
    entries: AsyncMutex<HashMap<String, (Instant, String)>>,
    ttl: Duration,
}

impl GetCache {
    /// Build from `PLANE_CACHE_TTL_SECS` (default 5s, matching the Python client).
    fn from_env() -> Self {
        let ttl_secs: u64 = std::env::var("PLANE_CACHE_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
        Self::new(Duration::from_secs(ttl_secs))
    }

    fn new(ttl: Duration) -> Self {
        Self { entries: AsyncMutex::new(HashMap::new()), ttl }
    }

    async fn get(&self, key: &str) -> Option<String> {
        let entries = self.entries.lock().await;
        entries.get(key).and_then(|(ts, body)| {
            if ts.elapsed() < self.ttl { Some(body.clone()) } else { None }
        })
    }

    async fn set(&self, key: String, body: String) {
        let mut entries = self.entries.lock().await;
        entries.insert(key, (Instant::now(), body));
    }
}

// ─── PlaneClient ─────────────────────────────────────────────────────────────

/// Shared HTTP client for the Plane CE API.
///
/// Constructed from environment variables. When `PLANE_API_URL` is absent,
/// `configured` is false and every tool returns `ToolError::NotConfigured`.
#[derive(Clone)]
pub struct PlaneClient {
    http: Client,
    base_url: Option<String>,
    /// Active token used for requests made directly through this client
    /// instance (the default identity, unless [`PlaneClient::for_identity`]
    /// produced this instance).
    api_key: Option<String>,
    /// Human name for the active token, if resolvable (see [`PlaneClient::from_env`]).
    identity_name: Option<String>,
    /// All configured named identities: lowercased name -> token. Populated
    /// from `PLANE_API_KEY_<NAME>` env vars only — never from another
    /// process's files.
    identities: Arc<HashMap<String, String>>,
    workspace: String,
    rate_limiter: Arc<RateLimiter>,
    cache: Arc<GetCache>,
}

/// Hand-written `Debug` impl: never prints `api_key` or `identities` (both
/// hold live credentials). Redacted as `Some(<redacted>)` / a bare count so
/// logs/panics/`{:?}` formatting can never leak a token.
impl std::fmt::Debug for PlaneClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("identity_name", &self.identity_name)
            .field("identities", &format!("<{} configured, redacted>", self.identities.len()))
            .field("workspace", &self.workspace)
            .finish()
    }
}

impl PlaneClient {
    /// Build a `PlaneClient` from environment variables.
    pub fn from_env() -> Self {
        let base_url = std::env::var("PLANE_API_URL").ok().map(|u| u.trim_end_matches('/').to_string());
        let api_key = std::env::var("PLANE_API_KEY").ok().filter(|v| !v.is_empty());
        let workspace = std::env::var("PLANE_WORKSPACE")
            .unwrap_or_else(|_| "moosenet".into());

        // Named identities: PLANE_API_KEY_<NAME> for any agent that needs its
        // own token (e.g. PLANE_API_KEY_AXON, PLANE_API_KEY_VIGIL). Read once
        // at process start from this process's own environment (populated by
        // the operator's secret manager) — never from another process's files.
        let mut identities: HashMap<String, String> = HashMap::new();
        for (k, v) in std::env::vars() {
            if let Some(name) = k.strip_prefix("PLANE_API_KEY_") {
                if !v.is_empty() {
                    identities.insert(name.to_lowercase(), v);
                }
            }
        }

        // Resolve a human name for the default PLANE_API_KEY token: prefer an
        // explicit PLANE_IDENTITY_NAME, else look for a PLANE_API_KEY_<NAME>
        // whose value happens to equal the default token.
        let identity_name = std::env::var("PLANE_IDENTITY_NAME")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                api_key.as_ref().and_then(|tok| {
                    identities.iter().find(|(_, v)| *v == tok).map(|(k, _)| k.clone())
                })
            });

        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");

        Self {
            http,
            base_url,
            api_key,
            identity_name,
            identities: Arc::new(identities),
            workspace,
            rate_limiter: Arc::new(RateLimiter::from_env()),
            cache: Arc::new(GetCache::from_env()),
        }
    }

    /// Returns true if both PLANE_API_URL and PLANE_API_KEY are configured.
    pub fn configured(&self) -> bool {
        self.base_url.is_some() && self.api_key.is_some()
    }

    /// Test-only constructor for other in-crate modules that call this
    /// module's tools in-process (e.g. `scribe::mod::ScribeReportDiscrepancy`,
    /// SCRB-04) and need a `PlaneClient` pointed at a local mock server.
    /// Mirrors this module's own `tests::mock_client` exactly (zero-interval
    /// rate limiter so tests aren't paced, a short-lived GET cache). Only
    /// compiled for test builds -- never available to production code, and
    /// never reads real credentials.
    #[cfg(test)]
    pub(crate) fn test_client_with_base_url(base_url: String) -> Arc<Self> {
        Arc::new(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build reqwest client"),
            base_url: Some(base_url),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter { last: AsyncMutex::new(None), min_interval: Duration::ZERO }),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        })
    }

    /// Return a `ToolError::NotConfigured` with helpful message.
    fn not_configured(&self) -> ToolError {
        ToolError::NotConfigured(
            "PLANE_API_URL and PLANE_API_KEY must be set to use Plane tools".into(),
        )
    }

    /// Return a clone of this client scoped to a named identity's token
    /// (from `PLANE_API_KEY_<NAME>`) instead of the default. The HTTP client,
    /// rate limiter, and GET cache are shared (same `Arc`s) — only the active
    /// token and its resolved name differ, so identities never contend for
    /// separate rate budgets and never leak each other's tokens.
    pub fn for_identity(&self, name: &str) -> Result<Self, ToolError> {
        let key = name.trim().to_lowercase();
        let token = self.identities.get(&key).cloned().ok_or_else(|| {
            ToolError::InvalidArgument(format!(
                "No Plane identity named '{name}' is configured (expected PLANE_API_KEY_{})",
                key.to_uppercase()
            ))
        })?;
        Ok(Self {
            api_key: Some(token),
            identity_name: Some(key),
            ..self.clone()
        })
    }

    /// The active identity's resolved name, if known.
    pub fn identity_name(&self) -> Option<&str> {
        self.identity_name.as_deref()
    }

    /// Build a GET-cache key that is unique per active token, not just per
    /// URL — see the doc comment on `get_json_cached` for why. Uses the raw
    /// token as part of an in-memory-only key (never logged, never printed:
    /// this struct's `Debug` impl is hand-written to redact it).
    fn cache_key(&self, url: &str) -> String {
        format!("{}\u{0}{}", self.api_key.as_deref().unwrap_or(""), url)
    }

    /// Build the base URL for workspace-scoped endpoints.
    fn workspace_url(&self) -> String {
        format!(
            "{}/api/v1/workspaces/{}/",
            self.base_url.as_deref().unwrap_or(""),
            self.workspace
        )
    }

    /// Resolve a project identifier (e.g. "LM") or a UUID to a project UUID.
    ///
    /// Plane CE's project-scoped endpoints require the project UUID in the path;
    /// passing a human identifier like "LM" yields a 404 ("Page not found").
    /// UUIDs are returned unchanged (no network call); anything else is looked up
    /// against the workspace project list, matching on `identifier`
    /// (case-insensitive) or exact `id`.
    async fn resolve_project_id(&self, project_id: &str) -> Result<String, ToolError> {
        if is_uuid(project_id) {
            return Ok(project_id.to_string());
        }
        let url = format!("{}projects/", self.workspace_url());
        let body = self.get_json_cached(&url).await?;
        let list: ApiList<Project> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse projects: {e}")))?;
        list.into_items()
            .into_iter()
            .find(|p| p.identifier.eq_ignore_ascii_case(project_id) || p.id == project_id)
            .map(|p| p.id)
            .ok_or_else(|| {
                ToolError::NotFound(format!(
                    "No Plane project matches identifier or id '{project_id}'"
                ))
            })
    }

    /// Execute a GET request with rate-limit retry (max 3 attempts, 3 s delay).
    async fn get_with_retry(&self, url: &str) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .get(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
        })
        .await
    }

    /// GET `url` as raw JSON text, serving from the in-memory TTL cache when
    /// available. On a cache miss, performs the request (through the same
    /// rate-limited, retrying transport as every other call) and populates the
    /// cache with the response body on success. Callers deserialize the
    /// returned string with `serde_json::from_str`.
    ///
    /// The cache key includes the active token, not just the URL: Plane GET
    /// responses are not uniformly workspace-scoped (e.g. `plane_list_projects`
    /// only returns projects the calling token's user belongs to, and member
    /// listings can vary by role), so two [`PlaneClient::for_identity`] clones
    /// sharing this cache's `Arc` must never be served each other's cached
    /// response for the same URL.
    async fn get_json_cached(&self, url: &str) -> Result<String, ToolError> {
        let cache_key = self.cache_key(url);
        if let Some(body) = self.cache.get(&cache_key).await {
            debug!("Plane GET cache hit: {url}");
            return Ok(body);
        }
        let resp = self.get_with_retry(url).await?;
        let resp = Self::check_status(resp).await?;
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to read response body: {e}")))?;
        self.cache.set(cache_key, body.clone()).await;
        Ok(body)
    }

    /// Execute a POST request with rate-limit retry.
    async fn post_with_retry(&self, url: &str, body: &Value) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .post(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Execute a PATCH request with rate-limit retry.
    async fn patch_with_retry(&self, url: &str, body: &Value) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .patch(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Execute a DELETE request with rate-limit retry.
    async fn delete_with_retry(&self, url: &str) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .delete(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
        })
        .await
    }

    /// Core retry loop, ported from the Python client's semantics:
    /// - every attempt is paced by the shared [`RateLimiter`] first
    /// - 401/403 are never retried (auth failures are terminal)
    /// - 429 respects a `Retry-After` header, falling back to the backoff table
    /// - 5xx and network errors retry with the same backoff table
    /// - max 3 attempts total
    async fn request_with_retry<F>(&self, build: F) -> Result<Response, ToolError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        const MAX_ATTEMPTS: u8 = 3;
        const BACKOFF: [u64; 3] = [2, 5, 15];
        // A hostile or misconfigured server can send an arbitrarily large
        // `Retry-After`; without a ceiling that would hang a tool call far
        // beyond what "max 3 attempts" implies. Clamp to a sane upper bound.
        const MAX_RETRY_AFTER_SECS: u64 = 60;

        let mut attempts = 0u8;
        loop {
            attempts += 1;
            self.rate_limiter.acquire().await;

            let sent = build().send().await;
            let resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    if attempts >= MAX_ATTEMPTS {
                        return Err(ToolError::Http(format!(
                            "Request failed after {attempts} attempts: {e}"
                        )));
                    }
                    let delay = BACKOFF[(attempts - 1) as usize];
                    warn!("Plane network error ({e}), retrying in {delay}s (attempt {attempts}/{MAX_ATTEMPTS})");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    continue;
                }
            };

            let status = resp.status();

            // Auth failures are terminal — never retried.
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Ok(resp);
            }

            if status == StatusCode::TOO_MANY_REQUESTS {
                if attempts >= MAX_ATTEMPTS {
                    return Err(ToolError::Http(
                        "Plane rate limit exceeded — try again later".into(),
                    ));
                }
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(BACKOFF[(attempts - 1) as usize])
                    .min(MAX_RETRY_AFTER_SECS);
                warn!("Plane 429 received, retrying in {retry_after}s (attempt {attempts}/{MAX_ATTEMPTS})");
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                continue;
            }

            if status.is_server_error() {
                if attempts >= MAX_ATTEMPTS {
                    // Exhausted retries — return the response as-is so
                    // check_status() surfaces a proper Http error with body.
                    return Ok(resp);
                }
                let delay = BACKOFF[(attempts - 1) as usize];
                warn!("Plane server error {status}, retrying in {delay}s (attempt {attempts}/{MAX_ATTEMPTS})");
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }

            return Ok(resp);
        }
    }

    /// Map non-success HTTP status to a clean ToolError.
    async fn check_status(resp: Response) -> Result<Response, ToolError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        match status {
            StatusCode::NOT_FOUND => Err(ToolError::NotFound(format!("Resource not found: {body}"))),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(ToolError::Http(format!("Plane authentication failed: {status}")))
            }
            StatusCode::UNPROCESSABLE_ENTITY => {
                Err(ToolError::InvalidArgument(format!("Invalid request: {body}")))
            }
            _ => Err(ToolError::Http(format!("Plane returned {status}: {body}"))),
        }
    }
}

// ─── Helper macro for guard boilerplate ──────────────────────────────────────

macro_rules! require_configured {
    ($self:expr) => {
        if !$self.client.configured() {
            return Err($self.client.not_configured());
        }
    };
}

macro_rules! require_arg {
    ($args:expr, $field:literal, $type:ident) => {
        $args
            .get($field)
            .and_then(|v| v.$type())
            .ok_or_else(|| ToolError::InvalidArgument(format!("missing required argument: {}", $field)))?
    };
}

// ─── 1. plane_list_projects ──────────────────────────────────────────────────

pub struct PlaneListProjects {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListProjects {
    fn name(&self) -> &str { "plane_list_projects" }
    fn description(&self) -> &str { "List all projects in the Plane workspace" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let url = format!("{}projects/", self.client.workspace_url());
        debug!("plane_list_projects GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Project> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse projects: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No projects found in workspace".into());
        }
        let mut out = format!("Found {} project(s):\n", items.len());
        for p in &items {
            out.push_str(&format!("  [{id}] {name} ({identifier})\n",
                id = p.id, name = p.name, identifier = p.identifier));
        }
        Ok(out)
    }
}

// ─── 2. plane_get_project ────────────────────────────────────────────────────

pub struct PlaneGetProject {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetProject {
    fn name(&self) -> &str { "plane_get_project" }
    fn description(&self) -> &str { "Get details for a specific Plane project by ID" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!("{}projects/{project_id}/", self.client.workspace_url());
        debug!("plane_get_project GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let p: Project = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse project: {e}")))?;
        Ok(format!(
            "Project: {name}\nID: {id}\nIdentifier: {identifier}\nDescription: {desc}",
            name = p.name,
            id = p.id,
            identifier = p.identifier,
            desc = p.description.as_deref().unwrap_or("(none)")
        ))
    }
}

// ─── 3. plane_list_work_items ────────────────────────────────────────────────

pub struct PlaneListWorkItems {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListWorkItems {
    fn name(&self) -> &str { "plane_list_work_items" }
    fn description(&self) -> &str { "List work items (issues) in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "limit": { "type": "integer", "description": "Max results to return (default 50)" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_work_items GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;
        let total = list.total_count();
        let items: Vec<Issue> = list.into_items().into_iter().take(limit).collect();
        if items.is_empty() {
            return Ok("No work items found".into());
        }
        let mut out = format!("Work items ({} shown of {}):\n", items.len(), total);
        for i in &items {
            let priority = i.priority.as_deref().unwrap_or("none");
            let seq = i.sequence_id.map(|s| format!("#{s}")).unwrap_or_default();
            out.push_str(&format!("  [{id}] {seq} {name} (priority: {priority})\n",
                id = i.id, seq = seq, name = i.name, priority = priority));
        }
        Ok(out)
    }
}

// ─── 4. plane_get_work_item ──────────────────────────────────────────────────

pub struct PlaneGetWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetWorkItem {
    fn name(&self) -> &str { "plane_get_work_item" }
    fn description(&self) -> &str { "Get details for a specific work item by ID" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            self.client.workspace_url()
        );
        debug!("plane_get_work_item GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let i: Issue = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issue: {e}")))?;
        Ok(format!(
            "Issue: {name}\nID: {id}\nSequence: {seq}\nPriority: {priority}\nState: {state}\nDescription: {desc}",
            name = i.name,
            id = i.id,
            seq = i.sequence_id.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            priority = i.priority.as_deref().unwrap_or("none"),
            state = i.state.as_deref().unwrap_or("unknown"),
            desc = i.description.as_deref().unwrap_or("(none)")
        ))
    }
}

// ─── 5. plane_create_work_item ───────────────────────────────────────────────

pub struct PlaneCreateWorkItem {
    client: Arc<PlaneClient>,
}

impl PlaneCreateWorkItem {
    /// Construct directly for an in-process, in-crate caller (e.g.
    /// `scribe::mod::ScribeReportDiscrepancy`, SCRB-04) that calls this
    /// tool's `execute()` as a plain function call rather than a second HTTP
    /// hop through the MCP registry -- the "ONE sanctioned path" for Plane
    /// access still applies (this IS that path, called in-process, same
    /// crate), it just isn't going through `register()`'s registry lookup.
    /// `pub(crate)` (not `pub`, per cycle 1 review): only an in-crate caller
    /// is a legitimate use case; no external API surface should be able to
    /// construct these tools directly, bypassing `register()`'s catalog.
    pub(crate) fn new(client: Arc<PlaneClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RustTool for PlaneCreateWorkItem {
    fn name(&self) -> &str { "plane_create_work_item" }
    fn description(&self) -> &str { "Create a new work item (issue) in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "name": { "type": "string", "description": "Issue title" },
                "description_html": { "type": "string", "description": "Issue description (HTML)" },
                "state": { "type": "string", "description": "State UUID" },
                "priority": { "type": "string", "description": "Priority: urgent/high/medium/low/none" },
                "due_date": { "type": "string", "description": "Due date (YYYY-MM-DD)" },
                "parent": { "type": "string", "description": "Parent issue UUID (for sub-issues)" },
                "label_ids": { "type": "array", "items": { "type": "string" }, "description": "Label UUIDs to attach" }
            },
            "required": ["project_id", "name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let name = require_arg!(args, "name", as_str);
        let mut body = json!({ "name": name });
        if let Some(v) = args.get("description_html").and_then(|v| v.as_str()) {
            body["description_html"] = json!(v);
        }
        if let Some(v) = args.get("state").and_then(|v| v.as_str()) {
            body["state"] = json!(v);
        }
        if let Some(v) = args.get("priority").and_then(|v| v.as_str()) {
            body["priority"] = json!(v);
        }
        if let Some(v) = args.get("due_date").and_then(|v| v.as_str()) {
            body["due_date"] = json!(v);
        }
        if let Some(v) = args.get("parent").and_then(|v| v.as_str()) {
            body["parent"] = json!(v);
        }
        if let Some(v) = args.get("label_ids").and_then(|v| v.as_array()) {
            body["label_ids"] = json!(v);
        }
        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_create_work_item POST {url}");
        let resp = self.client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created issue: {e}")))?;
        Ok(format!("Created issue: {name}\nID: {id}\nSequence: #{seq}",
            name = i.name, id = i.id,
            seq = i.sequence_id.unwrap_or(0)))
    }
}

// ─── 6. plane_update_work_item ───────────────────────────────────────────────

pub struct PlaneUpdateWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneUpdateWorkItem {
    fn name(&self) -> &str { "plane_update_work_item" }
    fn description(&self) -> &str { "Update fields on an existing Plane work item" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "name": { "type": "string", "description": "New title" },
                "description_html": { "type": "string", "description": "New description (HTML)" },
                "state": { "type": "string", "description": "New state UUID" },
                "priority": { "type": "string", "description": "New priority" },
                "due_date": { "type": "string", "description": "New due date (YYYY-MM-DD)" },
                "parent": { "type": "string", "description": "New parent issue UUID" },
                "label_ids": { "type": "array", "items": { "type": "string" }, "description": "New label UUIDs (replaces existing set)" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let mut body = json!({});
        for field in &["name", "description_html", "state", "priority", "due_date", "parent"] {
            if let Some(v) = args.get(field).and_then(|v| v.as_str()) {
                body[*field] = json!(v);
            }
        }
        if let Some(v) = args.get("label_ids").and_then(|v| v.as_array()) {
            body["label_ids"] = json!(v);
        }
        if body.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            return Err(ToolError::InvalidArgument("No fields to update provided".into()));
        }
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            self.client.workspace_url()
        );
        debug!("plane_update_work_item PATCH {url}");
        let resp = self.client.patch_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse updated issue: {e}")))?;
        Ok(format!("Updated issue: {name} (ID: {id})", name = i.name, id = i.id))
    }
}

// ─── 7. plane_delete_work_item ───────────────────────────────────────────────

pub struct PlaneDeleteWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneDeleteWorkItem {
    fn name(&self) -> &str { "plane_delete_work_item" }
    fn description(&self) -> &str { "Delete a Plane work item permanently" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID to delete" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            self.client.workspace_url()
        );
        debug!("plane_delete_work_item DELETE {url}");
        let resp = self.client.delete_with_retry(&url).await?;
        PlaneClient::check_status(resp).await?;
        Ok(format!("Deleted work item {issue_id}"))
    }
}

// ─── 8. plane_list_cycles ────────────────────────────────────────────────────

pub struct PlaneListCycles {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListCycles {
    fn name(&self) -> &str { "plane_list_cycles" }
    fn description(&self) -> &str { "List cycles (sprints) in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/cycles/",
            self.client.workspace_url()
        );
        debug!("plane_list_cycles GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Cycle> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse cycles: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No cycles found".into());
        }
        let mut out = format!("Found {} cycle(s):\n", items.len());
        for c in &items {
            let status = c.status.as_deref().unwrap_or("unknown");
            let start = c.start_date.as_deref().unwrap_or("-");
            let end = c.end_date.as_deref().unwrap_or("-");
            out.push_str(&format!("  [{id}] {name} ({status}) {start}..{end}\n",
                id = c.id, name = c.name, status = status, start = start, end = end));
        }
        Ok(out)
    }
}

// ─── 9. plane_get_cycle ──────────────────────────────────────────────────────

pub struct PlaneGetCycle {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetCycle {
    fn name(&self) -> &str { "plane_get_cycle" }
    fn description(&self) -> &str { "Get details for a specific Plane cycle" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "cycle_id": { "type": "string", "description": "Cycle UUID" }
            },
            "required": ["project_id", "cycle_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let cycle_id = require_arg!(args, "cycle_id", as_str);
        let url = format!(
            "{}projects/{project_id}/cycles/{cycle_id}/",
            self.client.workspace_url()
        );
        debug!("plane_get_cycle GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let c: Cycle = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse cycle: {e}")))?;
        Ok(format!(
            "Cycle: {name}\nID: {id}\nStatus: {status}\nDates: {start} to {end}",
            name = c.name, id = c.id,
            status = c.status.as_deref().unwrap_or("unknown"),
            start = c.start_date.as_deref().unwrap_or("-"),
            end = c.end_date.as_deref().unwrap_or("-")
        ))
    }
}

// ─── 10. plane_list_cycle_issues ─────────────────────────────────────────────

pub struct PlaneListCycleIssues {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListCycleIssues {
    fn name(&self) -> &str { "plane_list_cycle_issues" }
    fn description(&self) -> &str { "List issues in a specific Plane cycle" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "cycle_id": { "type": "string", "description": "Cycle UUID" }
            },
            "required": ["project_id", "cycle_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let cycle_id = require_arg!(args, "cycle_id", as_str);
        let url = format!(
            "{}projects/{project_id}/cycles/{cycle_id}/cycle-issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_cycle_issues GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse cycle issues: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No issues in this cycle".into());
        }
        let mut out = format!("Cycle issues ({}):\n", items.len());
        for i in &items {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 11. plane_list_modules ──────────────────────────────────────────────────

pub struct PlaneListModules {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListModules {
    fn name(&self) -> &str { "plane_list_modules" }
    fn description(&self) -> &str { "List modules in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/modules/",
            self.client.workspace_url()
        );
        debug!("plane_list_modules GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Module> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse modules: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No modules found".into());
        }
        let mut out = format!("Found {} module(s):\n", items.len());
        for m in &items {
            let status = m.status.as_deref().unwrap_or("unknown");
            out.push_str(&format!("  [{id}] {name} ({status})\n",
                id = m.id, name = m.name, status = status));
        }
        Ok(out)
    }
}

// ─── 12. plane_get_module ────────────────────────────────────────────────────

pub struct PlaneGetModule {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetModule {
    fn name(&self) -> &str { "plane_get_module" }
    fn description(&self) -> &str { "Get details for a specific Plane module" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "module_id": { "type": "string", "description": "Module UUID" }
            },
            "required": ["project_id", "module_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let module_id = require_arg!(args, "module_id", as_str);
        let url = format!(
            "{}projects/{project_id}/modules/{module_id}/",
            self.client.workspace_url()
        );
        debug!("plane_get_module GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let m: Module = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse module: {e}")))?;
        Ok(format!(
            "Module: {name}\nID: {id}\nStatus: {status}\nDates: {start} to {end}",
            name = m.name, id = m.id,
            status = m.status.as_deref().unwrap_or("unknown"),
            start = m.start_date.as_deref().unwrap_or("-"),
            end = m.target_date.as_deref().unwrap_or("-")
        ))
    }
}

// ─── 13. plane_create_module ─────────────────────────────────────────────────

pub struct PlaneCreateModule {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCreateModule {
    fn name(&self) -> &str { "plane_create_module" }
    fn description(&self) -> &str { "Create a new module in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "name": { "type": "string", "description": "Module name" },
                "description": { "type": "string", "description": "Module description" },
                "status": { "type": "string", "description": "Module status" },
                "start_date": { "type": "string", "description": "Start date (YYYY-MM-DD)" },
                "target_date": { "type": "string", "description": "Target date (YYYY-MM-DD)" }
            },
            "required": ["project_id", "name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let name = require_arg!(args, "name", as_str);
        let mut body = json!({ "name": name });
        for field in &["description", "status", "start_date", "target_date"] {
            if let Some(v) = args.get(field).and_then(|v| v.as_str()) {
                body[*field] = json!(v);
            }
        }
        let url = format!(
            "{}projects/{project_id}/modules/",
            self.client.workspace_url()
        );
        debug!("plane_create_module POST {url}");
        let resp = self.client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let m: Module = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created module: {e}")))?;
        Ok(format!("Created module: {name} (ID: {id})", name = m.name, id = m.id))
    }
}

// ─── 14. plane_list_module_issues ────────────────────────────────────────────

pub struct PlaneListModuleIssues {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListModuleIssues {
    fn name(&self) -> &str { "plane_list_module_issues" }
    fn description(&self) -> &str { "List issues in a specific Plane module" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "module_id": { "type": "string", "description": "Module UUID" }
            },
            "required": ["project_id", "module_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let module_id = require_arg!(args, "module_id", as_str);
        let url = format!(
            "{}projects/{project_id}/modules/{module_id}/module-issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_module_issues GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse module issues: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No issues in this module".into());
        }
        let mut out = format!("Module issues ({}):\n", items.len());
        for i in &items {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 15. plane_list_states ───────────────────────────────────────────────────

pub struct PlaneListStates {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListStates {
    fn name(&self) -> &str { "plane_list_states" }
    fn description(&self) -> &str { "List workflow states in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/states/",
            self.client.workspace_url()
        );
        debug!("plane_list_states GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<State> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse states: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No states found".into());
        }
        let mut out = format!("States ({}):\n", items.len());
        for s in &items {
            out.push_str(&format!("  [{id}] {name} (group: {group}, color: {color})\n",
                id = s.id, name = s.name, group = s.group, color = s.color));
        }
        Ok(out)
    }
}

// ─── 16. plane_list_labels ───────────────────────────────────────────────────

pub struct PlaneListLabels {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListLabels {
    fn name(&self) -> &str { "plane_list_labels" }
    fn description(&self) -> &str { "List labels in a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/labels/",
            self.client.workspace_url()
        );
        debug!("plane_list_labels GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Label> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse labels: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No labels found".into());
        }
        let mut out = format!("Labels ({}):\n", items.len());
        for l in &items {
            let color = l.color.as_deref().unwrap_or("-");
            out.push_str(&format!("  [{id}] {name} (color: {color})\n",
                id = l.id, name = l.name, color = color));
        }
        Ok(out)
    }
}

// ─── 17. plane_list_members ──────────────────────────────────────────────────

pub struct PlaneListMembers {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListMembers {
    fn name(&self) -> &str { "plane_list_members" }
    fn description(&self) -> &str { "List members of a Plane project" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/members/",
            self.client.workspace_url()
        );
        debug!("plane_list_members GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Member> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse members: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No members found".into());
        }
        let mut out = format!("Members ({}):\n", items.len());
        for m in &items {
            let name = m.member.as_ref()
                .and_then(|md| md.display_name.as_deref())
                .unwrap_or("unknown");
            out.push_str(&format!("  [{id}] {name} (role: {role})\n",
                id = m.id, name = name, role = m.role));
        }
        Ok(out)
    }
}

// ─── 18. plane_list_comments ─────────────────────────────────────────────────

pub struct PlaneListComments {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListComments {
    fn name(&self) -> &str { "plane_list_comments" }
    fn description(&self) -> &str { "List comments on a Plane work item" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/comments/",
            self.client.workspace_url()
        );
        debug!("plane_list_comments GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Comment> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse comments: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No comments on this issue".into());
        }
        let mut out = format!("Comments ({}):\n", items.len());
        for c in &items {
            let author = c.actor_detail.as_ref()
                .and_then(|a| a.display_name.as_deref())
                .unwrap_or("unknown");
            let text = c.comment_stripped.as_deref()
                .or(c.comment_html.as_deref())
                .unwrap_or("(empty)");
            out.push_str(&format!("  [{id}] {author}: {text}\n",
                id = c.id, author = author, text = text));
        }
        Ok(out)
    }
}

// ─── 19. plane_create_comment ────────────────────────────────────────────────

pub struct PlaneCreateComment {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCreateComment {
    fn name(&self) -> &str { "plane_create_comment" }
    fn description(&self) -> &str { "Add a comment to a Plane work item" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "comment": { "type": "string", "description": "Comment text" }
            },
            "required": ["project_id", "issue_id", "comment"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let comment_text = require_arg!(args, "comment", as_str);
        let body = json!({ "comment_html": format!("<p>{comment_text}</p>") });
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/comments/",
            self.client.workspace_url()
        );
        debug!("plane_create_comment POST {url}");
        let resp = self.client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let c: Comment = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created comment: {e}")))?;
        Ok(format!("Comment added (ID: {id})", id = c.id))
    }
}

// ─── 20. plane_list_issues_by_state ──────────────────────────────────────────

pub struct PlaneListIssuesByState {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListIssuesByState {
    fn name(&self) -> &str { "plane_list_issues_by_state" }
    fn description(&self) -> &str { "List work items filtered by state group (backlog/unstarted/started/completed/cancelled)" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "state_group": {
                    "type": "string",
                    "description": "State group to filter by",
                    "enum": ["backlog", "unstarted", "started", "completed", "cancelled"]
                },
                "limit": { "type": "integer", "description": "Max results (default 50)" }
            },
            "required": ["project_id", "state_group"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let state_group = require_arg!(args, "state_group", as_str);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

        // Fetch all issues then filter client-side (state_group query param is broken in Plane CE)
        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_issues_by_state GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let filtered: Vec<Issue> = list.into_items()
            .into_iter()
            .filter(|i| {
                i.state_detail.as_ref()
                    .map(|sd| sd.group.to_lowercase() == state_group.to_lowercase())
                    .unwrap_or(false)
            })
            .take(limit)
            .collect();

        if filtered.is_empty() {
            return Ok(format!("No issues in state group '{state_group}'"));
        }
        let mut out = format!("Issues in '{}' ({}):\n", state_group, filtered.len());
        for i in &filtered {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 21. plane_get_issue_by_sequence ─────────────────────────────────────────

pub struct PlaneGetIssueBySequence {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetIssueBySequence {
    fn name(&self) -> &str { "plane_get_issue_by_sequence" }
    fn description(&self) -> &str { "Get a work item by its human-readable sequence number (e.g. LM-42)" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "sequence_id": { "type": "integer", "description": "Sequence number (numeric part of LM-42 etc.)" }
            },
            "required": ["project_id", "sequence_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let sequence_id = args.get("sequence_id").and_then(|v| v.as_u64())
            .ok_or_else(|| ToolError::InvalidArgument("missing required argument: sequence_id".into()))?;

        // Fetch all and filter by sequence_id
        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_get_issue_by_sequence GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let found = list.into_items()
            .into_iter()
            .find(|i| i.sequence_id == Some(sequence_id));

        match found {
            None => Err(ToolError::NotFound(format!("No issue with sequence_id #{sequence_id}"))),
            Some(i) => Ok(format!(
                "Issue #{seq}: {name}\nID: {id}\nPriority: {priority}\nState: {state}",
                seq = sequence_id,
                name = i.name,
                id = i.id,
                priority = i.priority.as_deref().unwrap_or("none"),
                state = i.state.as_deref().unwrap_or("unknown")
            )),
        }
    }
}

// ─── 22. plane_list_work_items_filtered ──────────────────────────────────────

pub struct PlaneListWorkItemsFiltered {
    client: Arc<PlaneClient>,
}

impl PlaneListWorkItemsFiltered {
    /// See `PlaneCreateWorkItem::new`'s doc comment -- same rationale, and
    /// same `pub(crate)` tightening (cycle 1 review).
    pub(crate) fn new(client: Arc<PlaneClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RustTool for PlaneListWorkItemsFiltered {
    fn name(&self) -> &str { "plane_list_work_items_filtered" }
    fn description(&self) -> &str { "List work items with optional priority and/or label filters" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "priority": { "type": "string", "description": "Filter by priority: urgent/high/medium/low/none" },
                "label_id": { "type": "string", "description": "Filter by label UUID" },
                "limit": { "type": "integer", "description": "Max results (default 50)" }
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let priority_filter = args.get("priority").and_then(|v| v.as_str());
        let label_filter = args.get("label_id").and_then(|v| v.as_str());
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        debug!("plane_list_work_items_filtered GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let filtered: Vec<Issue> = list.into_items()
            .into_iter()
            .filter(|i| {
                let priority_ok = priority_filter.map(|p| {
                    i.priority.as_deref().unwrap_or("none").eq_ignore_ascii_case(p)
                }).unwrap_or(true);
                let label_ok = label_filter.map(|lf| {
                    i.label_ids.iter().any(|l| l == lf)
                }).unwrap_or(true);
                priority_ok && label_ok
            })
            .take(limit)
            .collect();

        if filtered.is_empty() {
            return Ok("No work items match the given filters".into());
        }
        let mut out = format!("Filtered work items ({}):\n", filtered.len());
        for i in &filtered {
            let priority = i.priority.as_deref().unwrap_or("none");
            out.push_str(&format!("  [{id}] {name} (priority: {priority})\n",
                id = i.id, name = i.name, priority = priority));
        }
        Ok(out)
    }
}

// ─── 23. plane_list_recent_activity ──────────────────────────────────────────

pub struct PlaneListRecentActivity {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListRecentActivity {
    fn name(&self) -> &str { "plane_list_recent_activity" }
    fn description(&self) -> &str { "List recent activity/audit events for a Plane work item" }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "limit": { "type": "integer", "description": "Max results (default 20)" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/activities/",
            self.client.workspace_url()
        );
        debug!("plane_list_recent_activity GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<Activity> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse activities: {e}")))?;
        let items: Vec<Activity> = list.into_items().into_iter().take(limit).collect();
        if items.is_empty() {
            return Ok("No recent activity".into());
        }
        let mut out = format!("Recent activity ({}):\n", items.len());
        for a in &items {
            let actor = a.actor_detail.as_ref()
                .and_then(|ad| ad.display_name.as_deref())
                .unwrap_or("unknown");
            let verb = a.verb.as_deref().unwrap_or("updated");
            let field = a.field.as_deref().unwrap_or("");
            out.push_str(&format!("  {actor} {verb} {field}\n",
                actor = actor, verb = verb, field = field));
        }
        Ok(out)
    }
}

// ─── 24. plane_close_work_item ───────────────────────────────────────────────

pub struct PlaneCloseWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCloseWorkItem {
    fn name(&self) -> &str { "plane_close_work_item" }
    fn description(&self) -> &str {
        "Close a work item by moving it to the first available 'completed' state"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID to close" }
            },
            "required": ["project_id", "issue_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);

        // Fetch states to find the 'completed' group
        let states_url = format!(
            "{}projects/{project_id}/states/",
            self.client.workspace_url()
        );
        debug!("plane_close_work_item: fetching states from {states_url}");
        let body = self.client.get_json_cached(&states_url).await?;
        let list: ApiList<State> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse states: {e}")))?;

        let completed_state = list.into_items()
            .into_iter()
            .find(|s| s.group.to_lowercase() == "completed")
            .ok_or_else(|| ToolError::NotFound("No 'completed' state found in this project".into()))?;

        // PATCH the issue to use the completed state
        let body = json!({ "state": completed_state.id });
        let issue_url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            self.client.workspace_url()
        );
        debug!("plane_close_work_item PATCH {issue_url}");
        let resp = self.client.patch_with_retry(&issue_url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse updated issue: {e}")))?;
        Ok(format!(
            "Closed work item: {name} (now in state '{state}')",
            name = i.name,
            state = completed_state.name
        ))
    }
}

// ─── 25. plane_get_state_by_name ─────────────────────────────────────────────

pub struct PlaneGetStateByName {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetStateByName {
    fn name(&self) -> &str { "plane_get_state_by_name" }
    fn description(&self) -> &str {
        "Resolve a Plane workflow state UUID by its human name (e.g. \"Backlog\", \"Done\"), case-insensitive"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "name": { "type": "string", "description": "State name to match, case-insensitive (e.g. \"Backlog\", \"Done\")" }
            },
            "required": ["project_id", "name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = self.client.resolve_project_id(project_id_arg).await?;
        let name = require_arg!(args, "name", as_str);
        let url = format!(
            "{}projects/{project_id}/states/",
            self.client.workspace_url()
        );
        debug!("plane_get_state_by_name GET {url}");
        let body = self.client.get_json_cached(&url).await?;
        let list: ApiList<State> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse states: {e}")))?;
        list.into_items()
            .into_iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .map(|s| format!("State '{}': {}", s.name, s.id))
            .ok_or_else(|| ToolError::NotFound(format!("No state named '{name}' in this project")))
    }
}

// ─── 26. plane_batch_create_work_items ───────────────────────────────────────

pub struct PlaneBatchCreateWorkItems {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneBatchCreateWorkItems {
    fn name(&self) -> &str { "plane_batch_create_work_items" }
    fn description(&self) -> &str {
        "Create multiple work items in a Plane project sequentially, returning each result"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "items": {
                    "type": "array",
                    "description": "Issues to create",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "description_html": { "type": "string" },
                            "priority": { "type": "string" },
                            "state": { "type": "string" }
                        },
                        "required": ["name"]
                    }
                }
            },
            "required": ["project_id", "items"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        require_configured!(self);
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let items = args
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidArgument("missing required argument: items".into()))?;
        if items.is_empty() {
            return Err(ToolError::InvalidArgument("items must not be empty".into()));
        }
        let project_id = self.client.resolve_project_id(project_id_arg).await?;

        let url = format!(
            "{}projects/{project_id}/issues/",
            self.client.workspace_url()
        );
        let mut out = format!("Batch-created {} issue(s):\n", items.len());
        for (index, item) in items.iter().enumerate() {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidArgument(format!("items[{index}] missing required field: name"))
                })?;
            let mut body = json!({ "name": name });
            if let Some(v) = item.get("description_html").and_then(|v| v.as_str()) {
                body["description_html"] = json!(v);
            }
            if let Some(v) = item.get("priority").and_then(|v| v.as_str()) {
                body["priority"] = json!(v);
            }
            if let Some(v) = item.get("state").and_then(|v| v.as_str()) {
                body["state"] = json!(v);
            }
            debug!("plane_batch_create_work_items [{index}] POST {url}");
            let resp = self.client.post_with_retry(&url, &body).await?;
            let resp = PlaneClient::check_status(resp).await?;
            let created: Issue = resp
                .json()
                .await
                .map_err(|e| ToolError::Http(format!("Failed to parse created issue [{index}]: {e}")))?;
            out.push_str(&format!(
                "  {}/{}: [{}] {} (#{})\n",
                index + 1,
                items.len(),
                created.id,
                created.name,
                created.sequence_id.unwrap_or(0)
            ));
        }
        Ok(out)
    }
}

// ─── 27. plane_whoami ────────────────────────────────────────────────────────

pub struct PlaneWhoami {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneWhoami {
    fn name(&self) -> &str { "plane_whoami" }
    fn description(&self) -> &str {
        "Report which configured Plane identity is active, or check whether a named identity is configured. Never inspects other processes' files — identities come only from this process's own PLANE_API_KEY_<NAME> environment."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "identity": { "type": "string", "description": "Optional identity name to check (e.g. \"axon\"). Omit to report the active default identity." }
            },
            "required": []
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        if let Some(identity) = args.get("identity").and_then(|v| v.as_str()) {
            let key = identity.trim().to_lowercase();
            let is_active_default = self.client.identity_name() == Some(key.as_str());
            if self.client.identities.contains_key(&key) || is_active_default {
                return Ok(format!("Identity '{identity}' is configured (token present)."));
            }
            return Err(ToolError::NotFound(format!(
                "No Plane identity named '{identity}' is configured (expected PLANE_API_KEY_{})",
                key.to_uppercase()
            )));
        }
        if !self.client.configured() {
            return Err(self.client.not_configured());
        }
        match self.client.identity_name() {
            Some(name) => Ok(format!("Active Plane identity: {name}")),
            None => Ok(
                "Active Plane identity: unknown (a default token is set but no PLANE_IDENTITY_NAME \
                 or matching PLANE_API_KEY_<NAME> is configured for it)"
                    .into(),
            ),
        }
    }
}

// ─── Register all plane tools ─────────────────────────────────────────────────

/// Register all 27 Plane CE tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    let client = Arc::new(PlaneClient::from_env());

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(PlaneListProjects { client: client.clone() }),
        Box::new(PlaneGetProject { client: client.clone() }),
        Box::new(PlaneListWorkItems { client: client.clone() }),
        Box::new(PlaneGetWorkItem { client: client.clone() }),
        Box::new(PlaneCreateWorkItem { client: client.clone() }),
        Box::new(PlaneUpdateWorkItem { client: client.clone() }),
        Box::new(PlaneDeleteWorkItem { client: client.clone() }),
        Box::new(PlaneListCycles { client: client.clone() }),
        Box::new(PlaneGetCycle { client: client.clone() }),
        Box::new(PlaneListCycleIssues { client: client.clone() }),
        Box::new(PlaneListModules { client: client.clone() }),
        Box::new(PlaneGetModule { client: client.clone() }),
        Box::new(PlaneCreateModule { client: client.clone() }),
        Box::new(PlaneListModuleIssues { client: client.clone() }),
        Box::new(PlaneListStates { client: client.clone() }),
        Box::new(PlaneListLabels { client: client.clone() }),
        Box::new(PlaneListMembers { client: client.clone() }),
        Box::new(PlaneListComments { client: client.clone() }),
        Box::new(PlaneCreateComment { client: client.clone() }),
        Box::new(PlaneListIssuesByState { client: client.clone() }),
        Box::new(PlaneGetIssueBySequence { client: client.clone() }),
        Box::new(PlaneListWorkItemsFiltered { client: client.clone() }),
        Box::new(PlaneListRecentActivity { client: client.clone() }),
        Box::new(PlaneCloseWorkItem { client: client.clone() }),
        Box::new(PlaneGetStateByName { client: client.clone() }),
        Box::new(PlaneBatchCreateWorkItems { client: client.clone() }),
        Box::new(PlaneWhoami { client: client.clone() }),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            tracing::warn!("Failed to register plane tool: {e}");
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    /// Build a PlaneClient pointing at the given mock server URL. Uses a
    /// zero-interval rate limiter so functional tests aren't slowed down by
    /// pacing — dedicated rate-limiting tests build their own `RateLimiter`
    /// with a real interval.
    fn mock_client(server: &MockServer) -> Arc<PlaneClient> {
        Arc::new(PlaneClient {
            http: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter { last: AsyncMutex::new(None), min_interval: Duration::ZERO }),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        })
    }

    /// Register a projects-list mock so `resolve_project_id` can map a non-UUID
    /// id/identifier back to itself. Matches on `id` (== `identifier` here).
    fn mock_projects(server: &MockServer, id: &str) {
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": id, "name": "Mock", "identifier": id, "network": 0}
            ]));
        });
    }

    // ── is_uuid helper ────────────────────────────────────────────────────────

    #[test]
    fn test_is_uuid_recognizes_canonical_uuid() {
        assert!(is_uuid("4ef3f3ec-e7ef-4af3-b258-881565e629f9")); // pii-test-fixture
        assert!(!is_uuid("LM"));
        assert!(!is_uuid("proj-abc"));
        assert!(!is_uuid("4ef3f3ec-e7ef-4af3-b258-881565e629f")); // 35 chars — pii-test-fixture
        assert!(!is_uuid("4ef3f3ecXe7ef-4af3-b258-881565e629f9")); // wrong separator — pii-test-fixture
    }

    // ── project identifier → UUID resolution ──────────────────────────────────

    #[tokio::test]
    async fn test_resolve_identifier_to_uuid_then_lists_issues() {
        let server = MockServer::start();
        // Resolution step: list projects, match identifier "LM".
        let projects_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "uuid-lm", "name": "Lumina Core", "identifier": "LM", "network": 0}
            ]));
        });
        // Issues fetched against the resolved UUID, not the identifier.
        let issues_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/uuid-lm/issues/");
            then.status(200).json_body(json!([
                {"id": "i1", "name": "Task", "project": "uuid-lm", "workspace": "testws", "sequence_id": 1}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListWorkItems { client };
        let result = tool.execute(json!({"project_id": "LM"})).await.unwrap();
        assert!(result.contains("Task"), "{result}");
        projects_mock.assert();
        issues_mock.assert();
    }

    // ── Not-configured guard ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_not_configured_when_env_absent() {
        // Client with no base_url
        let client = Arc::new(PlaneClient {
            http: Client::new(),
            base_url: None,
            api_key: None,
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "moosenet".into(),
            rate_limiter: Arc::new(RateLimiter { last: AsyncMutex::new(None), min_interval: Duration::ZERO }),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        });
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)),
            "Expected NotConfigured, got {err:?}");
    }

    // ── Auth header on all requests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_auth_header_sent_on_list_projects() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "test-api-key");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({})).await;
        mock.assert();
    }

    #[tokio::test]
    async fn test_auth_header_sent_on_create_work_item() {
        let server = MockServer::start();
        mock_projects(&server, "proj-1");
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/workspaces/testws/projects/proj-1/issues/")
                .header("x-api-key", "test-api-key");
            then.status(201).json_body(json!({
                "id": "issue-1",
                "name": "Test",
                "project": "proj-1",
                "workspace": "testws",
                "sequence_id": 1
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCreateWorkItem { client };
        let _ = tool.execute(json!({"project_id": "proj-1", "name": "Test"})).await;
        mock.assert();
    }

    // ── Correct HTTP methods and paths ────────────────────────────────────────

    #[tokio::test]
    async fn test_list_projects_get_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Alpha"), "Expected project name in output: {result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_get_project_by_id() {
        let server = MockServer::start();
        mock_projects(&server, "proj-abc");
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/proj-abc/");
            then.status(200).json_body(json!({
                "id": "proj-abc", "name": "My Project", "identifier": "MP", "network": 0
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({"project_id": "proj-abc"})).await.unwrap();
        assert!(result.contains("My Project"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_create_work_item_post_request() {
        let server = MockServer::start();
        mock_projects(&server, "proj-1");
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/workspaces/testws/projects/proj-1/issues/");
            then.status(201).json_body(json!({
                "id": "issue-99", "name": "Fix login bug",
                "project": "proj-1", "workspace": "testws", "sequence_id": 99
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCreateWorkItem { client };
        let result = tool.execute(json!({
            "project_id": "proj-1",
            "name": "Fix login bug",
            "priority": "high"
        })).await.unwrap();
        assert!(result.contains("Fix login bug"), "{result}");
        assert!(result.contains("99"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_update_work_item_patch_request() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(200).json_body(json!({
                "id": "i1", "name": "Updated name",
                "project": "p1", "workspace": "testws"
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneUpdateWorkItem { client };
        let result = tool.execute(json!({
            "project_id": "p1",
            "issue_id": "i1",
            "name": "Updated name"
        })).await.unwrap();
        assert!(result.contains("Updated name"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_delete_work_item_delete_request() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::DELETE).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(204);
        });
        let client = mock_client(&server);
        let tool = PlaneDeleteWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await.unwrap();
        assert!(result.contains("i1"), "{result}");
        mock.assert();
    }

    // ── 429 retry logic ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_429_returns_rate_limit_error_after_3_attempts() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(429);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("rate limit") || err.contains("HTTP error"),
            "Expected rate limit error, got: {err}");
        assert!(mock.hits() >= 3, "Expected at least 3 retries, got {}", mock.hits());
    }

    // ── 404 → NotFound error ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_404_returns_not_found_error() {
        let server = MockServer::start();
        mock_projects(&server, "bad-id");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/bad-id/");
            then.status(404).body("Not found");
        });
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({"project_id": "bad-id"})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    // ── Missing required argument ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_missing_required_arg_returns_invalid_argument() {
        let server = MockServer::start();
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_update_with_no_fields_returns_error() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let client = mock_client(&server);
        let tool = PlaneUpdateWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
    }

    // ── Empty response handled gracefully ─────────────────────────────────────

    #[tokio::test]
    async fn test_empty_project_list_returns_message() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("No projects"), "{result}");
    }

    // ── register() populates 24 tools ─────────────────────────────────────────

    #[test]
    fn test_register_all_plane_tools() {
        // Temporarily set env vars so client.configured() is true-ish
        // (not required for registration, only for execution)
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 27,
            "Expected 27 plane tools, got {}", registry.len());
    }

    #[test]
    fn test_all_plane_tool_names_unique() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        let names: Vec<String> = registry.list().iter().map(|t| t.name.clone()).collect();
        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(names.len(), deduped.len(),
            "Duplicate tool names found: {:?}", names);
    }

    #[test]
    fn test_all_plane_tools_have_descriptions() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        for info in registry.list() {
            assert!(!info.description.is_empty(),
                "Tool '{}' has empty description", info.name);
        }
    }

    #[test]
    fn test_all_plane_tools_have_valid_parameters_schema() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        for info in registry.list() {
            assert_eq!(info.parameters["type"], "object",
                "Tool '{}' parameters schema should have type: object", info.name);
        }
    }

    // ── Filter by state group (client-side) ───────────────────────────────────

    #[tokio::test]
    async fn test_list_issues_by_state_filters_correctly() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([
                {
                    "id": "i1", "name": "Open task",
                    "project": "p1", "workspace": "testws",
                    "state_detail": {"id": "s1", "name": "In Progress", "color": "#fff", "group": "started"}
                },
                {
                    "id": "i2", "name": "Done task",
                    "project": "p1", "workspace": "testws",
                    "state_detail": {"id": "s2", "name": "Done", "color": "#0f0", "group": "completed"}
                }
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListIssuesByState { client };
        let result = tool.execute(json!({"project_id": "p1", "state_group": "started"})).await.unwrap();
        assert!(result.contains("Open task"), "{result}");
        assert!(!result.contains("Done task"), "{result}");
    }

    // ── Paginated response ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_paginated_response_parsed_correctly() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!({
                "count": 2,
                "next": null,
                "previous": null,
                "results": [
                    {"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0},
                    {"id": "p2", "name": "Beta", "identifier": "BT", "network": 0}
                ]
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Alpha"), "{result}");
        assert!(result.contains("Beta"), "{result}");
    }

    // ── close_work_item fetches states then patches ───────────────────────────

    #[tokio::test]
    async fn test_close_work_item_uses_completed_state() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let _states_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/states/");
            then.status(200).json_body(json!([
                {"id": "s-done", "name": "Done", "color": "#0f0", "group": "completed", "project": "p1"},
                {"id": "s-todo", "name": "Todo", "color": "#fff", "group": "unstarted", "project": "p1"}
            ]));
        });
        let _patch_mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(200).json_body(json!({
                "id": "i1", "name": "My task",
                "project": "p1", "workspace": "testws",
                "state": "s-done"
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCloseWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await.unwrap();
        assert!(result.contains("Done") || result.contains("My task"), "{result}");
    }

    // ── get_issue_by_sequence finds correct issue ─────────────────────────────

    #[tokio::test]
    async fn test_get_issue_by_sequence_found() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([
                {"id": "i1", "name": "Task A", "sequence_id": 1, "project": "p1", "workspace": "testws"},
                {"id": "i42", "name": "Task B", "sequence_id": 42, "project": "p1", "workspace": "testws"}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneGetIssueBySequence { client };
        let result = tool.execute(json!({"project_id": "p1", "sequence_id": 42})).await.unwrap();
        assert!(result.contains("Task B"), "{result}");
        assert!(result.contains("42"), "{result}");
    }

    #[tokio::test]
    async fn test_get_issue_by_sequence_not_found() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneGetIssueBySequence { client };
        let result = tool.execute(json!({"project_id": "p1", "sequence_id": 99})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    // ── New tools: state-by-name, batch create, whoami ───────────────────────

    #[tokio::test]
    async fn test_get_state_by_name_case_insensitive() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/states/");
            then.status(200).json_body(json!([
                {"id": "s-done", "name": "Done", "color": "#0f0", "group": "completed", "project": "p1"}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneGetStateByName { client };
        let result = tool.execute(json!({"project_id": "p1", "name": "done"})).await.unwrap();
        assert!(result.contains("s-done"), "{result}");
    }

    #[tokio::test]
    async fn test_batch_create_work_items_creates_each_and_reports_all() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let post_mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(201).json_body(json!({
                "id": "generated", "name": "generated", "project": "p1", "workspace": "testws", "sequence_id": 1
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneBatchCreateWorkItems { client };
        let result = tool.execute(json!({
            "project_id": "p1",
            "items": [{"name": "Task A"}, {"name": "Task B"}, {"name": "Task C"}]
        })).await.unwrap();
        assert!(result.contains("Batch-created 3"), "{result}");
        assert_eq!(post_mock.hits(), 3, "Expected one POST per item");
    }

    #[tokio::test]
    async fn test_batch_create_rejects_empty_items() {
        let server = MockServer::start();
        let client = mock_client(&server);
        let tool = PlaneBatchCreateWorkItems { client };
        let result = tool.execute(json!({"project_id": "p1", "items": []})).await;
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    // ── Rate limiting: proves actual pacing, not just that a sleep call exists ──

    #[tokio::test]
    async fn test_rate_limiter_enforces_minimum_interval_between_calls() {
        let interval = Duration::from_millis(250);
        let limiter = RateLimiter { last: AsyncMutex::new(None), min_interval: interval };

        let start = Instant::now();
        for _ in 0..4 {
            limiter.acquire().await;
        }
        let elapsed = start.elapsed();

        // 4 calls through the gate = 3 enforced gaps of `interval`.
        let expected_min = interval * 3;
        assert!(
            elapsed >= expected_min,
            "Expected at least {expected_min:?} elapsed across 4 paced calls, got {elapsed:?}"
        );
        // Generous ceiling to catch a limiter that isn't pacing at all (e.g. sleeping way too long).
        assert!(
            elapsed < expected_min + Duration::from_millis(500),
            "Elapsed {elapsed:?} far exceeds expected pacing — limiter may be broken"
        );
    }

    #[tokio::test]
    async fn test_rate_limiter_paces_real_http_calls_through_client() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([]));
        });
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter { last: AsyncMutex::new(None), min_interval: Duration::from_millis(200) }),
            cache: Arc::new(GetCache::new(Duration::from_millis(1))), // effectively disabled
        };

        let start = Instant::now();
        for _ in 0..3 {
            let url = format!("{}projects/", client.workspace_url());
            let _ = client.get_with_retry(&url).await.unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(400),
            "3 real HTTP calls through a 200ms-paced client should take >= 400ms, got {elapsed:?}"
        );
    }

    // ── GET caching: proves a second call within TTL skips the network ───────

    #[tokio::test]
    async fn test_get_json_cached_serves_second_call_from_cache_within_ttl() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([{"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0}]));
        });
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter { last: AsyncMutex::new(None), min_interval: Duration::ZERO }),
            cache: Arc::new(GetCache::new(Duration::from_millis(300))),
        };
        let url = format!("{}projects/", client.workspace_url());

        let first = client.get_json_cached(&url).await.unwrap();
        let second = client.get_json_cached(&url).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(mock.hits(), 1, "Second call within TTL must be served from cache, not the network");
    }

    #[tokio::test]
    async fn test_get_json_cached_refetches_after_ttl_expiry() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([{"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0}]));
        });
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter { last: AsyncMutex::new(None), min_interval: Duration::ZERO }),
            cache: Arc::new(GetCache::new(Duration::from_millis(100))),
        };
        let url = format!("{}projects/", client.workspace_url());

        let _ = client.get_json_cached(&url).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = client.get_json_cached(&url).await.unwrap();

        assert_eq!(mock.hits(), 2, "A GET after TTL expiry must hit the network again");
    }

    // ── Retry/backoff: real mocked failure modes ──────────────────────────────

    #[tokio::test]
    async fn test_429_respects_retry_after_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(429).header("Retry-After", "1");
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };

        let start = Instant::now();
        let result = tool.execute(json!({})).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert_eq!(mock.hits(), 3, "Expected exactly 3 attempts");
        // 2 waits of 1s (Retry-After) between the 3 attempts.
        assert!(elapsed >= Duration::from_millis(1900), "Expected >= ~2s from Retry-After pacing, got {elapsed:?}");
        assert!(elapsed < Duration::from_secs(8), "Retry-After should be used instead of the larger backoff table, got {elapsed:?}");
    }

    #[tokio::test]
    async fn test_5xx_retries_then_fails() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(503);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert_eq!(mock.hits(), 3, "Expected 3 attempts on repeated 5xx");
    }

    #[tokio::test]
    async fn test_network_error_retries_then_fails() {
        // Nothing is listening on this port — every attempt is a connection error.
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_millis(300)).build().unwrap(),
            base_url: Some("http://127.0.0.1:1".into()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter { last: AsyncMutex::new(None), min_interval: Duration::ZERO }),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        };
        let tool = PlaneListProjects { client: Arc::new(client) };
        let result = tool.execute(json!({})).await;
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::Http(_)), "{err:?}");
        assert!(err.to_string().contains("3 attempts"), "Expected retry-exhaustion message, got: {err}");
    }

    #[tokio::test]
    async fn test_401_does_not_retry() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(401);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result.unwrap_err(), ToolError::Http(_)));
        assert_eq!(mock.hits(), 1, "401 must never be retried");
    }

    #[tokio::test]
    async fn test_403_does_not_retry() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(403);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result.unwrap_err(), ToolError::Http(_)));
        assert_eq!(mock.hits(), 1, "403 must never be retried");
    }

    // ── Multi-identity: no cross-contamination, correct attribution ──────────

    /// Build a client with a default token plus two named identities, all
    /// sharing one mock server.
    fn multi_identity_client(server: &MockServer) -> Arc<PlaneClient> {
        let mut identities = HashMap::new();
        identities.insert("axon".to_string(), "token-axon".to_string());
        identities.insert("vigil".to_string(), "token-vigil".to_string());
        Arc::new(PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("token-default".into()),
            identity_name: Some("default".into()),
            identities: Arc::new(identities),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter { last: AsyncMutex::new(None), min_interval: Duration::ZERO }),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        })
    }

    #[tokio::test]
    async fn test_for_identity_uses_correct_token_per_identity_no_cross_contamination() {
        let server = MockServer::start();
        let axon_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(200).json_body(json!([]));
        });
        let vigil_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-vigil");
            then.status(200).json_body(json!([]));
        });

        let base = multi_identity_client(&server);
        let axon_client = base.for_identity("axon").unwrap();
        let vigil_client = base.for_identity("VIGIL").unwrap(); // case-insensitive lookup

        assert_eq!(axon_client.identity_name(), Some("axon"));
        assert_eq!(vigil_client.identity_name(), Some("vigil"));

        let axon_url = format!("{}projects/", axon_client.workspace_url());
        let _ = axon_client.get_with_retry(&axon_url).await.unwrap();
        let vigil_url = format!("{}projects/", vigil_client.workspace_url());
        let _ = vigil_client.get_with_retry(&vigil_url).await.unwrap();

        // Each identity's request must have hit ONLY the mock matching its own
        // token — proving no cross-identity token leakage.
        assert_eq!(axon_mock.hits(), 1);
        assert_eq!(vigil_mock.hits(), 1);
    }

    #[tokio::test]
    async fn test_for_identity_shared_cache_does_not_leak_across_identities() {
        // Two identities sharing the same GetCache Arc (as for_identity always
        // shares it) must never be served each other's cached response for the
        // same URL — Plane GET responses are not uniformly workspace-scoped
        // (e.g. project visibility varies by the calling token's membership),
        // so this exercises the exact path a URL-only cache key would leak.
        let server = MockServer::start();
        let axon_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(200).json_body(json!([
                {"id": "axon-only-project", "name": "Axon's Project", "identifier": "AX", "network": 0}
            ]));
        });
        let vigil_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-vigil");
            then.status(200).json_body(json!([
                {"id": "vigil-only-project", "name": "Vigil's Project", "identifier": "VG", "network": 0}
            ]));
        });

        let base = multi_identity_client(&server);
        let axon_client = base.for_identity("axon").unwrap();
        let vigil_client = base.for_identity("vigil").unwrap();
        assert!(Arc::ptr_eq(&axon_client.cache, &vigil_client.cache), "test setup must share one cache Arc");

        let url = format!("{}projects/", axon_client.workspace_url());

        // Axon populates the shared cache first.
        let axon_body = axon_client.get_json_cached(&url).await.unwrap();
        assert!(axon_body.contains("Axon's Project"));

        // Vigil requests the SAME url within the same TTL window. A URL-only
        // cache key would return Axon's cached body here without ever
        // reaching the network with Vigil's own token.
        let vigil_body = vigil_client.get_json_cached(&url).await.unwrap();
        assert!(vigil_body.contains("Vigil's Project"), "Vigil must get its own data, got: {vigil_body}");
        assert!(!vigil_body.contains("Axon's Project"), "Vigil must never see Axon's cached response");

        assert_eq!(axon_mock.hits(), 1, "Axon's own request should hit the network once");
        assert_eq!(vigil_mock.hits(), 1, "Vigil must make its own network request, not reuse Axon's cache entry");
    }

    #[tokio::test]
    async fn test_for_identity_unknown_name_returns_error() {
        let server = MockServer::start();
        let base = multi_identity_client(&server);
        let result = base.for_identity("nonexistent-agent");
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_plane_whoami_reports_active_default_identity() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("default"), "{result}");
    }

    #[tokio::test]
    async fn test_plane_whoami_checks_named_identity_configured() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let result = tool.execute(json!({"identity": "axon"})).await.unwrap();
        assert!(result.contains("configured"), "{result}");
    }

    #[tokio::test]
    async fn test_plane_whoami_unknown_identity_not_found() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let result = tool.execute(json!({"identity": "ghost"})).await;
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_resolves_identity_name_from_matching_token() {
        // Isolate from other tests / the real environment via serial_test,
        // since PlaneClient::from_env() reads process-wide env vars.
        std::env::set_var("PLANE_API_URL", "http://example.invalid");
        std::env::set_var("PLANE_API_KEY", "shared-token-value");
        std::env::set_var("PLANE_API_KEY_SEER", "shared-token-value");
        std::env::remove_var("PLANE_IDENTITY_NAME");

        let client = PlaneClient::from_env();
        assert_eq!(client.identity_name(), Some("seer"));

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var("PLANE_API_KEY_SEER");
    }
}
