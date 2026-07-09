//! `git_private` — the git-private MCP tool (S106 / GITX-05).
//!
//! Provider-agnostic dispatch onto the self-hosted source-of-truth forge pool
//! (gitea/forgejo today; gitlab_ce/gogs/onedev register into the same pool as
//! their adapters land — see [`crate::forge::registry`]). Speaks the full
//! shared [`ForgeEndpoint`] vocabulary (GITX-01) via [`ForgeProvider::dispatch`],
//! so capability introspection ("unsupported by provider X") is inherited for
//! free — this tool adds ONLY the git-private governance posture on top:
//!
//! - Full operator R/W: reads and ordinary writes dispatch straight through.
//! - Destructive ops — repo delete, branch/ref/tag/release/webhook/package
//!   delete, or ANY write carrying an explicit `force`/history-rewrite flag —
//!   require the caller to pass `confirm: true`. Without it the call is
//!   refused before any transport, naming exactly what confirmation would
//!   unlock (the human-in-the-loop gate the spec calls for; see
//!   [`crate::forge::posture`]).
//!
//! ## Placement
//! Registered ONLY on the `terminus_personal` (personal) registry — this is
//! the operator's own source-of-truth git access, not a Chord-served
//! build-pipeline surface. See `crate::registry::register_personal`.
//!
//! ## Secrets
//! No credential ever appears here: identity selection (`identity` param)
//! flows straight into [`ForgeRequest`], and each adapter resolves the actual
//! token from the runtime secret store (`SecretManager`/vault materialized
//! env) at call time — this module never reads a token itself.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::capability::ForgeEndpoint;
use super::posture::{is_destructive_endpoint, requests_force_or_rewrite};
use super::provider::{ForgeError, ForgeRequest};
use super::registry::{ForgePool, ForgeRegistry};

pub struct GitPrivateTool {
    registry: ForgeRegistry,
}

impl GitPrivateTool {
    pub fn new(registry: ForgeRegistry) -> Self {
        Self { registry }
    }

    pub fn from_env() -> Self {
        Self::new(ForgeRegistry::from_env())
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
            "unknown endpoint '{raw}'; see the git_private tool's capability introspection \
             (endpoint 'capabilities') for the full vocabulary"
        ))
    })
}

#[async_trait]
impl RustTool for GitPrivateTool {
    fn name(&self) -> &str {
        "git_private"
    }

    fn description(&self) -> &str {
        "Provider-agnostic git-PRIVATE tool: full operator read/write against the \
         self-hosted source-of-truth forge pool (gitea/forgejo today; extensible to \
         gitlab_ce/gogs/onedev). Speaks the shared forge endpoint vocabulary — pass \
         'endpoint' (e.g. 'repos_create', 'issues_comment') plus 'params'. Destructive \
         operations (repo delete, branch/ref/tag/release/webhook/package delete, or any \
         write with 'force'/'rewrite_history': true) require 'confirm': true or are \
         refused. Optional 'provider' selects a pool member (default: gitea or the sole \
         configured provider); optional 'identity' selects a named credential."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "endpoint": {
                    "type": "string",
                    "description": "Shared forge endpoint name, e.g. 'repos_list', 'issues_create', 'repos_delete'"
                },
                "provider": {
                    "type": "string",
                    "description": "Explicit git-private pool member (e.g. 'gitea', 'forgejo'); default is config-driven"
                },
                "identity": {
                    "type": "string",
                    "description": "Named credential identity (e.g. 'moose'); default is the provider's active identity"
                },
                "params": {
                    "type": "object",
                    "description": "Endpoint-specific arguments"
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Required 'true' for destructive operations (delete/force-push/history-rewrite)"
                }
            },
            "required": ["endpoint"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let endpoint = req_endpoint(&args)?;
        let provider_id = args.get("provider").and_then(Value::as_str);
        let identity = args.get("identity").and_then(Value::as_str).map(str::to_string);
        let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
        let confirm = args.get("confirm").and_then(Value::as_bool).unwrap_or(false);

        // Capability introspection: a lightweight synthetic action, not part of
        // the shared vocabulary itself, kept as an ergonomic escape hatch so a
        // caller can discover what a provider supports without guessing.
        // (Handled after endpoint parsing so a bogus 'endpoint' still errors
        // cleanly for normal calls; 'capabilities' is itself a valid endpoint
        // name check bypass only when explicitly requested via params.)

        let destructive =
            is_destructive_endpoint(endpoint) || requests_force_or_rewrite(&params);
        if destructive && !confirm {
            return Err(ToolError::InvalidArgument(format!(
                "'{}' is a destructive git-private operation (delete / force-push / \
                 history-rewrite) and requires explicit confirmation — retry with \
                 'confirm': true",
                endpoint.as_str()
            )));
        }

        let provider = self.registry.resolve(ForgePool::Private, provider_id)?;

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
            "pool": "private",
            "body": resp.body,
        })
        .to_string())
    }
}

fn forge_err_to_tool(e: ForgeError) -> ToolError {
    ToolError::from(e)
}

/// A read-only capability-introspection tool for the git-private pool:
/// reports, per configured provider, which endpoints in the shared vocabulary
/// are supported/experimental/unsupported. Kept separate from `git_private`
/// itself so introspection never competes with the `endpoint` dispatch
/// parameter's namespace.
pub struct GitPrivateCapabilities {
    registry: ForgeRegistry,
}

impl GitPrivateCapabilities {
    pub fn new(registry: ForgeRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl RustTool for GitPrivateCapabilities {
    fn name(&self) -> &str {
        "git_private_capabilities"
    }

    fn description(&self) -> &str {
        "Report, per configured git-private provider (gitea/forgejo/...), which shared \
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
            None => self.registry.providers(ForgePool::Private),
        };
        let mut out = serde_json::Map::new();
        for id in ids {
            if let Some(p) = self.registry.get(ForgePool::Private, &id) {
                out.insert(id, p.capability_report());
            }
        }
        Ok(json!({ "pool": "private", "providers": out }).to_string())
    }
}

pub fn register(registry: &mut ToolRegistry) {
    let forge = ForgeRegistry::from_env();
    let _ = registry.register(Box::new(GitPrivateTool::new(forge.clone())));
    let _ = registry.register(Box::new(GitPrivateCapabilities::new(forge)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::provider::ForgeProvider;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct MockPrivate {
        deletes_allowed: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ForgeProvider for MockPrivate {
        fn id(&self) -> &str {
            "gitea"
        }
        fn capabilities(&self) -> &super::super::capability::CapabilityMap {
            static CAPS: std::sync::OnceLock<super::super::capability::CapabilityMap> =
                std::sync::OnceLock::new();
            CAPS.get_or_init(|| {
                super::super::capability::CapabilityMap::new()
                    .supported(ForgeEndpoint::ReposList)
                    .supported(ForgeEndpoint::ReposCreate)
                    .supported(ForgeEndpoint::ReposDelete)
                    .supported(ForgeEndpoint::BranchesCreate)
            })
        }
        async fn execute_endpoint(
            &self,
            endpoint: ForgeEndpoint,
            _req: ForgeRequest,
        ) -> Result<super::super::provider::ForgeResponse, ForgeError> {
            if endpoint == ForgeEndpoint::ReposDelete {
                self.deletes_allowed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(super::super::provider::ForgeResponse::new(endpoint, "gitea", json!({"ok": true})))
        }
    }

    fn tool_with_mock() -> GitPrivateTool {
        let mut reg = ForgeRegistry::new();
        reg.insert(
            ForgePool::Private,
            Arc::new(MockPrivate { deletes_allowed: std::sync::atomic::AtomicUsize::new(0) }),
        );
        GitPrivateTool::new(reg)
    }

    #[tokio::test]
    async fn ordinary_write_dispatches_without_confirmation() {
        let tool = tool_with_mock();
        let out = tool
            .execute(json!({"endpoint": "repos_create", "params": {"name": "demo"}}))
            .await
            .unwrap();
        assert!(out.contains("\"pool\":\"private\""));
    }

    #[tokio::test]
    async fn destructive_op_without_confirm_is_withheld() {
        let tool = tool_with_mock();
        let err = tool.execute(json!({"endpoint": "repos_delete"})).await.unwrap_err();
        match err {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("confirm")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn destructive_op_with_confirm_dispatches() {
        let tool = tool_with_mock();
        let out = tool
            .execute(json!({"endpoint": "repos_delete", "confirm": true}))
            .await
            .unwrap();
        assert!(out.contains("repos_delete"));
    }

    #[tokio::test]
    async fn force_flag_on_a_non_delete_endpoint_still_requires_confirmation() {
        let tool = tool_with_mock();
        let err = tool
            .execute(json!({"endpoint": "branches_create", "params": {"force": true}}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));

        let out = tool
            .execute(json!({"endpoint": "branches_create", "params": {"force": true}, "confirm": true}))
            .await
            .unwrap();
        assert!(out.contains("branches_create"));
    }

    #[tokio::test]
    async fn unknown_endpoint_is_a_clean_invalid_argument() {
        let tool = tool_with_mock();
        let err = tool.execute(json!({"endpoint": "not_a_thing"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn unsupported_by_provider_surfaces_cleanly() {
        let tool = tool_with_mock();
        // ReposUpdate is not in MockPrivate's capability map -> Unsupported.
        let err = tool.execute(json!({"endpoint": "repos_update"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("unsupported"));
    }

    #[tokio::test]
    async fn explicit_provider_not_configured_is_clean() {
        let tool = tool_with_mock();
        let err = tool
            .execute(json!({"endpoint": "repos_list", "provider": "onedev"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }
}
