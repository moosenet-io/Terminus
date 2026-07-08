//! The `ForgeProvider` trait — the common interface every forge adapter
//! implements (S106 / GITX-01).
//!
//! A provider adapter (Gitea-family, GitHub, GitLab, … in later items) wraps one
//! forge's API behind this trait. The trait pairs the shared endpoint vocabulary
//! ([`super::capability::ForgeEndpoint`]) with a capability-gated dispatch path:
//! an endpoint the adapter does not advertise returns a clean
//! [`ForgeError::Unsupported`] naming the provider, and an advertised-but-unwired
//! endpoint returns [`ForgeError::NotImplemented`] — never a fabricated result.
//!
//! No concrete adapters live here (they arrive in GITX-02/03/04/06). This item
//! ships only the trait, its request/response/error types, the credential
//! reference abstraction, and the negative (unsupported/not-implemented) path.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ToolError;

use super::capability::{CapabilityMap, ForgeEndpoint, SupportLevel};

/// A provider adapter's stable identifier, e.g. `"gitea"`, `"github"`,
/// `"gitlab_ce"`. Matches the provider ids named in the S106 provider list.
pub type ProviderId = &'static str;

/// A reference to a credential by its runtime secret KEY NAME — never the secret
/// value itself. Adapters resolve the actual token at call time from the runtime
/// secret store via `SecretManager` / `vault::manager().get(key_name)`, so no
/// credential literal ever appears in source, logs, or this struct.
///
/// Example key names follow the established per-identity conventions
/// (`GITEA_PAT_<NAME>`, `GITHUB_PAT_<NAME>`); the value behind them stays in the
/// vault. This type only carries the lookup key, keeping GITX-02/03/04's secret
/// access on the single sanctioned path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRef {
    key_name: String,
}

impl CredentialRef {
    /// Reference a credential by its vault key name (not its value).
    pub fn new(key_name: impl Into<String>) -> Self {
        Self { key_name: key_name.into() }
    }

    /// The vault key name to resolve via `SecretManager` / `vault::manager()`.
    pub fn key_name(&self) -> &str {
        &self.key_name
    }
}

/// A capability-scoped error from a forge dispatch. Distinguishes the "provider
/// cannot do this" cases (which are clean, expected outcomes of the capability
/// model) from genuine auth/transport failures.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ForgeError {
    /// The endpoint is not part of this provider's advertised capability map.
    /// A clean, expected outcome — the vocabulary is constant but availability
    /// varies, and this is how the surface says so without faking a call.
    #[error("endpoint '{endpoint}' is unsupported by provider '{provider}'")]
    Unsupported { provider: String, endpoint: &'static str },

    /// The provider advertises the endpoint but the adapter has not wired it yet
    /// (e.g. a stub provider from GITX-06). Also a clean, honest outcome.
    #[error("endpoint '{endpoint}' is not yet implemented for provider '{provider}'")]
    NotImplemented { provider: String, endpoint: &'static str },

    /// Authentication/authorization failed (bad or missing credential, scope).
    #[error("authentication failed for provider '{provider}': {message}")]
    Auth { provider: String, message: String },

    /// A transport/API-level failure talking to the forge.
    #[error("transport error for provider '{provider}': {message}")]
    Transport { provider: String, message: String },

    /// The request arguments were malformed for the endpoint.
    #[error("invalid forge request: {0}")]
    InvalidRequest(String),
}

impl From<ForgeError> for ToolError {
    fn from(e: ForgeError) -> Self {
        match e {
            // Capability negatives surface as invalid-argument: the caller asked
            // for something this provider does not offer.
            ForgeError::Unsupported { .. }
            | ForgeError::NotImplemented { .. }
            | ForgeError::InvalidRequest(_) => ToolError::InvalidArgument(e.to_string()),
            ForgeError::Auth { .. } => ToolError::NotConfigured(e.to_string()),
            ForgeError::Transport { .. } => ToolError::Http(e.to_string()),
        }
    }
}

/// A request to a forge endpoint. Endpoint-specific arguments live in `params`
/// (validated by the concrete adapter); `provider` optionally selects one member
/// of a multi-provider pool, and `identity` optionally selects a named
/// credential identity (resolved to a [`CredentialRef`] by the adapter).
#[derive(Debug, Clone, Default)]
pub struct ForgeRequest {
    /// Optional explicit provider id within a pool (e.g. `"codeberg"`).
    pub provider: Option<String>,
    /// Optional named credential identity (e.g. `"moose"`).
    pub identity: Option<String>,
    /// Endpoint-specific arguments.
    pub params: Value,
}

impl ForgeRequest {
    /// A request carrying only endpoint parameters.
    pub fn new(params: Value) -> Self {
        Self { params, ..Default::default() }
    }

    /// Builder: select a named credential identity.
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Builder: select an explicit provider within a pool.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }
}

/// The result of a successful forge dispatch: which endpoint/provider served it,
/// plus the endpoint-specific response body.
#[derive(Debug, Clone)]
pub struct ForgeResponse {
    pub endpoint: ForgeEndpoint,
    pub provider: String,
    pub body: Value,
}

impl ForgeResponse {
    pub fn new(endpoint: ForgeEndpoint, provider: impl Into<String>, body: Value) -> Self {
        Self { endpoint, provider: provider.into(), body }
    }
}

/// The common interface every forge adapter implements. Concrete adapters
/// (GITX-02/03/04/06) override [`ForgeProvider::execute_endpoint`] for the
/// endpoints they support and declare them in [`ForgeProvider::capabilities`].
///
/// The capability gate lives in the default [`ForgeProvider::dispatch`], so no
/// adapter can accidentally attempt an unsupported call: dispatch rejects
/// anything the map marks [`SupportLevel::Unsupported`] before any transport.
#[async_trait]
pub trait ForgeProvider: Send + Sync {
    /// Stable provider id (e.g. `"gitea"`, `"github"`).
    fn id(&self) -> &str;

    /// Human-readable name for logs/reports. Defaults to [`ForgeProvider::id`].
    fn display_name(&self) -> &str {
        self.id()
    }

    /// This adapter's advertised support for the shared vocabulary.
    fn capabilities(&self) -> &CapabilityMap;

    /// The declared level for a single endpoint.
    fn support_level(&self, endpoint: ForgeEndpoint) -> SupportLevel {
        self.capabilities().level(endpoint)
    }

    /// Whether this adapter offers the endpoint (supported or experimental).
    fn supports(&self, endpoint: ForgeEndpoint) -> bool {
        self.capabilities().supports(endpoint)
    }

    /// The per-adapter support map as JSON, grouped by domain — the capability
    /// introspection surface both forge tools expose to callers.
    fn capability_report(&self) -> Value {
        self.capabilities().report()
    }

    /// Dispatch a call to an endpoint. The default implementation enforces the
    /// capability gate: an [`SupportLevel::Unsupported`] endpoint returns a clean
    /// [`ForgeError::Unsupported`] naming this provider — never a fabricated
    /// result — and otherwise delegates to [`ForgeProvider::execute_endpoint`].
    ///
    /// Adapters normally do NOT override this; they override `execute_endpoint`.
    async fn dispatch(
        &self,
        endpoint: ForgeEndpoint,
        req: ForgeRequest,
    ) -> Result<ForgeResponse, ForgeError> {
        if !self.support_level(endpoint).is_available() {
            return Err(ForgeError::Unsupported {
                provider: self.id().to_string(),
                endpoint: endpoint.as_str(),
            });
        }
        self.execute_endpoint(endpoint, req).await
    }

    /// Perform an endpoint call the adapter has advertised support for. The
    /// default returns [`ForgeError::NotImplemented`] so a provider that declares
    /// an endpoint in its capability map but has not yet wired the call still
    /// fails cleanly and honestly (the GITX-06 stub posture). Real adapters
    /// override this for each supported endpoint.
    async fn execute_endpoint(
        &self,
        endpoint: ForgeEndpoint,
        _req: ForgeRequest,
    ) -> Result<ForgeResponse, ForgeError> {
        Err(ForgeError::NotImplemented {
            provider: self.id().to_string(),
            endpoint: endpoint.as_str(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credential_ref_carries_key_name_not_value() {
        let cred = CredentialRef::new("GITEA_PAT_MOOSE");
        assert_eq!(cred.key_name(), "GITEA_PAT_MOOSE");
    }

    #[test]
    fn forge_error_maps_to_tool_error_categories() {
        let unsupported = ForgeError::Unsupported {
            provider: "sourcehut".into(),
            endpoint: "pull_requests_create",
        };
        assert!(matches!(ToolError::from(unsupported), ToolError::InvalidArgument(_)));

        let auth = ForgeError::Auth { provider: "github".into(), message: "bad scope".into() };
        assert!(matches!(ToolError::from(auth), ToolError::NotConfigured(_)));

        let transport =
            ForgeError::Transport { provider: "gitea".into(), message: "connection refused".into() };
        assert!(matches!(ToolError::from(transport), ToolError::Http(_)));
    }

    #[test]
    fn request_builders_thread_provider_and_identity() {
        let req = ForgeRequest::new(json!({"repo": "demo"}))
            .with_identity("moose")
            .with_provider("codeberg");
        assert_eq!(req.identity.as_deref(), Some("moose"));
        assert_eq!(req.provider.as_deref(), Some("codeberg"));
        assert_eq!(req.params["repo"], "demo");
    }
}
