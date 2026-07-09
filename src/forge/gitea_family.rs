//! Gitea-family forge adapter (S106 / GITX-02).
//!
//! ONE Gitea-compatible-REST-API adapter implementing the [`ForgeProvider`]
//! trait (GITX-01), parameterised by base-URL + credentials, serving THREE
//! providers that all speak the Gitea REST v1 API:
//!
//! - **`gitea`** (git-private) — the operator's self-hosted source-of-truth
//!   forge. Uses the S105/GPAT `GITEA_PAT_<NAME>` multi-identity model
//!   (`GITEA_URL` + per-identity tokens, default identity `moose`), reusing the
//!   existing [`GiteaClient`] wholesale so the concrete `gitea_*` tools and this
//!   adapter share one client, one token store, and one auth scheme.
//! - **`forgejo`** (git-private) — a Gitea-compatible self-hosted forge. Single
//!   credential: `FORGEJO_URL` + `FORGEJO_TOKEN`.
//! - **`codeberg`** (git-public) — the recommended public target (Forgejo
//!   lineage, non-profit/EU). Single credential: `CODEBERG_TOKEN`, base URL
//!   defaults to Codeberg's host and is overridable via `CODEBERG_URL`.
//!
//! The three differ ONLY by base URL + credential source; the wire protocol is
//! identical, so a single adapter drives all of them. Config selects which
//! provider a constructor builds; nothing about the endpoint dispatch branches
//! on provider. This is the "one Gitea-compatible client" the S106 provider
//! list calls for (Gitea/Forgejo/Codeberg = one client).
//!
//! ## What this item is (and is NOT)
//! This is the adapter + its trait impl + its capability map only. The
//! git-private / git-public MCP *tools* (provider routing, posture enforcement,
//! the PII gate on public writes) are assembled later in GITX-05. The existing
//! `gitea_*` tools remain registered and unchanged — this adapter is additive.
//!
//! ## Secrets
//! Credentials are referenced by their runtime secret KEY NAME and resolved from
//! the process environment materialised by the runtime secret store
//! (`GITEA_PAT_<NAME>` / `FORGEJO_TOKEN` / `CODEBERG_TOKEN`) — never a literal in
//! source. Token values are `.trim()`-ed on the way in (see
//! [`crate::gitea::GiteaClient::with_token`] and `scan_gitea_identities`) so a
//! trailing newline in a stored PAT can never corrupt the `Authorization`
//! header. The token is held inside [`GiteaClient`], which redacts it from
//! `Debug`, and is never logged.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::env;

/// Characters that must never appear *literally* inside a single URL path
/// segment or query value built from caller-supplied input. Everything here is a
/// URL-structural or delimiter character: leaving any of them unescaped would let
/// a crafted `repo`/`branch`/`path`/`ref`/`sha` value break out of its slot and
/// redirect the authenticated request to a different — possibly destructive —
/// endpoint, or inject/truncate query parameters. RFC 3986 "unreserved"
/// characters (`A-Za-z0-9-._~`) are intentionally left un-encoded so ordinary
/// identifiers (e.g. the tag `v1.2.3`) pass through unchanged.
const URL_UNSAFE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'?')
    .add(b'\\')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'^')
    .add(b'[')
    .add(b']')
    .add(b'&')
    .add(b'=')
    .add(b'+')
    .add(b';')
    .add(b'@')
    .add(b'$')
    .add(b',')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'!');

/// Percent-encode one URL path segment or query value from caller input so it
/// cannot alter the request's structure. Path-traversal `.`/`..` segments are
/// NOT encoded here (percent-encoding does not help — WHATWG URL parsing treats
/// `%2e%2e` as a double-dot segment and still collapses it); they are rejected
/// up-front by [`reject_traversal`] / [`has_traversal_segment`] before any value
/// reaches this encoder, so the only remaining job here is to neutralise the
/// URL-structural / delimiter characters in [`URL_UNSAFE`]. RFC 3986 unreserved
/// characters pass through unchanged.
fn enc(v: &str) -> Cow<'_, str> {
    utf8_percent_encode(v, URL_UNSAFE).into()
}

/// Percent-encode a multi-segment path (e.g. `dir/sub/file.bin`): the `/`
/// separators are preserved, but each individual segment is encoded via [`enc`].
/// Callers MUST have already rejected traversal via [`reject_traversal`] — this
/// helper assumes no `.`/`..` segment survives.
fn enc_path(v: &str) -> String {
    v.split('/').map(enc).collect::<Vec<_>>().join("/")
}

/// True if `v`, interpreted as a URL path, contains a `.`/`..` traversal segment
/// in either raw OR percent-encoded form. Splits on both `/` and `\` because
/// WHATWG URL normalisation for http(s) schemes treats a backslash as a segment
/// separator, and matches the percent-encoded dot forms (`%2e`, `%2e%2e`, …)
/// case-insensitively because URL parsers decode those before the dot-segment
/// check. Any such value must be rejected — it cannot be safely encoded.
fn has_traversal_segment(v: &str) -> bool {
    v.split(['/', '\\']).any(|seg| {
        // Trim each segment: downstream extraction (`owner()` / `req_str`) trims
        // surrounding whitespace, so a value like `" .. "` would otherwise slip
        // past this check and become a bare `..` segment in the built URL.
        matches!(
            seg.trim().to_ascii_lowercase().as_str(),
            "." | ".." | "%2e" | "%2e." | ".%2e" | "%2e%2e"
        )
    })
}

/// Reject a caller-supplied path value that carries a traversal segment, mapping
/// it to a clean [`ForgeError::InvalidRequest`] rather than letting it reach the
/// URL builder.
fn reject_traversal(provider: &str, key: &str, v: &str) -> Result<(), ForgeError> {
    if has_traversal_segment(v) {
        let _ = provider;
        return Err(ForgeError::InvalidRequest(format!(
            "'{key}' contains a path-traversal segment ('.'/'..') and was refused"
        )));
    }
    Ok(())
}
use tracing::warn;

use crate::error::ToolError;
use crate::gitea::{
    build_cargo_metadata, build_cargo_publish_body, cargo_publish_max_crate_bytes,
    is_valid_owner_segment, pii_check, GiteaClient,
};

use super::capability::{CapabilityMap, ForgeEndpoint, SupportLevel};
use super::provider::{ForgeError, ForgeProvider, ForgeRequest, ForgeResponse, ProviderId};

/// Default public base URL for Codeberg when `CODEBERG_URL` is not set.
const CODEBERG_DEFAULT_URL: &str = "https://codeberg.org";

/// The Gitea-family adapter. One implementation, parameterised by
/// [`GiteaForge::provider_id`] + the wrapped [`GiteaClient`] (which carries the
/// base URL + resolved credential), serving Gitea / Forgejo / Codeberg.
pub struct GiteaForge {
    provider_id: ProviderId,
    client: GiteaClient,
    caps: CapabilityMap,
}

impl std::fmt::Debug for GiteaForge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delegates token redaction to GiteaClient's own Debug impl.
        f.debug_struct("GiteaForge")
            .field("provider_id", &self.provider_id)
            .field("client", &self.client)
            .field("supported_endpoints", &self.caps.count(SupportLevel::Supported))
            .finish()
    }
}

impl GiteaForge {
    /// Wrap an already-built [`GiteaClient`] as a provider in the family. The
    /// capability map is the shared Gitea-family map (see
    /// [`gitea_family_capabilities`]) — Gitea/Forgejo/Codeberg all speak the
    /// same REST v1 surface, so they advertise the same capabilities.
    pub fn new(provider_id: ProviderId, client: GiteaClient) -> Self {
        Self { provider_id, client, caps: gitea_family_capabilities() }
    }

    /// Build the **`gitea`** provider (git-private) from the environment, reusing
    /// [`GiteaClient::from_env`] — i.e. the S105/GPAT `GITEA_URL` +
    /// `GITEA_PAT_<NAME>` multi-identity model, default identity
    /// `GITEA_IDENTITY_NAME` (`moose`). This is the SAME client the concrete
    /// `gitea_*` tools use, so identity resolution, `gitea_cargo_publish`, and
    /// the git-relay posture all carry forward unchanged.
    pub fn gitea_from_env() -> Result<Self, ToolError> {
        Ok(Self::new("gitea", GiteaClient::from_env()?))
    }

    /// Build the **`forgejo`** provider (git-private) from `FORGEJO_URL` +
    /// `FORGEJO_TOKEN`. Single-credential (no `GITEA_PAT_<NAME>` model); the
    /// token is `.trim()`-ed by [`GiteaClient::with_token`].
    pub fn forgejo_from_env() -> Result<Self, ToolError> {
        let base_url = env::var("FORGEJO_URL").map_err(|_| {
            ToolError::NotConfigured("FORGEJO_URL environment variable is not set".to_string())
        })?;
        let token = env::var("FORGEJO_TOKEN").map_err(|_| {
            ToolError::NotConfigured("FORGEJO_TOKEN environment variable is not set".to_string())
        })?;
        let owner = env::var("FORGEJO_OWNER").unwrap_or_else(|_| "moosenet".to_string());
        Ok(Self::new(
            "forgejo",
            GiteaClient::with_token(base_url, token, owner, "forgejo")?,
        ))
    }

    /// Build the **`codeberg`** provider (git-public) from `CODEBERG_TOKEN`, base
    /// URL defaulting to Codeberg's public host (`CODEBERG_URL` overrides). Single
    /// credential; token `.trim()`-ed by [`GiteaClient::with_token`].
    ///
    /// NOTE: Codeberg is the git-public exfiltration surface. This adapter is the
    /// transport only — the unconditional PII gate on public writes is enforced
    /// by the git-public TOOL assembled in GITX-05, not here. (Content writes do
    /// still run the same content PII gate the `gitea_*` tools apply; see
    /// [`GiteaForge::content_write`].)
    pub fn codeberg_from_env() -> Result<Self, ToolError> {
        let base_url =
            env::var("CODEBERG_URL").unwrap_or_else(|_| CODEBERG_DEFAULT_URL.to_string());
        let token = env::var("CODEBERG_TOKEN").map_err(|_| {
            ToolError::NotConfigured("CODEBERG_TOKEN environment variable is not set".to_string())
        })?;
        let owner = env::var("CODEBERG_OWNER").unwrap_or_else(|_| "moosenet".to_string());
        Ok(Self::new(
            "codeberg",
            GiteaClient::with_token(base_url, token, owner, "codeberg")?,
        ))
    }

    /// Resolve the effective client for a request, honouring an optional named
    /// `identity` (only the `gitea` pool has a `GITEA_PAT_<NAME>` map; for the
    /// single-credential providers an identity naming the provider itself, or any
    /// unrecognised name, falls back to the sole configured token rather than
    /// erroring — those providers have exactly one credential).
    fn client_for<'a>(&'a self, req: &ForgeRequest) -> Result<Cow<'a, GiteaClient>, ForgeError> {
        match req.identity.as_deref() {
            Some(name) if !name.trim().is_empty() => match self.client.for_identity(name) {
                Ok(c) => Ok(Cow::Owned(c)),
                // Single-credential providers (forgejo/codeberg) have no named
                // identities; an `identity` argument is a no-op there.
                Err(_) if self.provider_id != "gitea" => Ok(Cow::Borrowed(&self.client)),
                Err(e) => Err(map_tool_err(self.provider_id, e)),
            },
            _ => Ok(Cow::Borrowed(&self.client)),
        }
    }

    /// The owner segment for a request: explicit `owner` param, else the
    /// provider's configured default owner.
    fn owner<'a>(&'a self, params: &'a Value) -> &'a str {
        params
            .get("owner")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.client.owner())
    }

    /// Content write (create OR update): PII-gate the content, base64-encode it,
    /// and POST (create) / PUT (update, when a `sha` is supplied) to the Gitea
    /// contents API. Mirrors the behaviour of the concrete `gitea_create_file` /
    /// `gitea_update_file` tools, including the content PII gate.
    async fn content_write(
        &self,
        client: &GiteaClient,
        params: &Value,
    ) -> Result<Value, ForgeError> {
        let owner = self.owner(params);
        let repo = req_str(params, "repo").map_err(|e| map_tool_err(self.provider_id, e))?;
        let path = req_str(params, "path").map_err(|e| map_tool_err(self.provider_id, e))?;
        // Traversal guard (this write path bypasses `call`): refuse any `.`/`..`
        // segment in the caller-supplied repo/path before building the URL.
        reject_traversal(self.provider_id, "repo", repo)?;
        reject_traversal(self.provider_id, "path", path)?;
        if let Some(o) = params.get("owner").and_then(Value::as_str) {
            reject_traversal(self.provider_id, "owner", o)?;
        }
        let content = req_str(params, "content").map_err(|e| map_tool_err(self.provider_id, e))?;
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Update via forge adapter");

        if let Some(reason) = pii_check(content) {
            warn!(
                "PII gate blocked content_write for {owner}/{repo}/{path} on provider '{}': {reason}",
                self.provider_id
            );
            return Err(ForgeError::InvalidRequest(format!(
                "file content rejected by PII gate: {reason}"
            )));
        }

        let encoded = B64.encode(content.as_bytes());
        let mut body = json!({ "message": message, "content": encoded });
        if let Some(branch) = params.get("branch").and_then(Value::as_str) {
            body["branch"] = json!(branch);
        }

        // `owner`, `repo`, and `path` are interpolated into the request URL, so
        // percent-encode them (path preserves `/` separators) — the raw values
        // above are used only for the PII log line, never for transport.
        let endpoint = format!(
            "/repos/{}/{}/contents/{}",
            enc(owner),
            enc(repo),
            enc_path(path)
        );
        // A supplied `sha` means "update an existing file" (PUT); its absence
        // means "create" (POST) — matching Gitea's contents API contract.
        let (method, sha) = match params.get("sha").and_then(Value::as_str) {
            Some(sha) if !sha.is_empty() => {
                body["sha"] = json!(sha);
                (Method::PUT, true)
            }
            _ => (Method::POST, false),
        };
        let _ = sha;
        client
            .request_value(method, &endpoint, Some(&body))
            .await
            .map_err(|e| map_tool_err(self.provider_id, e))
    }

    /// Cargo registry publish (`PackagesPublish`). The adapter is transport only:
    /// the caller supplies the already-packaged `.crate` bytes as base64
    /// (`crate_b64`) — file reading + the artifact-dir jail live in the concrete
    /// tool (`gitea_cargo_publish`, preserved). Reuses the shared cargo metadata +
    /// length-prefixed body builders and PUTs to the `/api/packages/...` endpoint
    /// (which is NOT under the `/api/v1` REST surface), using the client's own
    /// PAT auth scheme.
    async fn packages_publish(
        &self,
        client: &GiteaClient,
        params: &Value,
    ) -> Result<Value, ForgeError> {
        let p = self.provider_id;
        let owner = self.owner(params);
        if !is_valid_owner_segment(owner) {
            return Err(ForgeError::InvalidRequest(format!(
                "invalid registry owner '{owner}': must be a single org/user name"
            )));
        }
        let name = req_str(params, "name").map_err(|e| map_tool_err(p, e))?;
        let version = req_str(params, "version").map_err(|e| map_tool_err(p, e))?;
        let metadata = params.get("metadata").filter(|v| v.is_object()).ok_or_else(|| {
            ForgeError::InvalidRequest(
                "'metadata' (full Cargo publish metadata object incl. deps) is required".to_string(),
            )
        })?;
        let crate_b64 = req_str(params, "crate_b64").map_err(|e| map_tool_err(p, e))?;
        // Bound the ENCODED length before decoding: base64 inflates size by
        // ~4/3, so checking only the decoded length would still let a caller
        // force this process to allocate/hold an oversized intermediate
        // string in memory. `+ 4` gives a little slack for padding/newlines
        // around the true ceiling without meaningfully loosening it.
        let max_bytes = cargo_publish_max_crate_bytes();
        let max_b64_len = (max_bytes / 3 * 4) + 4;
        if crate_b64.trim().len() as u64 > max_b64_len {
            return Err(ForgeError::InvalidRequest(format!(
                "crate_b64 is too large: encoded length {} bytes exceeds the {max_bytes}-byte \
                 (.crate) limit — set CARGO_PUBLISH_MAX_CRATE_BYTES to raise it if this is a \
                 legitimate large crate",
                crate_b64.trim().len()
            )));
        }
        let crate_bytes = B64
            .decode(crate_b64.trim())
            .map_err(|e| ForgeError::InvalidRequest(format!("crate_b64 is not valid base64: {e}")))?;
        if crate_bytes.is_empty() {
            return Err(ForgeError::InvalidRequest(
                "decoded crate is empty — nothing to publish".to_string(),
            ));
        }
        // Belt-and-suspenders: also check the DECODED length (catches any edge
        // case in the encoded-length estimate above) before the bytes are
        // used to build the upload body.
        if crate_bytes.len() as u64 > max_bytes {
            return Err(ForgeError::InvalidRequest(format!(
                "decoded crate is too large: {} bytes exceeds the {max_bytes}-byte limit — set \
                 CARGO_PUBLISH_MAX_CRATE_BYTES to raise it if this is a legitimate large crate",
                crate_bytes.len()
            )));
        }

        let meta = build_cargo_metadata(name, version, Some(metadata));
        let meta_json = serde_json::to_vec(&meta)
            .map_err(|e| ForgeError::InvalidRequest(format!("failed to serialize metadata: {e}")))?;
        let body = build_cargo_publish_body(&meta_json, &crate_bytes);

        let url = format!(
            "{}/api/packages/{}/cargo/api/v1/crates/new",
            client.base_url().trim_end_matches('/'),
            owner,
        );
        let resp = client
            .http()
            .put(&url)
            .header("Authorization", client.authorization())
            .header("Content-Type", "application/octet-stream")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| ForgeError::Transport { provider: p.to_string(), message: format!("publish request failed: {e}") })?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ForgeError::Auth {
                provider: p.to_string(),
                message: format!(
                    "cargo publish returned {status}: token missing/invalid or lacks write:package scope"
                ),
            });
        }
        if status == StatusCode::CONFLICT {
            return Err(ForgeError::InvalidRequest(format!(
                "crate {name}@{version} already exists in the {owner} registry (409); bump version"
            )));
        }
        if !status.is_success() {
            let t = resp.text().await.unwrap_or_default();
            return Err(ForgeError::Transport {
                provider: p.to_string(),
                message: format!("cargo publish returned {status}: {t}"),
            });
        }
        Ok(json!({
            "published": true,
            "provider": p,
            "owner": owner,
            "name": name,
            "version": version,
        }))
    }

    /// Map a [`ForgeEndpoint`] to a Gitea REST call and execute it. Every branch
    /// builds the `/api/v1`-relative path + method + optional body from the
    /// request params, then goes through [`GiteaClient::request_value`] (except
    /// the cargo publish + content-write helpers, which have their own posture).
    async fn call(&self, endpoint: ForgeEndpoint, req: ForgeRequest) -> Result<Value, ForgeError> {
        use ForgeEndpoint::*;
        let p = self.provider_id;
        let client = self.client_for(&req)?;
        let client = client.as_ref();
        let params = &req.params;
        // Traversal guard: every caller-supplied value that can land in a URL
        // PATH segment is checked for `.`/`..` traversal (raw or percent-encoded)
        // and rejected before it reaches the URL builder. Percent-encoding alone
        // is insufficient for dot segments (URL parsers normalise `%2e%2e` too),
        // so these must be refused, not escaped. Query-only params are exempt
        // (they cannot traverse the path). The configured default owner is
        // trusted (from the secret-materialised env, not the caller).
        for key in [
            "owner", "repo", "branch", "tag", "ref", "sha", "basehead", "path",
            "collaborator", "type", "name", "version", "org", "old_branch",
        ] {
            if let Some(v) = params.get(key).and_then(Value::as_str) {
                reject_traversal(p, key, v)?;
            }
        }
        // `owner` and `repo` are interpolated into the request URL, so they are
        // percent-encoded here (once) — every path built below uses these safe
        // forms, never raw caller input.
        let owner = enc(self.owner(params)).into_owned();

        // Helpers scoped to this call for the common param shapes.
        let repo = || req_str(params, "repo").map(|r| enc(r).into_owned()).map_err(|e| map_tool_err(p, e));
        let idx = |k: &str| req_u64(params, k).map_err(|e| map_tool_err(p, e));
        let s = |k: &str| req_str(params, k).map_err(|e| map_tool_err(p, e));
        let get = |path: String| async move {
            client.request_value(Method::GET, &path, None).await.map_err(|e| map_tool_err(p, e))
        };
        let with_body = |method: Method, path: String, body: Value| async move {
            client.request_value(method, &path, Some(&body)).await.map_err(|e| map_tool_err(p, e))
        };
        let del = |path: String| async move {
            client.request_value(Method::DELETE, &path, None).await.map_err(|e| map_tool_err(p, e))
        };
        // Pass through any caller-provided `body` object for update-shaped
        // endpoints, defaulting to an empty object. NOTE: `body` here names the
        // whole update-fields OBJECT (e.g. `{"body": {"title": "...", "state":
        // "closed"}}`), not Gitea's own `body` text field on issues/PRs/releases
        // — that field, if the caller wants to set it, is one KEY *inside* this
        // object. A caller who instead passes a bare string (mistaking this for
        // "the description text") would otherwise have that string forwarded
        // verbatim as the entire JSON PATCH payload — Gitea then rejects it with
        // an opaque deserialize error far from the actual mistake (codex P2). Fail
        // clearly here instead.
        let passthrough = || -> Result<Value, ForgeError> {
            match params.get("body") {
                None => Ok(json!({})),
                Some(v) if v.is_object() => Ok(v.clone()),
                Some(_) => Err(ForgeError::InvalidRequest(
                    "'body' must be a JSON object containing the fields to update (e.g. \
                     {\"title\": \"...\", \"body\": \"...\", \"state\": \"closed\"}) — a bare \
                     string is not a valid update payload for this endpoint".to_string(),
                )),
            }
        };
        let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20).min(50);
        let page = params.get("page").and_then(Value::as_u64).unwrap_or(1).max(1);

        match endpoint {
            // ── Repos ──────────────────────────────────────────────────────────
            ReposList => {
                let q = enc(params.get("q").and_then(Value::as_str).unwrap_or(""));
                get(format!("/repos/search?q={q}&limit={limit}&page={page}")).await
            }
            ReposGet | ReposMetadata => get(format!("/repos/{owner}/{}", repo()?)).await,
            ReposCreate => {
                let name = s("name")?;
                let mut body = json!({
                    "name": name,
                    "private": params.get("private").and_then(Value::as_bool).unwrap_or(true),
                    "auto_init": params.get("auto_init").and_then(Value::as_bool).unwrap_or(false),
                });
                if let Some(d) = params.get("description").and_then(Value::as_str) {
                    body["description"] = json!(d);
                }
                // Create under an org if `owner` was explicitly given, else under
                // the authenticated user.
                let path = match params.get("owner").and_then(Value::as_str) {
                    Some(o) if !o.trim().is_empty() => format!("/orgs/{}/repos", enc(o.trim())),
                    _ => "/user/repos".to_string(),
                };
                with_body(Method::POST, path, body).await
            }
            ReposUpdate => with_body(Method::PATCH, format!("/repos/{owner}/{}", repo()?), passthrough()?).await,
            ReposDelete => del(format!("/repos/{owner}/{}", repo()?)).await,
            ReposFork => {
                let mut body = json!({});
                if let Some(org) = params.get("organization").and_then(Value::as_str) {
                    body["organization"] = json!(org);
                }
                with_body(Method::POST, format!("/repos/{owner}/{}/forks", repo()?), body).await
            }
            ReposMirrorConfig => get(format!("/repos/{owner}/{}/push_mirrors", repo()?)).await,
            ReposVisibility => {
                let private = params.get("private").and_then(Value::as_bool).ok_or_else(|| {
                    ForgeError::InvalidRequest("'private' (bool) is required".to_string())
                })?;
                with_body(Method::PATCH, format!("/repos/{owner}/{}", repo()?), json!({"private": private})).await
            }

            // ── Branches / refs ─────────────────────────────────────────────────
            BranchesList => get(format!("/repos/{owner}/{}/branches?limit={limit}&page={page}", repo()?)).await,
            BranchesGet => get(format!("/repos/{owner}/{}/branches/{}", repo()?, enc(s("branch")?))).await,
            BranchesCreate => {
                let mut body = json!({ "new_branch_name": s("branch")? });
                if let Some(old) = params.get("old_branch").and_then(Value::as_str) {
                    body["old_branch_name"] = json!(old);
                }
                with_body(Method::POST, format!("/repos/{owner}/{}/branches", repo()?), body).await
            }
            BranchesDelete => del(format!("/repos/{owner}/{}/branches/{}", repo()?, enc(s("branch")?))).await,
            BranchesProtection => get(format!("/repos/{owner}/{}/branch_protections", repo()?)).await,
            BranchesDefault => {
                let branch = s("branch")?;
                with_body(Method::PATCH, format!("/repos/{owner}/{}", repo()?), json!({"default_branch": branch})).await
            }
            RefsList => get(format!("/repos/{owner}/{}/git/refs", repo()?)).await,
            RefsGet => get(format!("/repos/{owner}/{}/git/refs/{}", repo()?, enc_path(s("ref")?))).await,
            RefsCreate => {
                let body = json!({ "ref": s("ref")?, "sha": s("sha")? });
                with_body(Method::POST, format!("/repos/{owner}/{}/git/refs", repo()?), body).await
            }
            RefsDelete => del(format!("/repos/{owner}/{}/git/refs/{}", repo()?, enc_path(s("ref")?))).await,

            // ── Commits ─────────────────────────────────────────────────────────
            CommitsList => {
                let sha = enc(params.get("sha").and_then(Value::as_str).unwrap_or(""));
                get(format!("/repos/{owner}/{}/commits?sha={sha}&limit={limit}&page={page}", repo()?)).await
            }
            CommitsGet => get(format!("/repos/{owner}/{}/git/commits/{}", repo()?, enc(s("sha")?))).await,
            CommitsCompareDiff => get(format!("/repos/{owner}/{}/compare/{}", repo()?, enc_path(s("basehead")?))).await,
            CommitsStatus => get(format!("/repos/{owner}/{}/commits/{}/status", repo()?, enc(s("sha")?))).await,

            // ── Pull / merge requests ───────────────────────────────────────────
            PullRequestsList => {
                let state = enc(params.get("state").and_then(Value::as_str).unwrap_or("open"));
                get(format!("/repos/{owner}/{}/pulls?state={state}&limit={limit}&page={page}", repo()?)).await
            }
            PullRequestsGet => get(format!("/repos/{owner}/{}/pulls/{}", repo()?, idx("index")?)).await,
            PullRequestsCreate => {
                let mut body = json!({ "title": s("title")?, "head": s("head")?, "base": s("base")? });
                if let Some(b) = params.get("body").and_then(Value::as_str) {
                    if let Some(reason) = pii_check(b) {
                        return Err(ForgeError::InvalidRequest(format!("PR body rejected by PII gate: {reason}")));
                    }
                    body["body"] = json!(b);
                }
                with_body(Method::POST, format!("/repos/{owner}/{}/pulls", repo()?), body).await
            }
            PullRequestsUpdate => with_body(Method::PATCH, format!("/repos/{owner}/{}/pulls/{}", repo()?, idx("index")?), passthrough()?).await,
            PullRequestsReview => {
                let mut body = json!({ "event": params.get("event").and_then(Value::as_str).unwrap_or("COMMENT") });
                if let Some(b) = params.get("body").and_then(Value::as_str) {
                    body["body"] = json!(b);
                }
                with_body(Method::POST, format!("/repos/{owner}/{}/pulls/{}/reviews", repo()?, idx("index")?), body).await
            }
            PullRequestsComment => {
                // PR comments are issue comments in the Gitea data model.
                let body = json!({ "body": s("comment")? });
                with_body(Method::POST, format!("/repos/{owner}/{}/issues/{}/comments", repo()?, idx("index")?), body).await
            }
            PullRequestsMerge => {
                // `Do` / `MergeMessageField` (PascalCase) are NOT a typo: Gitea's
                // merge endpoint (structs.MergePullRequestOption) genuinely uses
                // these exact JSON tags, unlike the rest of its snake_case API.
                // Mirrors the pre-existing `gitea_pull_request_merge` tool
                // (src/gitea/mod.rs, `body["MergeMessageField"]`, predates this
                // adapter) so both call sites agree with Gitea's real contract.
                let mut body = json!({ "Do": params.get("style").and_then(Value::as_str).unwrap_or("merge") });
                if let Some(m) = params.get("message").and_then(Value::as_str) {
                    body["MergeMessageField"] = json!(m);
                }
                with_body(Method::POST, format!("/repos/{owner}/{}/pulls/{}/merge", repo()?, idx("index")?), body).await
            }
            PullRequestsClose => with_body(Method::PATCH, format!("/repos/{owner}/{}/pulls/{}", repo()?, idx("index")?), json!({"state": "closed"})).await,

            // ── Issues ──────────────────────────────────────────────────────────
            IssuesList => {
                let state = enc(params.get("state").and_then(Value::as_str).unwrap_or("open"));
                get(format!("/repos/{owner}/{}/issues?state={state}&limit={limit}&page={page}", repo()?)).await
            }
            IssuesGet => get(format!("/repos/{owner}/{}/issues/{}", repo()?, idx("index")?)).await,
            IssuesCreate => {
                let mut body = json!({ "title": s("title")? });
                if let Some(b) = params.get("body").and_then(Value::as_str) {
                    if let Some(reason) = pii_check(b) {
                        return Err(ForgeError::InvalidRequest(format!("issue body rejected by PII gate: {reason}")));
                    }
                    body["body"] = json!(b);
                }
                with_body(Method::POST, format!("/repos/{owner}/{}/issues", repo()?), body).await
            }
            IssuesUpdate => with_body(Method::PATCH, format!("/repos/{owner}/{}/issues/{}", repo()?, idx("index")?), passthrough()?).await,
            IssuesComment => {
                let body = json!({ "body": s("comment")? });
                with_body(Method::POST, format!("/repos/{owner}/{}/issues/{}/comments", repo()?, idx("index")?), body).await
            }
            IssuesLabel => {
                let labels = params.get("labels").cloned().unwrap_or_else(|| json!([]));
                with_body(Method::POST, format!("/repos/{owner}/{}/issues/{}/labels", repo()?, idx("index")?), json!({"labels": labels})).await
            }
            IssuesAssign => {
                let assignees = params.get("assignees").cloned().unwrap_or_else(|| json!([]));
                with_body(Method::PATCH, format!("/repos/{owner}/{}/issues/{}", repo()?, idx("index")?), json!({"assignees": assignees})).await
            }
            IssuesClose => with_body(Method::PATCH, format!("/repos/{owner}/{}/issues/{}", repo()?, idx("index")?), json!({"state": "closed"})).await,

            // ── Releases / tags ─────────────────────────────────────────────────
            ReleasesList => get(format!("/repos/{owner}/{}/releases?limit={limit}&page={page}", repo()?)).await,
            ReleasesGet => get(format!("/repos/{owner}/{}/releases/{}", repo()?, idx("id")?)).await,
            ReleasesCreate => {
                let mut body = json!({ "tag_name": s("tag_name")? });
                for k in ["name", "body", "target_commitish"] {
                    if let Some(v) = params.get(k).and_then(Value::as_str) {
                        body[k] = json!(v);
                    }
                }
                for k in ["draft", "prerelease"] {
                    if let Some(v) = params.get(k).and_then(Value::as_bool) {
                        body[k] = json!(v);
                    }
                }
                with_body(Method::POST, format!("/repos/{owner}/{}/releases", repo()?), body).await
            }
            ReleasesUpdate => with_body(Method::PATCH, format!("/repos/{owner}/{}/releases/{}", repo()?, idx("id")?), passthrough()?).await,
            ReleasesDelete => del(format!("/repos/{owner}/{}/releases/{}", repo()?, idx("id")?)).await,
            ReleasesAssets => get(format!("/repos/{owner}/{}/releases/{}/assets", repo()?, idx("id")?)).await,
            TagsList => get(format!("/repos/{owner}/{}/tags?limit={limit}&page={page}", repo()?)).await,
            TagsGet => get(format!("/repos/{owner}/{}/tags/{}", repo()?, enc(s("tag")?))).await,
            TagsCreate => {
                let mut body = json!({ "tag_name": s("tag")? });
                if let Some(t) = params.get("target").and_then(Value::as_str) {
                    body["target"] = json!(t);
                }
                with_body(Method::POST, format!("/repos/{owner}/{}/tags", repo()?), body).await
            }
            TagsDelete => del(format!("/repos/{owner}/{}/tags/{}", repo()?, enc(s("tag")?))).await,

            // ── Webhooks ────────────────────────────────────────────────────────
            WebhooksList => get(format!("/repos/{owner}/{}/hooks", repo()?)).await,
            WebhooksCreate => with_body(Method::POST, format!("/repos/{owner}/{}/hooks", repo()?), passthrough()?).await,
            WebhooksUpdate => with_body(Method::PATCH, format!("/repos/{owner}/{}/hooks/{}", repo()?, idx("id")?), passthrough()?).await,
            WebhooksDelete => del(format!("/repos/{owner}/{}/hooks/{}", repo()?, idx("id")?)).await,
            WebhooksTest => with_body(Method::POST, format!("/repos/{owner}/{}/hooks/{}/tests", repo()?, idx("id")?), json!({})).await,

            // ── Packages / registry ─────────────────────────────────────────────
            PackagesList => {
                let ptype = enc(params.get("type").and_then(Value::as_str).unwrap_or(""));
                get(format!("/packages/{owner}?type={ptype}&page={page}")).await
            }
            PackagesGet => get(format!("/packages/{owner}/{}/{}/{}", enc(s("type")?), enc(s("name")?), enc(s("version")?))).await,
            PackagesPublish => self.packages_publish(client, params).await,
            PackagesDelete => del(format!("/packages/{owner}/{}/{}/{}", enc(s("type")?), enc(s("name")?), enc(s("version")?))).await,

            // ── Content ─────────────────────────────────────────────────────────
            ContentReadFile => {
                let git_ref = params.get("ref").and_then(Value::as_str).unwrap_or("");
                let q = if git_ref.is_empty() { String::new() } else { format!("?ref={}", enc(git_ref)) };
                get(format!("/repos/{owner}/{}/contents/{}{q}", repo()?, enc_path(s("path")?))).await
            }
            ContentWriteFile => self.content_write(client, params).await,
            ContentListTree => {
                let recursive = params.get("recursive").and_then(Value::as_bool).unwrap_or(false);
                get(format!("/repos/{owner}/{}/git/trees/{}?recursive={recursive}", repo()?, enc(s("sha")?))).await
            }
            ContentRawFetch => {
                // The Gitea `/raw/` endpoint serves EXACT FILE BYTES, not JSON —
                // it can carry arbitrary binary content. Routing it through the
                // JSON/text `request_value` helper would lossily UTF-8-decode
                // binary files and corrupt them, so fetch the raw bytes verbatim
                // and base64-encode them for a lossless round-trip in the JSON
                // tool response. This mirrors `GiteaClient::fetch_file_text`,
                // which base64-DECODES Gitea's `content` — same discipline,
                // inverse direction.
                let path = s("path")?;
                let git_ref = params.get("ref").and_then(Value::as_str).unwrap_or("");
                let q = if git_ref.is_empty() { String::new() } else { format!("?ref={}", enc(git_ref)) };
                let bytes = client
                    .request_raw(
                        Method::GET,
                        &format!("/repos/{owner}/{}/raw/{}{q}", repo()?, enc_path(path)),
                    )
                    .await
                    .map_err(|e| map_tool_err(p, e))?;
                Ok(json!({
                    "encoding": "base64",
                    "content": B64.encode(&bytes),
                    "path": path,
                    "size": bytes.len(),
                }))
            }

            // ── Org / collaboration ─────────────────────────────────────────────
            OrgMembers => {
                // `owner` is already encoded; an explicit `org` override is
                // encoded here before use.
                let org = params.get("org").and_then(Value::as_str).map(enc).map(Cow::into_owned).unwrap_or_else(|| owner.clone());
                get(format!("/orgs/{org}/members")).await
            }
            OrgTeams => {
                let org = params.get("org").and_then(Value::as_str).map(enc).map(Cow::into_owned).unwrap_or_else(|| owner.clone());
                get(format!("/orgs/{org}/teams")).await
            }
            OrgPermissions => {
                let collaborator = enc(s("collaborator")?);
                get(format!("/repos/{owner}/{}/collaborators/{collaborator}/permission", repo()?)).await
            }
        }
    }
}

#[async_trait]
impl ForgeProvider for GiteaForge {
    fn id(&self) -> &str {
        self.provider_id
    }

    fn capabilities(&self) -> &CapabilityMap {
        &self.caps
    }

    async fn execute_endpoint(
        &self,
        endpoint: ForgeEndpoint,
        req: ForgeRequest,
    ) -> Result<ForgeResponse, ForgeError> {
        let body = self.call(endpoint, req).await?;
        Ok(ForgeResponse::new(endpoint, self.provider_id, body))
    }
}

// ─── Capability map ──────────────────────────────────────────────────────────

/// The Gitea-family capability map: Gitea REST v1 supports essentially the ENTIRE
/// shared endpoint surface, so every [`ForgeEndpoint`] is advertised as
/// [`SupportLevel::Supported`]. Forgejo and Codeberg share the same API and thus
/// the same map. "Full capability set" (the S106 AC): the whole constant
/// vocabulary is available on this provider family.
pub fn gitea_family_capabilities() -> CapabilityMap {
    let mut caps = CapabilityMap::new();
    for ep in ForgeEndpoint::all() {
        caps.set(*ep, SupportLevel::Supported);
    }
    caps
}

// ─── Param helpers ───────────────────────────────────────────────────────────

/// Extract a required non-empty string param, else [`ToolError::InvalidArgument`].
fn req_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{key}' is required")))
}

/// Extract a required integer param (accepts a numeric string too).
fn req_u64(params: &Value, key: &str) -> Result<u64, ToolError> {
    params
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{key}' (integer) is required")))
}

/// Map a [`ToolError`] from the underlying client into the capability-scoped
/// [`ForgeError`], preserving the auth-vs-transport distinction so the negative
/// test for an unreachable instance sees a [`ForgeError::Transport`], and a
/// bad/absent credential surfaces as [`ForgeError::Auth`].
fn map_tool_err(provider: ProviderId, e: ToolError) -> ForgeError {
    let provider = provider.to_string();
    match e {
        ToolError::InvalidArgument(m) => ForgeError::InvalidRequest(m),
        ToolError::NotConfigured(m) => ForgeError::Auth { provider, message: m },
        ToolError::NotFound(m) => ForgeError::Transport { provider, message: m },
        ToolError::Conflict(m) => ForgeError::InvalidRequest(m),
        ToolError::Http(m) => {
            if m.contains("401") || m.contains("403") || m.contains("Unauthorized") || m.contains("Forbidden") {
                ForgeError::Auth { provider, message: m }
            } else {
                ForgeError::Transport { provider, message: m }
            }
        }
        other => ForgeError::Transport { provider, message: other.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    /// Build a `gitea`-pool adapter pointed at a mock server, with one named
    /// identity so the identity path is exercisable.
    fn mock_gitea(server: &MockServer) -> GiteaForge {
        let client = GiteaClient::with_token(server.base_url(), "test-token", "moosenet", "moose")
            .expect("with_token");
        GiteaForge::new("gitea", client)
    }

    #[test]
    fn capability_map_is_complete_and_all_supported() {
        let caps = gitea_family_capabilities();
        // Every endpoint in the constant vocabulary is Supported (full set).
        assert_eq!(caps.count(SupportLevel::Supported), ForgeEndpoint::all().len());
        assert_eq!(caps.count(SupportLevel::Unsupported), 0);
        // And the JSON report reflects it.
        let report = caps.report();
        assert_eq!(report["repos"]["repos_create"], "supported");
        assert_eq!(report["pull_requests"]["pull_requests_merge"], "supported");
        assert_eq!(report["packages"]["packages_publish"], "supported");
    }

    #[test]
    fn with_token_trims_trailing_newline() {
        // A PAT with a trailing newline must not corrupt the auth header — the
        // exact bug this item guards against (GITEA_PAT_MOOSE).
        let client =
            GiteaClient::with_token("https://example.test", "tok-value\n", "moosenet", "moose")
                .expect("with_token");
        assert_eq!(client.authorization(), "token tok-value");
    }

    #[test]
    fn with_token_rejects_whitespace_only() {
        let err = GiteaClient::with_token("https://example.test", "  \n", "moosenet", "moose");
        assert!(matches!(err, Err(ToolError::NotConfigured(_))));
    }

    #[test]
    fn provider_ids_are_stable() {
        let s = mock_gitea(&MockServer::start());
        assert_eq!(s.id(), "gitea");
        assert!(s.supports(ForgeEndpoint::ReposList));
    }

    #[tokio::test]
    async fn dispatches_repos_get_against_gitea_api() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/api/v1/repos/moosenet/demo");
            then.status(200).json_body(json!({"name": "demo", "full_name": "moosenet/demo"}));
        });
        let forge = mock_gitea(&server);
        let resp = forge
            .dispatch(ForgeEndpoint::ReposGet, ForgeRequest::new(json!({"repo": "demo"})))
            .await
            .expect("repos_get should dispatch");
        m.assert();
        assert_eq!(resp.provider, "gitea");
        assert_eq!(resp.body["name"], "demo");
    }

    #[tokio::test]
    async fn create_pr_posts_to_pulls_endpoint() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST).path("/api/v1/repos/moosenet/demo/pulls");
            then.status(201).json_body(json!({"number": 7, "title": "x"}));
        });
        let forge = mock_gitea(&server);
        let resp = forge
            .dispatch(
                ForgeEndpoint::PullRequestsCreate,
                ForgeRequest::new(json!({"repo": "demo", "title": "x", "head": "f", "base": "main"})),
            )
            .await
            .expect("create pr");
        m.assert();
        assert_eq!(resp.body["number"], 7);
    }

    #[tokio::test]
    async fn content_write_pii_gate_blocks_private_ip() {
        let server = MockServer::start();
        // No mock registered: the request must be blocked BEFORE any HTTP call.
        let forge = mock_gitea(&server);
        let err = forge
            .dispatch(
                ForgeEndpoint::ContentWriteFile,
                ForgeRequest::new(json!({
                    "repo": "demo", "path": "a.txt",
                    "content": "server at <internal-ip>", "message": "x" // pii-test-fixture
                })),
            )
            .await
            .expect_err("PII gate should block");
        assert!(matches!(err, ForgeError::InvalidRequest(_)), "{err:?}");
    }

    #[tokio::test]
    async fn packages_publish_rejects_oversized_crate(){
        // codex P2: packages_publish must reject an oversized crate_b64 BEFORE
        // ever issuing the upload request — no mock registered, so any HTTP
        // call here fails the test via `m.assert()`-style absence.
        let server = MockServer::start();
        let forge = mock_gitea(&server);
        // Force a tiny ceiling via the env override so the test doesn't need
        // to construct a real 64MiB+ payload.
        std::env::set_var("CARGO_PUBLISH_MAX_CRATE_BYTES", "16");
        let oversized = B64.encode(vec![b'x'; 1024]); // decodes to 1024 bytes >> 16-byte cap
        let err = forge
            .dispatch(
                ForgeEndpoint::PackagesPublish,
                ForgeRequest::new(json!({
                    "name": "demo", "version": "0.1.0",
                    "metadata": {"name": "demo", "vers": "0.1.0", "deps": []},
                    "crate_b64": oversized,
                })),
            )
            .await
            .expect_err("oversized crate_b64 must be rejected");
        std::env::remove_var("CARGO_PUBLISH_MAX_CRATE_BYTES");
        assert!(matches!(err, ForgeError::InvalidRequest(_)), "{err:?}");
    }

    #[tokio::test]
    async fn packages_publish_accepts_crate_within_limit() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT).path("/api/packages/moosenet/cargo/api/v1/crates/new");
            then.status(200).json_body(json!({"ok": true}));
        });
        let forge = mock_gitea(&server);
        let small = B64.encode(vec![b'x'; 32]);
        let resp = forge
            .dispatch(
                ForgeEndpoint::PackagesPublish,
                ForgeRequest::new(json!({
                    "name": "demo", "version": "0.1.0",
                    "metadata": {"name": "demo", "vers": "0.1.0", "deps": []},
                    "crate_b64": small,
                })),
            )
            .await
            .expect("small crate under the default limit should publish");
        m.assert();
        assert_eq!(resp.body["published"], true);
    }

    #[tokio::test]
    async fn issues_update_rejects_bare_string_body() {
        // codex P2: a caller who mistakes the update `body` param for Gitea's own
        // issue "body" text field (a bare string) must get a clear
        // InvalidRequest, not have that string silently forwarded as the whole
        // PATCH payload. No mock is registered — the request must never reach
        // the HTTP layer.
        let server = MockServer::start();
        let forge = mock_gitea(&server);
        let err = forge
            .dispatch(
                ForgeEndpoint::IssuesUpdate,
                ForgeRequest::new(json!({
                    "repo": "demo", "index": 3,
                    "body": "new description text",
                })),
            )
            .await
            .expect_err("bare string body must be rejected");
        assert!(matches!(err, ForgeError::InvalidRequest(_)), "{err:?}");
    }

    #[tokio::test]
    async fn issues_update_accepts_object_body() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/api/v1/repos/moosenet/demo/issues/3");
            then.status(200).json_body(json!({"number": 3, "state": "closed"}));
        });
        let forge = mock_gitea(&server);
        let resp = forge
            .dispatch(
                ForgeEndpoint::IssuesUpdate,
                ForgeRequest::new(json!({
                    "repo": "demo", "index": 3,
                    "body": {"state": "closed", "body": "new description text"},
                })),
            )
            .await
            .expect("object body should update");
        m.assert();
        assert_eq!(resp.body["state"], "closed");
    }

    #[tokio::test]
    async fn content_write_update_uses_put_when_sha_present() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(PUT).path("/api/v1/repos/moosenet/demo/contents/a.txt");
            then.status(200).json_body(json!({"content": {"sha": "new"}}));
        });
        let forge = mock_gitea(&server);
        forge
            .dispatch(
                ForgeEndpoint::ContentWriteFile,
                ForgeRequest::new(json!({
                    "repo": "demo", "path": "a.txt", "content": "clean text",
                    "message": "update", "sha": "old-sha"
                })),
            )
            .await
            .expect("update file");
        m.assert();
    }

    #[tokio::test]
    async fn unsupported_endpoint_never_reached_here_but_gate_holds() {
        // The Gitea family supports everything, so exercise the gate via a
        // capability map that drops one endpoint, proving dispatch refuses it
        // before transport (no mock registered).
        let server = MockServer::start();
        let client = GiteaClient::with_token(server.base_url(), "t", "moosenet", "moose").unwrap();
        let mut forge = GiteaForge::new("gitea", client);
        forge.caps.set(ForgeEndpoint::ReposDelete, SupportLevel::Unsupported);
        let err = forge
            .dispatch(ForgeEndpoint::ReposDelete, ForgeRequest::new(json!({"repo": "demo"})))
            .await
            .expect_err("unsupported must be refused");
        assert!(matches!(err, ForgeError::Unsupported { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn unreachable_instance_surfaces_transport_error() {
        // Negative test (AC): an unreachable instance must yield a clean
        // Transport error, not a panic or a fabricated result.
        let client = GiteaClient::with_token(
            // Reserved TEST-NET-1 address, guaranteed unroutable.
            "http://192.0.2.1:9", // pii-test-fixture
            "tok",
            "moosenet",
            "codeberg",
        )
        .unwrap();
        let forge = GiteaForge::new("codeberg", client);
        let err = forge
            .dispatch(ForgeEndpoint::ReposGet, ForgeRequest::new(json!({"repo": "demo"})))
            .await
            .expect_err("unreachable instance must error");
        match err {
            ForgeError::Transport { provider, .. } => assert_eq!(provider, "codeberg"),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merge_pr_targets_merge_endpoint_with_style() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/repos/moosenet/demo/pulls/3/merge")
                .json_body_partial(r#"{"Do":"squash"}"#);
            then.status(200);
        });
        let forge = mock_gitea(&server);
        forge
            .dispatch(
                ForgeEndpoint::PullRequestsMerge,
                ForgeRequest::new(json!({"repo": "demo", "index": 3, "style": "squash"})),
            )
            .await
            .expect("merge");
        m.assert();
    }

    #[tokio::test]
    async fn forgejo_and_codeberg_share_one_client_shape() {
        // Both single-credential providers build via with_token and drive the
        // identical dispatch path; only the id + base URL differ.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/repos/moosenet/demo/branches");
            then.status(200).json_body(json!([]));
        });
        for pid in ["forgejo", "codeberg"] {
            let client = GiteaClient::with_token(server.base_url(), "t", "moosenet", pid).unwrap();
            let forge = GiteaForge::new(pid, client);
            let resp = forge
                .dispatch(ForgeEndpoint::BranchesList, ForgeRequest::new(json!({"repo": "demo"})))
                .await
                .expect("branches list");
            assert_eq!(resp.provider, pid);
        }
    }

    #[test]
    fn enc_neutralizes_reserved_chars_but_leaves_unreserved() {
        // Structural characters that would break out of a segment or into the
        // query/fragment are percent-encoded; ordinary unreserved chars are not.
        assert_eq!(enc("a/b"), "a%2Fb");
        assert_eq!(enc("x?y#z"), "x%3Fy%23z");
        assert_eq!(enc("v1.2.3"), "v1.2.3");
        assert_eq!(enc_path("a/b/c.txt"), "a/b/c.txt");
    }

    #[test]
    fn traversal_detection_covers_raw_encoded_and_backslash_forms() {
        // Dot segments cannot be safely encoded (URL parsers normalise `%2e%2e`
        // too) — they must be detected and rejected, in raw + percent + `\` forms.
        assert!(has_traversal_segment(".."));
        assert!(has_traversal_segment("dir/../etc")); // pii-test-fixture
        assert!(has_traversal_segment("dir/%2e%2e/etc")); // pii-test-fixture
        assert!(has_traversal_segment("%2E%2E/x"));
        assert!(has_traversal_segment("a\\..\\b"));
        assert!(has_traversal_segment("."));
        // Legitimate paths / refs / dotted names are NOT traversal.
        assert!(!has_traversal_segment("a/b/c.txt"));
        assert!(!has_traversal_segment("v1.2.3"));
        assert!(!has_traversal_segment("heads/main"));
        assert!(!has_traversal_segment("base...head"));
    }

    #[tokio::test]
    async fn crafted_repo_slash_query_stays_one_encoded_segment() {
        // A `repo` value carrying `/` + `?` (but no dot segment) must NOT redirect
        // the authenticated request — it stays a single, encoded path segment.
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/api/v1/repos/moosenet/evil%2Fadmin%3Ffoo");
            then.status(200).json_body(json!({"ok": true}));
        });
        let forge = mock_gitea(&server);
        forge
            .dispatch(
                ForgeEndpoint::ReposGet,
                ForgeRequest::new(json!({"repo": "evil/admin?foo"})),
            )
            .await
            .expect("crafted repo must be encoded, not routed elsewhere");
        m.assert();
    }

    #[tokio::test]
    async fn crafted_repo_traversal_is_rejected_before_transport() {
        // A `..` traversal segment must be REFUSED before any HTTP call (no mock
        // registered — the request must never leave the process).
        let forge = mock_gitea(&MockServer::start());
        let err = forge
            .dispatch(
                ForgeEndpoint::ReposGet,
                ForgeRequest::new(json!({"repo": "evil/../admin"})),
            )
            .await
            .expect_err("traversal repo must be rejected");
        assert!(matches!(err, ForgeError::InvalidRequest(_)), "{err:?}");
    }

    #[tokio::test]
    async fn raw_fetch_traversal_path_is_rejected() {
        let forge = mock_gitea(&MockServer::start());
        let err = forge
            .dispatch(
                ForgeEndpoint::ContentRawFetch,
                ForgeRequest::new(json!({"repo": "demo", "path": "../../etc/passwd"})), // pii-test-fixture
            )
            .await
            .expect_err("traversal path must be rejected");
        assert!(matches!(err, ForgeError::InvalidRequest(_)), "{err:?}");
    }

    #[tokio::test]
    async fn raw_fetch_roundtrips_binary_bytes_through_base64() {
        // The `/raw/` endpoint can serve arbitrary binary content. Feed it bytes
        // that are NOT valid UTF-8 (a lone 0xFF, an embedded NUL, high bytes) and
        // prove they round-trip EXACTLY through the base64 tool response — i.e.
        // no lossy UTF-8 decode corrupts them (the codex P1 this fix closes).
        let raw: &[u8] = &[0x00, 0xFF, 0xFE, 0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x80];
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET).path("/api/v1/repos/moosenet/demo/raw/assets/logo.png");
            then.status(200).body(raw);
        });
        let forge = mock_gitea(&server);
        let resp = forge
            .dispatch(
                ForgeEndpoint::ContentRawFetch,
                ForgeRequest::new(json!({"repo": "demo", "path": "assets/logo.png"})),
            )
            .await
            .expect("raw fetch should dispatch");
        m.assert();
        assert_eq!(resp.body["encoding"], "base64");
        assert_eq!(resp.body["path"], "assets/logo.png");
        assert_eq!(resp.body["size"], raw.len());
        // The decoded content must equal the original bytes, byte-for-byte.
        let got = B64
            .decode(resp.body["content"].as_str().expect("content is a string"))
            .expect("content must be valid base64");
        assert_eq!(got, raw, "binary bytes must round-trip losslessly");
    }

    #[tokio::test]
    async fn raw_fetch_builds_path_with_ref_query_and_subdirs() {
        // Path construction: nested (slash-bearing) path segments are preserved
        // verbatim on the `/raw/` URL, and a `ref` is appended as a query param.
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/repos/moosenet/demo/raw/dir/sub/file.bin")
                .query_param("ref", "v1.2.3");
            then.status(200).body([0xDE, 0xAD, 0xBE, 0xEF].as_slice());
        });
        let forge = mock_gitea(&server);
        let resp = forge
            .dispatch(
                ForgeEndpoint::ContentRawFetch,
                ForgeRequest::new(json!({
                    "repo": "demo", "path": "dir/sub/file.bin", "ref": "v1.2.3"
                })),
            )
            .await
            .expect("raw fetch with ref should dispatch");
        m.assert();
        assert_eq!(resp.body["path"], "dir/sub/file.bin");
        let got = B64.decode(resp.body["content"].as_str().unwrap()).unwrap();
        assert_eq!(got, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }
}
