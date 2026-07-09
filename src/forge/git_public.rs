//! `git_public` — the git-public MCP tool (S106 / GITX-05).
//!
//! Provider-agnostic dispatch onto the hosted/public forge pool (codeberg,
//! github today; gitlab_saas/bitbucket/sourcehut/radicle register into the
//! same pool as their adapters land — see [`crate::forge::registry`]). Speaks
//! the same shared [`ForgeEndpoint`] vocabulary as `git_private`; the
//! difference is entirely POSTURE — this pool is the exfiltration surface, so
//! every write is gated:
//!
//! 1. **Unconditional PII gate** (reuses the GHMR sweep engine,
//!    [`crate::github::pii::pii_gate`]) scans the outbound content on every
//!    write. A failing sweep WITHHOLDS the operation — nothing is sent to the
//!    provider, no bypass, no cadence fast-path.
//! 2. **First-publish human gate**: the first write to a given
//!    `(provider, repo)` pair requires `confirm_first_publish: true`; once
//!    granted it is recorded (persisted) and never re-asked for that pair
//!    (the `mirror_activated` model).
//! 3. **Egress isolation**: a write may never carry a per-call host/base-URL
//!    override — each adapter's own compiled-in allowlist
//!    ([`crate::github::adapter::GitHubAdapter::host_allowed`] and
//!    equivalents) is the sole source of truth for where a provider's traffic
//!    goes; the tool layer refuses any attempt to smuggle a different
//!    destination through `params`. Reads are unrestricted.
//!
//! Order: egress check → PII gate → first-publish gate → dispatch. A call
//! that fails any earlier check never reaches a later one (and never reaches
//! the network).
//!
//! ## Placement
//! Registered ONLY on the CORE registry (`register_all` / Chord-served) — see
//! `crate::registry::register_all`. Not on `terminus_personal`.
//!
//! ## Mirror engine integration
//! The git-public mirror engine (`crate::forge::mirror`, renamed at GITX-08
//! from `crate::github::mirror` / GHMR) is git-public's swept-clean-tree
//! write path for a FULL repo mirror sync (as opposed to a single API write
//! like a PR comment): `git_public` exposes a `mirror_action` request
//! (`status`/`prepare`/`approve`/`push`) that forwards to the mirror engine's
//! core-tool logic ([`crate::forge::mirror::tools::dispatch_mirror_action`]),
//! which already carries its own unconditional PII gate and
//! fast-forward-only, no-force transport. `git_public` additionally treats a
//! successful `push` as activating that `(provider, repo)` pair for the
//! first-publish gate, so a subsequent direct API write (e.g. a PR comment on
//! the newly-mirrored repo) is not re-asked. The mirror dispatch is
//! provider-routable (a `provider` field, default `github`) — see the
//! `tools.rs` doc comment for why only `github` is wired today without
//! hardcoding it as the only possible target.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::github::pii::pii_gate;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::capability::ForgeEndpoint;
use super::posture::is_write_endpoint;
use super::provider::{ForgeError, ForgeRequest};
use super::registry::{ForgePool, ForgeRegistry};

/// Param keys that would let a caller redirect a write to a non-allowlisted
/// endpoint. None of these are legitimate params on any shared-vocabulary
/// endpoint — a write carrying one is refused outright (egress isolation is
/// each adapter's compiled-in allowlist; the tool layer refuses even the
/// ATTEMPT to override it).
const FORBIDDEN_HOST_OVERRIDE_KEYS: &[&str] =
    &["api_base", "base_url", "host", "endpoint_override", "url_override"];

/// Where the first-publish activation ledger persists across restarts. A
/// simple JSON array of `"provider:repo"` strings — deliberately NOT secret
/// data (no tokens), so a plain file (not the vault) is appropriate.
fn state_path() -> PathBuf {
    std::env::var("TERMINUS_GIT_PUBLIC_ACTIVATED_STATE")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join("terminus-git-public-mirror-activated.json")
        })
}

fn load_activated(path: &PathBuf) -> HashSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn save_activated(path: &PathBuf, set: &HashSet<String>) {
    let mut v: Vec<&String> = set.iter().collect();
    v.sort();
    if let Ok(s) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(path, s);
    }
}

pub struct GitPublicTool {
    registry: ForgeRegistry,
    state_path: PathBuf,
    activated: Mutex<HashSet<String>>,
}

impl GitPublicTool {
    pub fn new(registry: ForgeRegistry) -> Self {
        let state_path = state_path();
        let activated = Mutex::new(load_activated(&state_path));
        Self { registry, state_path, activated }
    }

    pub fn from_env() -> Self {
        Self::new(ForgeRegistry::from_env())
    }

    /// Test-only constructor pointing the activation ledger at an isolated
    /// path, so tests never share/mutate the real operator state file.
    #[cfg(test)]
    fn with_state_path(registry: ForgeRegistry, path: PathBuf) -> Self {
        let activated = Mutex::new(load_activated(&path));
        Self { registry, state_path: path, activated }
    }

    fn activation_key(provider_id: &str, params: &Value) -> String {
        let repo = params
            .get("repo")
            .and_then(Value::as_str)
            .or_else(|| params.get("name").and_then(Value::as_str))
            .unwrap_or("");
        format!("{provider_id}:{repo}")
    }

    fn is_activated(&self, key: &str) -> bool {
        self.activated.lock().expect("activation ledger lock poisoned").contains(key)
    }

    fn activate(&self, key: &str) {
        let mut guard = self.activated.lock().expect("activation ledger lock poisoned");
        if guard.insert(key.to_string()) {
            save_activated(&self.state_path, &guard);
        }
    }
}

fn req_endpoint(args: &Value) -> Result<ForgeEndpoint, ToolError> {
    let raw = args
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidArgument("'endpoint' is required".to_string()))?;
    ForgeEndpoint::from_str(raw).ok_or_else(|| {
        ToolError::InvalidArgument(format!(
            "unknown endpoint '{raw}'; see the git_public tool's capability introspection \
             (git_public_capabilities) for the full vocabulary"
        ))
    })
}

/// Egress isolation: refuse any param carrying a host/base-URL override.
fn assert_no_host_override(params: &Value) -> Result<(), ToolError> {
    if let Some(obj) = params.as_object() {
        for key in FORBIDDEN_HOST_OVERRIDE_KEYS {
            if obj.contains_key(*key) {
                return Err(ToolError::InvalidArgument(format!(
                    "egress isolation: git-public writes may not override the destination \
                     host via '{key}' — each provider adapter's own allowlist is the sole \
                     routing authority"
                )));
            }
        }
    }
    Ok(())
}

/// Flatten every string value in `v` (recursively) into one newline-joined
/// blob for the PII gate — the gate line-scans, so this preserves per-field
/// context while still catching a violation anywhere in the payload.
fn flatten_strings(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(items) => {
            for item in items {
                flatten_strings(item, out);
            }
        }
        Value::Object(map) => {
            for (k, val) in map {
                // Key names themselves can carry PII in pathological cases
                // (e.g. a literal IP used as a map key); scan them too.
                out.push_str(k);
                out.push('\n');
                flatten_strings(val, out);
            }
        }
        _ => {}
    }
}

#[async_trait]
impl RustTool for GitPublicTool {
    fn name(&self) -> &str {
        "git_public"
    }

    fn description(&self) -> &str {
        "Provider-agnostic git-PUBLIC tool: read/write against the hosted/mirror forge \
         pool (codeberg/github today; extensible to gitlab_saas/bitbucket/sourcehut/ \
         radicle) — the EXFILTRATION SURFACE. Every write is unconditionally PII-gated \
         (a failing sweep withholds the operation, no bypass), first-publish-gated per \
         (provider, repo) via 'confirm_first_publish': true, and forbidden from carrying \
         a per-call host override (egress isolation). Reads are unrestricted. Pass \
         'endpoint' (e.g. 'repos_create', 'issues_comment') plus 'params'; optional \
         'provider' selects a pool member (default: github or the sole configured \
         provider); optional 'identity' selects a named credential. Full-tree mirror \
         sync (git-private -> PII-gated git-public) routes through 'mirror_action' \
         (status/prepare/approve/push), delegating to the git-public mirror engine."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "endpoint": {
                    "type": "string",
                    "description": "Shared forge endpoint name, e.g. 'repos_list', 'issues_create'. Omit when using 'mirror_action'."
                },
                "mirror_action": {
                    "type": "string",
                    "enum": ["status", "prepare", "approve", "push"],
                    "description": "Route to the git-public mirror engine instead of a direct endpoint call"
                },
                "provider": {
                    "type": "string",
                    "description": "Explicit git-public pool member (e.g. 'codeberg', 'github'); default is config-driven"
                },
                "identity": {
                    "type": "string",
                    "description": "Named credential identity; default is the provider's active identity"
                },
                "params": {
                    "type": "object",
                    "description": "Endpoint-specific arguments (also carries 'repo' for the first-publish gate)"
                },
                "confirm_first_publish": {
                    "type": "boolean",
                    "description": "Required 'true' on the FIRST write to a given (provider, repo) pair"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        if let Some(action) = args.get("mirror_action").and_then(Value::as_str) {
            let mirror_args = args.get("params").cloned().unwrap_or_else(|| json!({}));
            let result =
                crate::forge::mirror::tools::dispatch_mirror_action(action, mirror_args.clone())
                    .await?;
            // A completed push activates the (provider, repo) pair for the
            // first-publish gate on subsequent direct API writes.
            if action == "push" {
                let provider_id = args
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("github");
                let key = Self::activation_key(provider_id, &mirror_args);
                self.activate(&key);
            }
            return Ok(result);
        }

        let endpoint = req_endpoint(&args)?;
        let provider_id = args.get("provider").and_then(Value::as_str);
        let identity = args.get("identity").and_then(Value::as_str).map(str::to_string);
        let params = args.get("params").cloned().unwrap_or_else(|| json!({}));

        let provider = self.registry.resolve(ForgePool::Public, provider_id)?;
        let resolved_provider_id = provider.id().to_string();

        if is_write_endpoint(endpoint) {
            // 1. Egress isolation — before any content leaves this process.
            assert_no_host_override(&params)?;

            // 2. Unconditional PII gate — hard block, no bypass.
            let mut content = String::new();
            flatten_strings(&params, &mut content);
            pii_gate(&content).map_err(|e| {
                tracing::warn!(
                    target: "forge.git_public",
                    provider = %resolved_provider_id,
                    endpoint = endpoint.as_str(),
                    "write WITHHELD by PII gate"
                );
                e
            })?;

            // 3. First-publish human gate, per (provider, repo).
            let key = Self::activation_key(&resolved_provider_id, &params);
            if !self.is_activated(&key) {
                let confirmed = args
                    .get("confirm_first_publish")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !confirmed {
                    return Err(ToolError::InvalidArgument(format!(
                        "first publish to provider '{resolved_provider_id}' for this repo is \
                         human-gated — retry with 'confirm_first_publish': true to confirm \
                         once (subsequent writes to the same repo/provider are not re-asked)"
                    )));
                }
                self.activate(&key);
            }
        }

        let mut req = ForgeRequest::new(params);
        if let Some(id) = identity {
            req = req.with_identity(id);
        }
        if let Some(p) = provider_id {
            req = req.with_provider(p);
        }

        let resp = provider.dispatch(endpoint, req).await.map_err(forge_err_to_tool)?;
        Ok(json!({
            "endpoint": resp.endpoint.as_str(),
            "provider": resp.provider,
            "pool": "public",
            "body": resp.body,
        })
        .to_string())
    }
}

fn forge_err_to_tool(e: ForgeError) -> ToolError {
    ToolError::from(e)
}

/// Read-only capability introspection for the git-public pool. Mirrors
/// [`super::git_private::GitPrivateCapabilities`].
pub struct GitPublicCapabilities {
    registry: ForgeRegistry,
}

impl GitPublicCapabilities {
    pub fn new(registry: ForgeRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl RustTool for GitPublicCapabilities {
    fn name(&self) -> &str {
        "git_public_capabilities"
    }

    fn description(&self) -> &str {
        "Report, per configured git-public provider (codeberg/github/...), which shared \
         forge endpoints are supported/experimental/unsupported."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "provider": { "type": "string", "description": "Restrict to one provider id" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let filter = args.get("provider").and_then(Value::as_str);
        let ids = match filter {
            Some(id) => vec![id.to_string()],
            None => self.registry.providers(ForgePool::Public),
        };
        let mut out = serde_json::Map::new();
        for id in ids {
            if let Some(p) = self.registry.get(ForgePool::Public, &id) {
                out.insert(id, p.capability_report());
            }
        }
        Ok(json!({ "pool": "public", "providers": out }).to_string())
    }
}

pub fn register(registry: &mut ToolRegistry) {
    let forge = ForgeRegistry::from_env();
    let _ = registry.register(Box::new(GitPublicTool::new(forge.clone())));
    let _ = registry.register(Box::new(GitPublicCapabilities::new(forge)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::provider::ForgeProvider;
    use std::sync::Arc;

    struct MockPublic;

    #[async_trait]
    impl ForgeProvider for MockPublic {
        fn id(&self) -> &str {
            "github"
        }
        fn capabilities(&self) -> &super::super::capability::CapabilityMap {
            static CAPS: std::sync::OnceLock<super::super::capability::CapabilityMap> =
                std::sync::OnceLock::new();
            CAPS.get_or_init(|| {
                super::super::capability::CapabilityMap::new()
                    .supported(ForgeEndpoint::ReposList)
                    .supported(ForgeEndpoint::ReposCreate)
                    .supported(ForgeEndpoint::IssuesComment)
            })
        }
        async fn execute_endpoint(
            &self,
            endpoint: ForgeEndpoint,
            _req: ForgeRequest,
        ) -> Result<super::super::provider::ForgeResponse, ForgeError> {
            Ok(super::super::provider::ForgeResponse::new(endpoint, "github", json!({"ok": true})))
        }
    }

    fn unique_state_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gitx05-test-{tag}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn tool_with_mock(tag: &str) -> GitPublicTool {
        let mut reg = ForgeRegistry::new();
        reg.insert(ForgePool::Public, Arc::new(MockPublic));
        GitPublicTool::with_state_path(reg, unique_state_path(tag))
    }

    #[tokio::test]
    async fn read_endpoint_is_never_gated() {
        let tool = tool_with_mock("read");
        let out = tool.execute(json!({"endpoint": "repos_list"})).await.unwrap();
        assert!(out.contains("\"pool\":\"public\""));
    }

    #[tokio::test]
    async fn write_with_pii_is_withheld_negative() {
        let tool = tool_with_mock("pii");
        let err = tool
            .execute(json!({
                "endpoint": "issues_comment",
                "params": {"repo": "demo", "body": "internal host is <internal-ip>"}, // pii-test-fixture
                "confirm_first_publish": true
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("BLOCKED") || err.to_string().to_lowercase().contains("pii"));
    }

    #[tokio::test]
    async fn clean_write_requires_first_publish_confirmation() {
        let tool = tool_with_mock("first-publish");
        let err = tool
            .execute(json!({
                "endpoint": "issues_comment",
                "params": {"repo": "demo", "body": "hello world"}
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("first publish"));
    }

    #[tokio::test]
    async fn first_publish_confirmed_then_subsequent_write_not_reasked() {
        let tool = tool_with_mock("first-publish-ok");
        let out1 = tool
            .execute(json!({
                "endpoint": "issues_comment",
                "params": {"repo": "demo", "body": "hello world"},
                "confirm_first_publish": true
            }))
            .await
            .unwrap();
        assert!(out1.contains("\"pool\":\"public\""));

        // Second write to the SAME repo/provider, no confirm_first_publish — must
        // proceed without re-asking.
        let out2 = tool
            .execute(json!({
                "endpoint": "issues_comment",
                "params": {"repo": "demo", "body": "a second clean comment"}
            }))
            .await
            .unwrap();
        assert!(out2.contains("\"pool\":\"public\""));
    }

    #[tokio::test]
    async fn host_override_param_is_refused_negative() {
        let tool = tool_with_mock("egress");
        let err = tool
            .execute(json!({
                "endpoint": "issues_comment",
                "params": {"repo": "demo", "body": "hi", "api_base": "https://evil.example/"},
                "confirm_first_publish": true
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("egress"));
    }

    #[tokio::test]
    async fn unknown_endpoint_is_a_clean_invalid_argument() {
        let tool = tool_with_mock("unknown-ep");
        let err = tool.execute(json!({"endpoint": "bogus"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn different_repos_get_independent_first_publish_gates() {
        let tool = tool_with_mock("independent-repos");
        tool.execute(json!({
            "endpoint": "issues_comment",
            "params": {"repo": "repo-a", "body": "hi"},
            "confirm_first_publish": true
        }))
        .await
        .unwrap();

        // repo-b has NOT been activated — must still require confirmation.
        let err = tool
            .execute(json!({
                "endpoint": "issues_comment",
                "params": {"repo": "repo-b", "body": "hi"}
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }
}
