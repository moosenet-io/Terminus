//! Provider-agnostic forge abstraction (S106 / GITX-01).
//!
//! Terminus's git tooling is being reshaped from provider-specific tools (a
//! "Gitea tool", a "GitHub tool") into two provider-AGNOSTIC domains that share
//! one comprehensive endpoint surface and differ only by provider pool and
//! governance posture:
//!
//! - **git-private** — self-hosted source-of-truth forges (full operator R/W).
//! - **git-public** — public/mirror forges (the exfiltration surface; the PII
//!   gate is load-bearing on every write).
//!
//! Both expose the SAME endpoint vocabulary — a forge is a forge. This module is
//! the foundation both tools sit on:
//!
//! - [`capability`] — the constant endpoint vocabulary ([`ForgeEndpoint`],
//!   grouped by [`ForgeDomain`]) plus the per-adapter [`CapabilityMap`] and its
//!   JSON introspection report. "Vocabulary constant; availability varies."
//! - [`provider`] — the [`ForgeProvider`] trait each adapter implements, with a
//!   capability-gated dispatch path that returns a clean "unsupported by
//!   provider X" ([`ForgeError::Unsupported`]) rather than faking a call, plus
//!   the request/response/error types and the [`CredentialRef`] vault-key
//!   abstraction (secrets resolved via `SecretManager`/vault, never literals).
//!
//! - [`gitea_family`] (GITX-02) — the first concrete adapter: ONE
//!   Gitea-compatible-REST-API client ([`GiteaForge`]) implementing
//!   [`ForgeProvider`], parameterised by base-URL + credentials to serve three
//!   providers — Gitea + Forgejo (git-private) and Codeberg (git-public).
//!
//! The GitHub and GitLab adapters (GITX-03/04), the optional stubs (GITX-06),
//! and the git-private/git-public tool assembly with posture enforcement
//! (GITX-05) build on this trait in later items.

pub mod capability;
pub mod gitea_family;
pub mod gitlab;
pub mod provider;

pub use capability::{CapabilityMap, ForgeDomain, ForgeEndpoint, SupportLevel};
pub use gitea_family::{gitea_family_capabilities, GiteaForge};
pub use provider::{
    CredentialRef, ForgeError, ForgeProvider, ForgeRequest, ForgeResponse, ProviderId,
};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    /// A minimal in-memory adapter used to exercise the trait's capability gate
    /// and dispatch paths without touching any network. It advertises a small
    /// subset of the vocabulary; one advertised endpoint is deliberately left
    /// unwired to exercise the "advertised but not implemented" path.
    struct MockForge {
        id: &'static str,
        caps: CapabilityMap,
    }

    impl MockForge {
        fn new() -> Self {
            let caps = CapabilityMap::new()
                .supported(ForgeEndpoint::ReposList)
                .supported(ForgeEndpoint::ReposGet)
                // advertised but intentionally not wired in execute_endpoint:
                .supported(ForgeEndpoint::IssuesCreate)
                .experimental(ForgeEndpoint::PackagesPublish);
            Self { id: "mock", caps }
        }
    }

    #[async_trait]
    impl ForgeProvider for MockForge {
        fn id(&self) -> &str {
            self.id
        }
        fn capabilities(&self) -> &CapabilityMap {
            &self.caps
        }
        async fn execute_endpoint(
            &self,
            endpoint: ForgeEndpoint,
            _req: ForgeRequest,
        ) -> Result<ForgeResponse, ForgeError> {
            match endpoint {
                ForgeEndpoint::ReposList | ForgeEndpoint::ReposGet => {
                    Ok(ForgeResponse::new(endpoint, self.id(), json!({"ok": true})))
                }
                // IssuesCreate & PackagesPublish are advertised but fall through
                // to the default NotImplemented via not being handled here — but
                // since we override execute_endpoint, handle them explicitly to
                // return NotImplemented, mirroring the trait default.
                other => Err(ForgeError::NotImplemented {
                    provider: self.id().to_string(),
                    endpoint: other.as_str(),
                }),
            }
        }
    }

    #[tokio::test]
    async fn supported_endpoint_dispatches() {
        let forge = MockForge::new();
        let resp = forge
            .dispatch(ForgeEndpoint::ReposList, ForgeRequest::new(json!({})))
            .await
            .expect("supported endpoint should dispatch");
        assert_eq!(resp.provider, "mock");
        assert_eq!(resp.body["ok"], true);
    }

    #[tokio::test]
    async fn unsupported_endpoint_is_rejected_cleanly() {
        let forge = MockForge::new();
        // ReposDelete is not in the capability map -> Unsupported, before any
        // execute_endpoint call.
        let err = forge
            .dispatch(ForgeEndpoint::ReposDelete, ForgeRequest::new(json!({})))
            .await
            .expect_err("unsupported endpoint must be rejected");
        match err {
            ForgeError::Unsupported { provider, endpoint } => {
                assert_eq!(provider, "mock");
                assert_eq!(endpoint, "repos_delete");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // And it maps to a clean, human-readable message mentioning the provider.
        let msg = ForgeError::Unsupported {
            provider: "mock".into(),
            endpoint: "repos_delete",
        }
        .to_string();
        assert!(msg.contains("unsupported by provider 'mock'"), "{msg}");
    }

    #[tokio::test]
    async fn advertised_but_unwired_endpoint_reports_not_implemented() {
        let forge = MockForge::new();
        let err = forge
            .dispatch(ForgeEndpoint::IssuesCreate, ForgeRequest::new(json!({})))
            .await
            .expect_err("advertised-but-unwired endpoint should fail honestly");
        assert!(matches!(err, ForgeError::NotImplemented { .. }), "{err:?}");
    }

    #[test]
    fn capability_report_reflects_the_adapter() {
        let forge = MockForge::new();
        let report = forge.capability_report();
        assert_eq!(report["repos"]["repos_list"], "supported");
        assert_eq!(report["repos"]["repos_delete"], "unsupported");
        assert_eq!(report["packages"]["packages_publish"], "experimental");
        // supports() agrees with the map.
        assert!(forge.supports(ForgeEndpoint::ReposList));
        assert!(!forge.supports(ForgeEndpoint::ReposDelete));
    }
}
