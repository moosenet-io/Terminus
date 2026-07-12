//! GitLab v4 provider adapter (S106 / GITX-04).
//!
//! Implements the provider-agnostic [`ForgeProvider`] trait (GITX-01) for GitLab
//! over its REST v4 API. ONE client, parameterized by base-URL + config, serves
//! BOTH pools named in the S106 provider list:
//!
//! - `gitlab_ce` — self-hosted GitLab CE/EE, base URL from `GITLAB_URL`. Belongs
//!   to the **git-private** pool (source-of-truth posture).
//! - `gitlab_saas` — hosted `gitlab.com`, fixed default base. Belongs to the
//!   **git-public** pool (the exfiltration surface; GITX-05 makes the PII gate
//!   load-bearing on its writes — this adapter only advertises the pool).
//!
//! [`GitLabVariant`] selects which of the two a given [`GitLabAdapter`] instance
//! is; every other behavior (auth, pagination, capability map, endpoint dispatch)
//! is identical, matching the spec's "ONE v4 client parameterized by base-URL +
//! creds/config (do NOT write two clients)" requirement.
//!
//! ## What this item is (and is NOT)
//! - IS: the GitLab *provider adapter* — repo/branch/commit/MR/issue/release/tag/
//!   webhook/package/content/org endpoints mapped onto the shared vocabulary, a
//!   truthful capability map, and per-identity credential resolution.
//! - IS NOT: the git-private/git-public MCP *tool* assembly, provider routing, or
//!   posture enforcement (PII gate, first-publish gate) — that is GITX-05. This
//!   adapter carries no write-posture logic of its own.
//!
//! ## Terminology mapping to the common surface
//! GitLab's vocabulary differs from the shared surface in two structural ways,
//! mapped consistently everywhere in this file:
//! - **Merge Request ↔ PR.** GitLab has no "pull request"; its Merge Request
//!   (MR) is the same concept and is what every `PullRequests*` endpoint here
//!   dispatches to. Request params use the shared surface's `number` key (as
//!   GitHub's adapter does) even though GitLab's own API calls this an `iid`
//!   (project-scoped internal ID, distinct from the global numeric `id`).
//! - **project ↔ repo.** GitLab's "project" is the shared surface's repo. A
//!   project is addressed by GitLab's `:id` path segment, which accepts either a
//!   numeric project ID or a URL-encoded `namespace/project` path — this adapter
//!   always uses the latter (`{owner}%2F{repo}`, built by
//!   [`GitLabAdapter::project_ref`]) so no numeric ID lookup is ever required.
//!
//! ## Credentials — single sanctioned path, per-identity, never literals
//! Tokens resolve from this process's environment, which is the vault access
//! path in this crate: [`crate::secrets_bootstrap`] materializes the runtime
//! secret store into env at startup, so an env read here IS the SecretManager
//! read (see `secrets_bootstrap::PAT_KEY_PREFIXES`, which includes
//! `GITLAB_PAT_`). Two credential shapes, mirroring Gitea (S105) and GitHub
//! (GITX-03):
//! - `GITLAB_PAT_<NAME>` — a named-identity token (e.g. `GITLAB_PAT_MOOSE`).
//!   Selected per call via the request's `identity`, or by the active default
//!   (`GITLAB_IDENTITY_NAME`, default `moose`). Shared across both variants —
//!   deployments needing distinct CE/SaaS credentials for the same name should
//!   provision distinct identities (e.g. `GITLAB_PAT_MOOSE_CE`).
//! - `GITLAB_TOKEN` — the unsuffixed operator token, used as the fallback when
//!   the active-default identity has no `GITLAB_PAT_<NAME>` provisioned.
//!
//! Every resolved token is `.trim()`-ed and NEVER logged — the [`std::fmt::Debug`]
//! impl redacts it.
//!
//! ## Egress isolation
//! Outbound requests are constrained to an allowlist derived from the configured
//! API base host, plus (for `gitlab_saas`) the `gitlab.com` family, extendable via
//! `GITLAB_EGRESS_ALLOWLIST`. Redirects are re-validated against the same
//! allowlist on every hop (fail-closed), matching the GitHub adapter's posture.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Map, Value};

use crate::forge::provider::{ForgeError, ForgeProvider, ForgeRequest, ForgeResponse};
use crate::forge::{CapabilityMap, ForgeEndpoint, SupportLevel};

/// Default hosted GitLab SaaS API base.
const GITLAB_SAAS_API: &str = "https://gitlab.com/api/v4";
/// Default namespace/group when a request omits `owner`.
const DEFAULT_GROUP: &str = "moosenet";
/// Env prefix marking a per-identity token: `GITLAB_PAT_<NAME>` → identity
/// `<name>` (lowercased). Single source of truth for the scan + lookup. Shared
/// by both variants (see module docs).
const GITLAB_IDENTITY_PREFIX: &str = "GITLAB_PAT_";
/// Active-default identity when neither `GITLAB_IDENTITY_NAME` nor a per-call
/// `identity` selects one.
const DEFAULT_GITLAB_IDENTITY: &str = "moose";
/// Runaway guard on `Link`-header pagination (GitLab v4 emits the same RFC 5988
/// `Link: rel="next"` header GitHub does for offset-paginated list endpoints).
const MAX_PAGES: u32 = 100;

/// Which of the two provider pools this adapter instance serves. Both variants
/// share the same client/dispatch logic; only id/display-name/pool/default base
/// differ (see module docs — "ONE client, parameterized by config").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitLabVariant {
    /// Self-hosted GitLab CE/EE — git-private pool. Base URL from `GITLAB_URL`.
    Ce,
    /// Hosted `gitlab.com` — git-public pool. Fixed default base.
    Saas,
}

impl GitLabVariant {
    fn provider_id(&self) -> &'static str {
        match self {
            GitLabVariant::Ce => "gitlab_ce",
            GitLabVariant::Saas => "gitlab_saas",
        }
    }
    fn display_name(&self) -> &'static str {
        match self {
            GitLabVariant::Ce => "GitLab CE",
            GitLabVariant::Saas => "GitLab SaaS",
        }
    }
    /// Whether this variant belongs to the git-public (exfiltration-surface)
    /// pool. Self-hosted CE is git-private; hosted SaaS is git-public.
    fn is_public_pool(&self) -> bool {
        matches!(self, GitLabVariant::Saas)
    }
}

/// Scan this process's own environment for `GITLAB_PAT_<NAME>` tokens, returning
/// a `lowercased-name -> token` map. The ONLY place the prefix is matched.
/// Empty-valued vars are skipped (set-but-empty == absent). Mirrors Gitea's
/// `scan_gitea_identities` / GitHub's `scan_github_identities`.
fn scan_gitlab_identities() -> HashMap<String, String> {
    let mut identities: HashMap<String, String> = HashMap::new();
    for (k, v) in std::env::vars() {
        if let Some(name) = k.strip_prefix(GITLAB_IDENTITY_PREFIX) {
            let token = v.trim().to_string();
            if !token.is_empty() {
                identities.insert(name.to_lowercase(), token);
            }
        }
    }
    identities
}

/// The GitLab [`ForgeProvider`] adapter — one client serving `gitlab_ce` and
/// `gitlab_saas` by [`GitLabVariant`] config. Holds a shared HTTP client, the
/// configured API base + default group, the resolved credential set, and the
/// egress allowlist. Cheap to clone (all heavy state behind `Arc`).
#[derive(Clone)]
pub struct GitLabAdapter {
    http: reqwest::Client,
    variant: GitLabVariant,
    api_base: String,
    default_group: String,
    default_identity: String,
    /// `GITLAB_PAT_<NAME>` tokens, lowercased-name -> trimmed token.
    identities: Arc<HashMap<String, String>>,
    /// Unsuffixed `GITLAB_TOKEN` fallback (trimmed), if set.
    fallback_token: Option<String>,
    /// Allowlisted outbound hosts (host or host:port), lowercased.
    allowlist: Arc<Vec<String>>,
    caps: Arc<CapabilityMap>,
}

/// Never print credential-bearing fields. Redacted so logs/panics/`{:?}` can
/// never leak a token.
impl std::fmt::Debug for GitLabAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabAdapter")
            .field("variant", &self.variant)
            .field("api_base", &self.api_base)
            .field("default_group", &self.default_group)
            .field("default_identity", &self.default_identity)
            .field("identities", &format!("<{} configured, redacted>", self.identities.len()))
            .field("fallback_token", &if self.fallback_token.is_some() { "<redacted>" } else { "<none>" })
            .field("allowlist", &self.allowlist)
            .finish()
    }
}

impl GitLabAdapter {
    /// Build the adapter from the process environment for a given variant.
    ///
    /// Never fails on missing credentials: capability introspection needs none,
    /// and each write resolves its token lazily (returning a clean
    /// [`ForgeError::Auth`] if unconfigured). It DOES fail on a missing base URL
    /// for [`GitLabVariant::Ce`] — see below.
    ///
    /// Config:
    /// - `GITLAB_URL` — the CE instance base (e.g. `https://gitlab.example.com`);
    ///   the adapter appends `/api/v4`. **Required** for [`GitLabVariant::Ce`]
    ///   (unless `GITLAB_API_BASE` is set): a self-hosted adapter must never
    ///   silently default to the public `gitlab.com` API, which would send a
    ///   CE-scoped credential to the wrong host. Returns
    ///   [`ForgeError::InvalidRequest`] if neither is configured.
    /// - `GITLAB_API_BASE` — direct API-base override for either variant (test
    ///   points at httpmock; takes priority over `GITLAB_URL`).
    /// - `GITLAB_GROUP` — default namespace/group (defaults to `moosenet`).
    /// - `GITLAB_IDENTITY_NAME` — active-default identity (defaults to `moose`).
    /// - `GITLAB_EGRESS_ALLOWLIST` — extra comma-separated allowlisted hosts.
    pub fn from_env(variant: GitLabVariant) -> Result<Self, ForgeError> {
        let provider = variant.provider_id();
        let explicit_base = std::env::var("GITLAB_API_BASE")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        let api_base = match explicit_base {
            Some(b) => b,
            None => match variant {
                GitLabVariant::Saas => GITLAB_SAAS_API.to_string(),
                // Fail CLOSED, never silently default a self-hosted CE adapter
                // to the public gitlab.com API: that would send a CE-scoped
                // credential to the wrong (public) host. `GITLAB_URL` is
                // mandatory for the CE variant.
                GitLabVariant::Ce => std::env::var("GITLAB_URL")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("{}/api/v4", s.trim_end_matches('/')))
                    .ok_or_else(|| ForgeError::InvalidRequest(
                        "gitlab_ce requires GITLAB_URL (or GITLAB_API_BASE) to be configured; refusing to default a self-hosted adapter to the public gitlab.com API".into()
                    ))?,
            },
        };
        let default_group = std::env::var("GITLAB_GROUP")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GROUP.to_string());
        let default_identity = std::env::var("GITLAB_IDENTITY_NAME")
            .ok()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GITLAB_IDENTITY.to_string());
        let identities = scan_gitlab_identities();
        let fallback_token = std::env::var("GITLAB_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let allowlist = Arc::new(build_allowlist(&api_base, variant));

        let redirect_policy = build_redirect_policy(allowlist.clone());

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("MooseNet-MCP/1.0")
            .redirect(redirect_policy)
            .build()
            .map_err(|e| ForgeError::Transport { provider: provider.into(), message: e.to_string() })?;

        Ok(Self {
            http,
            variant,
            allowlist,
            api_base,
            default_group,
            default_identity,
            identities: Arc::new(identities),
            fallback_token,
            caps: Arc::new(gitlab_capabilities()),
        })
    }

    /// Convenience: build the `gitlab_ce` adapter from the process environment.
    pub fn from_env_ce() -> Result<Self, ForgeError> {
        Self::from_env(GitLabVariant::Ce)
    }

    /// Convenience: build the `gitlab_saas` adapter from the process environment.
    pub fn from_env_saas() -> Result<Self, ForgeError> {
        Self::from_env(GitLabVariant::Saas)
    }

    /// Which pool this instance belongs to. `gitlab_saas` is git-PUBLIC (the
    /// exfiltration surface where GITX-05 makes the PII gate load-bearing on
    /// writes); `gitlab_ce` is git-PRIVATE (source-of-truth, full operator R/W).
    /// The adapter itself does not gate — it only advertises the pool.
    pub fn is_public_pool(&self) -> bool {
        self.variant.is_public_pool()
    }

    /// Names of all configured `GITLAB_PAT_<NAME>` identities (lowercased,
    /// sorted). Never returns — and cannot recover — token values.
    pub fn identity_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.identities.keys().cloned().collect();
        names.sort();
        names
    }

    /// Resolve the trimmed token for a request's identity selection. Mirrors
    /// [`crate::github::adapter::GitHubAdapter::resolve_token`]'s precedence.
    fn resolve_token(&self, identity: Option<&str>) -> Result<String, ForgeError> {
        let provider = self.variant.provider_id();
        let auth = |m: String| ForgeError::Auth { provider: provider.into(), message: m };
        let token = match identity.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => {
                let key = name.to_lowercase();
                self.identities.get(&key).cloned().ok_or_else(|| {
                    auth(format!(
                        "no GitLab identity named '{name}' is configured (expected {GITLAB_IDENTITY_PREFIX}{})",
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
                        "no GitLab credential configured (set {GITLAB_IDENTITY_PREFIX}{} or GITLAB_TOKEN)",
                        self.default_identity.to_uppercase()
                    ))
                })?,
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(auth("resolved GitLab token is empty".into()));
        }
        Ok(token)
    }

    /// Whether `url`'s authority (host or host:port) is on the egress allowlist.
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
    /// egress isolation first, maps 401/403 to [`ForgeError::Auth`], and every
    /// other non-2xx to [`ForgeError::Transport`]. A 2xx empty body yields JSON
    /// `null`.
    async fn call(
        &self,
        token: &str,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value, ForgeError> {
        let provider = self.variant.provider_id();
        if !self.host_allowed(url) {
            return Err(ForgeError::Transport {
                provider: provider.into(),
                message: format!(
                    "egress blocked: host of '{}' is not on the GitLab allowlist",
                    host_of(url).unwrap_or_default()
                ),
            });
        }
        let mut req = self.http.request(method, url).header("PRIVATE-TOKEN", token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ForgeError::Transport { provider: provider.into(), message: e.to_string() })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ForgeError::Transport { provider: provider.into(), message: e.to_string() })?;

        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ForgeError::Auth { provider: provider.into(), message: format!("HTTP {}: {}", status.as_u16(), text) });
        }
        if !status.is_success() {
            return Err(ForgeError::Transport { provider: provider.into(), message: format!("HTTP {}: {}", status.as_u16(), text) });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| ForgeError::Transport {
            provider: provider.into(),
            message: format!("invalid JSON from GitLab: {e}"),
        })
    }

    /// Fetch a file's *raw* bytes via GitLab's `/raw` file endpoint. Binary-safe:
    /// UTF-8 content is returned as `{ path, encoding: "utf-8", raw }`; non-UTF-8
    /// (binary) content is returned losslessly as
    /// `{ path, encoding: "base64", raw_base64 }`.
    async fn raw_fetch(&self, token: &str, url: &str, path: &str) -> Result<Value, ForgeError> {
        let provider = self.variant.provider_id();
        if !self.host_allowed(url) {
            return Err(ForgeError::Transport {
                provider: provider.into(),
                message: format!(
                    "egress blocked: host of '{}' is not on the GitLab allowlist",
                    host_of(url).unwrap_or_default()
                ),
            });
        }
        let resp = self
            .http
            .get(url)
            .header("PRIVATE-TOKEN", token)
            .send()
            .await
            .map_err(|e| ForgeError::Transport { provider: provider.into(), message: e.to_string() })?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ForgeError::Transport { provider: provider.into(), message: e.to_string() })?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ForgeError::Auth {
                provider: provider.into(),
                message: format!("HTTP {}: {}", status.as_u16(), String::from_utf8_lossy(&bytes)),
            });
        }
        if !status.is_success() {
            return Err(ForgeError::Transport {
                provider: provider.into(),
                message: format!("HTTP {}: {}", status.as_u16(), String::from_utf8_lossy(&bytes)),
            });
        }
        match std::str::from_utf8(&bytes) {
            Ok(text) => Ok(json!({ "path": path, "encoding": "utf-8", "raw": text })),
            Err(_) => Ok(json!({ "path": path, "encoding": "base64", "raw_base64": B64.encode(&bytes) })),
        }
    }

    /// GET a single page: parsed JSON body plus the `rel="next"` URL from the
    /// `Link` header, if any (GitLab v4 emits the same RFC 5988 shape GitHub
    /// does for offset-paginated list endpoints).
    async fn get_page(&self, token: &str, url: &str) -> Result<(Value, Option<String>), ForgeError> {
        let provider = self.variant.provider_id();
        if !self.host_allowed(url) {
            return Err(ForgeError::Transport {
                provider: provider.into(),
                message: format!(
                    "egress blocked: host of '{}' is not on the GitLab allowlist",
                    host_of(url).unwrap_or_default()
                ),
            });
        }
        let resp = self
            .http
            .get(url)
            .header("PRIVATE-TOKEN", token)
            .send()
            .await
            .map_err(|e| ForgeError::Transport { provider: provider.into(), message: e.to_string() })?;
        let status = resp.status();
        let next = resp
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|h| h.to_str().ok())
            .and_then(parse_next_link);
        let text = resp
            .text()
            .await
            .map_err(|e| ForgeError::Transport { provider: provider.into(), message: e.to_string() })?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ForgeError::Auth { provider: provider.into(), message: format!("HTTP {}: {}", status.as_u16(), text) });
        }
        if !status.is_success() {
            return Err(ForgeError::Transport { provider: provider.into(), message: format!("HTTP {}: {}", status.as_u16(), text) });
        }
        let body = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).map_err(|e| ForgeError::Transport {
                provider: provider.into(),
                message: format!("invalid JSON from GitLab: {e}"),
            })?
        };
        Ok((body, next))
    }

    /// GET a list endpoint, following GitLab's `Link: rel="next"` pagination and
    /// concatenating the array pages into one result (bounded by [`MAX_PAGES`]).
    /// A non-array first page (e.g. an error object shape) is returned as-is.
    ///
    /// Two safety properties beyond a naive follow-the-`next`-link loop:
    /// - **Same-origin pagination.** The server supplies each `next` URL in its
    ///   `Link` header; the loop refuses any whose normalized origin differs from
    ///   the INITIAL request's origin. Without this, a compromised/hostile forge
    ///   could point pagination at another allowlisted host (e.g. a
    ///   `GITLAB_EGRESS_ALLOWLIST` entry) and receive the `PRIVATE-TOKEN`.
    /// - **No silent truncation.** Hitting [`MAX_PAGES`] while a further page
    ///   still exists is a hard [`ForgeError::Transport`], not a quietly
    ///   truncated "success" — the caller is told the result is incomplete.
    async fn call_paginated(&self, token: &str, url: &str) -> Result<Value, ForgeError> {
        let provider = self.variant.provider_id();
        // Pin BOTH scheme and host (an "origin"): a bare host match would still
        // permit an https→http `next` on the same host, leaking PRIVATE-TOKEN in
        // plaintext.
        let origin = origin_of(url);
        let mut next = Some(url.to_string());
        let mut items: Vec<Value> = Vec::new();
        let mut pages: u32 = 0;
        while let Some(u) = next.take() {
            // Every hop after the first comes from a server-controlled `Link`
            // header — pin it to the initial origin so the credential can never
            // be redirected to a different host OR downgraded to plaintext.
            if origin_of(&u) != origin {
                return Err(ForgeError::Transport {
                    provider: provider.into(),
                    message: format!(
                        "egress blocked: paginated 'next' origin '{}' differs from the request origin",
                        origin_of(&u).unwrap_or_default()
                    ),
                });
            }
            let (body, nxt) = self.get_page(token, &u).await?;
            match body {
                Value::Array(mut a) => items.append(&mut a),
                other => return Ok(other),
            }
            pages += 1;
            if pages >= MAX_PAGES {
                if nxt.is_some() {
                    return Err(ForgeError::Transport {
                        provider: provider.into(),
                        message: format!(
                            "result exceeded MAX_PAGES ({MAX_PAGES}); refusing to return a truncated list"
                        ),
                    });
                }
                break;
            }
            next = nxt;
        }
        Ok(Value::Array(items))
    }

    // ── param helpers ────────────────────────────────────────────────────────

    /// The URL-encoded `owner`/namespace segment for a request (`params.owner`,
    /// else the configured default group).
    fn owner(&self, p: &Value) -> String {
        p.get("owner")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.default_group)
            .to_string()
    }

    /// A required, non-empty string param, returned VERBATIM (trimmed).
    fn req_str(p: &Value, key: &str) -> Result<String, ForgeError> {
        p.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ForgeError::InvalidRequest(format!("'{key}' is required")))
    }

    /// A required param, percent-encoded as a single URL path segment.
    fn req_seg(p: &Value, key: &str) -> Result<String, ForgeError> {
        Ok(pct(&Self::req_str(p, key)?, false))
    }

    /// A required integer param (MR/issue `number` mapped to GitLab's `iid`,
    /// release/hook/package id). Accepts a JSON number or a numeric string.
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

    /// A required JSON-object/array param passed through verbatim as a body.
    fn req_value(p: &Value, key: &str) -> Result<Value, ForgeError> {
        p.get(key)
            .cloned()
            .filter(|v| !v.is_null())
            .ok_or_else(|| ForgeError::InvalidRequest(format!("'{key}' object is required")))
    }

    /// GitLab addresses a project by its `:id` path segment, which accepts a
    /// numeric project ID OR a URL-encoded `namespace/project` path. This
    /// adapter always builds the latter (`{owner}%2F{repo}`) — the shared
    /// "project ↔ repo" mapping named in the module docs — so no separate
    /// numeric-ID lookup call is ever needed.
    fn project_ref(&self, p: &Value) -> Result<String, ForgeError> {
        let owner = self.owner(p);
        let repo = Self::req_str(p, "repo")?;
        Ok(pct(&format!("{owner}/{repo}"), false))
    }

    fn base(&self) -> &str {
        self.api_base.trim_end_matches('/')
    }

    /// Resolve a group/org path (the shared surface's `owner`) to GitLab's
    /// numeric namespace id, required by `POST /projects` (`ReposCreate`) —
    /// without it, GitLab creates the project in the caller's PERSONAL
    /// namespace rather than the intended group, silently misplacing it.
    /// Queries `GET /namespaces?search=<owner>` and requires an EXACT
    /// case-insensitive match on `full_path` (falling back to `path`), so an
    /// ambiguous/partial search match is never picked implicitly.
    async fn resolve_namespace_id(&self, token: &str, owner: &str) -> Result<i64, ForgeError> {
        let url = format!("{}/namespaces?search={}&per_page=100", self.base(), pct(owner, false));
        let body = self.call(token, reqwest::Method::GET, &url, None).await?;
        let items = body.as_array().cloned().unwrap_or_default();
        let owner_lc = owner.to_lowercase();
        let hit = items.iter().find(|ns| {
            let full_path = ns.get("full_path").and_then(Value::as_str).unwrap_or("");
            let path = ns.get("path").and_then(Value::as_str).unwrap_or("");
            full_path.eq_ignore_ascii_case(&owner_lc) || path.eq_ignore_ascii_case(&owner_lc)
        });
        match hit.and_then(|ns| ns.get("id")).and_then(Value::as_i64) {
            Some(id) => Ok(id),
            None => Err(ForgeError::InvalidRequest(format!(
                "no GitLab namespace found matching owner/group '{owner}' \
                 (pass an explicit 'namespace_id' to bypass this lookup)"
            ))),
        }
    }

    /// Resolve a GitLab username to its numeric user id via
    /// `GET /users?username=<name>` (exact match). GitLab's membership and
    /// assignment APIs key off numeric ids, whereas the shared surface (like
    /// GitHub's adapter) speaks usernames — this bridges the two so a caller can
    /// pass a `username`/`assignees` of names and have them resolved here.
    async fn resolve_user_id(&self, token: &str, username: &str) -> Result<i64, ForgeError> {
        let url = format!("{}/users?username={}", self.base(), pct(username, false));
        let body = self.call(token, reqwest::Method::GET, &url, None).await?;
        body.as_array()
            .and_then(|a| a.first())
            .and_then(|u| u.get("id"))
            .and_then(Value::as_i64)
            .ok_or_else(|| ForgeError::InvalidRequest(format!(
                "no GitLab user found matching username '{username}'"
            )))
    }

    /// Resolve the shared surface's assignee selection into GitLab's
    /// `assignee_ids` (numeric). Accepts EITHER `assignee_ids` (numeric array,
    /// GitLab-native — used verbatim) OR `assignees` (array of username strings,
    /// GitHub-style — each resolved via [`GitLabAdapter::resolve_user_id`]).
    /// Returns `None` when neither is present so the caller can omit the field.
    async fn resolve_assignee_ids(&self, token: &str, p: &Value) -> Result<Option<Vec<i64>>, ForgeError> {
        if let Some(ids) = p.get("assignee_ids").and_then(Value::as_array) {
            let out: Vec<i64> = ids.iter().filter_map(Value::as_i64).collect();
            return Ok(Some(out));
        }
        if let Some(names) = p.get("assignees").and_then(Value::as_array) {
            let mut out = Vec::with_capacity(names.len());
            for n in names {
                if let Some(name) = n.as_str() {
                    out.push(self.resolve_user_id(token, name).await?);
                }
            }
            return Ok(Some(out));
        }
        Ok(None)
    }
}

/// Build the egress allowlist: the API base host, plus (for `gitlab_saas`) the
/// constant `gitlab.com` family, plus any `GITLAB_EGRESS_ALLOWLIST` extras.
fn build_allowlist(api_base: &str, variant: GitLabVariant) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    if variant.is_public_pool() {
        hosts.push("gitlab.com".to_string());
    }
    if let Some(h) = host_of(api_base) {
        hosts.push(h.to_lowercase());
    }
    if let Ok(extra) = std::env::var("GITLAB_EGRESS_ALLOWLIST") {
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

/// Maximum redirect hops followed for a single request. Bounds an allowlisted
/// redirect loop (each hop still re-validated) to a small, finite chain rather
/// than running until the client's overall request timeout.
const MAX_REDIRECT_HOPS: usize = 5;

/// Authority (host or host:port, default ports dropped) of a [`url::Url`] from
/// the redirect policy — mirrors [`host_of`]'s normalization so the two agree
/// (the `url` crate already returns `None` from `port()` for a scheme's default
/// port, matching `host_of`'s default-port stripping).
fn url_authority(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str()?;
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    Some(authority.to_lowercase())
}

/// Build the egress-aware redirect policy shared by [`GitLabAdapter::from_env`]
/// and the test adapter constructor. The posture is deliberately stricter than a
/// bare allowlist check, because the credential travels in a CUSTOM header
/// (`PRIVATE-TOKEN`) that `reqwest` — unlike the standard `Authorization` header
/// — does NOT strip across a cross-origin redirect:
/// - **Same-origin only.** A redirect is followed ONLY when its authority equals
///   the ORIGINAL request's authority. A different (even allowlisted) host — a
///   CDN/asset host, another pool member — would otherwise silently receive the
///   `PRIVATE-TOKEN`; refusing cross-origin redirects closes that credential-leak
///   path entirely. (The allowlist still gates the initial request in
///   [`GitLabAdapter::call`]/etc.)
/// - **Bounded hop count** — [`MAX_REDIRECT_HOPS`] caps a same-origin redirect
///   loop so it cannot run until the client timeout.
/// - **No scheme downgrade** — a chain that started `https` may never follow a
///   `http` hop: dropping to plaintext would leak the credential header over an
///   unencrypted connection.
fn build_redirect_policy(_allowlist: Arc<Vec<String>>) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECT_HOPS {
            return attempt.stop();
        }
        let url = attempt.url();
        // The original request URL is the first entry in `previous()`.
        let origin = match attempt.previous().first() {
            Some(first) => first,
            None => return attempt.stop(),
        };
        if origin.scheme() == "https" && url.scheme() != "https" {
            return attempt.stop();
        }
        match (url_authority(url), url_authority(origin)) {
            (Some(target), Some(source)) if target == source => attempt.follow(),
            // Cross-origin (or unparseable) — never forward the credential header.
            _ => attempt.stop(),
        }
    })
}

/// Map the shared surface's GitHub-style `state` values (`open`/`closed`, the
/// vocabulary GitHub's adapter and most shared-surface callers use) onto
/// GitLab's own state values (`opened`/`closed`) for MR and issue listing.
/// Anything already spelled GitLab's way (`opened`, `merged`, `all`, …) — or
/// any value this adapter doesn't recognize — passes through unchanged, so a
/// caller using GitLab-native state names is never silently rejected.
fn map_state(state: Option<&str>) -> &str {
    match state {
        Some("open") => "opened",
        Some(other) => other,
        None => "opened",
    }
}

/// Normalize a `labels` param into the comma-separated string GitLab's REST v4
/// API expects (an empty string clears all labels). Accepts either the shared
/// surface's JSON array of strings (`["a", "b"]`, e.g. as produced by the
/// GitHub adapter's `labels` param) or an already-comma-separated string,
/// passed through as-is.
fn labels_string(v: &Value) -> String {
    match v {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(","),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// Normalize a webhook body to GitLab's flat v4 shape. GitLab expects a
/// top-level `url` plus per-event boolean flags (`push_events`,
/// `merge_requests_events`, …) and a `token` secret — NOT GitHub's nested
/// `config: { url, secret }` + `events: [ "push", "pull_request", … ]` array.
///
/// - A body that is already GitLab-flat (has a top-level `url`) passes through
///   VERBATIM, so a caller supplying native GitLab fields is never mangled.
/// - A GitHub-style body (`config.url` and/or an `events` array) is translated:
///   `config.url`→`url`, `config.secret`→`token`, and each recognized GitHub
///   event name is mapped to its GitLab boolean flag. Unknown event names are
///   ignored rather than guessed.
fn translate_webhook(hook: &Value) -> Value {
    // Already GitLab-native (flat url) — pass through untouched.
    if hook.get("url").and_then(Value::as_str).is_some() {
        return hook.clone();
    }
    let mut out = Map::new();
    if let Some(url) = hook.pointer("/config/url").and_then(Value::as_str) {
        out.insert("url".into(), json!(url));
    }
    if let Some(secret) = hook.pointer("/config/secret").and_then(Value::as_str) {
        out.insert("token".into(), json!(secret));
    }
    // NB: GitHub's `active` (hook enabled/disabled) has NO GitLab equivalent —
    // deliberately NOT mapped. Mapping it onto `enable_ssl_verification` (a
    // previous mistake) would make `active: false` silently DISABLE TLS
    // certificate verification, a security regression. A caller wanting to set
    // GitLab's TLS flag passes native `enable_ssl_verification` (preserved below
    // via the config passthrough of already-flat bodies).
    if let Some(events) = hook.get("events").and_then(Value::as_array) {
        for ev in events.iter().filter_map(Value::as_str) {
            let flag = match ev {
                "push" => "push_events",
                "pull_request" => "merge_requests_events",
                "issues" => "issues_events",
                "issue_comment" | "commit_comment" => "note_events",
                "release" => "releases_events",
                "pipeline" | "status" => "pipeline_events",
                "deployment" => "deployment_events",
                _ => continue,
            };
            out.insert(flag.into(), json!(true));
        }
    }
    Value::Object(out)
}

/// Normalize a shared-surface `updates` object for an issue or merge request
/// into GitLab's field names, so a provider-agnostic (GitHub-shaped) update body
/// isn't forwarded verbatim (where `body`/`state` would be ignored). Recognized
/// remappings — anything else passes through untouched, and a value already in
/// GitLab's spelling is never clobbered:
/// - `body` → `description`
/// - `state` (`open`/`closed`) → `state_event` (`reopen`/`close`)
/// - `labels` (array) → comma-separated string
fn normalize_issue_updates(updates: &Value) -> Value {
    let mut map = match updates {
        Value::Object(m) => m.clone(),
        _ => return updates.clone(),
    };
    if let Some(body) = map.remove("body") {
        map.entry("description").or_insert(body);
    }
    if !map.contains_key("state_event") {
        if let Some(state) = map.remove("state").as_ref().and_then(Value::as_str) {
            let ev = match state {
                "closed" | "close" => Some("close"),
                "open" | "opened" | "reopen" => Some("reopen"),
                _ => None,
            };
            if let Some(ev) = ev {
                map.insert("state_event".into(), json!(ev));
            }
        }
    }
    if let Some(labels) = map.get("labels").cloned() {
        if labels.is_array() {
            map.insert("labels".into(), json!(labels_string(&labels)));
        }
    }
    Value::Object(map)
}

/// Percent-encode a string for safe interpolation into a URL. Unreserved
/// characters (`A-Z a-z 0-9 - . _ ~`) pass through; everything else becomes
/// `%XX`. When `keep_slash` is set, `/` also passes through (hierarchical path
/// values); otherwise `/` is escaped too, which is how GitLab's
/// `namespace/project` project reference is safely embedded as a single `:id`
/// path segment (`%2F`).
fn pct(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Parse a `Link` header, returning the URL marked `rel="next"` if any.
fn parse_next_link(link: &str) -> Option<String> {
    for part in link.split(',') {
        let seg = part.trim();
        if let Some((url_part, params)) = seg.split_once(';') {
            if params.contains("rel=\"next\"") {
                let u = url_part.trim().trim_start_matches('<').trim_end_matches('>');
                if !u.is_empty() {
                    return Some(u.to_string());
                }
            }
        }
    }
    None
}

/// Extract the host (host or host:port) from a URL string, using the SAME
/// parser (`reqwest::Url`, i.e. the `url` crate) that reqwest uses to actually
/// dial — never a hand-rolled split. A bespoke parser diverges from reqwest on
/// adversarial inputs (backslashes, embedded `@`/userinfo, etc.), which would
/// let an egress check pass on one host while the request is sent to another —
/// an SSRF/credential-leak bypass. Parsing once here keeps the decision and the
/// dial in agreement. The `url` crate reports `None` from `port()` for a
/// scheme's default port, so `:443`(https)/`:80`(http) normalize to a bare host
/// automatically; a non-default explicit port is preserved.
fn host_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_lowercase();
    match parsed.port() {
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host),
    }
}

/// The full origin (`scheme://normalized-authority`) of a URL string, parsed
/// with `reqwest::Url` for the same reason as [`host_of`]. Unlike `host_of`,
/// this INCLUDES the scheme, so an origin comparison distinguishes `https://h`
/// from `http://h` — the check pagination uses so a same-host `https`→`http`
/// `next` link (a plaintext credential downgrade) is rejected, not just a
/// cross-host redirect.
fn origin_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_lowercase();
    let authority = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };
    Some(format!("{}://{}", parsed.scheme(), authority))
}

/// GitLab's advertised support for the shared vocabulary. GitLab v4 covers most
/// of the surface; the honest gaps:
/// - `RefsList`/`RefsGet`/`RefsCreate`/`RefsDelete` — GitLab v4 has no generic
///   ref-namespace API like GitHub's `git/refs` (only the concrete `branches`
///   and `tags` namespaces, both covered by their own endpoints below).
/// - `OrgTeams` — GitLab has no GitHub-style "team" resource; its closest
///   concept (subgroups) is structurally different enough (a subgroup is a
///   first-class namespace, not a team membership list) that claiming this
///   endpoint would misrepresent the API. Left honestly `Unsupported`.
///
/// One place GitLab is honestly MORE capable than the GitHub adapter:
/// `ReposMirrorConfig` (GitLab project `import_url`/`mirror` settings are a
/// real REST-settable pull-mirror config) and `PackagesPublish` (GitLab's
/// generic packages API is a direct REST `PUT` upload, not a separate wire
/// protocol) are both `Supported` here.
fn gitlab_capabilities() -> CapabilityMap {
    use ForgeEndpoint::*;
    let mut m = CapabilityMap::new();
    for ep in [
        // Repos (project)
        ReposList, ReposGet, ReposCreate, ReposUpdate, ReposDelete, ReposFork,
        ReposMirrorConfig, ReposVisibility, ReposMetadata,
        // Branches (refs namespace gap noted above)
        BranchesList, BranchesGet, BranchesCreate, BranchesDelete, BranchesProtection,
        BranchesDefault,
        // Commits
        CommitsList, CommitsGet, CommitsCompareDiff, CommitsStatus,
        // Merge requests (PR)
        PullRequestsList, PullRequestsGet, PullRequestsListComments, PullRequestsCreate,
        PullRequestsUpdate,
        PullRequestsReview, PullRequestsComment, PullRequestsMerge, PullRequestsClose,
        // Issues
        IssuesList, IssuesGet, IssuesCreate, IssuesUpdate, IssuesComment, IssuesLabel,
        IssuesAssign, IssuesClose,
        // Releases / tags
        ReleasesList, ReleasesGet, ReleasesCreate, ReleasesUpdate, ReleasesDelete,
        ReleasesAssets, TagsList, TagsGet, TagsCreate, TagsDelete,
        // Webhooks
        WebhooksList, WebhooksCreate, WebhooksUpdate, WebhooksDelete, WebhooksTest,
        // Packages (publish included — generic packages API is a direct REST PUT)
        PackagesList, PackagesGet, PackagesPublish, PackagesDelete,
        // Content
        ContentReadFile, ContentWriteFile, ContentListTree, ContentRawFetch,
        // Org (teams excluded — no GitLab equivalent)
        OrgMembers, OrgPermissions,
    ] {
        m.set(ep, SupportLevel::Supported);
    }
    m
}

#[async_trait]
impl ForgeProvider for GitLabAdapter {
    fn id(&self) -> &str {
        self.variant.provider_id()
    }

    fn display_name(&self) -> &str {
        self.variant.display_name()
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
        let provider_id = self.variant.provider_id().to_string();
        let ok = |body: Value| Ok(ForgeResponse::new(endpoint, provider_id.clone(), body));

        match endpoint {
            // ── Repos (project) ─────────────────────────────────────────────────
            ReposList => {
                let owner = self.owner(p);
                let enc = pct(&owner, false);
                // A GitLab namespace is either a GROUP or a USER, and they list
                // projects under different paths (`/groups/{id}/projects` vs
                // `/users/{id}/projects`). Try the group path first (the common
                // org case), and on a not-found/forbidden fall back to the user
                // path so an `owner` that is a personal namespace still lists.
                let group_url = format!("{api}/groups/{enc}/projects?per_page=100&order_by=updated_at");
                match self.call_paginated(&token, &group_url).await {
                    Ok(body) => ok(body),
                    Err(ForgeError::Transport { .. }) | Err(ForgeError::Auth { .. }) => {
                        let user_url = format!("{api}/users/{enc}/projects?per_page=100&order_by=updated_at");
                        ok(self.call_paginated(&token, &user_url).await?)
                    }
                    Err(e) => Err(e),
                }
            }
            ReposGet => {
                let pid = self.project_ref(p)?;
                let url = format!("{api}/projects/{pid}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ReposCreate => {
                let owner = self.owner(p);
                let name = Self::req_str(p, "name")?;
                // GitLab's POST /projects places a NEW project in the
                // authenticated user's personal namespace unless `namespace_id`
                // is given — passing `owner`/the default group through
                // unresolved would silently create in the wrong namespace. An
                // explicit `namespace_id` is used verbatim; otherwise resolve
                // `owner` (the shared surface's group/org) to its numeric id.
                let namespace_id = match p.get("namespace_id").and_then(Value::as_i64) {
                    Some(id) => Some(id),
                    None => Some(self.resolve_namespace_id(&token, &owner).await?),
                };
                let mut body = json!({
                    "name": name,
                    "description": p.get("description").and_then(Value::as_str).unwrap_or(""),
                    "visibility": p.get("visibility").and_then(Value::as_str)
                        .unwrap_or(if p.get("private").and_then(Value::as_bool).unwrap_or(false) { "private" } else { "public" }),
                    "path": p.get("path").and_then(Value::as_str).unwrap_or(&name),
                });
                if let Some(id) = namespace_id {
                    body["namespace_id"] = json!(id);
                }
                // Map the shared surface's `auto_init` (GitHub's "seed a README")
                // onto GitLab's equivalent `initialize_with_readme`.
                if let Some(init) = p.get("auto_init").and_then(Value::as_bool) {
                    body["initialize_with_readme"] = json!(init);
                }
                let url = format!("{api}/projects");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            ReposUpdate => {
                let pid = self.project_ref(p)?;
                let body = p.get("updates").cloned().unwrap_or_else(|| json!({}));
                let url = format!("{api}/projects/{pid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            ReposDelete => {
                let pid = self.project_ref(p)?;
                let url = format!("{api}/projects/{pid}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }
            ReposFork => {
                let pid = self.project_ref(p)?;
                let mut body = json!({});
                if let Some(ns) = p.get("namespace").and_then(Value::as_str) {
                    body["namespace"] = json!(ns);
                }
                let url = format!("{api}/projects/{pid}/fork");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            ReposMirrorConfig => {
                // GitLab pull-mirror config is a set of project attributes.
                let pid = self.project_ref(p)?;
                let mut body = json!({});
                if let Some(u) = p.get("import_url").and_then(Value::as_str) {
                    body["import_url"] = json!(u);
                }
                for k in ["mirror", "mirror_trigger_builds", "mirror_overwrites_diverged_branches"] {
                    if let Some(v) = p.get(k).and_then(Value::as_bool) {
                        body[k] = json!(v);
                    }
                }
                let url = format!("{api}/projects/{pid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            ReposVisibility => {
                let pid = self.project_ref(p)?;
                let visibility = Self::req_str(p, "visibility")?;
                let body = json!({ "visibility": visibility });
                let url = format!("{api}/projects/{pid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            ReposMetadata => {
                // Replace project topics — the closest REST metadata surface.
                let pid = self.project_ref(p)?;
                let topics = p.get("names").or_else(|| p.get("topics")).cloned().unwrap_or_else(|| json!([]));
                let body = json!({ "topics": topics });
                let url = format!("{api}/projects/{pid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }

            // ── Branches / refs ─────────────────────────────────────────────────
            BranchesList => {
                let pid = self.project_ref(p)?;
                let url = format!("{api}/projects/{pid}/repository/branches?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            BranchesGet => {
                let pid = self.project_ref(p)?;
                let branch = Self::req_seg(p, "branch")?;
                let url = format!("{api}/projects/{pid}/repository/branches/{branch}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            BranchesCreate => {
                let pid = self.project_ref(p)?;
                let branch = pct(&Self::req_str(p, "branch")?, false);
                let ref_ = pct(&Self::req_str(p, "sha").or_else(|_| Self::req_str(p, "ref"))?, false);
                let url = format!("{api}/projects/{pid}/repository/branches?branch={branch}&ref={ref_}");
                ok(self.call(&token, Method::POST, &url, None).await?)
            }
            BranchesDelete => {
                let pid = self.project_ref(p)?;
                let branch = Self::req_seg(p, "branch")?;
                let url = format!("{api}/projects/{pid}/repository/branches/{branch}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }
            BranchesProtection => {
                let pid = self.project_ref(p)?;
                let branch = Self::req_str(p, "branch")?;
                let mut body = p.get("protection").cloned().unwrap_or_else(|| json!({}));
                body["name"] = json!(branch);
                let url = format!("{api}/projects/{pid}/protected_branches");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            BranchesDefault => {
                let pid = self.project_ref(p)?;
                let default_branch = Self::req_str(p, "default_branch")?;
                let body = json!({ "default_branch": default_branch });
                let url = format!("{api}/projects/{pid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }

            // ── Commits ─────────────────────────────────────────────────────────
            CommitsList => {
                let pid = self.project_ref(p)?;
                let mut url = format!("{api}/projects/{pid}/repository/commits?per_page=100");
                if let Some(sha) = p.get("sha").and_then(Value::as_str) {
                    url.push_str(&format!("&ref_name={}", pct(sha, false)));
                }
                if let Some(path) = p.get("path").and_then(Value::as_str) {
                    url.push_str(&format!("&path={}", pct(path, false)));
                }
                ok(self.call_paginated(&token, &url).await?)
            }
            CommitsGet => {
                let pid = self.project_ref(p)?;
                let sha = Self::req_seg(p, "sha")?;
                let url = format!("{api}/projects/{pid}/repository/commits/{sha}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            CommitsCompareDiff => {
                let pid = self.project_ref(p)?;
                let from = pct(&Self::req_str(p, "base")?, false);
                let to = pct(&Self::req_str(p, "head")?, false);
                let url = format!("{api}/projects/{pid}/repository/compare?from={from}&to={to}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            CommitsStatus => {
                let pid = self.project_ref(p)?;
                let sha = Self::req_seg(p, "sha").or_else(|_| Self::req_seg(p, "ref"))?;
                let url = format!("{api}/projects/{pid}/repository/commits/{sha}/statuses?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }

            // ── Pull / merge requests ────────────────────────────────────────────
            PullRequestsList => {
                let pid = self.project_ref(p)?;
                let state = pct(map_state(p.get("state").and_then(Value::as_str)), false);
                let url = format!("{api}/projects/{pid}/merge_requests?state={state}&per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            PullRequestsGet => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let url = format!("{api}/projects/{pid}/merge_requests/{iid}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            PullRequestsListComments => {
                // GitLab exposes an MR's discussion thread as merge-request notes.
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let url = format!("{api}/projects/{pid}/merge_requests/{iid}/notes?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            PullRequestsCreate => {
                let pid = self.project_ref(p)?;
                let body = json!({
                    "title": Self::req_str(p, "title")?,
                    "source_branch": Self::req_str(p, "head")?,
                    "target_branch": Self::req_str(p, "base")?,
                    "description": p.get("body").and_then(Value::as_str).unwrap_or(""),
                });
                let url = format!("{api}/projects/{pid}/merge_requests");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            PullRequestsUpdate => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let body = normalize_issue_updates(&p.get("updates").cloned().unwrap_or_else(|| json!({})));
                let url = format!("{api}/projects/{pid}/merge_requests/{iid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            PullRequestsReview => {
                // GitLab has no GitHub-shaped "review" object; the closest REST
                // equivalents are the approve/unapprove endpoints. Map ONLY the
                // event values that have a real, unambiguous GitLab counterpart
                // — silently treating every event as an approval would perform
                // the OPPOSITE of a caller's intent for e.g. REQUEST_CHANGES.
                // `APPROVE` (or an omitted `event`, matching GitHub's default)
                // -> approve; `REQUEST_CHANGES`/`DISMISS` -> unapprove (the
                // closest "withdraw approval" honest mapping); anything else
                // (e.g. `COMMENT`, which has no dedicated GitLab review action —
                // use `pull_requests_comment` instead) is a clean rejection
                // rather than a fabricated approval.
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let event = p.get("event").and_then(Value::as_str).unwrap_or("APPROVE").to_uppercase();
                let action = match event.as_str() {
                    "APPROVE" => "approve",
                    "REQUEST_CHANGES" | "DISMISS" => "unapprove",
                    other => {
                        return Err(ForgeError::InvalidRequest(format!(
                            "GitLab pull_requests_review has no equivalent for event '{other}'; \
                             supported: APPROVE, REQUEST_CHANGES, DISMISS (use pull_requests_comment for a plain comment)"
                        )));
                    }
                };
                let mut body = json!({});
                if let Some(sha) = p.get("sha").and_then(Value::as_str) {
                    body["sha"] = json!(sha);
                }
                let url = format!("{api}/projects/{pid}/merge_requests/{iid}/{action}");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            PullRequestsComment => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let body = json!({ "body": Self::req_str(p, "body")? });
                let url = format!("{api}/projects/{pid}/merge_requests/{iid}/notes");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            PullRequestsMerge => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let mut body = json!({});
                // GitLab's merge takes a single `merge_commit_message`; the
                // shared surface may carry `commit_title` and/or `commit_message`
                // (GitHub-style). Combine both when present, else use whichever
                // is given, so neither is silently dropped.
                let title = p.get("commit_title").and_then(Value::as_str);
                let msg = p.get("commit_message").and_then(Value::as_str);
                let merge_msg = match (title, msg) {
                    (Some(t), Some(m)) => Some(format!("{t}\n\n{m}")),
                    (Some(t), None) => Some(t.to_string()),
                    (None, Some(m)) => Some(m.to_string()),
                    (None, None) => None,
                };
                if let Some(m) = merge_msg {
                    body["merge_commit_message"] = json!(m);
                }
                if let Some(sq) = p.get("squash").and_then(Value::as_bool) {
                    body["squash"] = json!(sq);
                }
                let url = format!("{api}/projects/{pid}/merge_requests/{iid}/merge");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            PullRequestsClose => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let body = json!({ "state_event": "close" });
                let url = format!("{api}/projects/{pid}/merge_requests/{iid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }

            // ── Issues ──────────────────────────────────────────────────────────
            IssuesList => {
                let pid = self.project_ref(p)?;
                let state = pct(map_state(p.get("state").and_then(Value::as_str)), false);
                let url = format!("{api}/projects/{pid}/issues?state={state}&per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            IssuesGet => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let url = format!("{api}/projects/{pid}/issues/{iid}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            IssuesCreate => {
                let pid = self.project_ref(p)?;
                let mut body = json!({ "title": Self::req_str(p, "title")? });
                if let Some(b) = p.get("body").and_then(Value::as_str) { body["description"] = json!(b); }
                if let Some(l) = p.get("labels") { body["labels"] = json!(labels_string(l)); }
                if let Some(ids) = self.resolve_assignee_ids(&token, p).await? {
                    body["assignee_ids"] = json!(ids);
                }
                let url = format!("{api}/projects/{pid}/issues");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            IssuesUpdate => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let body = normalize_issue_updates(&p.get("updates").cloned().unwrap_or_else(|| json!({})));
                let url = format!("{api}/projects/{pid}/issues/{iid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            IssuesComment => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let body = json!({ "body": Self::req_str(p, "body")? });
                let url = format!("{api}/projects/{pid}/issues/{iid}/notes");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            IssuesLabel => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                // An absent/empty `labels` clears all labels — GitLab's own
                // "empty string" semantics, which `labels_string` naturally
                // produces for an empty or missing array.
                let labels = p.get("labels").map(labels_string).unwrap_or_default();
                let body = json!({ "labels": labels });
                let url = format!("{api}/projects/{pid}/issues/{iid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            IssuesAssign => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                // Absent selection clears assignees (empty array), matching the
                // clear-semantics of the other issue mutators.
                let ids = self.resolve_assignee_ids(&token, p).await?.unwrap_or_default();
                let body = json!({ "assignee_ids": ids });
                let url = format!("{api}/projects/{pid}/issues/{iid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            IssuesClose => {
                let pid = self.project_ref(p)?;
                let iid = Self::req_num(p, "number")?;
                let body = json!({ "state_event": "close" });
                let url = format!("{api}/projects/{pid}/issues/{iid}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }

            // ── Releases / tags ─────────────────────────────────────────────────
            ReleasesList => {
                let pid = self.project_ref(p)?;
                let url = format!("{api}/projects/{pid}/releases?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            ReleasesGet => {
                let pid = self.project_ref(p)?;
                let tag = Self::req_seg(p, "tag_name")?;
                let url = format!("{api}/projects/{pid}/releases/{tag}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ReleasesCreate => {
                let pid = self.project_ref(p)?;
                let mut body = json!({ "tag_name": Self::req_str(p, "tag_name")? });
                for k in ["name", "description", "ref"] {
                    if let Some(v) = p.get(k).and_then(Value::as_str) { body[k] = json!(v); }
                }
                let url = format!("{api}/projects/{pid}/releases");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            ReleasesUpdate => {
                let pid = self.project_ref(p)?;
                let tag = Self::req_seg(p, "tag_name")?;
                let body = p.get("updates").cloned().unwrap_or_else(|| json!({}));
                let url = format!("{api}/projects/{pid}/releases/{tag}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            ReleasesDelete => {
                let pid = self.project_ref(p)?;
                let tag = Self::req_seg(p, "tag_name")?;
                let url = format!("{api}/projects/{pid}/releases/{tag}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }
            ReleasesAssets => {
                let pid = self.project_ref(p)?;
                let tag = Self::req_seg(p, "tag_name")?;
                let url = format!("{api}/projects/{pid}/releases/{tag}/assets/links?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            TagsList => {
                let pid = self.project_ref(p)?;
                let url = format!("{api}/projects/{pid}/repository/tags?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            TagsGet => {
                let pid = self.project_ref(p)?;
                let tag = Self::req_seg(p, "tag")?;
                let url = format!("{api}/projects/{pid}/repository/tags/{tag}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            TagsCreate => {
                let pid = self.project_ref(p)?;
                let tag = pct(&Self::req_str(p, "tag")?, false);
                let ref_ = pct(&Self::req_str(p, "sha").or_else(|_| Self::req_str(p, "ref"))?, false);
                let mut url = format!("{api}/projects/{pid}/repository/tags?tag_name={tag}&ref={ref_}");
                if let Some(msg) = p.get("message").and_then(Value::as_str) {
                    url.push_str(&format!("&message={}", pct(msg, false)));
                }
                ok(self.call(&token, Method::POST, &url, None).await?)
            }
            TagsDelete => {
                let pid = self.project_ref(p)?;
                let tag = Self::req_seg(p, "tag")?;
                let url = format!("{api}/projects/{pid}/repository/tags/{tag}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }

            // ── Webhooks ────────────────────────────────────────────────────────
            WebhooksList => {
                let pid = self.project_ref(p)?;
                let url = format!("{api}/projects/{pid}/hooks?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            WebhooksCreate => {
                let pid = self.project_ref(p)?;
                let body = translate_webhook(&Self::req_value(p, "hook")?);
                let url = format!("{api}/projects/{pid}/hooks");
                ok(self.call(&token, Method::POST, &url, Some(&body)).await?)
            }
            WebhooksUpdate => {
                let pid = self.project_ref(p)?;
                let id = Self::req_num(p, "id")?;
                // Accept either a GitHub-style `hook`/`updates` body (translated)
                // or GitLab-native fields (passed through by translate_webhook).
                let raw = p.get("hook").or_else(|| p.get("updates")).cloned().unwrap_or_else(|| json!({}));
                let body = translate_webhook(&raw);
                let url = format!("{api}/projects/{pid}/hooks/{id}");
                ok(self.call(&token, Method::PUT, &url, Some(&body)).await?)
            }
            WebhooksDelete => {
                let pid = self.project_ref(p)?;
                let id = Self::req_num(p, "id")?;
                let url = format!("{api}/projects/{pid}/hooks/{id}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }
            WebhooksTest => {
                let pid = self.project_ref(p)?;
                let id = Self::req_num(p, "id")?;
                let trigger = pct(p.get("trigger").and_then(Value::as_str).unwrap_or("push_events"), false);
                let url = format!("{api}/projects/{pid}/hooks/{id}/test/{trigger}");
                ok(self.call(&token, Method::POST, &url, Some(&json!({}))).await?)
            }

            // ── Packages ────────────────────────────────────────────────────────
            PackagesList => {
                let pid = self.project_ref(p)?;
                let url = format!("{api}/projects/{pid}/packages?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            PackagesGet => {
                let pid = self.project_ref(p)?;
                let id = Self::req_num(p, "package_id")?;
                let url = format!("{api}/projects/{pid}/packages/{id}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            PackagesPublish => {
                // GitLab's generic packages registry supports a direct REST PUT
                // upload — unlike GitHub, no separate wire protocol is required.
                let pid = self.project_ref(p)?;
                let name = pct(&Self::req_str(p, "package_name")?, false);
                let version = pct(&Self::req_str(p, "package_version")?, false);
                let file_name = pct(&Self::req_str(p, "file_name")?, false);
                let content_b64 = Self::req_str(p, "content_base64")?;
                let bytes = B64.decode(content_b64.as_bytes()).map_err(|e| {
                    ForgeError::InvalidRequest(format!("'content_base64' is not valid base64: {e}"))
                })?;
                let url = format!(
                    "{api}/projects/{pid}/packages/generic/{name}/{version}/{file_name}"
                );
                if !self.host_allowed(&url) {
                    return Err(ForgeError::Transport {
                        provider: provider_id.clone(),
                        message: format!(
                            "egress blocked: host of '{}' is not on the GitLab allowlist",
                            host_of(&url).unwrap_or_default()
                        ),
                    });
                }
                let resp = self
                    .http
                    .put(&url)
                    .header("PRIVATE-TOKEN", &token)
                    .body(bytes)
                    .send()
                    .await
                    .map_err(|e| ForgeError::Transport { provider: provider_id.clone(), message: e.to_string() })?;
                let status = resp.status();
                let text = resp
                    .text()
                    .await
                    .map_err(|e| ForgeError::Transport { provider: provider_id.clone(), message: e.to_string() })?;
                if status.as_u16() == 401 || status.as_u16() == 403 {
                    return Err(ForgeError::Auth { provider: provider_id.clone(), message: format!("HTTP {}: {}", status.as_u16(), text) });
                }
                if !status.is_success() {
                    return Err(ForgeError::Transport { provider: provider_id.clone(), message: format!("HTTP {}: {}", status.as_u16(), text) });
                }
                let body = if text.trim().is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(&text).unwrap_or(Value::Null)
                };
                ok(body)
            }
            PackagesDelete => {
                let pid = self.project_ref(p)?;
                let id = Self::req_num(p, "package_id")?;
                let url = format!("{api}/projects/{pid}/packages/{id}");
                ok(self.call(&token, Method::DELETE, &url, None).await?)
            }

            // ── Content ─────────────────────────────────────────────────────────
            ContentReadFile => {
                let pid = self.project_ref(p)?;
                // GitLab's `:file_path` is a single URL path SEGMENT — unlike
                // GitHub's hierarchical contents path, embedded `/` must be
                // percent-encoded as `%2F` (same rule as `project_ref`).
                let path = Self::req_seg(p, "path")?;
                let mut url = format!("{api}/projects/{pid}/repository/files/{path}?");
                let ref_ = p.get("ref").and_then(Value::as_str).unwrap_or("HEAD");
                url.push_str(&format!("ref={}", pct(ref_, false)));
                ok(self.call(&token, Method::GET, &url, None).await?)
            }
            ContentWriteFile => {
                let pid = self.project_ref(p)?;
                let path = Self::req_seg(p, "path")?;
                let message = Self::req_str(p, "message")?;
                let branch = p.get("branch").and_then(Value::as_str).unwrap_or("main");
                // Content may be given raw (utf-8) or already base64; GitLab
                // accepts either via an explicit `encoding` field. Default:
                // base64-encode the provided utf-8 `content` VERBATIM (never
                // trim it — trimming would silently change the file's bytes and
                // would reject a legitimately empty file).
                let (content, encoding) = match p.get("content_base64").and_then(Value::as_str) {
                    Some(b64) => (b64.to_string(), "base64"),
                    None => {
                        let raw = p.get("content").and_then(Value::as_str).ok_or_else(|| {
                            ForgeError::InvalidRequest(
                                "'content' (or 'content_base64') is required".into(),
                            )
                        })?;
                        (B64.encode(raw.as_bytes()), "base64")
                    }
                };
                let mut body = json!({
                    "branch": branch,
                    "content": content,
                    "encoding": encoding,
                    "commit_message": message,
                });
                // GitLab splits file writes into POST (create a new file, fails
                // if it exists) and PUT (update an existing file, fails if it
                // does not) — there is no single upsert verb. Infer the method
                // the same way the GitHub adapter keys off `sha`: an explicit
                // `create` bool wins; otherwise treat the presence of a prior
                // blob reference (`sha`/`last_commit_id`) as "this is an update"
                // (PUT) and its ABSENCE as "this is a new file" (POST). This
                // lets a generic caller create a new file with no extra flag,
                // which the previous PUT-by-default could not.
                if let Some(id) = p.get("last_commit_id").and_then(Value::as_str) {
                    body["last_commit_id"] = json!(id);
                }
                let create = match p.get("create").and_then(Value::as_bool) {
                    Some(explicit) => explicit,
                    None => p.get("sha").and_then(Value::as_str).is_none()
                        && p.get("last_commit_id").and_then(Value::as_str).is_none(),
                };
                let method = if create { Method::POST } else { Method::PUT };
                let url = format!("{api}/projects/{pid}/repository/files/{path}");
                ok(self.call(&token, method, &url, Some(&body)).await?)
            }
            ContentListTree => {
                let pid = self.project_ref(p)?;
                let mut url = format!("{api}/projects/{pid}/repository/tree?per_page=100");
                if let Some(path) = p.get("path").and_then(Value::as_str) {
                    // The tree `path` is a QUERY param and GitLab resolves it as
                    // a hierarchical directory path — keep its `/` separators
                    // (only escape truly reserved chars) so `docs/sub` addresses
                    // the intended subtree rather than a literal `docs%2Fsub`.
                    url.push_str(&format!("&path={}", pct(path, true)));
                }
                if let Some(r) = p.get("ref").and_then(Value::as_str) {
                    url.push_str(&format!("&ref={}", pct(r, false)));
                }
                if p.get("recursive").and_then(Value::as_bool).unwrap_or(false) {
                    url.push_str("&recursive=true");
                }
                ok(self.call_paginated(&token, &url).await?)
            }
            ContentRawFetch => {
                let pid = self.project_ref(p)?;
                let raw_path = Self::req_str(p, "path")?;
                let path = pct(&raw_path, false);
                let ref_ = p.get("ref").and_then(Value::as_str).unwrap_or("HEAD");
                let url = format!("{api}/projects/{pid}/repository/files/{path}/raw?ref={}", pct(ref_, false));
                ok(self.raw_fetch(&token, &url, &raw_path).await?)
            }

            // ── Org / collaboration ─────────────────────────────────────────────
            OrgMembers => {
                let owner = self.owner(p);
                let group = pct(&owner, false);
                let url = format!("{api}/groups/{group}/members/all?per_page=100");
                ok(self.call_paginated(&token, &url).await?)
            }
            OrgPermissions => {
                let pid = self.project_ref(p)?;
                // Accept EITHER a GitLab-native numeric `user_id` or a
                // GitHub-style `username` (resolved to an id here), so the shared
                // surface's username-keyed permission check works against GitLab.
                let user_id = match Self::req_num(p, "user_id") {
                    Ok(id) => id,
                    Err(_) => {
                        let username = Self::req_str(p, "username").map_err(|_| {
                            ForgeError::InvalidRequest("'user_id' or 'username' is required".into())
                        })?;
                        self.resolve_user_id(&token, &username).await?
                    }
                };
                let url = format!("{api}/projects/{pid}/members/all/{user_id}");
                ok(self.call(&token, Method::GET, &url, None).await?)
            }

            // Advertised as Unsupported in the capability map, so `dispatch`
            // rejects these before reaching here; the arms keep the match total.
            RefsList | RefsGet | RefsCreate | RefsDelete | OrgTeams => {
                Err(ForgeError::NotImplemented { provider: provider_id.clone(), endpoint: endpoint.as_str() })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{ForgeProvider, SupportLevel};
    use httpmock::prelude::*;
    use serial_test::serial;

    /// Build a `gitlab_ce`-variant adapter pointed at a test base URL, with a
    /// fixed token and no env dependence.
    fn test_adapter(base: &str) -> GitLabAdapter {
        test_adapter_variant(base, GitLabVariant::Ce)
    }

    fn test_adapter_variant(base: &str, variant: GitLabVariant) -> GitLabAdapter {
        let mut identities = HashMap::new();
        identities.insert("moose".to_string(), "testtoken".to_string());
        let allowlist = Arc::new(build_allowlist(base, variant));
        let redirect_policy = build_redirect_policy(allowlist.clone());
        GitLabAdapter {
            http: reqwest::Client::builder().redirect(redirect_policy).build().unwrap(),
            variant,
            api_base: base.to_string(),
            default_group: "moosenet".to_string(),
            default_identity: "moose".to_string(),
            identities: Arc::new(identities),
            fallback_token: None,
            allowlist,
            caps: Arc::new(gitlab_capabilities()),
        }
    }

    fn req(params: Value) -> ForgeRequest {
        ForgeRequest::new(params)
    }

    // ── variant / pool ────────────────────────────────────────────────────────

    #[test]
    fn ce_and_saas_share_logic_but_differ_in_id_and_pool() {
        let ce = test_adapter_variant("https://gitlab.internal.example/api/v4", GitLabVariant::Ce); // pii-test-fixture
        let saas = test_adapter_variant("https://gitlab.com/api/v4", GitLabVariant::Saas);
        assert_eq!(ce.id(), "gitlab_ce");
        assert_eq!(ce.display_name(), "GitLab CE");
        assert!(!ce.is_public_pool());
        assert_eq!(saas.id(), "gitlab_saas");
        assert_eq!(saas.display_name(), "GitLab SaaS");
        assert!(saas.is_public_pool());
        // Same capability map shape for both (one client, parameterized).
        assert_eq!(ce.capability_report(), saas.capability_report());
    }

    #[test]
    #[serial]
    fn from_env_ce_uses_gitlab_url_saas_uses_default() {
        let url = std::env::var("GITLAB_URL").ok();
        let api = std::env::var("GITLAB_API_BASE").ok();
        std::env::remove_var("GITLAB_API_BASE");
        std::env::set_var("GITLAB_URL", "https://gitlab.example.internal"); // pii-test-fixture
        let ce = GitLabAdapter::from_env_ce().unwrap();
        assert_eq!(ce.api_base, "https://gitlab.example.internal/api/v4"); // pii-test-fixture
        std::env::remove_var("GITLAB_URL");
        let saas = GitLabAdapter::from_env_saas().unwrap();
        assert_eq!(saas.api_base, GITLAB_SAAS_API);
        match url { Some(v) => std::env::set_var("GITLAB_URL", v), None => std::env::remove_var("GITLAB_URL") }
        match api { Some(v) => std::env::set_var("GITLAB_API_BASE", v), None => std::env::remove_var("GITLAB_API_BASE") }
    }

    #[test]
    #[serial]
    fn from_env_ce_fails_closed_without_base_url() {
        // A self-hosted CE adapter must NEVER silently default to the public
        // gitlab.com API — that would send a CE-scoped credential to the wrong
        // host. Missing both GITLAB_URL and GITLAB_API_BASE is a hard error.
        let url = std::env::var("GITLAB_URL").ok();
        let api = std::env::var("GITLAB_API_BASE").ok();
        std::env::remove_var("GITLAB_URL");
        std::env::remove_var("GITLAB_API_BASE");
        let err = GitLabAdapter::from_env_ce().expect_err("CE without a base URL must fail closed");
        assert!(matches!(err, ForgeError::InvalidRequest(_)), "got {err:?}");
        match url { Some(v) => std::env::set_var("GITLAB_URL", v), None => std::env::remove_var("GITLAB_URL") }
        match api { Some(v) => std::env::set_var("GITLAB_API_BASE", v), None => std::env::remove_var("GITLAB_API_BASE") }
    }

    // ── capability map ────────────────────────────────────────────────────────

    #[test]
    fn capability_map_marks_honest_gaps_unsupported() {
        let a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        for ep in [
            ForgeEndpoint::ReposList, ForgeEndpoint::ReposCreate, ForgeEndpoint::PullRequestsCreate,
            ForgeEndpoint::IssuesCreate, ForgeEndpoint::ReleasesCreate, ForgeEndpoint::WebhooksCreate,
            ForgeEndpoint::ContentWriteFile, ForgeEndpoint::OrgMembers,
            // Gaps GitHub has that GitLab does NOT: mirror config + packages publish
            // are genuinely supported here.
            ForgeEndpoint::ReposMirrorConfig, ForgeEndpoint::PackagesPublish,
        ] {
            assert_eq!(a.support_level(ep), SupportLevel::Supported, "{ep:?} should be supported");
        }
        // Honest gaps: no generic ref namespace, no team resource.
        for ep in [ForgeEndpoint::RefsList, ForgeEndpoint::RefsGet, ForgeEndpoint::RefsCreate,
                   ForgeEndpoint::RefsDelete, ForgeEndpoint::OrgTeams] {
            assert_eq!(a.support_level(ep), SupportLevel::Unsupported, "{ep:?} should be unsupported");
        }
        let report = a.capability_report();
        assert_eq!(report["branches"]["refs_list"], "unsupported");
        assert_eq!(report["org"]["org_teams"], "unsupported");
        assert_eq!(report["repos"]["repos_mirror_config"], "supported");
    }

    #[tokio::test]
    async fn dispatch_rejects_unsupported_endpoint_cleanly() {
        let a = test_adapter("http://127.0.0.1:1");
        let err = a
            .dispatch(ForgeEndpoint::RefsList, req(json!({ "repo": "r" })))
            .await
            .expect_err("refs_list is unsupported");
        match err {
            ForgeError::Unsupported { provider, endpoint } => {
                assert_eq!(provider, "gitlab_ce");
                assert_eq!(endpoint, "refs_list");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    // ── egress isolation ──────────────────────────────────────────────────────

    #[test]
    fn egress_allowlist_permits_base_blocks_others() {
        let a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        assert!(a.host_allowed("https://gitlab.example.internal/api/v4/projects/x")); // pii-test-fixture
        assert!(!a.host_allowed("https://evil.example.com/steal"));
        assert!(!a.host_allowed("http://169.254.169.254/latest/meta-data")); // pii-test-fixture
    }

    #[test]
    fn saas_allowlist_includes_gitlab_com_family() {
        let a = test_adapter_variant("https://gitlab.com/api/v4", GitLabVariant::Saas);
        assert!(a.host_allowed("https://gitlab.com/api/v4/projects/x"));
    }

    #[test]
    fn ce_allowlist_does_not_include_gitlab_com() {
        // Self-hosted CE must not implicitly trust the public SaaS host.
        let a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        assert!(!a.host_allowed("https://gitlab.com/api/v4/projects/x"));
    }

    #[tokio::test]
    async fn redirect_to_non_allowlisted_host_is_not_followed() {
        let server = MockServer::start();
        let redirect = server.mock(|when, then| {
            when.method(GET).path_contains("/projects/");
            then.status(302).header("location", "https://evil.example.com/stolen"); // pii-test-fixture
        });
        let a = test_adapter(&server.base_url());
        let err = a
            .dispatch(ForgeEndpoint::ReposGet, req(json!({ "repo": "r" })))
            .await
            .expect_err("cross-host redirect must not be followed");
        assert!(matches!(err, ForgeError::Transport { .. }), "got {err:?}");
        redirect.assert();
    }

    #[tokio::test]
    async fn call_refuses_non_allowlisted_host_without_dialing() {
        let a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
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
        let a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        assert_eq!(a.resolve_token(Some("moose")).unwrap(), "testtoken");
        assert_eq!(a.resolve_token(Some("MOOSE")).unwrap(), "testtoken");
    }

    #[test]
    fn resolve_token_unknown_identity_is_auth_error() {
        let a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        let err = a.resolve_token(Some("nobody")).expect_err("unknown identity");
        assert!(matches!(err, ForgeError::Auth { .. }));
    }

    #[test]
    fn resolve_token_falls_back_to_gitlab_token() {
        let mut a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        a.identities = Arc::new(HashMap::new());
        a.fallback_token = Some("fallbacktok".to_string());
        assert_eq!(a.resolve_token(None).unwrap(), "fallbacktok");
    }

    #[test]
    fn resolve_token_no_credential_is_auth_error() {
        let mut a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        a.identities = Arc::new(HashMap::new());
        a.fallback_token = None;
        let err = a.resolve_token(None).expect_err("no credential");
        assert!(matches!(err, ForgeError::Auth { .. }));
    }

    #[test]
    #[serial]
    fn from_env_trims_token_and_scans_pat_identities() {
        let tok = std::env::var("GITLAB_TOKEN").ok();
        let pat = std::env::var("GITLAB_PAT_HARMONY").ok();
        std::env::set_var("GITLAB_TOKEN", "  padded-token\n");
        std::env::set_var("GITLAB_PAT_HARMONY", " harmony-pat ");
        let a = GitLabAdapter::from_env_saas().unwrap();
        assert_eq!(a.fallback_token.as_deref(), Some("padded-token"));
        assert_eq!(a.resolve_token(Some("harmony")).unwrap(), "harmony-pat");
        assert!(a.identity_names().contains(&"harmony".to_string()));
        match tok { Some(v) => std::env::set_var("GITLAB_TOKEN", v), None => std::env::remove_var("GITLAB_TOKEN") }
        match pat { Some(v) => std::env::set_var("GITLAB_PAT_HARMONY", v), None => std::env::remove_var("GITLAB_PAT_HARMONY") }
    }

    // ── negative auth/scope test ─────────────────────────────────────────────

    #[tokio::test]
    async fn auth_failure_maps_403_to_forge_auth_error() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path_contains("/projects/");
            then.status(403).body("{\"message\":\"403 Forbidden\"}");
        });
        let a = test_adapter(&server.base_url());
        let err = a
            .dispatch(ForgeEndpoint::ReposGet, req(json!({ "repo": "r" })))
            .await
            .expect_err("403 must map to Auth");
        assert!(matches!(err, ForgeError::Auth { .. }), "got {err:?}");
        m.assert();
    }

    #[tokio::test]
    async fn unauthorized_401_maps_to_auth_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path_contains("/projects/");
            then.status(401);
        });
        let a = test_adapter(&server.base_url());
        let err = a
            .dispatch(ForgeEndpoint::ReposGet, req(json!({ "repo": "r" })))
            .await
            .expect_err("401 must map to Auth");
        assert!(matches!(err, ForgeError::Auth { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn server_error_maps_to_transport() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path_contains("/projects/");
            then.status(500);
        });
        let a = test_adapter(&server.base_url());
        let err = a
            .dispatch(ForgeEndpoint::ReposGet, req(json!({ "repo": "r" })))
            .await
            .expect_err("500 must map to Transport");
        assert!(matches!(err, ForgeError::Transport { .. }), "got {err:?}");
    }

    // ── MR ↔ PR / project ↔ repo terminology mapping ─────────────────────────

    #[tokio::test]
    async fn project_ref_encodes_owner_slash_repo() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/projects/moosenet%2Fdemo"); // pii-test-fixture
            then.status(200).json_body(json!({ "id": 1, "path_with_namespace": "moosenet/demo" }));
        });
        let a = test_adapter(&server.base_url());
        let resp = a.dispatch(ForgeEndpoint::ReposGet, req(json!({ "repo": "demo" }))).await.unwrap();
        assert_eq!(resp.body["path_with_namespace"], "moosenet/demo");
        m.assert();
    }

    #[tokio::test]
    async fn pull_requests_create_hits_merge_requests_endpoint() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/projects/moosenet%2Fdemo/merge_requests") // pii-test-fixture
                .json_body(json!({
                    "title": "add x",
                    "source_branch": "feature",
                    "target_branch": "main",
                    "description": "",
                }));
            then.status(201).json_body(json!({ "iid": 7, "title": "add x" }));
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(
                ForgeEndpoint::PullRequestsCreate,
                req(json!({ "repo": "demo", "title": "add x", "head": "feature", "base": "main" })),
            )
            .await
            .unwrap();
        assert_eq!(resp.body["iid"], 7);
        m.assert();
    }

    #[tokio::test]
    async fn pull_requests_get_uses_number_as_iid() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/projects/moosenet%2Fdemo/merge_requests/42"); // pii-test-fixture
            then.status(200).json_body(json!({ "iid": 42 }));
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::PullRequestsGet, req(json!({ "repo": "demo", "number": 42 })))
            .await
            .unwrap();
        assert_eq!(resp.body["iid"], 42);
        m.assert();
    }

    #[tokio::test]
    async fn pull_requests_close_uses_state_event() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT)
                .path("/projects/moosenet%2Fdemo/merge_requests/3") // pii-test-fixture
                .json_body(json!({ "state_event": "close" }));
            then.status(200).json_body(json!({ "state": "closed" }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(ForgeEndpoint::PullRequestsClose, req(json!({ "repo": "demo", "number": 3 })))
            .await
            .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn pull_requests_list_maps_github_style_open_to_opened() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/projects/moosenet%2Fdemo/merge_requests") // pii-test-fixture
                .query_param("state", "opened");
            then.status(200).json_body(json!([]));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(ForgeEndpoint::PullRequestsList, req(json!({ "repo": "demo", "state": "open" })))
            .await
            .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn issues_list_maps_github_style_open_to_opened() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/projects/moosenet%2Fdemo/issues") // pii-test-fixture
                .query_param("state", "opened");
            then.status(200).json_body(json!([]));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(ForgeEndpoint::IssuesList, req(json!({ "repo": "demo", "state": "open" })))
            .await
            .unwrap();
        m.assert();
    }

    #[test]
    fn labels_string_normalizes_array_and_string_and_empty() {
        assert_eq!(labels_string(&json!(["a", "b", "c"])), "a,b,c");
        assert_eq!(labels_string(&json!("a,b")), "a,b");
        assert_eq!(labels_string(&json!([])), "");
        assert_eq!(labels_string(&Value::Null), "");
    }

    #[tokio::test]
    async fn issues_label_sends_comma_separated_string() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT)
                .path("/projects/moosenet%2Fdemo/issues/9") // pii-test-fixture
                .json_body(json!({ "labels": "bug,urgent" }));
            then.status(200).json_body(json!({}));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::IssuesLabel,
            req(json!({ "repo": "demo", "number": 9, "labels": ["bug", "urgent"] })),
        )
        .await
        .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn issues_label_absent_clears_to_empty_string() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT)
                .path("/projects/moosenet%2Fdemo/issues/9") // pii-test-fixture
                .json_body(json!({ "labels": "" }));
            then.status(200).json_body(json!({}));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(ForgeEndpoint::IssuesLabel, req(json!({ "repo": "demo", "number": 9 })))
            .await
            .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn pull_requests_review_approve_hits_approve_endpoint() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/projects/moosenet%2Fdemo/merge_requests/5/approve"); // pii-test-fixture
            then.status(201).json_body(json!({ "approved": true }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(ForgeEndpoint::PullRequestsReview, req(json!({ "repo": "demo", "number": 5, "event": "APPROVE" })))
            .await
            .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn pull_requests_review_request_changes_hits_unapprove_endpoint() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/projects/moosenet%2Fdemo/merge_requests/5/unapprove"); // pii-test-fixture
            then.status(201).json_body(json!({ "approved": false }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::PullRequestsReview,
            req(json!({ "repo": "demo", "number": 5, "event": "REQUEST_CHANGES" })),
        )
        .await
        .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn pull_requests_review_unmapped_event_is_invalid_request_not_a_fabricated_approval() {
        let a = test_adapter("http://127.0.0.1:1");
        let err = a
            .dispatch(
                ForgeEndpoint::PullRequestsReview,
                req(json!({ "repo": "demo", "number": 5, "event": "COMMENT" })),
            )
            .await
            .expect_err("COMMENT has no GitLab review-endpoint equivalent");
        assert!(matches!(err, ForgeError::InvalidRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn repos_create_resolves_namespace_id_from_owner() {
        let server = MockServer::start();
        let ns = server.mock(|when, then| {
            when.method(GET).path("/namespaces").query_param("search", "moosenet");
            then.status(200).json_body(json!([
                { "id": 77, "path": "moosenet", "full_path": "moosenet" }
            ]));
        });
        let create = server.mock(|when, then| {
            when.method(POST).path("/projects").json_body_partial(json!({ "namespace_id": 77 }).to_string());
            then.status(201).json_body(json!({ "id": 1, "namespace_id": 77 }));
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::ReposCreate, req(json!({ "name": "demo" })))
            .await
            .unwrap();
        assert_eq!(resp.body["namespace_id"], 77);
        ns.assert();
        create.assert();
    }

    #[tokio::test]
    async fn repos_create_explicit_namespace_id_skips_lookup() {
        let server = MockServer::start();
        // No /namespaces mock registered — a call to it would 404 and fail the
        // whole dispatch, proving the explicit id bypassed the lookup.
        let create = server.mock(|when, then| {
            when.method(POST).path("/projects").json_body_partial(json!({ "namespace_id": 42 }).to_string());
            then.status(201).json_body(json!({ "id": 1, "namespace_id": 42 }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(ForgeEndpoint::ReposCreate, req(json!({ "name": "demo", "namespace_id": 42 })))
            .await
            .unwrap();
        create.assert();
    }

    #[tokio::test]
    async fn repos_create_unknown_namespace_is_invalid_request() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/namespaces");
            then.status(200).json_body(json!([]));
        });
        let a = test_adapter(&server.base_url());
        let err = a
            .dispatch(ForgeEndpoint::ReposCreate, req(json!({ "name": "demo" })))
            .await
            .expect_err("no matching namespace");
        assert!(matches!(err, ForgeError::InvalidRequest(_)), "got {err:?}");
    }

    // ── redirect hop bound / scheme downgrade (P1 hardening) ─────────────────

    #[tokio::test]
    async fn redirect_chain_is_bounded() {
        // Every hop stays on the same allowlisted host, so only the hop-count
        // cap can stop this chain — an unbounded policy would loop until the
        // client timeout instead of failing fast.
        let server = MockServer::start();
        let base = server.base_url();
        for i in 0..10u32 {
            server.mock(|when, then| {
                when.method(GET).path(format!("/loop{i}"));
                then.status(302).header("location", format!("{base}/loop{}", i + 1));
            });
        }
        let a = test_adapter(&base);
        let err = a
            .call("t", reqwest::Method::GET, &format!("{base}/loop0"), None)
            .await
            .expect_err("unbounded redirect loop must be capped");
        assert!(matches!(err, ForgeError::Transport { .. }), "got {err:?}");
    }

    // ── pagination ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_endpoint_follows_link_pagination() {
        let server = MockServer::start();
        let base = server.base_url();
        let page1 = server.mock(|when, then| {
            when.method(GET).path("/projects/moosenet%2Fdemo/repository/branches") // pii-test-fixture
                .query_param("per_page", "100");
            then.status(200)
                .header("Link", format!("<{base}/projects/moosenet%2Fdemo/repository/branches?page=2>; rel=\"next\""))
                .json_body(json!([{ "name": "a" }]));
        });
        let page2 = server.mock(|when, then| {
            when.method(GET).path("/projects/moosenet%2Fdemo/repository/branches") // pii-test-fixture
                .query_param("page", "2");
            then.status(200).json_body(json!([{ "name": "b" }]));
        });
        let a = test_adapter(&base);
        let resp = a
            .dispatch(ForgeEndpoint::BranchesList, req(json!({ "repo": "demo" })))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "a");
        assert_eq!(arr[1]["name"], "b");
        page1.assert();
        page2.assert();
    }

    #[test]
    fn parse_next_link_extracts_next_only() {
        let link = "<https://x/y?page=2>; rel=\"next\", <https://x/y?page=9>; rel=\"last\"";
        assert_eq!(parse_next_link(link).as_deref(), Some("https://x/y?page=2"));
        assert_eq!(parse_next_link("<https://x/y?page=9>; rel=\"last\""), None);
    }

    // ── binary-safe raw fetch ─────────────────────────────────────────────────

    #[tokio::test]
    async fn raw_fetch_wraps_text() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/projects/moosenet%2Fdemo/repository/files/docs%2Fa.md/raw"); // pii-test-fixture
            then.status(200).body("hello world");
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::ContentRawFetch, req(json!({ "repo": "demo", "path": "docs/a.md" })))
            .await
            .unwrap();
        assert_eq!(resp.body["encoding"], "utf-8");
        assert_eq!(resp.body["raw"], "hello world");
        assert_eq!(resp.body["path"], "docs/a.md");
        m.assert();
    }

    #[tokio::test]
    async fn raw_fetch_binary_content_is_base64_lossless() {
        let server = MockServer::start();
        let bin: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01, 0x02, 0x9C];
        let m = server.mock(|when, then| {
            when.method(GET).path("/projects/moosenet%2Fdemo/repository/files/img.bin/raw");
            then.status(200).body(bin.clone());
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::ContentRawFetch, req(json!({ "repo": "demo", "path": "img.bin" })))
            .await
            .unwrap();
        assert_eq!(resp.body["encoding"], "base64");
        let decoded = B64.decode(resp.body["raw_base64"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, bin);
        m.assert();
    }

    #[tokio::test]
    async fn content_write_base64_encodes_utf8_content_no_sha_defaults_to_create_post() {
        // No `sha`/`last_commit_id`/`create` -> inferred as a NEW file (POST),
        // so a generic caller can create a file without an extra flag.
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/projects/moosenet%2Fdemo/repository/files/a.md") // pii-test-fixture
                .json_body(json!({
                    "branch": "main",
                    "content": B64.encode("hello"),
                    "encoding": "base64",
                    "commit_message": "create",
                }));
            then.status(201).json_body(json!({ "file_path": "a.md" }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::ContentWriteFile,
            req(json!({ "repo": "demo", "path": "a.md", "content": "hello", "message": "create", "branch": "main" })),
        )
        .await
        .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn content_write_with_last_commit_id_infers_update_put() {
        // Presence of a prior-blob reference -> inferred as an UPDATE (PUT), and
        // the reference is forwarded to GitLab for optimistic concurrency.
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT)
                .path("/projects/moosenet%2Fdemo/repository/files/a.md") // pii-test-fixture
                .json_body(json!({
                    "branch": "main",
                    "content": B64.encode("hello"),
                    "encoding": "base64",
                    "commit_message": "update",
                    "last_commit_id": "abc123",
                }));
            then.status(200).json_body(json!({ "file_path": "a.md" }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::ContentWriteFile,
            req(json!({ "repo": "demo", "path": "a.md", "content": "hello", "message": "update", "branch": "main", "last_commit_id": "abc123" })),
        )
        .await
        .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn content_write_explicit_create_false_forces_put() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT).path("/projects/moosenet%2Fdemo/repository/files/a.md"); // pii-test-fixture
            then.status(200).json_body(json!({ "file_path": "a.md" }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::ContentWriteFile,
            req(json!({ "repo": "demo", "path": "a.md", "content": "x", "message": "m", "create": false })),
        )
        .await
        .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn content_write_requires_some_content() {
        let a = test_adapter("http://127.0.0.1:1");
        let err = a
            .dispatch(
                ForgeEndpoint::ContentWriteFile,
                req(json!({ "repo": "demo", "path": "a.md", "message": "m" })),
            )
            .await
            .expect_err("missing content");
        assert!(matches!(err, ForgeError::InvalidRequest(_)));
    }

    // ── generic packages publish (a real REST PUT, unlike GitHub) ────────────

    #[tokio::test]
    async fn packages_publish_puts_raw_bytes() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT)
                .path("/projects/moosenet%2Fdemo/packages/generic/demo/1.0.0/demo.tar.gz") // pii-test-fixture
                .body("payload-bytes");
            then.status(201).json_body(json!({ "message": "201 Created" }));
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(
                ForgeEndpoint::PackagesPublish,
                req(json!({
                    "repo": "demo",
                    "package_name": "demo",
                    "package_version": "1.0.0",
                    "file_name": "demo.tar.gz",
                    "content_base64": B64.encode("payload-bytes"),
                })),
            )
            .await
            .unwrap();
        assert_eq!(resp.body["message"], "201 Created");
        m.assert();
    }

    // ── missing required param ────────────────────────────────────────────────

    #[tokio::test]
    async fn missing_required_param_is_invalid_request() {
        let a = test_adapter("http://127.0.0.1:1");
        let err = a
            .dispatch(ForgeEndpoint::PullRequestsCreate, req(json!({ "repo": "demo" })))
            .await
            .expect_err("missing title/head/base");
        assert!(matches!(err, ForgeError::InvalidRequest(_)));
    }

    #[test]
    fn pct_encodes_reserved_keeps_slash_optionally() {
        assert_eq!(pct("a b", false), "a%20b");
        assert_eq!(pct("moosenet/demo", false), "moosenet%2Fdemo");
        assert_eq!(pct("docs/a.md", true), "docs/a.md");
        assert_eq!(pct("docs/a b.md", true), "docs/a%20b.md");
    }

    #[test]
    fn debug_impl_redacts_credentials() {
        let a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("testtoken"));
        assert!(dbg.contains("redacted"));
    }

    // ── secondary-review hardening (agy P1/P2) ───────────────────────────────

    #[test]
    fn host_of_strips_default_ports() {
        // https:443 and http:80 normalize to bare host (matches url crate).
        assert_eq!(host_of("https://gitlab.example.com:443/api/v4/x").as_deref(), Some("gitlab.example.com")); // pii-test-fixture
        assert_eq!(host_of("http://gitlab.example.com:80/x").as_deref(), Some("gitlab.example.com")); // pii-test-fixture
        // A non-default explicit port is preserved.
        assert_eq!(host_of("https://gitlab.example.com:8443/x").as_deref(), Some("gitlab.example.com:8443")); // pii-test-fixture
    }

    #[test]
    fn allowlist_with_explicit_443_matches_bare_host() {
        // A base configured with an explicit :443 must still allow the bare-host
        // form (and vice versa) — no false egress block from default-port drift.
        let a = test_adapter("https://gitlab.example.internal:443/api/v4"); // pii-test-fixture
        assert!(a.host_allowed("https://gitlab.example.internal/api/v4/projects/x")); // pii-test-fixture
        assert!(a.host_allowed("https://gitlab.example.internal:443/api/v4/projects/x")); // pii-test-fixture
    }

    #[tokio::test]
    async fn cross_origin_redirect_to_allowlisted_host_is_not_followed() {
        // SaaS allowlists gitlab.com in addition to the mock base. A redirect
        // from the base to gitlab.com is CROSS-ORIGIN and must NOT be followed —
        // otherwise the PRIVATE-TOKEN (a custom header reqwest does not strip)
        // would be sent to a different host. It surfaces as a Transport error.
        let server = MockServer::start();
        let redirect = server.mock(|when, then| {
            when.method(GET).path_contains("/projects/");
            then.status(302).header("location", "https://gitlab.com/api/v4/projects/stolen");
        });
        let a = test_adapter_variant(&server.base_url(), GitLabVariant::Saas);
        let err = a
            .dispatch(ForgeEndpoint::ReposGet, req(json!({ "repo": "r" })))
            .await
            .expect_err("cross-origin redirect (even to an allowlisted host) must not be followed");
        assert!(matches!(err, ForgeError::Transport { .. }), "got {err:?}");
        redirect.assert();
    }

    #[test]
    fn translate_webhook_maps_github_shape_to_gitlab_flat() {
        let gh = json!({
            "config": { "url": "https://hook.example/x", "secret": "s3cr3t" }, // pii-test-fixture
            "events": ["push", "pull_request", "issues"],
            "active": true
        });
        let gl = translate_webhook(&gh);
        assert_eq!(gl["url"], "https://hook.example/x"); // pii-test-fixture
        assert_eq!(gl["token"], "s3cr3t");
        assert_eq!(gl["push_events"], true);
        assert_eq!(gl["merge_requests_events"], true);
        assert_eq!(gl["issues_events"], true);
        // GitHub `active` must NOT be mapped (never onto enable_ssl_verification).
        assert!(gl.get("enable_ssl_verification").is_none());
    }

    #[test]
    fn translate_webhook_passes_gitlab_native_verbatim() {
        let gl_native = json!({ "url": "https://hook.example/y", "push_events": true }); // pii-test-fixture
        assert_eq!(translate_webhook(&gl_native), gl_native);
    }

    #[tokio::test]
    async fn webhooks_create_translates_github_hook() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/projects/moosenet%2Fdemo/hooks") // pii-test-fixture
                .json_body_partial(json!({ "url": "https://hook.example/x", "push_events": true }).to_string()); // pii-test-fixture
            then.status(201).json_body(json!({ "id": 1 }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::WebhooksCreate,
            req(json!({ "repo": "demo", "hook": { "config": { "url": "https://hook.example/x" }, "events": ["push"] } })), // pii-test-fixture
        )
        .await
        .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn issues_create_resolves_assignee_usernames_to_ids() {
        let server = MockServer::start();
        let users = server.mock(|when, then| {
            when.method(GET).path("/users").query_param("username", "alice");
            then.status(200).json_body(json!([{ "id": 501, "username": "alice" }]));
        });
        let create = server.mock(|when, then| {
            when.method(POST)
                .path("/projects/moosenet%2Fdemo/issues") // pii-test-fixture
                .json_body_partial(json!({ "assignee_ids": [501] }).to_string());
            then.status(201).json_body(json!({ "iid": 1 }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::IssuesCreate,
            req(json!({ "repo": "demo", "title": "t", "assignees": ["alice"] })),
        )
        .await
        .unwrap();
        users.assert();
        create.assert();
    }

    #[tokio::test]
    async fn issues_create_accepts_native_assignee_ids_without_lookup() {
        let server = MockServer::start();
        // No /users mock — a lookup would 404 and fail the dispatch.
        let create = server.mock(|when, then| {
            when.method(POST)
                .path("/projects/moosenet%2Fdemo/issues") // pii-test-fixture
                .json_body_partial(json!({ "assignee_ids": [7, 8] }).to_string());
            then.status(201).json_body(json!({ "iid": 1 }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::IssuesCreate,
            req(json!({ "repo": "demo", "title": "t", "assignee_ids": [7, 8] })),
        )
        .await
        .unwrap();
        create.assert();
    }

    #[tokio::test]
    async fn org_permissions_resolves_username() {
        let server = MockServer::start();
        let users = server.mock(|when, then| {
            when.method(GET).path("/users").query_param("username", "bob");
            then.status(200).json_body(json!([{ "id": 99, "username": "bob" }]));
        });
        let perm = server.mock(|when, then| {
            when.method(GET).path("/projects/moosenet%2Fdemo/members/all/99"); // pii-test-fixture
            then.status(200).json_body(json!({ "access_level": 40 }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(ForgeEndpoint::OrgPermissions, req(json!({ "repo": "demo", "username": "bob" })))
            .await
            .unwrap();
        users.assert();
        perm.assert();
    }

    #[tokio::test]
    async fn repos_list_falls_back_to_user_namespace() {
        let server = MockServer::start();
        let group = server.mock(|when, then| {
            when.method(GET).path("/groups/someuser/projects");
            then.status(404).body("{\"message\":\"404 Group Not Found\"}");
        });
        let user = server.mock(|when, then| {
            when.method(GET).path("/users/someuser/projects");
            then.status(200).json_body(json!([{ "id": 1, "path": "p" }]));
        });
        let a = test_adapter(&server.base_url());
        let resp = a
            .dispatch(ForgeEndpoint::ReposList, req(json!({ "owner": "someuser" })))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
        group.assert();
        user.assert();
    }

    #[test]
    fn normalize_issue_updates_maps_github_fields() {
        let out = normalize_issue_updates(&json!({ "body": "b", "state": "closed", "labels": ["x", "y"] }));
        assert_eq!(out["description"], "b");
        assert!(out.get("body").is_none());
        assert_eq!(out["state_event"], "close");
        assert!(out.get("state").is_none());
        assert_eq!(out["labels"], "x,y");
        // open -> reopen
        let reopen = normalize_issue_updates(&json!({ "state": "open" }));
        assert_eq!(reopen["state_event"], "reopen");
        // GitLab-native fields untouched
        let native = normalize_issue_updates(&json!({ "description": "d", "state_event": "close" }));
        assert_eq!(native["description"], "d");
        assert_eq!(native["state_event"], "close");
    }

    #[tokio::test]
    async fn issues_update_translates_body_and_state() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT)
                .path("/projects/moosenet%2Fdemo/issues/4") // pii-test-fixture
                .json_body(json!({ "description": "new text", "state_event": "close" }));
            then.status(200).json_body(json!({ "iid": 4 }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::IssuesUpdate,
            req(json!({ "repo": "demo", "number": 4, "updates": { "body": "new text", "state": "closed" } })),
        )
        .await
        .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn paginated_next_to_different_origin_is_blocked() {
        // A server-supplied `Link: next` pointing at a DIFFERENT host must be
        // refused — otherwise the PRIVATE-TOKEN would be sent there. Surfaces as
        // a Transport egress-blocked error.
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/projects/moosenet%2Fdemo/repository/branches"); // pii-test-fixture
            then.status(200)
                .header("Link", "<https://gitlab.com/api/v4/projects/1/repository/branches?page=2>; rel=\"next\"")
                .json_body(json!([{ "name": "a" }]));
        });
        let a = test_adapter_variant(&server.base_url(), GitLabVariant::Saas);
        let err = a
            .dispatch(ForgeEndpoint::BranchesList, req(json!({ "repo": "demo" })))
            .await
            .expect_err("cross-origin pagination must be blocked");
        assert!(matches!(err, ForgeError::Transport { .. }), "got {err:?}");
        m.assert();
    }

    #[test]
    fn origin_of_includes_scheme_and_normalizes_port() {
        assert_eq!(origin_of("https://h.example/x").as_deref(), Some("https://h.example")); // pii-test-fixture
        assert_eq!(origin_of("https://h.example:443/x").as_deref(), Some("https://h.example")); // pii-test-fixture
        // Same host, different scheme -> different origin (downgrade guard).
        assert_ne!(origin_of("https://h.example/x"), origin_of("http://h.example/x")); // pii-test-fixture
    }

    #[test]
    fn host_of_agrees_with_reqwest_on_adversarial_urls() {
        // A hand-rolled parser could read the host as `gitlab.example` while
        // reqwest dials `evil.example` (backslash treated as a path separator).
        // Because host_of parses with reqwest::Url, the egress decision matches
        // what actually gets dialed — no SSRF/credential-leak differential.
        let tricky = "https://evil.example\\@gitlab.example/api/v4/projects"; // pii-test-fixture
        let via_host_of = host_of(tricky);
        let via_reqwest = reqwest::Url::parse(tricky).ok().and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{}:{}", h.to_lowercase(), p),
                None => h.to_lowercase(),
            })
        });
        assert_eq!(via_host_of, via_reqwest);
        // And such a host is NOT on a normal allowlist -> refused.
        let a = test_adapter("https://gitlab.example.internal/api/v4"); // pii-test-fixture
        assert!(!a.host_allowed(tricky));
    }

    #[tokio::test]
    async fn paginated_next_https_to_http_same_host_is_blocked() {
        // The mock server is http; simulate the inverse guard by pinning an
        // https origin and asserting an http same-host `next` is rejected via
        // origin_of (scheme-inclusive). We test the helper-backed decision
        // directly to avoid needing a live TLS endpoint.
        assert_ne!(origin_of("https://gitlab.internal/api"), origin_of("http://gitlab.internal/api")); // pii-test-fixture
    }

    #[tokio::test]
    async fn pull_requests_merge_combines_title_and_message() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT)
                .path("/projects/moosenet%2Fdemo/merge_requests/2/merge") // pii-test-fixture
                .json_body_partial(json!({ "merge_commit_message": "T\n\nBody" }).to_string());
            then.status(200).json_body(json!({ "state": "merged" }));
        });
        let a = test_adapter(&server.base_url());
        a.dispatch(
            ForgeEndpoint::PullRequestsMerge,
            req(json!({ "repo": "demo", "number": 2, "commit_title": "T", "commit_message": "Body" })),
        )
        .await
        .unwrap();
        m.assert();
    }
}
