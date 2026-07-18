//! Gitea tools: 15 RustTool implementations for the Gitea source-control API.
//!
//! All tools use `reqwest` for typed HTTP calls. Write operations include a PII
//! gate that scans content for private IP ranges and API-key patterns before
//! submitting to Gitea — this was MISSING from the Python gitea_tools.py.
//!
//! ## Configuration (env vars)
//! - `GITEA_URL`   — base URL, e.g. `https://gitea.example.com` (required)
//! - `GITEA_PAT_<NAME>` — a **named identity** personal access token (e.g.
//!   `GITEA_PAT_MOOSE`, `GITEA_PAT_HARMONY`, `GITEA_PAT_LUMINA`). This is the
//!   multi-identity model that replaced the single unsuffixed `GITEA_TOKEN`
//!   (mirrors the Plane `PLANE_PAT_<NAME>` convention). **BREAKING:** the tool
//!   no longer reads an unsuffixed `GITEA_TOKEN` — the effective token is the
//!   resolved identity's `GITEA_PAT_<NAME>`.
//! - `GITEA_IDENTITY_NAME` — which named identity is the active default when a
//!   call passes no `identity` argument (default `"moose"` — Gitea is the
//!   operator's infra git storage; NOTE this differs from Plane's `lumina`).
//! - `GITEA_OWNER` — default repo owner/organisation (default: `"moosenet"`)

pub mod types;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

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
/// - Bare API key patterns: long hex strings (≥32 chars) or `sk-...` tokens  // pii-test-fixture
pub(crate) fn pii_check(content: &str) -> Option<String> {
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

    // API key patterns: `sk-` prefixed tokens (OpenAI-style)  // pii-test-fixture
    if content.contains("sk-") {  // pii-test-fixture
        let sk_idx = content.find("sk-").unwrap();  // pii-test-fixture
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

/// Env-var prefix that marks a per-agent named-identity token. A variable
/// `GITEA_PAT_<NAME>` registers the identity `<name>` (lowercased). This is the
/// single source of truth for the prefix — the `from_env` scan and the
/// `gitea_list_identities` tool both derive from it, so the two can never drift.
/// Mirrors Plane's `PLANE_IDENTITY_PREFIX` exactly.
const GITEA_IDENTITY_PREFIX: &str = "GITEA_PAT_";

/// The active-default identity used when neither `GITEA_IDENTITY_NAME` nor a
/// per-call `identity` argument selects one. Per the S105/GPAT consolidation the
/// operator persona `moose` is the default — Gitea is the operator's infra git
/// storage, so a no-`identity` call acts as `GITEA_PAT_MOOSE`. This deliberately
/// DIFFERS from Plane (whose default is `lumina`).
const DEFAULT_GITEA_IDENTITY: &str = "moose";

/// Scan this process's own environment for `GITEA_PAT_<NAME>` named-identity
/// tokens, returning a `lowercased-name -> token` map. This is the ONLY place
/// the prefix is matched against the environment. Empty-valued vars are skipped
/// (a set-but-empty secret is treated as absent), and names are lowercased so a
/// later duplicate differing only by case collapses onto the same entry —
/// matching how [`GiteaClient::for_identity`] lowercases on lookup. Never reads
/// another process's files. Mirrors Plane's `scan_named_identities`.
///
/// Token values are `.trim()`-ed before storage: a stored PAT that arrives with
/// a trailing newline or surrounding whitespace (a common shape when a secret is
/// materialised from a file or `echo`-ed into the runtime store) would otherwise
/// be interpolated verbatim into the `Authorization: token <PAT>` header and make
/// Gitea reject every request as unauthenticated. Trimming here means a stray
/// newline in the credential value can never break the auth header again
/// (this bit us on `GITEA_PAT_MOOSE`). A value that is only whitespace trims to
/// empty and is treated as absent, exactly like an unset var.
///
/// Trimming alone only strips LEADING/TRAILING whitespace — a token with
/// INTERIOR whitespace or control characters (e.g. a PAT accidentally
/// materialised with an embedded newline or space in the middle, such as
/// `"abc\ndef"`) trims to itself and would slip through unchanged, either
/// corrupting the `Authorization` header value or reqwest's `HeaderValue`
/// parser rejecting it with an opaque "builder error" far from the actual
/// cause (codex P1). Every token is validated with
/// [`reject_interior_whitespace`] after trimming; a bad `GITEA_PAT_<NAME>`
/// fails loudly here, at scan time, with the offending identity named in the
/// error, rather than surfacing as a confusing HTTP client build failure
/// later.
fn scan_gitea_identities() -> Result<HashMap<String, String>, ToolError> {
    let mut identities: HashMap<String, String> = HashMap::new();
    for (k, v) in env::vars() {
        if let Some(name) = k.strip_prefix(GITEA_IDENTITY_PREFIX) {
            let token = v.trim();
            if !token.is_empty() {
                reject_interior_whitespace(token).map_err(|e| {
                    ToolError::InvalidArgument(format!(
                        "{GITEA_IDENTITY_PREFIX}{name}: {e}"
                    ))
                })?;
                identities.insert(name.to_lowercase(), token.to_string());
            }
        }
    }
    Ok(identities)
}

/// Reject a (already-trimmed) token that still contains interior whitespace
/// or ASCII control characters. A well-formed PAT is a single contiguous run
/// of visible characters; leading/trailing whitespace is handled by `.trim()`
/// at the call site, but an INTERIOR space, tab, or newline (e.g. a secret
/// materialised as two lines glued together, or a copy-paste that captured a
/// stray space mid-token) is not something trimming can catch. Sending such a
/// value verbatim in an `Authorization: token <PAT>` header either builds a
/// header reqwest's `HeaderValue` parser rejects outright (control bytes) or
/// — worse — a header that parses fine but can never match the real
/// credential (an interior space), silently masquerading as "just" a 401.
/// Reject it explicitly instead, with a message that names the actual
/// problem instead of leaking the token value.
fn reject_interior_whitespace(token: &str) -> Result<(), ToolError> {
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ToolError::InvalidArgument(
            "token contains interior whitespace or control characters after trimming — \
             refusing to build an auth header from it (check the secret for an embedded \
             newline/space/tab)"
                .to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub struct GiteaClient {
    http: Client,
    base_url: String,
    /// Active token used for requests made directly through this client
    /// instance (the resolved active-default identity, unless
    /// [`GiteaClient::for_identity`] produced this instance).
    token: String,
    /// Human name for the active token (the active-default identity name, or the
    /// name passed to [`GiteaClient::for_identity`]).
    identity_name: Option<String>,
    /// All configured named identities: lowercased name -> token. Populated from
    /// `GITEA_PAT_<NAME>` env vars only — never from another process's files.
    identities: Arc<HashMap<String, String>>,
    owner: String,
}

/// Hand-written `Debug` impl: never prints `token` or `identities` (both hold
/// live credentials). Redacted so logs/panics/`{:?}` can never leak a token.
impl std::fmt::Debug for GiteaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GiteaClient")
            .field("base_url", &self.base_url)
            .field("token", &if self.token.is_empty() { "<empty>" } else { "<redacted>" })
            .field("identity_name", &self.identity_name)
            .field("identities", &format!("<{} configured, redacted>", self.identities.len()))
            .field("owner", &self.owner)
            .finish()
    }
}

impl GiteaClient {
    /// Build from environment variables.
    ///
    /// Returns `Err(ToolError::NotConfigured)` if `GITEA_URL` is not set.
    ///
    /// **BREAKING (S105/GPAT):** no longer reads an unsuffixed `GITEA_TOKEN`.
    /// The active-default token is the `GITEA_PAT_<NAME>` selected by
    /// `GITEA_IDENTITY_NAME` (default `moose`); a per-call `identity` argument
    /// overrides it via [`GiteaClient::resolve_identity`].
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = env::var("GITEA_URL").map_err(|_| {
            ToolError::NotConfigured("GITEA_URL environment variable is not set".to_string())
        })?;
        let owner = env::var("GITEA_OWNER").unwrap_or_else(|_| "moosenet".to_string());

        // Named identities: GITEA_PAT_<NAME> for any agent that needs its own
        // token (e.g. GITEA_PAT_MOOSE, GITEA_PAT_HARMONY, GITEA_PAT_LUMINA).
        // Read once at process start from this process's own environment.
        let identities = scan_gitea_identities()?;

        // Active-default identity name: explicit GITEA_IDENTITY_NAME, else the
        // built-in default `moose`. Lowercased to match the identities map /
        // `for_identity` lookup.
        let identity_name = env::var("GITEA_IDENTITY_NAME")
            .ok()
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_GITEA_IDENTITY.to_string());

        // Resolve the active-default TOKEN so a named default genuinely ACTS as
        // that identity. If the default name has no configured GITEA_PAT_<NAME>,
        // the token is empty and calls fail with a clear auth error from Gitea —
        // the operator must provision the identity's PAT.
        let token = identities.get(&identity_name).cloned().unwrap_or_default();

        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;

        Ok(Self {
            http,
            base_url,
            token,
            identity_name: Some(identity_name),
            identities: Arc::new(identities),
            owner,
        })
    }

    /// Build a client from an explicit base URL + single token, for a
    /// single-credential provider in the Gitea family (Forgejo `FORGEJO_TOKEN`,
    /// Codeberg `CODEBERG_TOKEN`) that does NOT use the `GITEA_PAT_<NAME>`
    /// multi-identity model. The token is `.trim()`-ed for the same reason
    /// [`scan_gitea_identities`] trims (a trailing newline in a materialised
    /// secret must never reach the auth header). `identity_name` is a display
    /// label only (e.g. the provider id); no named-identity map is populated, so
    /// [`GiteaClient::for_identity`] on such a client resolves nothing.
    ///
    /// Returns `Err(ToolError::NotConfigured)` if the trimmed token is empty.
    pub fn with_token(
        base_url: impl Into<String>,
        token: impl AsRef<str>,
        owner: impl Into<String>,
        identity_name: impl Into<String>,
    ) -> Result<Self, ToolError> {
        let token = token.as_ref().trim().to_string();
        if token.is_empty() {
            return Err(ToolError::NotConfigured(
                "Gitea-family provider token is empty".to_string(),
            ));
        }
        reject_interior_whitespace(&token)?;
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.into(),
            token,
            identity_name: Some(identity_name.into()),
            identities: Arc::new(HashMap::new()),
            owner: owner.into(),
        })
    }

    /// Return a clone of this client scoped to a named identity's token (from
    /// `GITEA_PAT_<NAME>`) instead of the active default. The HTTP client and
    /// identities map are shared (clone of `Arc`) — only the active token and
    /// its resolved name differ, so identities never leak each other's tokens.
    /// Mirrors Plane's `for_identity`.
    pub fn for_identity(&self, name: &str) -> Result<Self, ToolError> {
        let key = name.trim().to_lowercase();
        let token = self.identities.get(&key).cloned().ok_or_else(|| {
            ToolError::InvalidArgument(format!(
                "No Gitea identity named '{name}' is configured (expected {GITEA_IDENTITY_PREFIX}{})",
                key.to_uppercase()
            ))
        })?;
        Ok(Self {
            token,
            identity_name: Some(key),
            ..self.clone()
        })
    }

    /// Resolve the effective client for a single tool invocation from its raw
    /// args. This is the ONE shared dispatch point every Gitea CRUD tool uses to
    /// pick the token it authenticates with, so the selection rule lives in one
    /// place rather than at every call site.
    ///
    /// - A non-empty `identity` string argument selects that named
    ///   `GITEA_PAT_<NAME>` identity (via [`GiteaClient::for_identity`]),
    ///   returning an owned, token-scoped clone.
    /// - Otherwise the call acts as this client's **active default** identity
    ///   (returned borrowed, no clone), resolved at construction from
    ///   `GITEA_IDENTITY_NAME` (default `moose`).
    ///
    /// The `identity` argument is consumed here for token selection only — it is
    /// never placed into a request body and never logged.
    fn resolve_identity<'a>(&'a self, args: &Value) -> Result<Cow<'a, Self>, ToolError> {
        match args.get("identity").and_then(|v| v.as_str()) {
            Some(name) if !name.trim().is_empty() => Ok(Cow::Owned(self.for_identity(name)?)),
            _ => Ok(Cow::Borrowed(self)),
        }
    }

    /// The active identity's resolved name, if known.
    pub fn identity_name(&self) -> Option<&str> {
        self.identity_name.as_deref()
    }

    /// Names of all configured named identities (lowercased, sorted for stable
    /// output). These are exactly the names [`GiteaClient::for_identity`] can
    /// resolve. Never returns — and cannot be used to recover — token values.
    pub fn identity_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.identities.keys().cloned().collect();
        names.sort();
        names
    }

    fn api(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url.trim_end_matches('/'), path)
    }

    /// The raw resolved token string for this client's active identity — a
    /// crate-internal escape hatch for a caller (e.g. the mirror engine's
    /// `sync-source` git transport, S111E/MIRR-04) that needs the bare
    /// credential to hand to `git` via `GIT_ASKPASS`, rather than going
    /// through this client's own HTTP methods. Never logged by callers.
    fn raw_token(&self) -> &str {
        &self.token
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

    /// PATCH request sending JSON body, returning parsed JSON. EGJS-02: added
    /// for `gitea_close_pr` (`PATCH /repos/{owner}/{repo}/pulls/{index}`),
    /// mirroring `put`'s error mapping.
    async fn patch<B, T>(&self, path: &str, body: &B) -> Result<T, ToolError>
    where
        B: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let url = self.api(path);
        debug!("PATCH {url}");
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound("Resource not found in Gitea".to_string()));
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

    /// Fetch a file's decoded text content from the configured owner's repo.
    ///
    /// Used by tool modules (sentinel, vigil) that need to read a Gitea-hosted
    /// status/briefing file directly rather than exposing a full `RustTool`
    /// (like [`ReadFile`]) for it. Returns `Err(ToolError::NotFound(_))` when
    /// the file does not exist — callers typically treat that as "no data yet"
    /// rather than a hard failure.
    pub async fn fetch_file_text(&self, repo: &str, path: &str) -> Result<String, ToolError> {
        let endpoint = format!("/repos/{}/{}/contents/{}", self.owner, repo, path);
        let fc: GiteaFileContent = self.get(&endpoint).await?;

        let raw_content = fc.content.unwrap_or_default();
        // Gitea wraps lines with newlines in the base64 — strip them.
        let clean = raw_content.replace('\n', "").replace('\r', "");
        let decoded = B64
            .decode(&clean)
            .map_err(|e| ToolError::Http(format!("Failed to decode file content: {e}")))?;
        Ok(String::from_utf8_lossy(&decoded).to_string())
    }

    /// Resolve `owner` field: use explicit override or fall back to configured default.
    fn resolve_owner<'a>(&'a self, override_owner: Option<&'a str>) -> &'a str {
        override_owner.unwrap_or(&self.owner)
    }

    /// Open a pull request via `POST /repos/{owner}/{repo}/pulls`, reusing this
    /// client's identity resolution, PII gate, and HTTP transport. `args` is the
    /// same shape [`CreatePr`] accepts: `repo` (required), `title` (required),
    /// `head` (required), `base` (required), optional `body`, optional `owner`
    /// override, optional `identity`. Returns the typed PR.
    ///
    /// This is the single create-pull implementation shared by the `gitea_create_pr`
    /// tool and by PROMO-01's `plane_prefix_promote`, so neither hand-rolls a second
    /// HTTP client or a second PII gate.
    pub async fn create_pull(&self, args: &Value) -> Result<GiteaPullRequest, ToolError> {
        let client = self.resolve_identity(args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let title = args["title"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'title' is required".to_string()))?;
        let head = args["head"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'head' is required".to_string()))?;
        let base = args["base"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'base' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        // PII gate on the PR body if provided (same guard as the tool path).
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
        client.post(&endpoint, &body).await
    }

    // ── Accessors + generic transport for the forge adapter (GITX-02) ──────────
    //
    // The Gitea-family `ForgeProvider` adapter (`crate::forge::gitea_family`)
    // drives the SAME client — base URL, resolved `GITEA_PAT_<NAME>` token, HTTP
    // pool, and PAT auth scheme — so Gitea/Forgejo/Codeberg all authenticate
    // exactly the way the concrete `gitea_*` tools do. These accessors keep the
    // token private (only an opaque `Authorization` header is exposed) while
    // giving the adapter a single generic request path for the full shared
    // endpoint surface.

    /// The configured base URL (e.g. `https://gitea.example.com`), no trailing
    /// `/api/v1`. Used by the adapter for the Cargo publish endpoint, which lives
    /// under `/api/packages/...` rather than the `/api/v1/...` REST surface.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The configured default owner/organisation.
    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    /// The shared `reqwest` client (connection pool + timeout).
    pub(crate) fn http(&self) -> &Client {
        &self.http
    }

    /// The `Authorization` header value for the active identity's token. Kept as
    /// an opaque header string so the token itself never leaves the client.
    pub(crate) fn authorization(&self) -> String {
        self.auth_header()
    }

    /// Build a `/api/v1`-relative endpoint into a full URL.
    pub(crate) fn api_url(&self, path: &str) -> String {
        self.api(path)
    }

    /// Generic JSON request against the Gitea REST API (`/api/v1` surface),
    /// returning the parsed response body (or [`Value::Null`] for an empty
    /// success body, e.g. a `204`). Reuses the client's auth header, base URL,
    /// and HTTP pool so the adapter's dispatch is a thin mapping layer over the
    /// exact same transport the concrete `gitea_*` tools use.
    ///
    /// A `404` maps to [`ToolError::NotFound`]; any other non-2xx maps to
    /// [`ToolError::Http`] carrying the status + body. A `body` of `None` sends
    /// no request body (GET/DELETE); `Some(_)` sends it as JSON.
    pub(crate) async fn request_value(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ToolError> {
        let url = self.api(path);
        debug!("{method} {url}");
        let mut rb = self
            .http
            .request(method, &url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json");
        if let Some(b) = body {
            rb = rb.header("Content-Type", "application/json").json(b);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound("Resource not found in Gitea".to_string()));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Gitea returned {status}: {body_text}")));
        }
        let text = resp.text().await.unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| ToolError::Http(format!("JSON parse error: {e}")))
    }

    /// Fetch a `/api/v1`-relative endpoint that returns **raw file bytes** rather
    /// than JSON (e.g. the Gitea `/repos/{owner}/{repo}/raw/{path}` endpoint),
    /// returning the exact bytes with no lossy UTF-8 decode. The raw endpoint can
    /// serve arbitrary binary content, so routing it through the JSON/text
    /// [`request_value`](Self::request_value) helper would corrupt non-UTF-8
    /// bytes; callers base64-encode the result for a lossless round-trip in the
    /// JSON tool response (mirroring how [`fetch_file_text`](Self::fetch_file_text)
    /// decodes Gitea's base64 `content` — same discipline, inverse direction).
    ///
    /// A `404` maps to [`ToolError::NotFound`]; any other non-2xx maps to
    /// [`ToolError::Http`] carrying the status + body.
    pub(crate) async fn request_raw(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<Vec<u8>, ToolError> {
        let url = self.api(path);
        debug!("{method} {url} (raw bytes)");
        let resp = self
            .http
            .request(method, &url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/octet-stream")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound("Resource not found in Gitea".to_string()));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Gitea returned {status}: {body_text}")));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to read raw body: {e}")))?;
        Ok(bytes.to_vec())
    }
}

/// Resolve a raw Gitea git-transport credential — the single sanctioned read
/// point for anything outside this module that needs a bare `GITEA_PAT_<NAME>`
/// token string to hand to `git` (via `GIT_ASKPASS`), rather than going through
/// this module's own HTTP tool methods. Used by the mirror engine's
/// `sync-source` action (S111E/MIRR-04) for its `git clone`/`git fetch`
/// transport, mirroring `crate::github::github_token()`'s shape exactly.
///
/// Resolution order (delegates to [`GiteaClient::from_env`] /
/// [`GiteaClient::for_identity`], never a raw `std::env::var(GITEA_PAT_...)`
/// read outside this module):
/// 1. `identity` (if given) selects that named `GITEA_PAT_<NAME>` identity.
/// 2. Otherwise the active-default identity (`GITEA_IDENTITY_NAME`, default
///    `"moose"`).
///
/// Returns `NotConfigured` (never a panic, never a partial value) when the
/// resolved identity has no token, and the caller must NEVER log or echo the
/// returned string.
pub(crate) fn gitea_token(identity: Option<&str>) -> Result<String, ToolError> {
    let client = GiteaClient::from_env()?;
    let client = match identity {
        Some(name) if !name.trim().is_empty() => client.for_identity(name)?,
        _ => client,
    };
    let token = client.raw_token();
    if token.is_empty() {
        return Err(ToolError::NotConfigured(format!(
            "no Gitea token resolved for identity '{}' — provision GITEA_PAT_{}",
            client.identity_name().unwrap_or("?"),
            client.identity_name().unwrap_or("?").to_uppercase()
        )));
    }
    Ok(token.to_string())
}

// ─── Shared optional `identity` argument ─────────────────────────────────────
//
// Every Gitea CRUD tool exposes the same optional `identity` argument, resolved
// centrally by `GiteaClient::resolve_identity`. These two helpers keep the
// schema fragment and its documentation in a single source of truth so all
// tools describe it identically and can never drift. Mirrors the Plane tool.

/// JSON-schema fragment for the optional `identity` argument.
fn identity_param_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional Gitea identity to act as: a configured GITEA_PAT_<NAME> \
                        identity name (e.g. \"moose\", \"harmony\", \"lumina\"). Omit to use the \
                        active default identity (GITEA_IDENTITY_NAME, default \"moose\"). Call \
                        gitea_list_identities to see the configured names."
    })
}

/// Add the shared optional `identity` property to a tool's parameter schema.
/// Idempotent and safe on any `{ "type": "object", "properties": { .. } }`
/// schema — inserts the `identity` property without disturbing the tool's own
/// arguments or its `required` list (identity is always optional).
fn with_identity_param(mut schema: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
        props.insert("identity".to_string(), identity_param_schema());
    }
    schema
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
        with_identity_param(json!({
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
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    // TERM-PREREQ-GITEA-LISTREPOS (unblocks HCAT-25): expose a typed structured
    // response so egress callers (harmony `GiteaClient::list_repos`) recover a
    // `Vec<GiteaRepo>` without text-scraping — the last GiteaClient method still
    // on direct REST. Shape mirrors the plane/gitea siblings:
    // `{ owner, page, shown, items:[GiteaRepo] }`.
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl ListRepos {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let limit = args["limit"].as_u64().unwrap_or(50).min(50);
        let page = args["page"].as_u64().unwrap_or(1).max(1);

        let path = format!(
            "/repos/search?owner={}&limit={}&page={}",
            client.owner, limit, page
        );
        let raw: Value = client.get(&path).await?;
        // Gitea search returns {"data": [...], "ok": true}
        let repos: Vec<GiteaRepo> = serde_json::from_value(
            raw["data"].clone(),
        )
        .map_err(|e| ToolError::Http(format!("Failed to parse repo list: {e}")))?;

        // `GiteaRepo` is `Serialize`; the structured payload is the same typed
        // objects harmony deserializes back into `Vec<Repo>`.
        let structured = json!({
            "owner": client.owner,
            "page": page,
            "shown": repos.len(),
            "items": repos,
        });

        if repos.is_empty() {
            return Ok((format!("No repositories found for '{}'.", client.owner), structured));
        }

        let mut out = format!(
            "Repositories for '{}' (page {}, showing {}):\n\n",
            client.owner,
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
        Ok((out, structured))
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
        with_identity_param(json!({
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
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl GetRepo {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        let path = format!("/repos/{}/{}", owner, repo);
        let r: GiteaRepo = client.get(&path).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(format!("Repository '{owner}/{repo}' not found")),
            other => other,
        })?;

        let text = format!(
            "Repository: {}\nDescription: {}\nURL: {}\nDefault branch: {}\nPrivate: {}\nStars: {} | Forks: {} | Open issues: {}\nUpdated: {}",
            r.full_name,
            if r.description.is_empty() { "(none)".to_string() } else { r.description.clone() },
            r.html_url,
            r.default_branch,
            r.private,
            r.stars_count,
            r.forks_count,
            r.open_issues_count,
            r.updated.clone().unwrap_or_default(),
        );
        let structured = serde_json::to_value(&r)
            .map_err(|e| ToolError::Http(format!("Failed to serialize repo: {e}")))?;
        Ok((text, structured))
    }
}

// 2b. create_repo
//
// Wraps POST {GITEA_URL}/api/v1/orgs/{org}/repos. Uses a direct reqwest call
// (rather than GiteaClient::post) so we can surface 422 (already exists) and
// 401/403 (auth) as clear, distinct errors. Credentials come from the shared
// GiteaClient (GITEA_URL + the resolved GITEA_PAT_<NAME> identity token), never
// std::env::var here or hardcoded.
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
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "org":         { "type": "string",  "description": "Organisation to create the repo under" },
                "name":        { "type": "string",  "description": "Repository name" },
                "description": { "type": "string",  "description": "Repository description (optional)" },
                "private":     { "type": "boolean", "description": "Private repo? Default true", "default": true }
            },
            "required": ["org", "name"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl CreateRepo {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
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
        let url = client.api(&endpoint);
        debug!("POST {url}");
        let resp = client
            .http
            .post(&url)
            .header("Authorization", client.auth_header())
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
                "Gitea authentication/authorisation failed ({}). Check the resolved \
                 GITEA_PAT_<NAME> identity's token scope (needs write:organization for org repos).",
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

        let structured = json!({
            "full_name": repo.get("full_name").and_then(Value::as_str).unwrap_or(""),
            "html_url":  repo.get("html_url").and_then(Value::as_str).unwrap_or(""),
            "clone_url": repo.get("clone_url").and_then(Value::as_str).unwrap_or(""),
            "ssh_url":   repo.get("ssh_url").and_then(Value::as_str).unwrap_or(""),
        });
        Ok((structured.to_string(), structured))
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
        with_identity_param(json!({
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
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl CreateFile {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' is required".to_string()))?;
        let content = args["content"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'content' is required".to_string()))?;
        let message = args["message"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'message' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

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
        let resp: GiteaFileResponse = client.post(&endpoint, &body).await?;

        let text = format!(
            "File created: {}/{}/{}\nCommit: {}",
            owner,
            repo,
            path,
            resp.commit.sha,
        );
        let structured = serde_json::to_value(&resp)
            .map_err(|e| ToolError::Http(format!("Failed to serialize response: {e}")))?;
        Ok((text, structured))
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
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Repository name" },
                "path":   { "type": "string", "description": "File path within the repo" },
                "ref":    { "type": "string", "description": "Branch, tag, or commit SHA (optional)" },
                "owner":  { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "path"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl ReadFile {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        let mut endpoint = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        if let Some(git_ref) = args["ref"].as_str() {
            endpoint.push_str(&format!("?ref={}", git_ref));
        }

        let fc: GiteaFileContent = client.get(&endpoint).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(format!("File not found in repo: {owner}/{repo}/{path}")),
            other => other,
        })?;

        // Decode base64 content
        let raw_content = fc.content.clone().unwrap_or_default();
        // Gitea wraps lines with newlines in the base64 — strip them
        let clean = raw_content.replace('\n', "").replace('\r', "");
        let decoded = B64
            .decode(&clean)
            .map_err(|e| ToolError::Http(format!("Failed to decode file content: {e}")))?;
        let text = String::from_utf8_lossy(&decoded).to_string();

        let out = format!(
            "File: {owner}/{repo}/{path}\nSHA: {}\nSize: {} bytes\n\n---\n{text}",
            fc.sha, fc.size
        );
        // EGJS-02: alongside the decoded UTF-8 `content`, also surface the raw,
        // un-decoded base64 Gitea returned (`content_base64`) — harmony's
        // `FileContent.content_base64` type contract expects base64, and
        // re-encoding the lossily-decoded UTF-8 text would corrupt non-UTF-8
        // file content (see LHEG-06's `get_file` gap note).
        let structured = json!({
            "owner": owner,
            "repo": repo,
            "path": path,
            "sha": fc.sha,
            "size": fc.size,
            "content": text,
            "content_base64": clean,
        });
        Ok((out, structured))
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
        with_identity_param(json!({
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
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl UpdateFile {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' is required".to_string()))?;
        let content = args["content"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'content' is required".to_string()))?;
        let message = args["message"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'message' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        // PII gate before fetching SHA (fail fast)
        if let Some(reason) = pii_check(content) {
            warn!("PII gate blocked update_file on {owner}/{repo}/{path}: {reason}");
            return Err(ToolError::InvalidArgument(format!(
                "Content rejected by PII gate: {reason}"
            )));
        }

        // Fetch current SHA — required by Gitea for updates
        let sha = client.get_file_sha(repo, path).await.map_err(|e| match e {
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
        let resp: GiteaFileResponse = client.put(&endpoint, &body).await?;

        let text = format!(
            "File updated: {owner}/{repo}/{path}\nCommit: {}",
            resp.commit.sha,
        );
        let structured = serde_json::to_value(&resp)
            .map_err(|e| ToolError::Http(format!("Failed to serialize response: {e}")))?;
        Ok((text, structured))
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
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":    { "type": "string", "description": "Repository name" },
                "path":    { "type": "string", "description": "File path within the repo" },
                "message": { "type": "string", "description": "Commit message" },
                "branch":  { "type": "string", "description": "Branch (optional)" },
                "owner":   { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "path", "message"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl DeleteFile {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'path' is required".to_string()))?;
        let message = args["message"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'message' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        // Fetch current SHA — required by Gitea
        let sha = client.get_file_sha(repo, path).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(format!("File not found in repo: {owner}/{repo}/{path}")),
            other => other,
        })?;

        let body = GiteaDeleteFileRequest {
            message: message.to_string(),
            sha: sha.clone(),
            branch: args["branch"].as_str().map(str::to_string),
        };

        let endpoint = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        client.delete_with_body(&endpoint, &body).await?;

        let text = format!("File deleted: {owner}/{repo}/{path}");
        let structured = json!({
            "owner": owner,
            "repo": repo,
            "path": path,
            "deleted": true,
            "sha": sha,
        });
        Ok((text, structured))
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
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "state": { "type": "string", "description": "Filter by state: open | closed | all (default: open)", "enum": ["open", "closed", "all"] },
                "limit": { "type": "integer", "description": "Max results (default 20)", "default": 20 },
                "page":  { "type": "integer", "description": "Page number (default 1)", "default": 1 },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl ListPrs {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let state = args["state"].as_str().unwrap_or("open");
        let limit = args["limit"].as_u64().unwrap_or(20).min(50);
        let page = args["page"].as_u64().unwrap_or(1).max(1);
        let owner = client.resolve_owner(args["owner"].as_str());

        let endpoint = format!(
            "/repos/{}/{}/pulls?state={}&limit={}&page={}",
            owner, repo, state, limit, page
        );
        let prs: Vec<GiteaPullRequest> = client.get(&endpoint).await?;

        let structured = json!({ "items": prs });
        if prs.is_empty() {
            return Ok((format!("No {} pull requests in {owner}/{repo}.", state), structured));
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
        Ok((out, structured))
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
        with_identity_param(json!({
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
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl CreatePr {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        // Delegates to the shared `GiteaClient::create_pull` helper so the tool
        // and PROMO-01's `plane_prefix_promote` share one create-pull path.
        let pr: GiteaPullRequest = self.client.create_pull(&args).await?;

        let text = format!(
            "Pull request created: #{} — {}\nURL: {}\n{} → {}",
            pr.number, pr.title, pr.html_url, pr.head.ref_name, pr.base.ref_name,
        );
        let structured = serde_json::to_value(&pr)
            .map_err(|e| ToolError::Http(format!("Failed to serialize pull request: {e}")))?;
        Ok((text, structured))
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
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Repository name" },
                "pr":     { "type": "integer", "description": "Pull request number" },
                "style":  { "type": "string", "description": "Merge style: merge | rebase | squash (default: merge)", "enum": ["merge", "rebase", "squash"] },
                "message": { "type": "string", "description": "Merge commit message (optional)" },
                "owner":  { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "pr"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let pr_num = args["pr"].as_u64()
            .ok_or_else(|| ToolError::InvalidArgument("'pr' must be an integer".to_string()))?;
        let style = args["style"].as_str().unwrap_or("merge");
        let owner = client.resolve_owner(args["owner"].as_str());

        let mut body = json!({ "Do": style });
        if let Some(msg) = args["message"].as_str() {
            body["MergeMessageField"] = json!(msg);
        }

        let endpoint = format!("/repos/{}/{}/pulls/{}/merge", owner, repo, pr_num);
        // Merge endpoint returns 200 with no body on success
        let url = client.api(&endpoint);
        let resp = client
            .http
            .post(&url)
            .header("Authorization", client.auth_header())
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

        // S111E/MIRR-04: this is the single clean "a gated merge to internal
        // main just completed" call site the build pipeline's Stage 6 (merge)
        // actually goes through — best-effort refresh the mirror engine's
        // parking-lot checkout of this repo's internal main, so the git-public
        // mirror runner picks up the change on its next tick without waiting
        // for a separate manual sync-source call. This MUST NEVER fail the
        // merge itself (the merge above already succeeded on Gitea): a
        // sync-source failure here (e.g. TERMINUS_MIRROR_SOURCE_ROOT /
        // TERMINUS_MIRROR_INTERNAL_REMOTE_<REPO> not configured on this host)
        // is logged and swallowed — the mirror runner self-heals by re-syncing
        // on its next scheduled tick, exactly like a missed mirror push does
        // (see git_public_mirror_push's failure protocol).
        if let Err(e) = crate::forge::mirror::tools::dispatch_mirror_action(
            "sync-source",
            json!({ "repo": repo }),
        )
        .await
        {
            tracing::warn!(
                target: "mirror_audit",
                event = "sync_source_after_merge_failed",
                repo = %repo,
                pr = pr_num,
                error = %e,
                "post-merge mirror sync-source failed (non-fatal — PR #{pr_num} merged \
                 successfully; the mirror runner will re-sync '{repo}' on its next tick)"
            );
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
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "path":  { "type": "string", "description": "Directory path (empty for root)" },
                "ref":   { "type": "string", "description": "Branch, tag, or commit SHA (optional)" },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl ListDirectory {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo  = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let path  = args["path"].as_str().unwrap_or("").trim_matches('/');
        let owner = client.resolve_owner(args["owner"].as_str());

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

        let entries: Vec<Value> = client.get(&endpoint).await
            .map_err(|e| match e {
                ToolError::NotFound(_) => ToolError::NotFound(
                    format!("Path not found: {owner}/{repo}/{path}")),
                other => other,
            })?;

        let structured = json!({ "entries": entries });
        let mut out = format!("Directory: {owner}/{repo}/{}\n{} entries:\n",
            if path.is_empty() { "/" } else { path }, entries.len());
        for e in &entries {
            let kind = e["type"].as_str().unwrap_or("?");
            let name = e["name"].as_str().unwrap_or("?");
            let indicator = if kind == "dir" { "📁" } else { "📄" };
            out.push_str(&format!("  {indicator} {name}\n"));
        }
        Ok((out, structured))
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
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "limit": { "type": "integer", "description": "Max results (default 30)", "default": 30 },
                "page":  { "type": "integer", "description": "Page number (default 1)", "default": 1 },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl ListBranches {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let limit = args["limit"].as_u64().unwrap_or(30).min(50);
        let page = args["page"].as_u64().unwrap_or(1).max(1);
        let owner = client.resolve_owner(args["owner"].as_str());

        let endpoint = format!(
            "/repos/{}/{}/branches?limit={}&page={}",
            owner, repo, limit, page
        );
        let branches: Vec<GiteaBranchInfo> = client.get(&endpoint).await?;

        let structured = json!({ "items": branches });
        if branches.is_empty() {
            return Ok((format!("No branches found in {owner}/{repo}."), structured));
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
        Ok((out, structured))
    }
}

// ─── EGJS-02: gitea_create_branch ─────────────────────────────────────────────
//
// Missing entirely from the terminus-rs catalogue prior to this item — harmony's
// `git::` bypass surface needed it (LHEG-06 remainder: "Gitea, no tool exists at
// all on the terminus primary: ... create_branch"). Wraps
// `POST /repos/{owner}/{repo}/branches`.

pub struct CreateBranch {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for CreateBranch {
    fn name(&self) -> &str { "gitea_create_branch" }

    fn description(&self) -> &str {
        "Create a new branch in a Gitea repository from an existing branch (defaults to the repo's default branch)."
    }

    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":        { "type": "string", "description": "Repository name" },
                "branch":      { "type": "string", "description": "New branch name" },
                "old_branch":  { "type": "string", "description": "Branch to create from (optional, defaults to the repo's default branch)" },
                "owner":       { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "branch"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl CreateBranch {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let branch = args["branch"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'branch' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        let mut body = json!({ "new_branch_name": branch });
        if let Some(old_branch) = args["old_branch"].as_str() {
            body["old_branch_name"] = json!(old_branch);
        }

        let endpoint = format!("/repos/{}/{}/branches", owner, repo);
        let b: GiteaBranchInfo = client.post(&endpoint, &body).await?;

        let text = format!("Branch created: {owner}/{repo}@{}", b.name);
        let structured = serde_json::to_value(&b)
            .map_err(|e| ToolError::Http(format!("Failed to serialize branch: {e}")))?;
        Ok((text, structured))
    }
}

// ─── EGJS-02: gitea_delete_branch ─────────────────────────────────────────────

pub struct DeleteBranch {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for DeleteBranch {
    fn name(&self) -> &str { "gitea_delete_branch" }

    fn description(&self) -> &str {
        "Delete a branch from a Gitea repository."
    }

    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":   { "type": "string", "description": "Repository name" },
                "branch": { "type": "string", "description": "Branch name to delete" },
                "owner":  { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "branch"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl DeleteBranch {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let branch = args["branch"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'branch' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        let endpoint = format!("/repos/{}/{}/branches/{}", owner, repo, branch);
        let url = client.api(&endpoint);
        debug!("DELETE {url}");
        let resp = client
            .http
            .delete(&url)
            .header("Authorization", client.auth_header())
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound(format!("Branch '{branch}' not found in {owner}/{repo}")));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!("Gitea returned {status}: {body_text}")));
        }

        let text = format!("Branch deleted: {owner}/{repo}@{branch}");
        let structured = json!({ "owner": owner, "repo": repo, "branch": branch, "deleted": true });
        Ok((text, structured))
    }
}

// ─── EGJS-02: gitea_close_pr ──────────────────────────────────────────────────
//
// Close a pull request WITHOUT merging it — distinct from `gitea_merge_pr`.
// Missing from the catalogue prior to this item (LHEG-06 remainder).

pub struct ClosePr {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for ClosePr {
    fn name(&self) -> &str { "gitea_close_pr" }

    fn description(&self) -> &str {
        "Close a pull request in a Gitea repository WITHOUT merging it."
    }

    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "pr":    { "type": "integer", "description": "Pull request number" },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "pr"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl ClosePr {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let pr_num = args["pr"].as_u64()
            .ok_or_else(|| ToolError::InvalidArgument("'pr' must be an integer".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        let body = json!({ "state": "closed" });
        let endpoint = format!("/repos/{}/{}/pulls/{}", owner, repo, pr_num);
        let pr: GiteaPullRequest = client.patch(&endpoint, &body).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(format!("Pull request #{pr_num} not found in {owner}/{repo}")),
            other => other,
        })?;

        let text = format!("Pull request #{} closed in {owner}/{repo}.", pr.number);
        let structured = serde_json::to_value(&pr)
            .map_err(|e| ToolError::Http(format!("Failed to serialize pull request: {e}")))?;
        Ok((text, structured))
    }
}

// ─── EGJS-02: gitea_get_pr_diff ───────────────────────────────────────────────
//
// Missing from the catalogue prior to this item; production call site was
// `review::reviewer::run_review_batch` in harmony (LHEG-06 remainder). Wraps
// Gitea's `.diff` suffix endpoint, which returns raw unified-diff text rather
// than JSON — reuses `GiteaClient::request_raw` (already used for binary file
// content) so non-UTF-8 diff bytes are never corrupted before the lossy
// `String::from_utf8_lossy` at the very end (a unified diff is text but may
// contain non-UTF-8 bytes inside binary-file hunks).

pub struct GetPrDiff {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for GetPrDiff {
    fn name(&self) -> &str { "gitea_get_pr_diff" }

    fn description(&self) -> &str {
        "Get the unified diff for a pull request in a Gitea repository."
    }

    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "repo":  { "type": "string", "description": "Repository name" },
                "pr":    { "type": "integer", "description": "Pull request number" },
                "owner": { "type": "string", "description": "Owner override (optional)" }
            },
            "required": ["repo", "pr"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.run(args).await?.0)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let (text, structured) = self.run(args).await?;
        Ok(ToolOutput { text, structured: Some(structured) })
    }
}

impl GetPrDiff {
    async fn run(&self, args: Value) -> Result<(String, Value), ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let repo = args["repo"].as_str()
            .ok_or_else(|| ToolError::InvalidArgument("'repo' is required".to_string()))?;
        let pr_num = args["pr"].as_u64()
            .ok_or_else(|| ToolError::InvalidArgument("'pr' must be an integer".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());

        let endpoint = format!("/repos/{}/{}/pulls/{}.diff", owner, repo, pr_num);
        let bytes = client.request_raw(reqwest::Method::GET, &endpoint).await.map_err(|e| match e {
            ToolError::NotFound(_) => ToolError::NotFound(format!("Pull request #{pr_num} not found in {owner}/{repo}")),
            other => other,
        })?;
        let diff = String::from_utf8_lossy(&bytes).to_string();

        let text = format!("Diff for PR #{pr_num} in {owner}/{repo} ({} bytes):\n\n{diff}", bytes.len());
        let structured = json!({
            "owner": owner,
            "repo": repo,
            "pr": pr_num,
            "diff": diff,
        });
        Ok((text, structured))
    }
}

// ─── Cargo registry publish ──────────────────────────────────────────────────
//
// `cargo publish` is, on the wire, an authenticated HTTP PUT of a packaged
// `.crate` file to the registry's publish endpoint. Gitea implements the Cargo
// registry API, so we recreate that PUT here and route it through a resolved
// `GITEA_PAT_<NAME>` identity token (the active default GITEA_IDENTITY_NAME, or
// the optional `identity` argument) — meaning no `cargo publish` token ever has
// to live on the dev box or be spread across build/serving hosts. Publishing
// honours the same identity model as every other Gitea tool (S105/GPAT); it is
// no longer a single fixed publisher identity.
//
// Endpoint (verified against Gitea 1.25.x):
//   PUT {GITEA_URL}/api/packages/{owner}/cargo/api/v1/crates/new
//   Authorization: token <GITEA_PAT_NAME>  (the PAT scheme all GiteaClient calls
//                                          use; a `Bearer` prefix would make
//                                          Gitea treat the PAT as OAuth2)
//   Body: the standard Cargo publish binary frame —
//     u32-LE(metadata_json_len) || metadata_json || u32-LE(crate_len) || crate_bytes
//
// Note this endpoint lives under `/api/packages/...`, NOT the `/api/v1/...`
// Gitea REST surface used by every other tool in this module, so it builds its
// URL directly from `base_url` rather than via `GiteaClient::api()`.

/// Assemble the Cargo publish request body: the standard length-prefixed binary
/// frame that `cargo publish` sends and that the registry expects.
///
/// Layout (all lengths little-endian u32):
///   `u32(metadata_json.len) || metadata_json || u32(crate_bytes.len) || crate_bytes`
pub(crate) fn build_cargo_publish_body(metadata_json: &[u8], crate_bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + metadata_json.len() + crate_bytes.len());
    body.extend_from_slice(&(metadata_json.len() as u32).to_le_bytes());
    body.extend_from_slice(metadata_json);
    body.extend_from_slice(&(crate_bytes.len() as u32).to_le_bytes());
    body.extend_from_slice(crate_bytes);
    body
}

/// Index URL for the public crates.io registry. This is the Cargo registry
/// index's own convention for identifying crates.io as a dependency's source
/// (see the `registry` field in the Cargo registry API's publish metadata
/// format) — NOT an infra value of ours, so it is not subject to the S1
/// hardcoded-infra-value rule.
pub(crate) const CRATES_IO_INDEX_URL: &str = "https://github.com/rust-lang/crates.io-index";

/// Build the Cargo publish metadata JSON.
///
/// The registry publish API requires a metadata object whose only truly
/// mandatory fields are `name` and `vers`; every other field has a well-defined
/// empty default (arrays → `[]`, maps → `{}`, optional strings → `null`). We
/// emit the full field set with those defaults so lenient and strict registries
/// alike accept it.
///
/// The caller supplies a `provided` metadata object — REQUIRED at the tool
/// boundary, extracted on the dev box (deps/features/... included) — whose keys
/// are layered over the defaults; defaulting `deps` to empty for a crate that
/// actually has dependencies would make Gitea write an incorrect registry index.
/// `name` and `vers` are then force-set from the explicit `name`/`vers`
/// arguments so the framed metadata can never disagree with the tool's stated
/// target. (`provided: None` is retained only for helper-level unit tests.)
pub(crate) fn build_cargo_metadata(name: &str, vers: &str, provided: Option<&Value>) -> Value {
    let mut meta = json!({
        "name": name,
        "vers": vers,
        "deps": [],
        "features": {},
        "authors": [],
        "description": Value::Null,
        "documentation": Value::Null,
        "homepage": Value::Null,
        "readme": Value::Null,
        "readme_file": Value::Null,
        "keywords": [],
        "categories": [],
        "license": Value::Null,
        "license_file": Value::Null,
        "repository": Value::Null,
        "badges": {},
        "links": Value::Null,
    });

    if let (Some(Value::Object(src)), Value::Object(dst)) = (provided, &mut meta) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }

    // Force name + vers to the explicit arguments — the framed metadata must
    // always match the target the tool was asked to publish.
    meta["name"] = json!(name);
    meta["vers"] = json!(vers);

    // Default each dependency's `registry` field to crates.io when the caller
    // didn't specify one at all. In a Cargo registry index entry, an omitted
    // `registry` field means "this dep lives in the SAME registry as the
    // crate being published" — i.e. this private Gitea registry. A crate
    // published here (terminus-rs) almost always depends on ordinary
    // crates.io crates (tokio, serde, async-imap, ...), so leaving `registry`
    // unset on those deps makes cargo try to resolve them against this
    // private index and fail unresolvable. This bit terminus-rs 1.3.0, whose
    // deps were indexed with `registry: null` (all-crates.io-deps).
    //
    // A dep that explicitly sets `registry` — to a real value OR to `null` —
    // is left untouched: that's how an advanced caller expresses "this dep
    // really is in the same registry as the crate being published" (e.g. an
    // intra-gitea dependency), and we must not overwrite an intentional
    // choice.
    if let Some(deps) = meta.get_mut("deps").and_then(Value::as_array_mut) {
        for dep in deps.iter_mut() {
            if let Value::Object(dep_obj) = dep {
                if !dep_obj.contains_key("registry") {
                    dep_obj.insert("registry".to_string(), json!(CRATES_IO_INDEX_URL));
                }
            }
        }
    }

    meta
}

/// Default upper bound on the `.crate` artifact size (64 MiB). A packaged crate
/// is normally well under a few MiB; this cap exists purely to stop a caller
/// from pointing the tool at an unbounded/huge file and exhausting memory.
/// Overridable via `CARGO_PUBLISH_MAX_CRATE_BYTES`. `pub(crate)` because the
/// Gitea-family [`ForgeProvider`] adapter's `packages_publish` (which accepts
/// crate bytes as base64 rather than a file path) enforces the same ceiling
/// against the decoded byte length — one size limit, shared, rather than two
/// consts that can silently drift apart.
pub(crate) const DEFAULT_MAX_CRATE_BYTES: u64 = 64 * 1024 * 1024;

/// Resolve the effective max `.crate` byte ceiling for cargo publish: the
/// `CARGO_PUBLISH_MAX_CRATE_BYTES` env override if set to a valid positive
/// integer, else [`DEFAULT_MAX_CRATE_BYTES`]. Shared by both the file-path
/// publish tool (`gitea_cargo_publish`) and the `ForgeProvider` adapter's
/// `packages_publish` (base64 body) so a single knob controls both.
pub(crate) fn cargo_publish_max_crate_bytes() -> u64 {
    env::var("CARGO_PUBLISH_MAX_CRATE_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_CRATE_BYTES)
}

/// True if `owner` is a single, safe registry path segment.
///
/// The owner is interpolated into the publish URL and paired with a privileged
/// bearer token, so a value like `../../other-org` (or one containing a slash)
/// must never be allowed to re-target the request at a different endpoint.
/// Gitea org/user names are alphanumeric plus `-`, `_`, `.`; we additionally
/// require a non-empty value that is not `.`/`..` and contains no path
/// separators.
pub(crate) fn is_valid_owner_segment(owner: &str) -> bool {
    if owner.is_empty() || owner == "." || owner == ".." {
        return false;
    }
    owner
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Open a `.crate` once and read it under all safety constraints against THAT
/// handle, so no time-of-check/time-of-use gap can be exploited between checks
/// and the read:
/// - the open handle must refer to a **regular file** (rejects directories and
///   unbounded devices such as `/dev/zero`);
/// - when `artifact_dir` is `Some`, the *actually opened* file (resolved from
///   the file descriptor, not the caller's path string) must live inside the
///   canonicalized artifact directory — this defeats a symlink swapped in after
///   an earlier path-level check;
/// - at most `max_bytes + 1` bytes are read, so an oversized or growing source
///   can never exhaust memory, and the size bound is enforced on what was read.
fn read_bounded_crate(
    path: &std::path::Path,
    raw: &str,
    max_bytes: u64,
    artifact_dir: Option<&str>,
) -> Result<Vec<u8>, ToolError> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| {
        ToolError::InvalidArgument(format!("Failed to read .crate file at '{raw}': {e}"))
    })?;
    let meta = file.metadata().map_err(|e| {
        ToolError::InvalidArgument(format!("Failed to stat .crate file at '{raw}': {e}"))
    })?;
    if !meta.is_file() {
        return Err(ToolError::InvalidArgument(format!(
            "crate_path '{raw}' is not a regular file (directories and devices are refused)."
        )));
    }

    // Jail check against the OPEN handle, closing the resolve→open race.
    if let Some(dir) = artifact_dir {
        let root = std::path::Path::new(dir).canonicalize().map_err(|e| {
            ToolError::InvalidArgument(format!(
                "Configured artifact directory could not be resolved: {e}"
            ))
        })?;
        let opened = opened_file_real_path(&file, path);
        if !opened.starts_with(&root) {
            return Err(ToolError::InvalidArgument(format!(
                "crate_path '{raw}' resolves outside the permitted artifact directory."
            )));
        }
    }

    let mut buf = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut buf).map_err(|e| {
        ToolError::InvalidArgument(format!("Failed to read .crate file at '{raw}': {e}"))
    })?;
    if buf.len() as u64 > max_bytes {
        return Err(ToolError::InvalidArgument(format!(
            "crate_path '{raw}' exceeds the {max_bytes}-byte publish limit."
        )));
    }
    Ok(buf)
}

/// Resolve the real filesystem path of an already-open file from its descriptor,
/// so a jail check reflects the inode that was actually opened rather than a
/// caller-supplied path that may have been swapped. On Linux this reads
/// `/proc/self/fd/<fd>`; if that is unavailable it falls back to canonicalizing
/// the original path (still symlink-resolved, just without the fd guarantee).
fn opened_file_real_path(file: &std::fs::File, fallback: &std::path::Path) -> std::path::PathBuf {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let link = format!("/proc/self/fd/{}", file.as_raw_fd());
        if let Ok(real) = std::fs::read_link(&link) {
            return real;
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
    }
    fallback
        .canonicalize()
        .unwrap_or_else(|_| fallback.to_path_buf())
}

/// Validate and canonicalize the caller-supplied `crate_path` before any bytes
/// are read, so `gitea_cargo_publish` cannot be turned into an arbitrary
/// host-file exfiltration or a memory-exhaustion primitive.
///
/// Enforced unconditionally:
/// - the path must end in `.crate` (case-insensitive);
/// - it must resolve (`canonicalize`, which also follows symlinks) to an
///   existing **regular file** — rejecting directories, and character/block
///   devices such as `/dev/zero` that would otherwise stream forever;
/// - its size must be `<= max_bytes` — checked via metadata BEFORE reading, so
///   an oversized file is refused without ever being buffered.
///
/// Enforced when `artifact_dir` is `Some` (operator opt-in via
/// `CARGO_PUBLISH_ARTIFACT_DIR`): the canonicalized crate path must live inside
/// the canonicalized artifact directory — a path jail that confines reads to a
/// dedicated staging area, matching the path-jailed posture of the `dev` tools.
fn resolve_crate_path(
    raw: &str,
    max_bytes: u64,
    artifact_dir: Option<&str>,
) -> Result<std::path::PathBuf, ToolError> {
    let path = std::path::Path::new(raw);

    // Extension gate first — cheap, and blocks the obvious "read /etc/passwd"
    // shape before touching the filesystem.
    let ext_ok = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("crate"));
    if !ext_ok {
        return Err(ToolError::InvalidArgument(format!(
            "crate_path '{raw}' must point to a .crate file (produced by `cargo package`)."
        )));
    }

    // Canonicalize: resolves symlinks and requires the file to exist. A path
    // that cannot be resolved (missing, unreadable parent) is refused here.
    let canonical = path.canonicalize().map_err(|e| {
        ToolError::InvalidArgument(format!("Failed to access .crate file at '{raw}': {e}"))
    })?;

    let meta = std::fs::metadata(&canonical).map_err(|e| {
        ToolError::InvalidArgument(format!("Failed to stat .crate file at '{raw}': {e}"))
    })?;
    if !meta.is_file() {
        return Err(ToolError::InvalidArgument(format!(
            "crate_path '{raw}' is not a regular file (directories and devices are refused)."
        )));
    }
    if meta.len() > max_bytes {
        return Err(ToolError::InvalidArgument(format!(
            "crate_path '{raw}' is {} bytes, exceeding the {max_bytes}-byte publish limit.",
            meta.len()
        )));
    }

    if let Some(dir) = artifact_dir {
        let root = std::path::Path::new(dir).canonicalize().map_err(|e| {
            ToolError::InvalidArgument(format!(
                "Configured artifact directory could not be resolved: {e}"
            ))
        })?;
        if !canonical.starts_with(&root) {
            return Err(ToolError::InvalidArgument(format!(
                "crate_path '{raw}' resolves outside the permitted artifact directory."
            )));
        }
    }

    Ok(canonical)
}

/// `gitea_cargo_publish` — publish a packaged `.crate` to the Gitea Cargo
/// registry using Terminus's own token.
pub struct CargoPublish {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for CargoPublish {
    fn name(&self) -> &str { "gitea_cargo_publish" }

    fn description(&self) -> &str {
        "Publish a packaged Rust .crate file (from token-less `cargo package`) to the Gitea \
         Cargo registry using a resolved GITEA_PAT_<NAME> identity's token (the active default \
         GITEA_IDENTITY_NAME, or the optional `identity` argument), so no cargo-publish token \
         lives on the dev box. Inputs: crate_path, name, version, metadata (the full Cargo \
         publish metadata incl. deps — extract it on the dev box), optional owner and identity."
    }

    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "crate_path": {
                    "type": "string",
                    "description": "Path (on the host running Terminus) to the local .crate file produced by `cargo package`. Must be an existing regular .crate file within the size limit; when CARGO_PUBLISH_ARTIFACT_DIR is configured it must reside inside that directory."
                },
                "name": {
                    "type": "string",
                    "description": "Crate name being published"
                },
                "version": {
                    "type": "string",
                    "description": "Crate version being published (e.g. 1.2.0)"
                },
                "owner": {
                    "type": "string",
                    "description": "Registry owner/org (optional; defaults to the configured GITEA_OWNER, normally 'moosenet')"
                },
                "metadata": {
                    "type": "object",
                    "description": "Required. The Cargo *publish-wire* metadata object (the exact schema cargo PUTs to a registry — NOT `cargo metadata` output, whose schema differs). Key fields: deps [{name, version_req, features, optional, default_features, target, kind}], features {}, and optional authors/description/license/repository/... Defaulting deps to empty would write an INCORRECT registry index for a crate with dependencies, so pass the real deps. name/vers are always overridden from the explicit arguments; any omitted optional fields are filled with empty defaults."
                }
            },
            "required": ["crate_path", "name", "version", "metadata"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let crate_path = args["crate_path"].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'crate_path' is required".to_string()))?;
        let name = args["name"].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'name' is required".to_string()))?;
        let version = args["version"].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'version' is required".to_string()))?;
        let owner = client.resolve_owner(args["owner"].as_str());
        // Reject an owner that could re-target the URL (path traversal / slash)
        // — it is interpolated into the endpoint alongside the bearer token.
        if !is_valid_owner_segment(owner) {
            return Err(ToolError::InvalidArgument(format!(
                "Invalid registry owner '{owner}': must be a single Gitea org/user name \
                 (alphanumerics, '-', '_', '.')."
            )));
        }
        // Full publish metadata is REQUIRED: defaulting deps to empty would make
        // Gitea write an incorrect registry index for any crate with
        // dependencies, breaking downstream consumers. The dev box extracts the
        // real metadata (deps/features/...) and passes it here.
        let provided_metadata = args.get("metadata").filter(|v| v.is_object()).ok_or_else(|| {
            ToolError::InvalidArgument(
                "'metadata' (the full Cargo publish metadata object, including deps) is required. \
                 Extract it on the dev box — a name+version-only publish would write an incorrect \
                 registry index for any crate with dependencies.".to_string(),
            )
        })?;

        // Validate the path BEFORE reading: `.crate` extension, existing regular
        // file (rejects dirs and unbounded devices like /dev/zero), size bound,
        // and an optional artifact-directory jail. This stops the tool being
        // used to read arbitrary host files or exhaust memory.
        let max_bytes = cargo_publish_max_crate_bytes();
        let artifact_dir = env::var("CARGO_PUBLISH_ARTIFACT_DIR").ok();
        let canonical_path = resolve_crate_path(crate_path, max_bytes, artifact_dir.as_deref())?;

        // Read the packaged .crate (an opaque gzip tarball), enforcing the size
        // bound and the artifact-dir jail against the OPEN handle to close the
        // resolve→open TOCTOU gap.
        let crate_bytes =
            read_bounded_crate(&canonical_path, crate_path, max_bytes, artifact_dir.as_deref())?;
        if crate_bytes.is_empty() {
            return Err(ToolError::InvalidArgument(format!(
                "The .crate file at '{crate_path}' is empty — nothing to publish."
            )));
        }

        // Build the length-prefixed publish frame. NOTE: no PII gate runs over
        // the crate bytes — the artifact is an opaque binary and the publish
        // target is the INTERNAL Gitea registry (legitimately holding internal
        // repository URLs), not the public GitHub mirror. Scanning it would only
        // false-positive on binary content or block valid internal references.
        let metadata = build_cargo_metadata(name, version, Some(provided_metadata));
        let metadata_json = serde_json::to_vec(&metadata)
            .map_err(|e| ToolError::Execution(format!("Failed to serialize crate metadata: {e}")))?;
        let body = build_cargo_publish_body(&metadata_json, &crate_bytes);

        let url = format!(
            "{}/api/packages/{}/cargo/api/v1/crates/new",
            client.base_url.trim_end_matches('/'),
            owner,
        );
        debug!("PUT {url} ({}-byte crate)", crate_bytes.len());

        // Publisher identity: the resolved GITEA_PAT_<NAME> token (active default
        // GITEA_IDENTITY_NAME, or the optional `identity` argument). Use the SAME
        // `Authorization: token <PAT>` scheme every other GiteaClient request uses
        // (a Gitea PAT under a `Bearer` prefix is treated as an OAuth2 credential
        // and rejected). The token is NEVER logged or echoed into any
        // result/error below.
        let resp = client
            .http
            .put(&url)
            .header("Authorization", client.auth_header())
            .header("Content-Type", "application/octet-stream")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Publish request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ToolError::Http(
                "Gitea Cargo publish returned 401 Unauthorized — the resolved GITEA_PAT_<NAME> \
                 identity token is missing or invalid.".to_string(),
            ));
        }
        if status == StatusCode::FORBIDDEN {
            return Err(ToolError::Http(format!(
                "Gitea Cargo publish returned 403 Forbidden for {owner}/{name}@{version}. The \
                 resolved GITEA_PAT_<NAME> identity token almost certainly lacks the \
                 `write:package` scope required to publish to the Cargo registry — regenerate the \
                 token in the runtime secret store with that scope."
            )));
        }
        if status == StatusCode::CONFLICT {
            return Err(ToolError::Conflict(format!(
                "Crate {name}@{version} already exists in the {owner} Cargo registry (Gitea \
                 returned 409). Bump the version to publish."
            )));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!(
                "Gitea Cargo publish returned {status} for {owner}/{name}@{version}: {body_text}"
            )));
        }

        // Success. The Cargo publish API returns a JSON body that may carry a
        // `warnings` object; surface it if present.
        let warnings = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("warnings").cloned());

        let registry_url = format!(
            "{}/{}/-/packages/cargo/{}/{}",
            client.base_url.trim_end_matches('/'),
            owner,
            name,
            version,
        );

        Ok(json!({
            "published": true,
            "name": name,
            "version": version,
            "owner": owner,
            "registry_url": registry_url,
            "warnings": warnings.unwrap_or(Value::Null),
        })
        .to_string())
    }
}

// ─── gitea_cargo_yank ──────────────────────────────────────────────────────

/// `gitea_cargo_yank` — yank (or unyank) a crate version in the Gitea Cargo
/// registry using Terminus's own resolved-identity token.
///
/// Yank is the REVERSIBLE Cargo registry primitive: it marks a version
/// unusable for NEW dependency resolution while `Cargo.lock` files that
/// already pin it continue to work unchanged. This is the correct tool for
/// retiring a broken/poisoned release (e.g. bad metadata that resolves to
/// `registry:null` dependencies) — prefer it over a hard package delete,
/// which is destructive and irreversible. Gitea's Cargo package registry
/// implements the standard Cargo registry web API
/// (<https://doc.rust-lang.org/cargo/reference/registry-web-api.html#yank>):
/// `DELETE .../crates/{crate}/{version}/yank` sets `yanked = true`, and
/// `PUT .../crates/{crate}/{version}/unyank` clears it.
pub struct CargoYank {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for CargoYank {
    fn name(&self) -> &str { "gitea_cargo_yank" }

    fn description(&self) -> &str {
        "Yank (or unyank) a crate version in the Gitea Cargo registry using a resolved \
         GITEA_PAT_<NAME> identity's token (the active default GITEA_IDENTITY_NAME, or the \
         optional `identity` argument). Yanking marks the version unusable for NEW dependency \
         resolution while Cargo.lock files that already reference it keep working — the \
         reversible primitive to retire a broken/poisoned release without deleting it. Inputs: \
         crate, version, optional unyank (default false = yank; true clears the yank), \
         optional owner, optional identity."
    }

    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "crate": {
                    "type": "string",
                    "description": "Crate name in the registry"
                },
                "version": {
                    "type": "string",
                    "description": "Crate version to yank or unyank (e.g. 1.3.0)"
                },
                "unyank": {
                    "type": "boolean",
                    "description": "If true, CLEAR the yank (make the version resolvable again). Default false = yank (mark the version unusable for new resolution)."
                },
                "owner": {
                    "type": "string",
                    "description": "Registry owner/org (optional; defaults to the configured GITEA_OWNER, normally 'moosenet')"
                }
            },
            "required": ["crate", "version"]
        }))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let name = args["crate"].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'crate' is required".to_string()))?;
        let version = args["version"].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'version' is required".to_string()))?;
        let unyank = args["unyank"].as_bool().unwrap_or(false);
        let owner = client.resolve_owner(args["owner"].as_str());

        // The owner, crate name, and version are all interpolated into the
        // request URL alongside a privileged bearer token, so each must be
        // validated as a single, safe path segment before use — the same
        // defense `gitea_cargo_publish` applies to `owner`.
        if !is_valid_owner_segment(owner) {
            return Err(ToolError::InvalidArgument(format!(
                "Invalid registry owner '{owner}': must be a single Gitea org/user name \
                 (alphanumerics, '-', '_', '.')."
            )));
        }
        if !is_valid_owner_segment(name) {
            return Err(ToolError::InvalidArgument(format!(
                "Invalid crate name '{name}': must contain only alphanumerics, '-', '_', '.'."
            )));
        }
        if !is_valid_owner_segment(version) {
            return Err(ToolError::InvalidArgument(format!(
                "Invalid version '{version}': must contain only alphanumerics, '-', '_', '.'."
            )));
        }

        let action = if unyank { "unyank" } else { "yank" };
        let url = format!(
            "{}/api/packages/{}/cargo/api/v1/crates/{}/{}/{}",
            client.base_url.trim_end_matches('/'),
            owner,
            name,
            version,
            action,
        );
        debug!("{} {url}", if unyank { "PUT" } else { "DELETE" });

        let req = if unyank {
            client.http.put(&url)
        } else {
            client.http.delete(&url)
        };

        let resp = req
            .header("Authorization", client.auth_header())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Cargo {action} request failed: {e}")))?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ToolError::Http(format!(
                "Gitea Cargo {action} returned 401 Unauthorized — the resolved GITEA_PAT_<NAME> \
                 identity token is missing or invalid."
            )));
        }
        if status == StatusCode::FORBIDDEN {
            return Err(ToolError::Http(format!(
                "Gitea Cargo {action} returned 403 Forbidden for {owner}/{name}@{version}. The \
                 resolved GITEA_PAT_<NAME> identity token almost certainly lacks the \
                 `write:package` scope required to {action} a Cargo registry version — \
                 regenerate the token in the runtime secret store with that scope."
            )));
        }
        if status == StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound(format!(
                "Crate {owner}/{name}@{version} was not found in the Cargo registry (Gitea \
                 returned 404) — cannot {action} a version that doesn't exist."
            )));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Http(format!(
                "Gitea Cargo {action} returned {status} for {owner}/{name}@{version}: {body_text}"
            )));
        }

        Ok(json!({
            "action": action,
            "yanked": !unyank,
            "name": name,
            "version": version,
            "owner": owner,
        })
        .to_string())
    }
}

// ─── gitea_list_identities ────────────────────────────────────────────────────

/// Lists the names of every configured `GITEA_PAT_<NAME>` identity so a caller
/// can see which identity it may act as before performing Gitea work. Names only
/// — never token values. Mirrors `plane_list_identities`.
pub struct GiteaListIdentities {
    client: GiteaClient,
}

#[async_trait]
impl RustTool for GiteaListIdentities {
    fn name(&self) -> &str { "gitea_list_identities" }

    fn description(&self) -> &str {
        "List the names of all configured Gitea identities (from GITEA_PAT_<NAME> environment vars) so you can see which identity to act as before performing Gitea work. Returns names only, never token values, plus the active_default identity. Every Gitea tool takes an optional `identity` argument set to one of these names to act AS that identity; omitting it uses the active default (GITEA_IDENTITY_NAME, default \"moose\"). Use the identity matching who should act on the repo rather than always the default."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        // Derived from the client's already-scanned identities map (populated
        // once at start via `scan_gitea_identities`), so the list is exactly
        // what `for_identity()` can resolve — never a fresh, divergent env scan.
        let names = self.client.identity_names();
        let count = names.len();
        let active_default = self.client.identity_name().map(|s| s.to_string());
        let mut out = json!({
            "identities": names,
            "count": count,
            "active_default": active_default,
            "prefix": GITEA_IDENTITY_PREFIX,
        });
        if count == 0 {
            out["note"] = json!(format!(
                "No named Gitea identities configured. Provision named identities as \
                 {GITEA_IDENTITY_PREFIX}<NAME> (e.g. {GITEA_IDENTITY_PREFIX}MOOSE)."
            ));
        }
        serde_json::to_string(&out)
            .map_err(|e| ToolError::Execution(format!("failed to serialize identity list: {e}")))
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
            let _ = registry.register(Box::new(CargoPublish { client: client.clone() }));
            let _ = registry.register(Box::new(CargoYank { client: client.clone() }));
            let _ = registry.register(Box::new(GiteaListIdentities { client: client.clone() }));
            let _ = registry.register(Box::new(ListDirectory { client: client.clone() }));
            // EGJS-02: harmony egress remainder — tools that did not exist at all
            // on the terminus primary (LHEG-06 gap notes).
            let _ = registry.register(Box::new(CreateBranch { client: client.clone() }));
            let _ = registry.register(Box::new(DeleteBranch { client: client.clone() }));
            let _ = registry.register(Box::new(ClosePr { client: client.clone() }));
            let _ = registry.register(Box::new(GetPrDiff { client }));
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
            stub!("gitea_cargo_publish", "Publish a .crate to the Gitea Cargo registry (not configured)");
            stub!("gitea_cargo_yank", "Yank/unyank a crate version in the Gitea Cargo registry (not configured)");
            stub!("gitea_list_identities", "List configured Gitea identities (not configured)");
            stub!("gitea_list_directory", "List directory contents in Gitea (not configured)");
            stub!("gitea_create_branch", "Create a branch in a Gitea repository (not configured)");
            stub!("gitea_delete_branch", "Delete a branch from a Gitea repository (not configured)");
            stub!("gitea_close_pr", "Close a Gitea pull request without merging (not configured)");
            stub!("gitea_get_pr_diff", "Get the diff for a Gitea pull request (not configured)");
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
            token: "<REDACTED-SECRET>".to_string(),
            identity_name: Some("moose".to_string()),
            identities: Arc::new(HashMap::new()),
            owner: "testorg".to_string(),
        }
    }

    /// Like `mock_client` but with a set of named `GITEA_PAT_<NAME>` identities
    /// pre-loaded (name -> token), so identity-resolution paths can be exercised
    /// without mutating the process environment.
    fn mock_client_with_identities(
        server: &MockServer,
        default_name: &str,
        identities: &[(&str, &str)],
    ) -> GiteaClient {
        let map: HashMap<String, String> = identities
            .iter()
            .map(|(n, t)| (n.to_lowercase(), t.to_string()))
            .collect();
        let token = map.get(default_name).cloned().unwrap_or_default();
        GiteaClient {
            http: Client::new(),
            base_url: server.base_url(),
            token,
            identity_name: Some(default_name.to_string()),
            identities: Arc::new(map),
            owner: "testorg".to_string(),
        }
    }

    // ── Token whitespace hardening (codex P1) ───────────────────────────────

    #[test]
    fn test_reject_interior_whitespace_rejects_embedded_space() {
        let err = reject_interior_whitespace("abc def").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("interior whitespace"), "unexpected message: {msg}");
    }

    #[test]
    fn test_reject_interior_whitespace_rejects_embedded_newline() {
        // A token glued together with a newline in the middle (e.g. two secret
        // lines concatenated) trims to itself and must still be rejected.
        let err = reject_interior_whitespace("abc\ndef").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_reject_interior_whitespace_rejects_control_char() {
        let err = reject_interior_whitespace("abc\tdef").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_reject_interior_whitespace_allows_clean_token() {
        assert!(reject_interior_whitespace("gta-clean-token-value-xyz").is_ok());
    }

    #[test]
    fn test_with_token_rejects_interior_whitespace() {
        // GiteaClient::with_token trims leading/trailing whitespace but must not
        // let an INTERIOR space/newline through — that would either corrupt the
        // `Authorization` header or build one that silently never matches the
        // real credential (codex P1: whitespace-trim bypass).
        let result = GiteaClient::with_token(
            "https://example.invalid",
            "  abc\ndef  ", // pii-test-fixture
            "testorg",
            "forgejo",
        );
        assert!(result.is_err(), "token with interior newline must be rejected");
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgument(_)),
            "expected InvalidArgument, got {err:?}"
        );
    }

    #[test]
    fn test_with_token_accepts_clean_trimmed_token() {
        let result = GiteaClient::with_token(
            "https://example.invalid",
            "  clean-token-value  ", // pii-test-fixture
            "testorg",
            "forgejo",
        );
        assert!(result.is_ok(), "clean token with only leading/trailing whitespace should pass");
    }

    #[test]
    fn test_scan_gitea_identities_rejects_interior_whitespace() {
        // Isolate from the real process environment: no other test may set
        // GITEA_PAT_* vars concurrently, so this is intentionally a single,
        // narrowly-scoped var set/unset around the call.
        // SAFETY: tests in this crate do not run env-mutating tests in parallel
        // across this specific var name; guarded by a unique identity name.
        std::env::set_var("GITEA_PAT_GITX02WSTEST", "abc\ndef"); // pii-test-fixture
        let result = scan_gitea_identities();
        std::env::remove_var("GITEA_PAT_GITX02WSTEST");
        let err = result.expect_err("interior-whitespace token must be rejected");
        assert!(matches!(err, ToolError::InvalidArgument(_)));
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
        let result = pii_check("Connect to <internal-ip> for service");  // pii-test-fixture
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
        let result = pii_check("Address: <internal-ip>");  // pii-test-fixture
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
        let result = pii_check("key=<REDACTED-SECRET>");  // pii-test-fixture
        assert!(result.is_some(), "Should detect sk- API key");
    }

    #[test]
    fn test_pii_gate_blocks_long_hex() {
        let result = pii_check("secret=abcdef1234567890abcdef1234567890ab");  // pii-test-fixture
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

    // TERM-PREREQ-GITEA-LISTREPOS: execute_structured returns a typed
    // { owner, page, shown, items:[GiteaRepo] } payload egress callers deserialize.
    #[tokio::test]
    async fn test_list_repos_execute_structured_returns_typed_items() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/search")
                .query_param("owner", "testorg");
            then.status(200).json_body(serde_json::json!({
                "data": [
                    {"id": 1, "name": "lumina", "full_name": "testorg/lumina",
                     "description": "Project docs", "private": false,
                     "html_url": "http://example.com/testorg/lumina",
                     "clone_url": "http://example.com/testorg/lumina.git",
                     "default_branch": "main", "stars_count": 0, "forks_count": 0,
                     "open_issues_count": 0, "updated": null}
                ],
                "ok": true
            }));
        });
        let tool = ListRepos { client: mock_client(&server) };
        let out = tool.execute_structured(serde_json::json!({})).await.unwrap();
        mock.assert();
        assert!(out.text.contains("testorg/lumina"), "text: {}", out.text);
        let s = out.structured.expect("structured payload present");
        assert_eq!(s["shown"], 1);
        assert_eq!(s["owner"], "testorg");
        let items = s["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["full_name"], "testorg/lumina");
        assert_eq!(items[0]["clone_url"], "http://example.com/testorg/lumina.git");
        assert_eq!(items[0]["default_branch"], "main");
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
                "updated": "2026-06-07T00:00:00Z"  // pii-test-fixture
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

    // ── EGJS-01: structuredContent ──────────────────────────────────────────

    #[tokio::test]
    async fn test_get_repo_execute_structured_carries_typed_repo() {
        let server = MockServer::start();
        server.mock(|when, then| {
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
                "updated": "2026-06-07T00:00:00Z"  // pii-test-fixture
            }));
        });

        let tool = GetRepo { client: mock_client(&server) };
        let output = tool
            .execute_structured(serde_json::json!({"repo": "lumina"}))
            .await
            .unwrap();
        assert!(output.text.contains("testorg/lumina"));
        let structured = output.structured.expect("expected structuredContent");
        assert_eq!(structured["full_name"], "testorg/lumina");
        assert_eq!(structured["default_branch"], "main");
        assert_eq!(structured["stars_count"], 3);
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

    // ── fetch_file_text (GiteaClient helper used by sentinel/vigil) ────────

    #[tokio::test]
    async fn test_fetch_file_text_decodes_base64() {
        let server = MockServer::start();
        let encoded = base64::engine::general_purpose::STANDARD.encode("status: ok\n");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/lumina-sentinel/contents/checks/latest-self-health.md");
            then.status(200).json_body(serde_json::json!({
                "type": "file",
                "encoding": "base64",
                "size": 11,
                "name": "latest-self-health.md",
                "path": "checks/latest-self-health.md",
                "content": encoded,
                "sha": "deadbeef",
                "url": "http://example.com",
                "html_url": "http://example.com"
            }));
        });

        let client = mock_client(&server);
        let text = client
            .fetch_file_text("lumina-sentinel", "checks/latest-self-health.md")
            .await
            .unwrap();
        mock.assert();
        assert_eq!(text, "status: ok\n");
    }

    #[tokio::test]
    async fn test_fetch_file_text_404_returns_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/lumina-vigil/contents/briefings/latest-morning.md");
            then.status(404).json_body(serde_json::json!({"message": "Not Found"}));
        });

        let client = mock_client(&server);
        let err = client
            .fetch_file_text("lumina-vigil", "briefings/latest-morning.md")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
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
                    "created_at": "2026-06-07T00:00:00Z",  // pii-test-fixture
                    "updated_at": "2026-06-07T00:00:00Z"  // pii-test-fixture
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

    #[tokio::test]
    async fn test_list_prs_execute_structured_carries_typed_items() {
        let server = MockServer::start();
        server.mock(|when, then| {
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
                    "created_at": "2026-06-07T00:00:00Z",  // pii-test-fixture
                    "updated_at": "2026-06-07T00:00:00Z"  // pii-test-fixture
                }
            ]));
        });

        let tool = ListPrs { client: mock_client(&server) };
        let output = tool
            .execute_structured(serde_json::json!({"repo": "myrepo"}))
            .await
            .unwrap();
        assert!(output.text.contains("#42"));
        let structured = output.structured.expect("expected structuredContent");
        assert_eq!(structured["items"][0]["number"], 42);
        assert_eq!(structured["items"][0]["head"]["ref"], "CHORD-07-gitea-tools");
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
                "created_at": "2026-06-07T00:00:00Z",  // pii-test-fixture
                "updated_at": "2026-06-07T00:00:00Z"  // pii-test-fixture
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
                    "commit": { "id": "abcdef1234567890", "message": "init", "timestamp": null },  // pii-test-fixture
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
                "ssh_url": "<email>:myorg/newrepo.git",  // pii-test-fixture
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
        assert_eq!(v["ssh_url"], "<email>:myorg/newrepo.git");  // pii-test-fixture
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
        assert!(msg.contains("GITEA_PAT"));
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

    // ── cargo publish: body framing ────────────────────────────────────────

    #[test]
    fn test_build_cargo_publish_body_exact_bytes() {
        // Known metadata `{}` (2 bytes) + crate payload `CRATE` (5 bytes) must
        // frame to: u32-LE(2) || "{}" || u32-LE(5) || "CRATE".
        let body = build_cargo_publish_body(b"{}", b"CRATE");
        let expected: Vec<u8> = vec![
            0x02, 0x00, 0x00, 0x00, // metadata length = 2, little-endian
            b'{', b'}', // metadata bytes
            0x05, 0x00, 0x00, 0x00, // crate length = 5, little-endian
            b'C', b'R', b'A', b'T', b'E', // crate bytes
        ];
        assert_eq!(body, expected, "publish frame must be exactly length-prefixed");
    }

    #[test]
    fn test_build_cargo_publish_body_empty_metadata_and_crate() {
        let body = build_cargo_publish_body(b"", b"");
        assert_eq!(body, vec![0, 0, 0, 0, 0, 0, 0, 0], "two zero-length u32 prefixes");
    }

    // ── cargo publish: metadata construction ───────────────────────────────

    #[test]
    fn test_build_cargo_metadata_minimal_defaults() {
        let m = build_cargo_metadata("terminus-rs", "1.2.0", None);
        assert_eq!(m["name"], serde_json::json!("terminus-rs"));
        assert_eq!(m["vers"], serde_json::json!("1.2.0"));
        // Empty defaults for every optional field.
        assert_eq!(m["deps"], serde_json::json!([]));
        assert_eq!(m["features"], serde_json::json!({}));
        assert_eq!(m["authors"], serde_json::json!([]));
        assert_eq!(m["keywords"], serde_json::json!([]));
        assert_eq!(m["categories"], serde_json::json!([]));
        assert_eq!(m["badges"], serde_json::json!({}));
        assert!(m["description"].is_null());
        assert!(m["license"].is_null());
        assert!(m["repository"].is_null());
    }

    #[test]
    fn test_build_cargo_metadata_merges_provided_and_forces_name_vers() {
        // Provided metadata carries real fields AND a stale name/vers that must
        // be overridden by the explicit arguments.
        let provided = serde_json::json!({
            "name": "WRONG-NAME",
            "vers": "9.9.9",
            "description": "a test crate",
            "license": "MIT",
            "repository": "http://gitea.example.com/moosenet/Terminus",
            "deps": [{ "name": "serde", "version_req": "^1" }]
        });
        let m = build_cargo_metadata("terminus-rs", "1.2.0", Some(&provided));
        // name/vers forced to explicit args, not the provided stale values.
        assert_eq!(m["name"], serde_json::json!("terminus-rs"));
        assert_eq!(m["vers"], serde_json::json!("1.2.0"));
        // Provided fields layered over defaults.
        assert_eq!(m["description"], serde_json::json!("a test crate"));
        assert_eq!(m["license"], serde_json::json!("MIT"));
        assert_eq!(m["deps"][0]["name"], serde_json::json!("serde"));
    }

    // ── cargo publish: dep registry defaulting (TERM-73) ────────────────────
    //
    // Regression coverage for the 1.3.0 publish defect: a dep with no
    // `registry` key must be indexed as crates.io, never left `null`
    // (`null`/omitted means "same registry as this crate" — the private
    // Gitea index — which crates.io deps can never resolve against).

    #[test]
    fn test_build_cargo_metadata_dep_without_registry_gets_crates_io_default() {
        let provided = serde_json::json!({
            "deps": [{ "name": "tokio", "version_req": "^1" }]
        });
        let m = build_cargo_metadata("terminus-rs", "1.3.1", Some(&provided));
        assert_eq!(
            m["deps"][0]["registry"],
            serde_json::json!(CRATES_IO_INDEX_URL),
            "a dep with no registry key must default to the crates.io index"
        );
    }

    #[test]
    fn test_build_cargo_metadata_dep_with_explicit_registry_is_untouched() {
        let provided = serde_json::json!({
            "deps": [{
                "name": "internal-crate",
                "version_req": "^1",
                "registry": "https://example.invalid/private-index"
            }]
        });
        let m = build_cargo_metadata("terminus-rs", "1.3.1", Some(&provided));
        assert_eq!(
            m["deps"][0]["registry"],
            serde_json::json!("https://example.invalid/private-index"),
            "an explicit registry value must be preserved, not overwritten"
        );
    }

    #[test]
    fn test_build_cargo_metadata_dep_with_explicit_null_registry_stays_null() {
        let provided = serde_json::json!({
            "deps": [{
                "name": "same-registry-dep",
                "version_req": "^1",
                "registry": Value::Null
            }]
        });
        let m = build_cargo_metadata("terminus-rs", "1.3.1", Some(&provided));
        assert!(
            m["deps"][0]["registry"].is_null(),
            "an explicit null registry (intra-gitea dep) must be left as null, not defaulted"
        );
    }

    #[test]
    fn test_build_cargo_metadata_empty_deps_stays_empty() {
        let provided = serde_json::json!({ "deps": [] });
        let m = build_cargo_metadata("terminus-rs", "1.3.1", Some(&provided));
        assert_eq!(m["deps"], serde_json::json!([]));
    }

    // ── cargo publish: HTTP behavior ───────────────────────────────────────

    fn write_temp_crate(bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join(format!("gcargo_test_{}.crate", uuid::Uuid::new_v4()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[tokio::test]
    async fn test_cargo_publish_correct_url_bearer_auth_and_success() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(PUT)
                // Cargo publish endpoint lives under /api/packages, not /api/v1.
                .path("/api/packages/testorg/cargo/api/v1/crates/new")
                // Terminus's own token, sent with Gitea's PAT `token` scheme.
                .header("Authorization", "token test-token");
            then.status(200)
                .json_body(serde_json::json!({ "warnings": { "other": [] } }));
        });

        let tmp = write_temp_crate(b"fake-crate-bytes");
        let tool = CargoPublish { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({
                "crate_path": tmp.to_str().unwrap(),
                "name": "terminus-rs",
                "version": "1.2.0",
                "metadata": { "deps": [], "features": {} }
            }))
            .await
            .unwrap();
        std::fs::remove_file(&tmp).ok();

        mock.assert();
        assert!(result.contains("terminus-rs"));
        assert!(result.contains("1.2.0"));
        assert!(result.contains("/testorg/-/packages/cargo/terminus-rs/1.2.0"));
        assert!(result.contains("\"published\":true"));
    }

    #[tokio::test]
    async fn test_cargo_publish_owner_override() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/api/packages/otherorg/cargo/api/v1/crates/new");
            then.status(200).json_body(serde_json::json!({}));
        });

        let tmp = write_temp_crate(b"bytes");
        let tool = CargoPublish { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({
                "crate_path": tmp.to_str().unwrap(),
                "name": "foo",
                "version": "0.1.0",
                "owner": "otherorg",
                "metadata": {}
            }))
            .await
            .unwrap();
        std::fs::remove_file(&tmp).ok();
        mock.assert();
        assert!(result.contains("/otherorg/-/packages/cargo/foo/0.1.0"));
    }

    #[tokio::test]
    async fn test_cargo_publish_403_surfaces_write_package_scope() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(PUT);
            then.status(403).body("permission denied");
        });

        let tmp = write_temp_crate(b"bytes");
        let tool = CargoPublish { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({
                "crate_path": tmp.to_str().unwrap(),
                "name": "foo",
                "version": "1.0.0",
                "metadata": {}
            }))
            .await
            .unwrap_err();
        std::fs::remove_file(&tmp).ok();
        let msg = err.to_string();
        assert!(msg.contains("403"), "should surface the 403");
        assert!(msg.contains("write:package"), "should name the missing scope");
    }

    #[tokio::test]
    async fn test_cargo_publish_409_already_exists() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(PUT);
            then.status(409).body("crate version already exists");
        });

        let tmp = write_temp_crate(b"bytes");
        let tool = CargoPublish { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({
                "crate_path": tmp.to_str().unwrap(),
                "name": "foo",
                "version": "1.0.0",
                "metadata": {}
            }))
            .await
            .unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_cargo_publish_missing_crate_file() {
        let server = MockServer::start();
        let tool = CargoPublish { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({
                "crate_path": "/definitely/not/here/nope.crate",
                "name": "foo",
                "version": "1.0.0",
                "metadata": {}
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to access .crate file"));
    }

    #[tokio::test]
    async fn test_cargo_publish_requires_metadata() {
        let server = MockServer::start();
        let tmp = write_temp_crate(b"bytes");
        let tool = CargoPublish { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({
                "crate_path": tmp.to_str().unwrap(),
                "name": "foo",
                "version": "1.0.0"
            }))
            .await
            .unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(err.to_string().contains("'metadata'"));
    }

    // ── cargo publish: crate_path guard (resolve_crate_path) ───────────────

    #[test]
    fn test_resolve_crate_path_rejects_non_crate_extension() {
        let tmp = std::env::temp_dir()
            .join(format!("gcargo_notcrate_{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, b"data").unwrap();
        let err = resolve_crate_path(tmp.to_str().unwrap(), DEFAULT_MAX_CRATE_BYTES, None)
            .unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(err.to_string().contains("must point to a .crate file"));
    }

    #[test]
    fn test_resolve_crate_path_rejects_missing_file() {
        let err = resolve_crate_path("/no/such/thing.crate", DEFAULT_MAX_CRATE_BYTES, None)
            .unwrap_err();
        assert!(err.to_string().contains("Failed to access .crate file"));
    }

    #[test]
    fn test_resolve_crate_path_rejects_directory() {
        // A directory renamed with a .crate suffix must still be refused.
        let dir = std::env::temp_dir()
            .join(format!("gcargo_dir_{}.crate", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let err = resolve_crate_path(dir.to_str().unwrap(), DEFAULT_MAX_CRATE_BYTES, None)
            .unwrap_err();
        std::fs::remove_dir(&dir).ok();
        assert!(err.to_string().contains("not a regular file"));
    }

    #[test]
    fn test_resolve_crate_path_rejects_oversized() {
        let tmp = std::env::temp_dir()
            .join(format!("gcargo_big_{}.crate", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, vec![0u8; 4096]).unwrap();
        // max_bytes below the file size → refused before any read.
        let err = resolve_crate_path(tmp.to_str().unwrap(), 1024, None).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(err.to_string().contains("exceeding"));
    }

    #[test]
    fn test_resolve_crate_path_jail_allows_inside_and_rejects_outside() {
        let root = std::env::temp_dir()
            .join(format!("gcargo_jail_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();

        // Inside the jail → accepted.
        let inside = root.join("pkg.crate");
        std::fs::write(&inside, b"data").unwrap();
        let ok = resolve_crate_path(
            inside.to_str().unwrap(),
            DEFAULT_MAX_CRATE_BYTES,
            Some(root.to_str().unwrap()),
        );
        assert!(ok.is_ok(), "file inside the artifact dir should be allowed");

        // Outside the jail → refused.
        let outside = std::env::temp_dir()
            .join(format!("gcargo_outside_{}.crate", uuid::Uuid::new_v4()));
        std::fs::write(&outside, b"data").unwrap();
        let err = resolve_crate_path(
            outside.to_str().unwrap(),
            DEFAULT_MAX_CRATE_BYTES,
            Some(root.to_str().unwrap()),
        )
        .unwrap_err();

        std::fs::remove_file(&inside).ok();
        std::fs::remove_dir(&root).ok();
        std::fs::remove_file(&outside).ok();
        assert!(err.to_string().contains("outside the permitted artifact directory"));
    }

    #[test]
    fn test_is_valid_owner_segment() {
        assert!(is_valid_owner_segment("moosenet"));
        assert!(is_valid_owner_segment("my-org_1.x"));
        assert!(!is_valid_owner_segment(""));
        assert!(!is_valid_owner_segment("."));
        assert!(!is_valid_owner_segment(".."));
        assert!(!is_valid_owner_segment("../other"));
        assert!(!is_valid_owner_segment("a/b"));
        assert!(!is_valid_owner_segment("a\\b"));
        assert!(!is_valid_owner_segment("org space"));
    }

    #[tokio::test]
    async fn test_cargo_publish_rejects_traversal_owner() {
        let server = MockServer::start();
        let tmp = write_temp_crate(b"bytes");
        let tool = CargoPublish { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({
                "crate_path": tmp.to_str().unwrap(),
                "name": "foo",
                "version": "1.0.0",
                "owner": "../../secret-org"
            }))
            .await
            .unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(err.to_string().contains("Invalid registry owner"));
    }

    #[test]
    fn test_read_bounded_crate_enforces_limit_during_read() {
        let tmp = std::env::temp_dir()
            .join(format!("gcargo_bounded_{}.crate", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, vec![7u8; 4096]).unwrap();

        // Within the limit → full bytes returned.
        let ok = read_bounded_crate(&tmp, tmp.to_str().unwrap(), 8192, None).unwrap();
        assert_eq!(ok.len(), 4096);

        // Below the size → refused without buffering more than max_bytes+1.
        let err = read_bounded_crate(&tmp, tmp.to_str().unwrap(), 1024, None).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn test_read_bounded_crate_jail_rejects_outside_via_open_handle() {
        // A file outside the jail must be refused by the fd-based containment
        // check, not merely by the earlier path-level pre-check.
        let root = std::env::temp_dir()
            .join(format!("gcargo_rbjail_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let outside = std::env::temp_dir()
            .join(format!("gcargo_rboutside_{}.crate", uuid::Uuid::new_v4()));
        std::fs::write(&outside, b"data").unwrap();

        let err = read_bounded_crate(
            &outside,
            outside.to_str().unwrap(),
            DEFAULT_MAX_CRATE_BYTES,
            Some(root.to_str().unwrap()),
        )
        .unwrap_err();

        // A file inside the jail is accepted.
        let inside = root.join("ok.crate");
        std::fs::write(&inside, b"data").unwrap();
        let ok = read_bounded_crate(
            &inside,
            inside.to_str().unwrap(),
            DEFAULT_MAX_CRATE_BYTES,
            Some(root.to_str().unwrap()),
        );

        std::fs::remove_file(&outside).ok();
        std::fs::remove_file(&inside).ok();
        std::fs::remove_dir(&root).ok();
        assert!(err.to_string().contains("outside the permitted artifact directory"));
        assert!(ok.is_ok(), "file inside the jail should be read");
    }

    #[tokio::test]
    async fn test_cargo_publish_requires_name_and_version() {
        let server = MockServer::start();
        let tmp = write_temp_crate(b"bytes");
        let tool = CargoPublish { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({ "crate_path": tmp.to_str().unwrap(), "version": "1.0.0" }))
            .await
            .unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(err.to_string().contains("'name' is required"));
    }

    #[tokio::test]
    async fn test_cargo_publish_never_leaks_token_in_error() {
        // Token sourced from the GiteaClient (populated at startup from the
        // runtime secret store, never read raw here). On failure it must NEVER
        // appear in the surfaced error.
        let secret_token = "<REDACTED-SECRET>"; // pii-test-fixture
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(PUT);
            then.status(500).body("internal error");
        });
        let client = GiteaClient {
            http: Client::new(),
            base_url: server.base_url(),
            token: secret_token.to_string(),
            identity_name: Some("moose".to_string()),
            identities: Arc::new(HashMap::new()),
            owner: "testorg".to_string(),
        };
        let tmp = write_temp_crate(b"bytes");
        let tool = CargoPublish { client };
        let err = tool
            .execute(serde_json::json!({
                "crate_path": tmp.to_str().unwrap(),
                "name": "foo",
                "version": "1.0.0",
                "metadata": {}
            }))
            .await
            .unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(
            !err.to_string().contains(secret_token),
            "token must never appear in an error message"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_register_adds_cargo_publish_with_url() {
        let url_backup = std::env::var("GITEA_URL").ok();
        std::env::set_var("GITEA_URL", "http://example.com");
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        if let Some(v) = url_backup { std::env::set_var("GITEA_URL", v); } else { std::env::remove_var("GITEA_URL"); }
        assert!(reg.contains("gitea_cargo_publish"));
    }

    // ── gitea_cargo_yank ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_cargo_yank_correct_url_method_and_bearer_auth() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/packages/testorg/cargo/api/v1/crates/terminus-rs/1.3.0/yank")
                .header("Authorization", "token test-token");
            then.status(200).json_body(serde_json::json!({ "ok": true }));
        });

        let tool = CargoYank { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({ "crate": "terminus-rs", "version": "1.3.0" }))
            .await
            .unwrap();

        mock.assert();
        assert!(result.contains("\"action\":\"yank\""));
        assert!(result.contains("\"yanked\":true"));
        assert!(result.contains("terminus-rs"));
        assert!(result.contains("1.3.0"));
    }

    #[tokio::test]
    async fn test_cargo_unyank_uses_put_and_clears_yank() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/api/packages/testorg/cargo/api/v1/crates/terminus-rs/1.3.0/unyank")
                .header("Authorization", "token test-token");
            then.status(200).json_body(serde_json::json!({ "ok": true }));
        });

        let tool = CargoYank { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({
                "crate": "terminus-rs",
                "version": "1.3.0",
                "unyank": true
            }))
            .await
            .unwrap();

        mock.assert();
        assert!(result.contains("\"action\":\"unyank\""));
        assert!(result.contains("\"yanked\":false"));
    }

    #[tokio::test]
    async fn test_cargo_yank_owner_override() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/packages/otherorg/cargo/api/v1/crates/foo/0.1.0/yank");
            then.status(200).json_body(serde_json::json!({}));
        });

        let tool = CargoYank { client: mock_client(&server) };
        let result = tool
            .execute(serde_json::json!({
                "crate": "foo",
                "version": "0.1.0",
                "owner": "otherorg"
            }))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("\"owner\":\"otherorg\""));
    }

    #[tokio::test]
    async fn test_cargo_yank_requires_crate_and_version() {
        let server = MockServer::start();
        let tool = CargoYank { client: mock_client(&server) };

        let err = tool
            .execute(serde_json::json!({ "version": "1.0.0" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'crate' is required"));

        let err = tool
            .execute(serde_json::json!({ "crate": "foo" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'version' is required"));
    }

    #[tokio::test]
    async fn test_cargo_yank_rejects_traversal_owner_crate_and_version() {
        let server = MockServer::start();
        let tool = CargoYank { client: mock_client(&server) };

        let err = tool
            .execute(serde_json::json!({
                "crate": "foo",
                "version": "1.0.0",
                "owner": "../../secret-org"
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid registry owner"));

        let err = tool
            .execute(serde_json::json!({ "crate": "../foo", "version": "1.0.0" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid crate name"));

        let err = tool
            .execute(serde_json::json!({ "crate": "foo", "version": "../1.0.0" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid version"));
    }

    #[tokio::test]
    async fn test_cargo_yank_404_missing_crate_or_version() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(DELETE);
            then.status(404).body("not found");
        });

        let tool = CargoYank { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({ "crate": "nope", "version": "9.9.9" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        assert!(err.to_string().contains("was not found"));
    }

    #[tokio::test]
    async fn test_cargo_yank_403_surfaces_write_package_scope() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(DELETE);
            then.status(403).body("permission denied");
        });

        let tool = CargoYank { client: mock_client(&server) };
        let err = tool
            .execute(serde_json::json!({ "crate": "foo", "version": "1.0.0" }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("write:package"));
    }

    #[tokio::test]
    async fn test_cargo_yank_never_leaks_token_in_error() {
        let secret_token = "<REDACTED-SECRET>"; // pii-test-fixture
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(DELETE);
            then.status(500).body("internal error");
        });
        let client = GiteaClient {
            http: Client::new(),
            base_url: server.base_url(),
            token: secret_token.to_string(),
            identity_name: Some("moose".to_string()),
            identities: Arc::new(HashMap::new()),
            owner: "testorg".to_string(),
        };
        let tool = CargoYank { client };
        let err = tool
            .execute(serde_json::json!({ "crate": "foo", "version": "1.0.0" }))
            .await
            .unwrap_err();
        assert!(
            !err.to_string().contains(secret_token),
            "token must never appear in an error message"
        );
    }

    #[tokio::test]
    async fn test_cargo_yank_uses_resolved_identity_token() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/packages/testorg/cargo/api/v1/crates/foo/0.1.0/yank")
                .header("Authorization", "token tok-harmony");
            then.status(200).json_body(serde_json::json!({}));
        });
        let client = mock_client_with_identities(
            &server,
            "moose",
            &[("moose", "tok-moose"), ("harmony", "tok-harmony")], // pii-test-fixture
        );
        let tool = CargoYank { client };
        let result = tool
            .execute(serde_json::json!({
                "crate": "foo",
                "version": "0.1.0",
                "identity": "harmony"
            }))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("\"yanked\":true"));
    }

    #[test]
    #[serial_test::serial]
    fn test_register_adds_cargo_yank_with_url() {
        let url_backup = std::env::var("GITEA_URL").ok();
        std::env::set_var("GITEA_URL", "http://example.com");
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        if let Some(v) = url_backup { std::env::set_var("GITEA_URL", v); } else { std::env::remove_var("GITEA_URL"); }
        assert!(reg.contains("gitea_cargo_yank"));
    }

    #[test]
    #[serial_test::serial]
    fn test_register_adds_cargo_yank_stub_when_not_configured() {
        let url_backup = std::env::var("GITEA_URL").ok();
        std::env::remove_var("GITEA_URL");
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        if let Some(v) = url_backup { std::env::set_var("GITEA_URL", v); }
        assert!(reg.contains("gitea_cargo_yank"));
    }

    // ── GPAT (S105): multi-identity (GITEA_PAT_<NAME>) ─────────────────────────
    //
    // These mirror the Plane PPAT tests. Env-var tests run #[serial] and clear
    // the relevant keys before AND after, since env mutation is process-global.

    const GPAT_TEST_ENV_KEYS: &[&str] = &[
        "GITEA_URL",
        "GITEA_OWNER",
        "GITEA_IDENTITY_NAME",
        "GITEA_PAT_MOOSE",
        "GITEA_PAT_HARMONY",
        "GITEA_PAT_LUMINA",
        "GITEA_PAT_BLANK",
    ];

    fn clear_gpat_env() {
        for k in GPAT_TEST_ENV_KEYS {
            std::env::remove_var(k);
        }
    }

    // 1. Identity scan: GITEA_PAT_<NAME> vars populate the identities map
    //    (lowercased), a blank value is treated as absent, and no unrelated key
    //    is imported.
    #[test]
    #[serial_test::serial]
    fn test_from_env_scans_gitea_pat_identities() {
        clear_gpat_env();
        std::env::set_var("GITEA_URL", "http://example.com");
        std::env::set_var("GITEA_PAT_MOOSE", "tok-moose"); // pii-test-fixture
        std::env::set_var("GITEA_PAT_HARMONY", "tok-harmony"); // pii-test-fixture
        std::env::set_var("GITEA_PAT_LUMINA", "tok-lumina"); // pii-test-fixture
        std::env::set_var("GITEA_PAT_BLANK", ""); // set-but-empty → absent

        let client = GiteaClient::from_env().unwrap();
        let mut names = client.identity_names();
        names.sort();
        assert_eq!(names, vec!["harmony", "lumina", "moose"]);
        // A blank PAT is never registered.
        assert!(client.for_identity("blank").is_err());

        clear_gpat_env();
    }

    // 2. Default identity is MOOSE (differs from Plane's lumina) when
    //    GITEA_IDENTITY_NAME is unset — the active-default token IS moose's.
    #[test]
    #[serial_test::serial]
    fn test_default_identity_is_moose() {
        clear_gpat_env();
        std::env::set_var("GITEA_URL", "http://example.com");
        std::env::set_var("GITEA_PAT_MOOSE", "tok-moose"); // pii-test-fixture
        std::env::set_var("GITEA_PAT_LUMINA", "tok-lumina"); // pii-test-fixture

        let client = GiteaClient::from_env().unwrap();
        assert_eq!(client.identity_name(), Some("moose"));
        assert_eq!(client.token, "tok-moose");

        clear_gpat_env();
    }

    // 3. GITEA_IDENTITY_NAME selects the active-default identity's token.
    #[test]
    #[serial_test::serial]
    fn test_gitea_identity_name_selects_default_token() {
        clear_gpat_env();
        std::env::set_var("GITEA_URL", "http://example.com");
        std::env::set_var("GITEA_IDENTITY_NAME", "Harmony"); // case-insensitive
        std::env::set_var("GITEA_PAT_MOOSE", "tok-moose"); // pii-test-fixture
        std::env::set_var("GITEA_PAT_HARMONY", "tok-harmony"); // pii-test-fixture

        let client = GiteaClient::from_env().unwrap();
        assert_eq!(client.identity_name(), Some("harmony"));
        assert_eq!(client.token, "tok-harmony");

        clear_gpat_env();
    }

    // 4. gitea_list_identities: 3 identities, sorted, active_default present,
    //    and NO token value ever appears in the output (no-leak).
    #[tokio::test]
    async fn test_gitea_list_identities_lists_names_no_value_leak() {
        let server = MockServer::start();
        let client = mock_client_with_identities(
            &server,
            "moose",
            &[
                ("moose", "SECRET-MOOSE-TOKEN"),     // pii-test-fixture
                ("harmony", "SECRET-HARMONY-TOKEN"), // pii-test-fixture
                ("lumina", "SECRET-LUMINA-TOKEN"),   // pii-test-fixture
            ],
        );
        let tool = GiteaListIdentities { client };
        let out = tool.execute(serde_json::json!({})).await.unwrap();

        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["identities"],
            serde_json::json!(["harmony", "lumina", "moose"])
        );
        assert_eq!(parsed["count"], 3);
        assert_eq!(parsed["active_default"], "moose");
        assert_eq!(parsed["prefix"], "GITEA_PAT_");
        // No token value may leak into the listing output.
        for secret in ["SECRET-MOOSE-TOKEN", "SECRET-HARMONY-TOKEN", "SECRET-LUMINA-TOKEN"] {
            assert!(!out.contains(secret), "identity listing leaked a token value");
        }
    }

    // 5. resolve_identity dispatch: the optional `identity` arg selects that
    //    identity's token for the request. We assert the request carried the
    //    SELECTED identity's `Authorization: token <pat>` header, not the
    //    default's — proving the arg is threaded through a CRUD tool.
    #[tokio::test]
    async fn test_identity_arg_dispatches_selected_token() {
        let server = MockServer::start();
        // Endpoint accepts ONLY harmony's token.
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/somerepo")
                .header("Authorization", "token tok-harmony");
            then.status(200).json_body(serde_json::json!({
                "id": 1,
                "name": "somerepo",
                "full_name": "testorg/somerepo",
                "description": "",
                "html_url": "http://example.com/testorg/somerepo",
                "clone_url": "http://example.com/testorg/somerepo.git",
                "default_branch": "main",
                "private": true,
                "stars_count": 0,
                "forks_count": 0,
                "open_issues_count": 0,
                "updated": null
            }));
        });
        let client = mock_client_with_identities(
            &server,
            "moose",
            &[("moose", "tok-moose"), ("harmony", "tok-harmony")], // pii-test-fixture
        );
        let tool = GetRepo { client };
        let result = tool
            .execute(serde_json::json!({ "repo": "somerepo", "identity": "harmony" }))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("testorg/somerepo"));
    }

    // 5b. resolve_identity default path: with NO `identity` arg, the request
    //     carries the active-default (moose) token.
    #[tokio::test]
    async fn test_default_identity_used_when_no_identity_arg() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/r2")
                .header("Authorization", "token tok-moose");
            then.status(200).json_body(serde_json::json!({
                "id": 2, "name": "r2", "full_name": "testorg/r2", "description": "",
                "html_url": "http://example.com/r2", "clone_url": "http://example.com/r2.git",
                "default_branch": "main", "private": true, "stars_count": 0,
                "forks_count": 0, "open_issues_count": 0, "updated": null
            }));
        });
        let client = mock_client_with_identities(
            &server,
            "moose",
            &[("moose", "tok-moose"), ("harmony", "tok-harmony")], // pii-test-fixture
        );
        let tool = GetRepo { client };
        tool.execute(serde_json::json!({ "repo": "r2" })).await.unwrap();
        mock.assert();
    }

    // 6. Unknown identity → InvalidArgument (from resolve_identity), before any
    //    network call. Also proves cargo_publish is wired to resolve_identity.
    #[tokio::test]
    async fn test_unknown_identity_is_rejected() {
        let server = MockServer::start();
        let client = mock_client_with_identities(
            &server,
            "moose",
            &[("moose", "tok-moose")], // pii-test-fixture
        );
        let tool = GetRepo { client };
        let err = tool
            .execute(serde_json::json!({ "repo": "r", "identity": "ghost" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("GITEA_PAT_GHOST"));
    }

    // 7. gitea_cargo_publish uses the RESOLVED identity's token: publishing with
    //    identity=harmony must authenticate with harmony's PAT.
    #[tokio::test]
    async fn test_cargo_publish_uses_resolved_identity_token() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/api/packages/testorg/cargo/api/v1/crates/new")
                .header("Authorization", "token tok-harmony");
            then.status(200).json_body(serde_json::json!({}));
        });
        let client = mock_client_with_identities(
            &server,
            "moose",
            &[("moose", "tok-moose"), ("harmony", "tok-harmony")], // pii-test-fixture
        );
        let tmp = write_temp_crate(b"crate-bytes");
        let tool = CargoPublish { client };
        let result = tool
            .execute(serde_json::json!({
                "crate_path": tmp.to_str().unwrap(),
                "name": "foo",
                "version": "0.1.0",
                "identity": "harmony",
                "metadata": {}
            }))
            .await
            .unwrap();
        std::fs::remove_file(&tmp).ok();
        mock.assert();
        assert!(result.contains("\"published\":true"));
    }

    // 8. Backward-compat / no-leak: a client with no GITEA_PAT_* configured still
    //    constructs (URL only) with an empty default token, lists zero
    //    identities, and its Debug output never reveals the token.
    #[test]
    #[serial_test::serial]
    fn test_no_identities_configured_is_empty_and_debug_redacts() {
        clear_gpat_env();
        std::env::set_var("GITEA_URL", "http://example.com");

        let client = GiteaClient::from_env().unwrap();
        assert!(client.identity_names().is_empty());
        assert_eq!(client.identity_name(), Some("moose")); // default name still set
        assert_eq!(client.token, ""); // no GITEA_PAT_MOOSE → empty token

        // Debug must never print a real token; with a token set, it is redacted.
        let with_tok = GiteaClient {
            token: "<REDACTED-SECRET>".to_string(), // pii-test-fixture
            ..client.clone()
        };
        let dbg = format!("{with_tok:?}");
        assert!(!dbg.contains("SUPER-SECRET"), "Debug leaked the token");
        assert!(dbg.contains("<redacted>"));

        clear_gpat_env();
    }

    // ── EGJS-02: structuredContent on existing write tools ─────────────────

    #[tokio::test]
    async fn test_create_file_structured_content_has_commit_sha() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/repos/testorg/myrepo/contents/README.md");
            then.status(201).json_body(serde_json::json!({
                "content": null,
                "commit": {"sha": "abc123", "url": "http://example.com", "html_url": "http://example.com", "message": "init"}
            }));
        });
        let tool = CreateFile { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({
            "repo": "myrepo", "path": "README.md", "content": "# Hello world", "message": "init"
        })).await.unwrap();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["commit"]["sha"], "abc123");
    }

    #[tokio::test]
    async fn test_update_file_structured_content_has_commit_sha() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/contents/README.md");
            then.status(200).json_body(serde_json::json!({
                "type": "file", "encoding": "base64", "size": 5, "name": "README.md",
                "path": "README.md", "content": "aGVsbG8=", "sha": "oldsha",
                "url": "http://example.com", "html_url": "http://example.com"
            }));
        });
        server.mock(|when, then| {
            when.method(PUT)
                .path("/api/v1/repos/testorg/myrepo/contents/README.md");
            then.status(200).json_body(serde_json::json!({
                "content": null,
                "commit": {"sha": "def456", "url": "http://example.com", "html_url": "http://example.com", "message": "update"}
            }));
        });
        let tool = UpdateFile { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({
            "repo": "myrepo", "path": "README.md", "content": "new content", "message": "update"
        })).await.unwrap();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["commit"]["sha"], "def456");
    }

    #[tokio::test]
    async fn test_delete_file_structured_content() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/contents/stale.md");
            then.status(200).json_body(serde_json::json!({
                "type": "file", "encoding": "base64", "size": 5, "name": "stale.md",
                "path": "stale.md", "content": "aGVsbG8=", "sha": "delsha",
                "url": "http://example.com", "html_url": "http://example.com"
            }));
        });
        server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/v1/repos/testorg/myrepo/contents/stale.md");
            then.status(200).json_body(serde_json::json!({}));
        });
        let tool = DeleteFile { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({
            "repo": "myrepo", "path": "stale.md", "message": "remove"
        })).await.unwrap();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["deleted"], true);
        assert_eq!(structured["path"], "stale.md");
        assert_eq!(structured["sha"], "delsha");
    }

    #[tokio::test]
    async fn test_create_repo_structured_content() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/v1/orgs/moosenet/repos");
            then.status(201).json_body(serde_json::json!({
                "full_name": "moosenet/new-tool",
                "html_url": "http://example.com/moosenet/new-tool",
                "clone_url": "http://example.com/moosenet/new-tool.git",
                "ssh_url": "<email>:moosenet/new-tool.git" // pii-test-fixture
            }));
        });
        let tool = CreateRepo { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({
            "org": "moosenet", "name": "new-tool"
        })).await.unwrap();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["full_name"], "moosenet/new-tool");
    }

    #[tokio::test]
    async fn test_read_file_structured_content_has_base64() {
        let server = MockServer::start();
        let encoded = base64::engine::general_purpose::STANDARD.encode("Hello, Gitea!");
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/contents/hello.txt");
            then.status(200).json_body(serde_json::json!({
                "type": "file", "encoding": "base64", "size": 13, "name": "hello.txt",
                "path": "hello.txt", "content": encoded, "sha": "deadbeef",
                "url": "http://example.com", "html_url": "http://example.com"
            }));
        });
        let tool = ReadFile { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({"repo": "myrepo", "path": "hello.txt"})).await.unwrap();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["content"], "Hello, Gitea!");
        assert_eq!(structured["content_base64"], encoded);
    }

    // ── EGJS-02: new tools ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_create_branch_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/repos/testorg/myrepo/branches")
                .json_body(serde_json::json!({"new_branch_name": "feature/x", "old_branch_name": "main"}));
            then.status(201).json_body(serde_json::json!({
                "name": "feature/x",
                "commit": {"id": "abcdef1234567890", "message": "init", "timestamp": "2026-01-01T00:00:00Z"},
                "protected": false
            }));
        });
        let tool = CreateBranch { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({
            "repo": "myrepo", "branch": "feature/x", "old_branch": "main"
        })).await.unwrap();
        mock.assert();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["name"], "feature/x");
        assert!(output.text.contains("feature/x"));
    }

    #[tokio::test]
    async fn test_create_branch_requires_repo_and_branch() {
        let server = MockServer::start();
        let tool = CreateBranch { client: mock_client(&server) };
        let err = tool.execute(serde_json::json!({"repo": "myrepo"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_delete_branch_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/v1/repos/testorg/myrepo/branches/feature/x");
            then.status(204);
        });
        let tool = DeleteBranch { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({
            "repo": "myrepo", "branch": "feature/x"
        })).await.unwrap();
        mock.assert();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["deleted"], true);
    }

    #[tokio::test]
    async fn test_delete_branch_404_returns_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/v1/repos/testorg/myrepo/branches/ghost");
            then.status(404).json_body(serde_json::json!({"message": "Not Found"}));
        });
        let tool = DeleteBranch { client: mock_client(&server) };
        let err = tool.execute(serde_json::json!({"repo": "myrepo", "branch": "ghost"})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_close_pr_correct_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH)
                .path("/api/v1/repos/testorg/myrepo/pulls/5")
                .json_body(serde_json::json!({"state": "closed"}));
            then.status(200).json_body(serde_json::json!({
                "id": 1, "number": 5, "state": "closed", "title": "Some PR", "body": null,
                "html_url": "http://example.com/pulls/5",
                "user": {"login": "moose", "full_name": null},
                "head": {"label": "h", "ref": "feature", "sha": "abc", "repo": null},
                "base": {"label": "b", "ref": "main", "sha": "def", "repo": null},
                "mergeable": null, "merged": false,
                "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
            }));
        });
        let tool = ClosePr { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({"repo": "myrepo", "pr": 5})).await.unwrap();
        mock.assert();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["state"], "closed");
        assert_eq!(structured["number"], 5);
    }

    #[tokio::test]
    async fn test_close_pr_404_returns_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::PATCH)
                .path("/api/v1/repos/testorg/myrepo/pulls/99");
            then.status(404).json_body(serde_json::json!({"message": "Not Found"}));
        });
        let tool = ClosePr { client: mock_client(&server) };
        let err = tool.execute(serde_json::json!({"repo": "myrepo", "pr": 99})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_get_pr_diff_returns_raw_diff_text() {
        let server = MockServer::start();
        let diff_text = "diff --git a/foo b/foo\n--- a/foo\n+++ b/foo\n@@ -1 +1 @@\n-old\n+new\n";
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/pulls/7.diff");
            then.status(200).body(diff_text);
        });
        let tool = GetPrDiff { client: mock_client(&server) };
        let output = tool.execute_structured(serde_json::json!({"repo": "myrepo", "pr": 7})).await.unwrap();
        mock.assert();
        let structured = output.structured.expect("structuredContent must be present");
        assert_eq!(structured["diff"], diff_text);
        assert!(output.text.contains("old"));
    }

    #[tokio::test]
    async fn test_get_pr_diff_404_returns_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/testorg/myrepo/pulls/404.diff");
            then.status(404).body("Not Found");
        });
        let tool = GetPrDiff { client: mock_client(&server) };
        let err = tool.execute(serde_json::json!({"repo": "myrepo", "pr": 404})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[test]
    fn test_new_tools_expose_optional_identity_param() {
        let server = MockServer::start();
        let client = mock_client(&server);
        for schema in [
            CreateBranch { client: client.clone() }.parameters(),
            DeleteBranch { client: client.clone() }.parameters(),
            ClosePr { client: client.clone() }.parameters(),
            GetPrDiff { client }.parameters(),
        ] {
            assert_eq!(schema["properties"]["identity"]["type"], "string");
            let required = schema["required"].as_array().unwrap();
            assert!(!required.iter().any(|r| r == "identity"));
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_register_configured_registers_new_tools() {
        clear_gpat_env();
        std::env::set_var("GITEA_URL", "http://example.com");
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        let names: Vec<String> = registry.list().into_iter().map(|t| t.name).collect();
        for name in [
            "gitea_create_branch", "gitea_delete_branch", "gitea_close_pr", "gitea_get_pr_diff",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} must be registered");
        }
        std::env::remove_var("GITEA_URL");
    }

    // ── S111E/MIRR-04: merge → sync-source non-fatal hook ───────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn merge_pr_succeeds_even_when_sync_source_is_unconfigured() {
        // sync-source's underlying gitea_token() reads GITEA_URL/GITEA_PAT_* from
        // the REAL process environment (independent of the mock `GiteaClient`
        // injected into the tool below), so clearing them here reproduces "this
        // host has no TERMINUS_MIRROR_SOURCE_ROOT / Gitea credential configured
        // for the mirror engine" — exactly the failure the post-merge hook must
        // swallow (log + continue) rather than propagate, since the merge itself
        // already succeeded on Gitea before the hook ever runs.
        let url_backup = std::env::var("GITEA_URL").ok();
        let root_backup = std::env::var("TERMINUS_MIRROR_SOURCE_ROOT").ok();
        std::env::remove_var("GITEA_URL");
        std::env::remove_var("TERMINUS_MIRROR_SOURCE_ROOT");

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/repos/testorg/myrepo/pulls/7/merge");
            then.status(200);
        });
        let tool = MergePr { client: mock_client(&server) };
        let result = tool.execute(serde_json::json!({"repo": "myrepo", "pr": 7})).await;

        if let Some(v) = url_backup { std::env::set_var("GITEA_URL", v); } else { std::env::remove_var("GITEA_URL"); }
        if let Some(v) = root_backup { std::env::set_var("TERMINUS_MIRROR_SOURCE_ROOT", v); } else { std::env::remove_var("TERMINUS_MIRROR_SOURCE_ROOT"); }

        mock.assert();
        let result = result.expect("merge must succeed even though sync-source is unconfigured");
        assert!(result.contains("merged into"), "unexpected result: {result}");
    }
}
