//! GitHub provider adapter (S106 / GITX-03).
//!
//! Implements the provider-agnostic [`ForgeProvider`] trait (GITX-01) for GitHub
//! over its REST v3 API (with a GraphQL v4 helper for the endpoints a future
//! caller needs it for). This is the **git-PUBLIC** pool's `github` adapter: the
//! same comprehensive endpoint vocabulary every forge speaks, wired against the
//! GitHub API.
//!
//! ## What this item is (and is NOT)
//! - IS: the GitHub *provider adapter* — repo/branch/ref/commit/PR/issue/release/
//!   tag/webhook/package/content/org endpoints, a truthful capability map, and
//!   per-identity credential resolution, plus the git-public pool marker and
//!   egress isolation for outbound calls.
//! - IS NOT: the git-public MCP *tool* (assembled in GITX-05, which routes
//!   providers + enforces the PII-gate write posture + first-publish gate). No
//!   PII gate or posture enforcement lives here — the adapter only advertises
//!   that it belongs to the public (exfiltration) pool; GITX-05 makes that
//!   load-bearing.
//! - IS NOT: the GHMR mirror engine (`src/github/mirror/`). That is a separate
//!   git-transport write path integrated as git-public's swept-tree writer in
//!   GITX-05; this adapter does not touch it, and the existing `github_*` tools
//!   in [`super`] continue to work unchanged.
//!
//! ## Credentials — single sanctioned path, per-identity, never literals
//! Tokens are resolved from the process environment, which is the vault access
//! path in this crate: [`crate::secrets_bootstrap`] materializes the runtime
//! secret store into env at startup, so an env read here IS the SecretManager
//! read. Two credential shapes are supported, mirroring the Gitea (S105/GPAT)
//! model:
//! - `GITHUB_PAT_<NAME>` — a named-identity token (e.g. `GITHUB_PAT_MOOSE`).
//!   Selected per call via the request's `identity`, or by the active default
//!   (`GITHUB_IDENTITY_NAME`, default `moose`).
//! - `GITHUB_TOKEN` — the unsuffixed operator token, used as the fallback when
//!   the active-default identity has no `GITHUB_PAT_<NAME>` provisioned (keeps
//!   the existing single-token deployments working during the transition).
//!
//! Every resolved token is `.trim()`-ed (a trailing newline from a secret file
//! or env injection is a classic silent-`401` cause) and is NEVER logged — the
//! [`std::fmt::Debug`] impl redacts it.
//!
//! ## Egress isolation
//! Outbound requests are constrained to an allowlist of GitHub hosts (the
//! configured API base host plus the `github.com` family, extendable via
//! `GITHUB_EGRESS_ALLOWLIST`). Every request passes [`GitHubAdapter::host_allowed`]
//! before a socket is opened; a non-allowlisted host is refused locally rather
//! than dialed — the adapter cannot be pointed at an arbitrary exfil endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};

use crate::forge::provider::{ForgeError, ForgeProvider, ForgeRequest, ForgeResponse};
use crate::forge::{CapabilityMap, ForgeEndpoint};

/// Default GitHub REST/GraphQL API base (production).
const GITHUB_API: &str = "https://api.github.com";
/// Default target org/owner when a request omits `owner`.
const DEFAULT_ORG: &str = "moosenet-io";
/// Env prefix marking a per-identity token: `GITHUB_PAT_<NAME>` → identity
/// `<name>` (lowercased). Single source of truth for the scan + lookup.
const GITHUB_IDENTITY_PREFIX: &str = "GITHUB_PAT_";
/// Active-default identity when neither `GITHUB_IDENTITY_NAME` nor a per-call
/// `identity` selects one. Matches Gitea's operator-persona default (`moose`).
const DEFAULT_GITHUB_IDENTITY: &str = "moose";

/// Hosts always permitted for outbound GitHub traffic, independent of config.
const DEFAULT_ALLOWED_HOSTS: &[&str] = &["api.github.com", "github.com", "uploads.github.com"];

/// Scan this process's own environment for `GITHUB_PAT_<NAME>` tokens, returning
/// a `lowercased-name -> token` map. The ONLY place the prefix is matched.
/// Empty-valued vars are skipped (set-but-empty == absent); names lowercased so
/// a case-variant duplicate collapses onto one entry. Never reads another
/// process's files. Mirrors Gitea's `scan_gitea_identities`.
fn scan_github_identities() -> HashMap<String, String> {
    let mut identities: HashMap<String, String> = HashMap::new();
    for (k, v) in std::env::vars() {
        if let Some(name) = k.strip_prefix(GITHUB_IDENTITY_PREFIX) {
            let token = v.trim().to_string();
            if !token.is_empty() {
                identities.insert(name.to_lowercase(), token);
            }
        }
    }
    identities
}

/// The GitHub [`ForgeProvider`] adapter. Holds a shared HTTP client, the
/// configured API base + default owner, the resolved credential set, and the
/// egress allowlist. Cheap to clone (all heavy state behind `Arc`).
#[derive(Clone)]
pub struct GitHubAdapter {
    http: reqwest::Client,
    api_base: String,
    default_owner: String,
    default_identity: String,
    /// `GITHUB_PAT_<NAME>` tokens, lowercased-name -> trimmed token.
    identities: Arc<HashMap<String, String>>,
    /// Unsuffixed `GITHUB_TOKEN` fallback (trimmed), if set.
    fallback_token: Option<String>,
    /// Allowlisted outbound hosts (host or host:port), lowercased.
    allowlist: Arc<Vec<String>>,
    caps: Arc<CapabilityMap>,
}

/// Never print credential-bearing fields. Redacted so logs/panics/`{:?}` can
/// never leak a token.
impl std::fmt::Debug for GitHubAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubAdapter")
            .field("api_base", &self.api_base)
            .field("default_owner", &self.default_owner)
            .field("default_identity", &self.default_identity)
            .field("identities", &format!("<{} configured, redacted>", self.identities.len()))
            .field("fallback_token", &if self.fallback_token.is_some() { "<redacted>" } else { "<none>" })
            .field("allowlist", &self.allowlist)
            .finish()
    }
}

impl GitHubAdapter {
    /// Build the adapter from the process environment.
    ///
    /// Never fails on missing credentials: capability introspection needs none,
    /// and each write resolves its token lazily (returning a clean
    /// [`ForgeError::Auth`] if unconfigured) — so a credential-less deployment
    /// still gets an honest adapter rather than a construction panic.
    ///
    /// Config (all optional):
    /// - `GITHUB_API_BASE` — override the API base (test points at httpmock;
    ///   defaults to `https://api.github.com`).
    /// - `GITHUB_ORG` — default owner (defaults to `moosenet-io`).
    /// - `GITHUB_IDENTITY_NAME` — active-default identity (defaults to `moose`).
    /// - `GITHUB_EGRESS_ALLOWLIST` — extra comma-separated allowlisted hosts.
    pub fn from_env() -> Result<Self, ForgeError> {
        let api_base = std::env::var("GITHUB_API_BASE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| GITHUB_API.to_string());
        let default_owner = std::env::var("GITHUB_ORG")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ORG.to_string());
        let default_identity = std::env::var("GITHUB_IDENTITY_NAME")
            .ok()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GITHUB_IDENTITY.to_string());
        let identities = scan_github_identities();
        let fallback_token = std::env::var("GITHUB_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("MooseNet-MCP/1.0")
            .build()
            .map_err(|e| ForgeError::Transport { provider: "github".into(), message: e.to_string() })?;

        Ok(Self {
            http,
            allowlist: Arc::new(build_allowlist(&api_base)),
            api_base,
            default_owner,
            default_identity,
            identities: Arc::new(identities),
            fallback_token,
            caps: Arc::new(github_capabilities()),
        })
    }

    /// This adapter belongs to the git-PUBLIC provider pool — the exfiltration
    /// surface where GITX-05 makes the PII gate load-bearing on writes. The
    /// adapter itself does not gate; it only advertises the pool so the tool
    /// assembly can apply the right posture.
    pub fn is_public_pool(&self) -> bool {
        true
    }

    /// Names of all configured `GITHUB_PAT_<NAME>` identities (lowercased,
    /// sorted). Never returns — and cannot recover — token values.
    pub fn identity_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.identities.keys().cloned().collect();
        names.sort();
        names
    }

    /// Resolve the trimmed token for a request's identity selection.
    ///
    /// - A non-empty `identity` selects that `GITHUB_PAT_<NAME>` token; an
    ///   unknown identity is a clean [`ForgeError::Auth`].
    /// - Otherwise the active-default identity's token is used, falling back to
    ///   the unsuffixed `GITHUB_TOKEN` when that identity has no PAT.
    /// - No configured credential at all is a clean [`ForgeError::Auth`], never
    ///   an empty `Authorization` header sent to GitHub.
    fn resolve_token(&self, identity: Option<&str>) -> Result<String, ForgeError> {
        let auth = |m: String| ForgeError::Auth { provider: "github".into(), message: m };
        let token = match identity.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => {
                let key = name.to_lowercase();
                self.identities.get(&key).cloned().ok_or_else(|| {
                    auth(format!(
                        "no GitHub identity named '{name}' is configured (expected {GITHUB_IDENTITY_PREFIX}{})",
                        key.to_uppercase()
                    ))
                })?
            }
            None => self
                .identities
                .get(&self.default_identity)
                .cloned()
                .or_else(|| self.fallback_token.clone())
                .ok_or_else(|| {
                    auth(format!(
                        "no GitHub credential configured (set {GITHUB_IDENTITY_PREFIX}{} or GITHUB_TOKEN)",
                        self.default_identity.to_uppercase()
                    ))
                })?,
        };
        // Defensive re-trim: the maps already hold trimmed values, but a stray
        // whitespace token must never reach the Authorization header as a "valid"
        // credential — treat it as absent.
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(auth("resolved GitHub token is empty".into()));
        }
        Ok(token)
    }

    /// Whether `url`'s authority (host or host:port) is on the egress allowlist.
    /// Exact-authority matching — a different port is a different, non-allowlisted
    /// endpoint. Exposed for tests; used by every outbound call.
    pub fn host_allowed(&self, url: &str) -> bool {
        match host_of(url) {
            Some(host) => {
                let host = host.to_lowercase();
                self.allowlist.iter().any(|h| h == &host)
            }
            None => false,
        }
    }

    // ── HTTP core ────────────────────────────────────────────────────────────

    /// Perform one REST call, returning the parsed JSON body on 2xx. Enforces
    /// egress isolation first, maps 401/403 to [`ForgeError::Auth`] (the
    /// auth/scope-failure surface), and every other non-2xx to
    /// [`ForgeError::Transport`]. A 2xx empty body (e.g. `204`) yields JSON
    /// `null`.
    async fn call(
        &self,
        token: &str,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value, ForgeError> {
        if !self.host_allowed(url) {
            return Err(ForgeError::Transport {
                provider: "github".into(),
                message: format!(
                    "egress blocked: host of '{}' is not on the GitHub allowlist",
                    host_of(url).unwrap_or_default()
                ),
            });
        }
        let mut req = self
            .http
            .request(method, url)
            .header("Authorization", format!("token {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28"); // pii-test-fixture
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ForgeError::Transport { provider: "github".into(), message: e.to_string() })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ForgeError::Transport { provider: "github".into(), message: e.to_string() })?;

        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ForgeError::Auth {
                provider: "github".into(),
                message: format!("HTTP {}: {}", status.as_u16(), text),
            });
        }
        if !status.is_success() {
            return Err(ForgeError::Transport {
                provider: "github".into(),
                message: format!("HTTP {}: {}", status.as_u16(), text),
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| ForgeError::Transport {
            provider: "github".into(),
            message: format!("invalid JSON from GitHub: {e}"),
        })
    }

    /// Fetch a file's *raw* bytes via the contents API (`Accept: raw`), returning
    /// the text wrapped as `{ "path": …, "raw": … }`. Egress-checked like
    /// [`GitHubAdapter::call`]. Non-2xx maps the same way.
    async fn raw_fetch(&self, token: &str, url: &str, path: &str) -> Result<Value, ForgeError> {
        if !self.host_allowed(url) {
            return Err(ForgeError::Transport {
                provider: "github".into(),
                message: format!(
                    "egress blocked: host of '{}' is not on the GitHub allowlist",
                    host_of(url).unwrap_or_default()
                ),
            });
        }
        let resp = self
            .http
            .get(url)
            .header("Authorization", format!("token {token}"))
            .header("Accept", "application/vnd.github.raw")
            .header("X-GitHub-Api-Version", "2022-11-28") // pii-test-fixture
            .send()
            .await
            .map_err(|e| ForgeError::Transport { provider: "github".into(), message: e.to_string() })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ForgeError::Transport { provider: "github".into(), message: e.to_string() })?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ForgeError::Auth { provider: "github".into(), message: format!("HTTP {}: {}", status.as_u16(), text) });
        }
        if !status.is_success() {
            return Err(ForgeError::Transport { provider: "github".into(), message: format!("HTTP {}: {}", status.as_u16(), text) });
        }
        Ok(json!({ "path": path, "raw": text }))
    }

    /// Minimal GraphQL v4 helper for the endpoints REST v3 cannot express (kept
    /// public so callers/tests can exercise it; the shared surface stays REST,
    /// but a forge is a forge and some queries only exist on v4). Posts to
    /// `{api_base}/graphql`, egress-checked, auth/transport-mapped like REST.
    pub async fn graphql(
        &self,
        identity: Option<&str>,
        query: &str,
        variables: Value,
    ) -> Result<Value, ForgeError> {
        let token = self.resolve_token(identity)?;
        let url = format!("{}/graphql", self.api_base.trim_end_matches('/'));
        self.call(&token, reqwest::Method::POST, &url, Some(&json!({ "query": query, "variables": variables })))
            .await
    }

    // ── param helpers ────────────────────────────────────────────────────────

    /// The `owner` for a request (`params.owner`, else the configured default).
    fn owner<'a>(&'a self, p: &'a Value) -> String {
        p.get("owner")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.default_owner)
            .to_string()
    }

    /// A required, non-empty string param.
    fn req_str(p: &Value, key: &str) -> Result<String, ForgeError> {
        p.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ForgeError::InvalidRequest(format!("'{key}' is required")))
    }

    /// `(owner, repo)` — repo required.
    fn owner_repo(&self, p: &Value) -> Result<(String, String), ForgeError> {
        Ok((self.owner(p), Self::req_str(p, "repo")?))
    }

    fn base(&self) -> &str {
        self.api_base.trim_end_matches('/')
    }
}

/// Build the egress allowlist: the API base host (so a test/self-hosted proxy
/// base is reachable) plus the constant `github.com` family, plus any
/// `GITHUB_EGRESS_ALLOWLIST` extras. All lowercased.
fn build_allowlist(api_base: &str) -> Vec<String> {
    let mut hosts: Vec<String> = DEFAULT_ALLOWED_HOSTS.iter().map(|s| s.to_string()).collect();
    if let Some(h) = host_of(api_base) {
        // The configured base's FULL authority (host or host:port). A specific
        // port is matched specifically — no bare-host wildcard that would open
        // every port on a test/self-hosted address.
        hosts.push(h.to_lowercase());
    }
    if let Ok(extra) = std::env::var("GITHUB_EGRESS_ALLOWLIST") {
        for h in extra.split(',') {
            let h = h.trim().to_lowercase();
            if !h.is_empty() {
                hosts.push(h);
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Extract the host (host or host:port) from a URL string, without pulling in a
/// URL-parsing crate: strip scheme, take up to the first `/`, drop any userinfo.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_string())
    }
}

/// GitHub's advertised support for the shared vocabulary. GitHub REST v3 covers
/// nearly the whole surface; the two honest gaps are:
/// - `ReposMirrorConfig` — GitHub has no pull-mirror configuration REST endpoint.
/// - `PackagesPublish` — publishing goes through a registry wire protocol
///   (npm/Cargo/OCI/…), not a single REST call the adapter can make.
///
/// Both are left [`SupportLevel::Unsupported`] so the capability map never claims
/// a call the adapter cannot honestly make.
fn github_capabilities() -> CapabilityMap {
    use ForgeEndpoint::*;
    let mut m = CapabilityMap::new();
    for ep in [
        // Repos
        ReposList, ReposGet, ReposCreate, ReposUpdate, ReposDelete, ReposFork,
        ReposVisibility, ReposMetadata,
        // Branches / refs
        BranchesList, BranchesGet, BranchesCreate, BranchesDelete, BranchesProtection,
        BranchesDefault, RefsList, RefsGet, RefsCreate, RefsDelete,
        // Commits
        CommitsList, CommitsGet, CommitsCompareDiff, CommitsStatus,
        // Pull requests
        PullRequestsList, PullRequestsGet, PullRequestsCreate, PullRequestsUpdate,
        PullRequestsReview, PullRequestsComment, PullRequestsMerge, PullRequestsClose,
        // Issues
        IssuesList, IssuesGet, IssuesCreate, IssuesUpdate, IssuesComment, IssuesLabel,
        IssuesAssign, IssuesClose,
        // Releases / tags
        ReleasesList, ReleasesGet, ReleasesCreate, ReleasesUpdate, ReleasesDelete,
        ReleasesAssets, TagsList, TagsGet, TagsCreate, TagsDelete,
        // Webhooks
        WebhooksList, WebhooksCreate, WebhooksUpdate, WebhooksDelete, WebhooksTest,
        // Packages (publish excluded — registry protocol, not REST)
        PackagesList, PackagesGet, PackagesDelete,
        // Content
        ContentReadFile, ContentWriteFile, ContentListTree, ContentRawFetch,
        // Org
        OrgMembers, OrgTeams, OrgPermissions,
    ] {
        m.set(ep, crate::forge::SupportLevel::Supported);
    }
    m
}

#[async_trait]
impl ForgeProvider for GitHubAdapter {
    fn id(&self) -> &str {
        "github"
    }

    fn display_name(&self) -> &str {
        "GitHub"
    }

    fn capabilities(&self) -> &CapabilityMap {
        &self.caps
    }

    async fn execute_endpoint(
        &self,
        endpoint: ForgeEndpoint,
        req: ForgeRequest,
    ) -> Result<ForgeResponse, ForgeError> {
        use reqwest::Method;
        use ForgeEndpoint::*;

        let p = &req.params;
        let token = self.resolve_token(req.identity.as_deref())?;
        let api = self.base().to_string();
        let ok = |body: Value| Ok(ForgeResponse::new(endpoint, "github", body));

        match endpoint {
            // ── Repos ─────────────────────────────────────────────────────────
            ReposList => {
                let owner = self.owner(p);
                let url = format!("{api}/orgs/{owner}/repos?per_page=100&sort=updated");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ReposGet => {
                let (owner, repo) = self.owner_repo(p)?;
                let url = format!("{api}/repos/{owner}/{repo}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ReposCreate => {
                let owner = self.owner(p);
                let name = Self::req_str(p, "name")?;
                let body = json!({
                    "name": name,
                    "description": p.get("description").and_then(Value::as_str).unwrap_or(""),
                    "private": p.get("private").and_then(Value::as_bool).unwrap_or(false),
                    "auto_init": p.get("auto_init").and_then(Value::as_bool).unwrap_or(false),
                });
                let url = format!("{api}/orgs/{owner}/repos");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            ReposUpdate => {
                let (owner, repo) = self.owner_repo(p)?;
                let body = p.get("updates").cloned().unwrap_or_else(|| json!({}));
                let url = format!("{api}/repos/{owner}/{repo}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }
            ReposDelete => {
                let (owner, repo) = self.owner_repo(p)?;
                let url = format!("{api}/repos/{owner}/{repo}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }
            ReposFork => {
                let (owner, repo) = self.owner_repo(p)?;
                let mut body = json!({});
                if let Some(org) = p.get("organization").and_then(Value::as_str) {
                    body["organization"] = json!(org);
                }
                let url = format!("{api}/repos/{owner}/{repo}/forks");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            ReposVisibility => {
                let (owner, repo) = self.owner_repo(p)?;
                let mut body = json!({});
                if let Some(v) = p.get("private").and_then(Value::as_bool) {
                    body["private"] = json!(v);
                }
                if let Some(v) = p.get("visibility").and_then(Value::as_str) {
                    body["visibility"] = json!(v);
                }
                let url = format!("{api}/repos/{owner}/{repo}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }
            ReposMetadata => {
                // Replace repository topics (the closest REST metadata surface).
                let (owner, repo) = self.owner_repo(p)?;
                let names = p.get("names").cloned().unwrap_or_else(|| json!([]));
                let body = json!({ "names": names });
                let url = format!("{api}/repos/{owner}/{repo}/topics");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }

            // ── Branches / refs ─────────────────────────────────────────────────
            BranchesList => {
                let (owner, repo) = self.owner_repo(p)?;
                let url = format!("{api}/repos/{owner}/{repo}/branches?per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            BranchesGet => {
                let (owner, repo) = self.owner_repo(p)?;
                let branch = Self::req_str(p, "branch")?;
                let url = format!("{api}/repos/{owner}/{repo}/branches/{branch}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            BranchesCreate => {
                let (owner, repo) = self.owner_repo(p)?;
                let branch = Self::req_str(p, "branch")?;
                let sha = Self::req_str(p, "sha")?;
                let body = json!({ "ref": format!("refs/heads/{branch}"), "sha": sha });
                let url = format!("{api}/repos/{owner}/{repo}/git/refs");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            BranchesDelete => {
                let (owner, repo) = self.owner_repo(p)?;
                let branch = Self::req_str(p, "branch")?;
                let url = format!("{api}/repos/{owner}/{repo}/git/refs/heads/{branch}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }
            BranchesProtection => {
                let (owner, repo) = self.owner_repo(p)?;
                let branch = Self::req_str(p, "branch")?;
                let body = Self::req_value(p, "protection")?;
                let url = format!("{api}/repos/{owner}/{repo}/branches/{branch}/protection");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            BranchesDefault => {
                let (owner, repo) = self.owner_repo(p)?;
                let default_branch = Self::req_str(p, "default_branch")?;
                let body = json!({ "default_branch": default_branch });
                let url = format!("{api}/repos/{owner}/{repo}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }
            RefsList => {
                let (owner, repo) = self.owner_repo(p)?;
                let url = match p.get("ref").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                    Some(r) => format!("{api}/repos/{owner}/{repo}/git/matching-refs/{r}"),
                    None => format!("{api}/repos/{owner}/{repo}/git/refs"),
                };
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            RefsGet => {
                let (owner, repo) = self.owner_repo(p)?;
                let r = Self::req_str(p, "ref")?;
                let url = format!("{api}/repos/{owner}/{repo}/git/ref/{r}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            RefsCreate => {
                let (owner, repo) = self.owner_repo(p)?;
                let r = Self::req_str(p, "ref")?;
                let sha = Self::req_str(p, "sha")?;
                let full = if r.starts_with("refs/") { r } else { format!("refs/{r}") };
                let body = json!({ "ref": full, "sha": sha });
                let url = format!("{api}/repos/{owner}/{repo}/git/refs");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            RefsDelete => {
                let (owner, repo) = self.owner_repo(p)?;
                let r = Self::req_str(p, "ref")?;
                let r = r.strip_prefix("refs/").unwrap_or(&r);
                let url = format!("{api}/repos/{owner}/{repo}/git/refs/{r}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }

            // ── Commits ─────────────────────────────────────────────────────────
            CommitsList => {
                let (owner, repo) = self.owner_repo(p)?;
                let mut url = format!("{api}/repos/{owner}/{repo}/commits?per_page=100");
                if let Some(sha) = p.get("sha").and_then(Value::as_str) {
                    url.push_str(&format!("&sha={sha}"));
                }
                if let Some(path) = p.get("path").and_then(Value::as_str) {
                    url.push_str(&format!("&path={path}"));
                }
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            CommitsGet => {
                let (owner, repo) = self.owner_repo(p)?;
                let sha = Self::req_str(p, "sha")?;
                let url = format!("{api}/repos/{owner}/{repo}/commits/{sha}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            CommitsCompareDiff => {
                let (owner, repo) = self.owner_repo(p)?;
                let basehead = format!("{}...{}", Self::req_str(p, "base")?, Self::req_str(p, "head")?);
                let url = format!("{api}/repos/{owner}/{repo}/compare/{basehead}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            CommitsStatus => {
                let (owner, repo) = self.owner_repo(p)?;
                let r = Self::req_str(p, "ref")?;
                let url = format!("{api}/repos/{owner}/{repo}/commits/{r}/status");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }

            // ── Pull requests ───────────────────────────────────────────────────
            PullRequestsList => {
                let (owner, repo) = self.owner_repo(p)?;
                let state = p.get("state").and_then(Value::as_str).unwrap_or("open");
                let url = format!("{api}/repos/{owner}/{repo}/pulls?state={state}&per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            PullRequestsGet => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let url = format!("{api}/repos/{owner}/{repo}/pulls/{n}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            PullRequestsCreate => {
                let (owner, repo) = self.owner_repo(p)?;
                let body = json!({
                    "title": Self::req_str(p, "title")?,
                    "head": Self::req_str(p, "head")?,
                    "base": Self::req_str(p, "base")?,
                    "body": p.get("body").and_then(Value::as_str).unwrap_or(""),
                });
                let url = format!("{api}/repos/{owner}/{repo}/pulls");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            PullRequestsUpdate => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let body = p.get("updates").cloned().unwrap_or_else(|| json!({}));
                let url = format!("{api}/repos/{owner}/{repo}/pulls/{n}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }
            PullRequestsReview => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let mut body = json!({ "event": p.get("event").and_then(Value::as_str).unwrap_or("COMMENT") });
                if let Some(b) = p.get("body").and_then(Value::as_str) {
                    body["body"] = json!(b);
                }
                let url = format!("{api}/repos/{owner}/{repo}/pulls/{n}/reviews");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            PullRequestsComment => {
                // PR conversation comments are issue comments in GitHub's model.
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let body = json!({ "body": Self::req_str(p, "body")? });
                let url = format!("{api}/repos/{owner}/{repo}/issues/{n}/comments");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            PullRequestsMerge => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let mut body = json!({});
                if let Some(m) = p.get("merge_method").and_then(Value::as_str) {
                    body["merge_method"] = json!(m);
                }
                if let Some(t) = p.get("commit_title").and_then(Value::as_str) {
                    body["commit_title"] = json!(t);
                }
                let url = format!("{api}/repos/{owner}/{repo}/pulls/{n}/merge");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            PullRequestsClose => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let body = json!({ "state": "closed" });
                let url = format!("{api}/repos/{owner}/{repo}/pulls/{n}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }

            // ── Issues ──────────────────────────────────────────────────────────
            IssuesList => {
                let (owner, repo) = self.owner_repo(p)?;
                let state = p.get("state").and_then(Value::as_str).unwrap_or("open");
                let url = format!("{api}/repos/{owner}/{repo}/issues?state={state}&per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            IssuesGet => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let url = format!("{api}/repos/{owner}/{repo}/issues/{n}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            IssuesCreate => {
                let (owner, repo) = self.owner_repo(p)?;
                let mut body = json!({ "title": Self::req_str(p, "title")? });
                if let Some(b) = p.get("body").and_then(Value::as_str) { body["body"] = json!(b); }
                if let Some(l) = p.get("labels") { body["labels"] = l.clone(); }
                if let Some(a) = p.get("assignees") { body["assignees"] = a.clone(); }
                let url = format!("{api}/repos/{owner}/{repo}/issues");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            IssuesUpdate => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let body = p.get("updates").cloned().unwrap_or_else(|| json!({}));
                let url = format!("{api}/repos/{owner}/{repo}/issues/{n}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }
            IssuesComment => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let body = json!({ "body": Self::req_str(p, "body")? });
                let url = format!("{api}/repos/{owner}/{repo}/issues/{n}/comments");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            IssuesLabel => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let body = json!({ "labels": p.get("labels").cloned().unwrap_or_else(|| json!([])) });
                let url = format!("{api}/repos/{owner}/{repo}/issues/{n}/labels");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            IssuesAssign => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let body = json!({ "assignees": p.get("assignees").cloned().unwrap_or_else(|| json!([])) });
                let url = format!("{api}/repos/{owner}/{repo}/issues/{n}/assignees");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            IssuesClose => {
                let (owner, repo) = self.owner_repo(p)?;
                let n = Self::req_num(p, "number")?;
                let body = json!({ "state": "closed" });
                let url = format!("{api}/repos/{owner}/{repo}/issues/{n}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }

            // ── Releases / tags ─────────────────────────────────────────────────
            ReleasesList => {
                let (owner, repo) = self.owner_repo(p)?;
                let url = format!("{api}/repos/{owner}/{repo}/releases?per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ReleasesGet => {
                let (owner, repo) = self.owner_repo(p)?;
                let id = Self::req_num(p, "id")?;
                let url = format!("{api}/repos/{owner}/{repo}/releases/{id}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ReleasesCreate => {
                let (owner, repo) = self.owner_repo(p)?;
                let mut body = json!({ "tag_name": Self::req_str(p, "tag_name")? });
                for k in ["name", "body", "target_commitish"] {
                    if let Some(v) = p.get(k).and_then(Value::as_str) { body[k] = json!(v); }
                }
                for k in ["draft", "prerelease"] {
                    if let Some(v) = p.get(k).and_then(Value::as_bool) { body[k] = json!(v); }
                }
                let url = format!("{api}/repos/{owner}/{repo}/releases");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            ReleasesUpdate => {
                let (owner, repo) = self.owner_repo(p)?;
                let id = Self::req_num(p, "id")?;
                let body = p.get("updates").cloned().unwrap_or_else(|| json!({}));
                let url = format!("{api}/repos/{owner}/{repo}/releases/{id}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }
            ReleasesDelete => {
                let (owner, repo) = self.owner_repo(p)?;
                let id = Self::req_num(p, "id")?;
                let url = format!("{api}/repos/{owner}/{repo}/releases/{id}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }
            ReleasesAssets => {
                let (owner, repo) = self.owner_repo(p)?;
                let id = Self::req_num(p, "id")?;
                let url = format!("{api}/repos/{owner}/{repo}/releases/{id}/assets?per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            TagsList => {
                let (owner, repo) = self.owner_repo(p)?;
                let url = format!("{api}/repos/{owner}/{repo}/tags?per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            TagsGet => {
                let (owner, repo) = self.owner_repo(p)?;
                let tag = Self::req_str(p, "tag")?;
                let url = format!("{api}/repos/{owner}/{repo}/git/ref/tags/{tag}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            TagsCreate => {
                // Create a lightweight tag ref pointing at a commit SHA.
                let (owner, repo) = self.owner_repo(p)?;
                let tag = Self::req_str(p, "tag")?;
                let sha = Self::req_str(p, "sha")?;
                let body = json!({ "ref": format!("refs/tags/{tag}"), "sha": sha });
                let url = format!("{api}/repos/{owner}/{repo}/git/refs");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            TagsDelete => {
                let (owner, repo) = self.owner_repo(p)?;
                let tag = Self::req_str(p, "tag")?;
                let url = format!("{api}/repos/{owner}/{repo}/git/refs/tags/{tag}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }

            // ── Webhooks ────────────────────────────────────────────────────────
            WebhooksList => {
                let (owner, repo) = self.owner_repo(p)?;
                let url = format!("{api}/repos/{owner}/{repo}/hooks?per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            WebhooksCreate => {
                let (owner, repo) = self.owner_repo(p)?;
                let body = Self::req_value(p, "hook")?;
                let url = format!("{api}/repos/{owner}/{repo}/hooks");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            WebhooksUpdate => {
                let (owner, repo) = self.owner_repo(p)?;
                let id = Self::req_num(p, "id")?;
                let body = p.get("updates").cloned().unwrap_or_else(|| json!({}));
                let url = format!("{api}/repos/{owner}/{repo}/hooks/{id}");
                ok(self.call(&token, Method::PATCH, &url, Some(&body)).await?)
            }
            WebhooksDelete => {
                let (owner, repo) = self.owner_repo(p)?;
                let id = Self::req_num(p, "id")?;
                let url = format!("{api}/repos/{owner}/{repo}/hooks/{id}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }
            WebhooksTest => {
                let (owner, repo) = self.owner_repo(p)?;
                let id = Self::req_num(p, "id")?;
                let url = format!("{api}/repos/{owner}/{repo}/hooks/{id}/tests");
                ok(self.call(&token, Method::POST, &url, Some(&json!({}))).await?)
            }

            // ── Packages ────────────────────────────────────────────────────────
            PackagesList => {
                let owner = self.owner(p);
                let ptype = p.get("package_type").and_then(Value::as_str).unwrap_or("container");
                let url = format!("{api}/orgs/{owner}/packages?package_type={ptype}&per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            PackagesGet => {
                let owner = self.owner(p);
                let ptype = Self::req_str(p, "package_type")?;
                let name = Self::req_str(p, "package_name")?;
                let url = format!("{api}/orgs/{owner}/packages/{ptype}/{name}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            PackagesDelete => {
                let owner = self.owner(p);
                let ptype = Self::req_str(p, "package_type")?;
                let name = Self::req_str(p, "package_name")?;
                let url = format!("{api}/orgs/{owner}/packages/{ptype}/{name}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }

            // ── Content ─────────────────────────────────────────────────────────
            ContentReadFile => {
                let (owner, repo) = self.owner_repo(p)?;
                let path = Self::req_str(p, "path")?;
                let mut url = format!("{api}/repos/{owner}/{repo}/contents/{path}");
                if let Some(r) = p.get("ref").and_then(Value::as_str) {
                    url.push_str(&format!("?ref={r}"));
                }
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ContentWriteFile => {
                let (owner, repo) = self.owner_repo(p)?;
                let path = Self::req_str(p, "path")?;
                let message = Self::req_str(p, "message")?;
                // Content may be given raw (utf-8) or already base64; GitHub wants
                // base64. Default: base64-encode the provided utf-8 `content`.
                let content_b64 = match p.get("content_base64").and_then(Value::as_str) {
                    Some(b64) => b64.to_string(),
                    None => B64.encode(Self::req_str(p, "content")?.as_bytes()),
                };
                let mut body = json!({ "message": message, "content": content_b64 });
                for k in ["sha", "branch"] {
                    if let Some(v) = p.get(k).and_then(Value::as_str) { body[k] = json!(v); }
                }
                let url = format!("{api}/repos/{owner}/{repo}/contents/{path}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            ContentListTree => {
                let (owner, repo) = self.owner_repo(p)?;
                let tree_sha = Self::req_str(p, "tree_sha")?;
                let recursive = p.get("recursive").and_then(Value::as_bool).unwrap_or(false);
                let mut url = format!("{api}/repos/{owner}/{repo}/git/trees/{tree_sha}");
                if recursive { url.push_str("?recursive=1"); }
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ContentRawFetch => {
                let (owner, repo) = self.owner_repo(p)?;
                let path = Self::req_str(p, "path")?;
                let mut url = format!("{api}/repos/{owner}/{repo}/contents/{path}");
                if let Some(r) = p.get("ref").and_then(Value::as_str) {
                    url.push_str(&format!("?ref={r}"));
                }
                ok(self.raw_fetch(&token, &url, &path).await?)
            }

            // ── Org / collaboration ─────────────────────────────────────────────
            OrgMembers => {
                let owner = self.owner(p);
                let url = format!("{api}/orgs/{owner}/members?per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            OrgTeams => {
                let owner = self.owner(p);
                let url = format!("{api}/orgs/{owner}/teams?per_page=100");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            OrgPermissions => {
                let (owner, repo) = self.owner_repo(p)?;
                let username = Self::req_str(p, "username")?;
                let url = format!("{api}/repos/{owner}/{repo}/collaborators/{username}/permission");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }

            // Advertised as Unsupported in the capability map, so `dispatch`
            // rejects these before reaching here; the arm keeps the match total.
            ReposMirrorConfig | PackagesPublish => Err(ForgeError::NotImplemented {
                provider: "github".into(),
                endpoint: endpoint.as_str(),
            }),
        }
    }
}

impl GitHubAdapter {
    /// A required JSON-object/array param passed through verbatim as a body.
    fn req_value(p: &Value, key: &str) -> Result<Value, ForgeError> {
        p.get(key)
            .cloned()
            .filter(|v| !v.is_null())
            .ok_or_else(|| ForgeError::InvalidRequest(format!("'{key}' object is required")))
    }

    /// A required integer param (issue/PR number, release/hook id). Accepts a
    /// JSON number or a numeric string.
    fn req_num(p: &Value, key: &str) -> Result<i64, ForgeError> {
        if let Some(n) = p.get(key).and_then(Value::as_i64) {
            return Ok(n);
        }
        if let Some(s) = p.get(key).and_then(Value::as_str) {
            if let Ok(n) = s.trim().parse::<i64>() {
                return Ok(n);
            }
        }
        Err(ForgeError::InvalidRequest(format!("'{key}' must be an integer")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{SupportLevel, ForgeProvider};
    use httpmock::prelude::*;
    use serial_test::serial;

    /// Build an adapter pointed at a test base URL, with a fixed token and no
    /// env dependence. The allowlist is derived from `base` so the httpmock host
    /// is reachable while arbitrary hosts stay blocked.
    fn test_adapter(base: &str) -> GitHubAdapter {
        let mut identities = HashMap::new();
        identities.insert("moose".to_string(), "testtoken".to_string());
        GitHubAdapter {
            http: reqwest::Client::builder().build().unwrap(),
            api_base: base.to_string(),
            default_owner: "moosenet-io".to_string(),
            default_identity: "moose".to_string(),
            identities: Arc::new(identities),
            fallback_token: None,
            allowlist: Arc::new(build_allowlist(base)),
            caps: Arc::new(github_capabilities()),
        }
    }

    fn req(params: Value) -> ForgeRequest {
        ForgeRequest::new(params)
    }

    // ── capability map ────────────────────────────────────────────────────────

    #[test]
    fn provider_id_and_pool() {
        let a = test_adapter("https://api.github.com");
        assert_eq!(a.id(), "github");
        assert_eq!(a.display_name(), "GitHub");
        assert!(a.is_public_pool());
    }

    #[test]
    fn capability_map_marks_honest_gaps_unsupported() {
        let a = test_adapter("https://api.github.com");
        // Broadly supported.
        for ep in [
            ForgeEndpoint::ReposList, ForgeEndpoint::ReposCreate, ForgeEndpoint::PullRequestsCreate,
            ForgeEndpoint::IssuesCreate, ForgeEndpoint::ReleasesCreate, ForgeEndpoint::WebhooksCreate,
            ForgeEndpoint::ContentWriteFile, ForgeEndpoint::OrgMembers, ForgeEndpoint::RefsDelete,
        ] {
            assert_eq!(a.support_level(ep), SupportLevel::Supported, "{ep:?} should be supported");
        }
        // The two honest gaps.
        assert_eq!(a.support_level(ForgeEndpoint::ReposMirrorConfig), SupportLevel::Unsupported);
        assert_eq!(a.support_level(ForgeEndpoint::PackagesPublish), SupportLevel::Unsupported);
        // Report covers the full vocabulary and reflects the gaps.
        let report = a.capability_report();
        assert_eq!(report["repos"]["repos_mirror_config"], "unsupported");
        assert_eq!(report["packages"]["packages_publish"], "unsupported");
        assert_eq!(report["repos"]["repos_list"], "supported");
    }

    #[tokio::test]
    async fn dispatch_rejects_unsupported_endpoint_cleanly() {
        // No network — the capability gate rejects before any transport.
        let a = test_adapter("http://127.0.0.1:1");
        let err = a
            .dispatch(ForgeEndpoint::ReposMirrorConfig, req(json!({ "repo": "r" })))
            .await
            .expect_err("mirror-config is unsupported");
        match err {
            ForgeError::Unsupported { provider, endpoint } => {
                assert_eq!(provider, "github");
                assert_eq!(endpoint, "repos_mirror_config");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    // ── egress isolation ──────────────────────────────────────────────────────

    #[test]
    fn host_of_parses_authority() {
        assert_eq!(host_of("https://api.github.com/repos/x").as_deref(), Some("api.github.com"));
        assert_eq!(host_of("http://127.0.0.1:8080/y").as_deref(), Some("127.0.0.1:8080")); // pii-test-fixture
        assert_eq!(host_of("https://<email>/z").as_deref(), Some("evil.example.com")); // pii-test-fixture
        assert_eq!(host_of("not-a-url").as_deref(), Some("not-a-url"));
    }

    #[test]
    fn egress_allowlist_permits_github_blocks_others() {
        let a = test_adapter("https://api.github.com");
        assert!(a.host_allowed("https://api.github.com/orgs/x/repos"));
        assert!(a.host_allowed("https://github.com/x"));
        assert!(a.host_allowed("https://uploads.github.com/z"));
        // Arbitrary exfil host is refused locally.
        assert!(!a.host_allowed("https://evil.example.com/steal"));
        assert!(!a.host_allowed("http://169.254.169.254/latest/meta-data")); // pii-test-fixture
    }

    #[test]
    fn test_base_host_is_allowlisted() {
        let a = test_adapter("http://127.0.0.1:34567"); // pii-test-fixture
        assert!(a.host_allowed("http://127.0.0.1:34567/repos/x/y")); // pii-test-fixture
        // A different port on the same address is NOT the same authority.
        assert!(!a.host_allowed("http://127.0.0.1:99/other"));
    }

    #[tokio::test]
    async fn call_refuses_non_allowlisted_host_without_dialing() {
        let a = test_adapter("https://api.github.com");
        // Points execute at a repo whose owner/repo build a github.com URL, but
        // force a disallowed host directly via the low-level call.
        let err = a
            .call("t", reqwest::Method::GET, "https://evil.example.com/x", None)
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ForgeError::Transport { .. }));
        assert!(err.to_string().contains("egress blocked"));
    }

    // ── identity / token resolution ───────────────────────────────────────────

    #[test]
    fn resolve_token_selects_named_identity() {
        let a = test_adapter("https://api.github.com");
        assert_eq!(a.resolve_token(Some("moose")).unwrap(), "testtoken");
        // Case-insensitive.
        assert_eq!(a.resolve_token(Some("MOOSE")).unwrap(), "testtoken");
    }

    #[test]
    fn resolve_token_unknown_identity_is_auth_error() {
        let a = test_adapter("https://api.github.com");
        let err = a.resolve_token(Some("nobody")).expect_err("unknown identity");
        assert!(matches!(err, ForgeError::Auth { .. }));
    }

    #[test]
    fn resolve_token_falls_back_to_github_token() {
        let mut a = test_adapter("https://api.github.com");
        // No PAT for the default identity, but an unsuffixed token is set.
        a.identities = Arc::new(HashMap::new());
        a.fallback_token = Some("fallbacktok".to_string());
        assert_eq!(a.resolve_token(None).unwrap(), "fallbacktok");
    }

    #[test]
    fn resolve_token_no_credential_is_auth_error() {
        let mut a = test_adapter("https://api.github.com");
        a.identities = Arc::new(HashMap::new());
        a.fallback_token = None;
        let err = a.resolve_token(None).expect_err("no credential");
        assert!(matches!(err, ForgeError::Auth { .. }));
    }

    #[test]
    #[serial]
    fn from_env_trims_token_and_scans_pat_identities() {
        let tok = std::env::var("GITHUB_TOKEN").ok();
        let pat = std::env::var("GITHUB_PAT_HARMONY").ok();
        std::env::set_var("GITHUB_TOKEN", "  padded-token\n");
        std::env::set_var("GITHUB_PAT_HARMONY", " harmony-pat ");
        let a = GitHubAdapter::from_env().unwrap();
        // Trailing newline/space trimmed on the fallback token.
        assert_eq!(a.fallback_token.as_deref(), Some("padded-token"));
        // PAT identity discovered + trimmed.
        assert_eq!(a.resolve_token(Some("harmony")).unwrap(), "harmony-pat");
        assert!(a.identity_names().contains(&"harmony".to_string()));
        // restore
        match tok { Some(v) => std::env::set_var("GITHUB_TOKEN", v), None => std::env::remove_var("GITHUB_TOKEN") }
        match pat { Some(v) => std::env::set_var("GITHUB_PAT_HARMONY", v), None => std::env::remove_var("GITHUB_PAT_HARMONY") }
    }

    #[test]
    fn debug_impl_redacts_credentials() {
        let a = test_adapter("https://api.github.com");
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("testtoken"), "token leaked in Debug: {dbg}");
        assert!(dbg.contains("redacted"));
    }

    // ── endpoint dispatch against a mocked API ──────────────────────────────────

    #[tokio::test]
    async fn repos_list_hits_org_endpoint() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/orgs/moosenet-io/repos");
            then.status(200).json_body(json!([{ "name": "a" }]));
        });
        let a = test_adapter(&server.base_url());
        let resp = a.dispatch(ForgeEndpoint::ReposList, req(json!({}))).await.unwrap();
        assert_eq!(resp.provider, "github");
        assert_eq!(resp.body[0]["name"], "a");
        m.assert();
    }

    #[tokio::test]
    async fn repos_create_posts_expected_body() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/orgs/moosenet-io/repos")
                .json_body(json!({ "name": "demo", "description": "d", "private": false, "auto_init": false }));
            then.status(201).json_body(json!({ "full_name": "moosenet-io/demo" }));
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::ReposCreate, req(json!({ "name": "demo", "description": "d" })))
            .await
            .unwrap();
        assert_eq!(resp.body["full_name"], "moosenet-io/demo");
        m.assert();
    }

    #[tokio::test]
    async fn issues_create_and_pr_create() {
        let server = MockServer::start();
        let issue = server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/issues")
                .json_body(json!({ "title": "bug", "body": "boom" }));
            then.status(201).json_body(json!({ "number": 7 }));
        });
        let pr = server.mock(|when, then| {
            when.method(POST).path("/repos/moosenet-io/r/pulls")
                .json_body(json!({ "title": "feat", "head": "topic", "base": "main", "body": "" }));
            then.status(201).json_body(json!({ "number": 9 }));
        });
        let a = test_adapter(&server.base_url());
        let ri = a.dispatch(ForgeEndpoint::IssuesCreate, req(json!({ "repo": "r", "title": "bug", "body": "boom" }))).await.unwrap();
        assert_eq!(ri.body["number"], 7);
        let rp = a.dispatch(ForgeEndpoint::PullRequestsCreate, req(json!({ "repo": "r", "title": "feat", "head": "topic", "base": "main" }))).await.unwrap();
        assert_eq!(rp.body["number"], 9);
        issue.assert();
        pr.assert();
    }

    #[tokio::test]
    async fn content_write_base64_encodes_utf8_content() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(httpmock::Method::PUT).path("/repos/moosenet-io/r/contents/docs/x.md")
                .json_body(json!({ "message": "add", "content": B64.encode("hello".as_bytes()) }));
            then.status(201).json_body(json!({ "commit": { "sha": "abc" } }));
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::ContentWriteFile, req(json!({ "repo": "r", "path": "docs/x.md", "message": "add", "content": "hello" })))
            .await
            .unwrap();
        assert_eq!(resp.body["commit"]["sha"], "abc");
        m.assert();
    }

    #[tokio::test]
    async fn refs_delete_strips_refs_prefix() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(httpmock::Method::DELETE).path("/repos/moosenet-io/r/git/refs/heads/topic");
            then.status(204);
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::RefsDelete, req(json!({ "repo": "r", "ref": "refs/heads/topic" })))
            .await
            .unwrap();
        // 204 → null body.
        assert!(resp.body.is_null());
        m.assert();
    }

    #[tokio::test]
    async fn raw_fetch_wraps_text() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r/contents/README.md");
            then.status(200).body("# raw markdown");
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::ContentRawFetch, req(json!({ "repo": "r", "path": "README.md" })))
            .await
            .unwrap();
        assert_eq!(resp.body["raw"], "# raw markdown");
        assert_eq!(resp.body["path"], "README.md");
        m.assert();
    }

    #[tokio::test]
    async fn graphql_helper_posts_query() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/graphql")
                .json_body(json!({ "query": "query{viewer{login}}", "variables": {} }));
            then.status(200).json_body(json!({ "data": { "viewer": { "login": "moose" } } }));
        });
        let a = test_adapter(&server.base_url());
        let v = a.graphql(Some("moose"), "query{viewer{login}}", json!({})).await.unwrap();
        assert_eq!(v["data"]["viewer"]["login"], "moose");
        m.assert();
    }

    // ── NEGATIVE: auth / scope failure (required by AC) ─────────────────────────

    #[tokio::test]
    async fn auth_failure_maps_403_to_forge_auth_error() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/orgs/moosenet-io/repos");
            then.status(403).json_body(json!({ "message": "Resource not accessible by personal access token" }));
        });
        let a = test_adapter(&server.base_url());
        let err = a
            .dispatch(ForgeEndpoint::ReposCreate, req(json!({ "name": "demo" })))
            .await
            .expect_err("403 must surface as an auth/scope failure");
        match err {
            ForgeError::Auth { provider, message } => {
                assert_eq!(provider, "github");
                assert!(message.contains("403"));
            }
            other => panic!("expected Auth, got {other:?}"),
        }
        m.assert();
    }

    #[tokio::test]
    async fn unauthorized_401_maps_to_auth_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r");
            then.status(401).json_body(json!({ "message": "Bad credentials" }));
        });
        let a = test_adapter(&server.base_url());
        let err = a.dispatch(ForgeEndpoint::ReposGet, req(json!({ "repo": "r" }))).await.unwrap_err();
        assert!(matches!(err, ForgeError::Auth { .. }));
    }

    #[tokio::test]
    async fn server_error_maps_to_transport() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/repos/moosenet-io/r");
            then.status(500).body("boom");
        });
        let a = test_adapter(&server.base_url());
        let err = a.dispatch(ForgeEndpoint::ReposGet, req(json!({ "repo": "r" }))).await.unwrap_err();
        assert!(matches!(err, ForgeError::Transport { .. }));
    }

    // ── missing-param validation ────────────────────────────────────────────────

    #[tokio::test]
    async fn missing_required_param_is_invalid_request() {
        let a = test_adapter("http://127.0.0.1:1");
        // repo required for ReposGet.
        let err = a.dispatch(ForgeEndpoint::ReposGet, req(json!({}))).await.unwrap_err();
        assert!(matches!(err, ForgeError::InvalidRequest(_)));
        // number required for IssuesGet.
        let err2 = a.dispatch(ForgeEndpoint::IssuesGet, req(json!({ "repo": "r" }))).await.unwrap_err();
        assert!(matches!(err2, ForgeError::InvalidRequest(_)));
    }
}
