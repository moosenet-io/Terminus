//! Optional/experimental provider stubs (S106 / GITX-06).
//!
//! Five providers named in the S106 provider list are NOT wired up as full
//! adapters here — they are STUBS: structure + an HONEST, per-provider
//! [`CapabilityMap`] so the git-private/git-public tools (GITX-05) KNOW these
//! providers exist and can report their real (often reduced) surfaces via
//! capability introspection, without ever faking a call or a capability the
//! provider does not actually offer.
//!
//! - **Bitbucket** (`bitbucket`, Cloud REST 2.0) — git-public pool. Broad REST
//!   surface, but Bitbucket Cloud has no GitHub-style "Releases" feature and no
//!   generic package registry — both advertised `Unsupported`, not faked.
//! - **SourceHut** (`sourcehut`, REST+GraphQL) — git-public pool. **Reduced
//!   capability set by design**: sr.ht is patch-email-workflow-based, so it has
//!   no web pull-request surface and no package registry — `PullRequests*` and
//!   `Packages*` are `Unsupported`. Its per-service webhook model doesn't match
//!   this vocabulary's single generic webhook surface 1:1, so `Webhooks*` is
//!   advertised `Experimental`.
//! - **Gogs** (`gogs`, minimal Gitea-lineage) — git-private pool. An older,
//!   deliberately minimal Gitea fork: no branch-protection API, no package
//!   registry, and no webhook test-delivery endpoint.
//! - **OneDev** (`onedev`) — git-private pool. Modern self-hosted forge with a
//!   fairly complete REST surface, including a real package-registry feature
//!   (Maven/npm/Docker), so its map is close to full.
//! - **Radicle** (`radicle`, p2p) — git-public-ish/experimental pool.
//!   Peer-to-peer; most write operations happen over the `rad`/git protocol,
//!   not a central REST API, and there is no org/collaboration concept at all
//!   (p2p has no central membership list). Its read-only HTTP surface
//!   (`radicle-httpd`) is advertised `Experimental`; everything else is
//!   `Unsupported`.
//!
//! ## What a stub IS and IS NOT
//! A stub implements [`ForgeProvider::id`] and [`ForgeProvider::capabilities`]
//! with a real, provider-specific [`CapabilityMap`] — that part is NOT a
//! placeholder, it is meant to be an accurate account of what each provider's
//! API can do. What a stub deliberately does NOT do is override
//! [`ForgeProvider::execute_endpoint`]: every endpoint, even one advertised
//! `Supported`, falls through to the trait's default implementation, which
//! returns [`ForgeError::NotImplemented`] naming the endpoint and provider.
//! `dispatch` therefore behaves correctly with no extra code here:
//! - an endpoint the map marks `Unsupported` → [`ForgeError::Unsupported`]
//!   (rejected before any transport is even considered);
//! - an endpoint the map marks `Supported`/`Experimental` (declared,
//!   not-yet-wired) → [`ForgeError::NotImplemented`].
//!
//! Neither path ever fabricates a response or claims a capability the
//! provider does not have.
//!
//! ## Credentials
//! Each stub resolves ONE runtime secret key name via [`CredentialRef`],
//! following the established `<PROVIDER>_TOKEN` convention (mirroring the
//! single-credential Forgejo/Codeberg adapters in GITX-02): `BITBUCKET_TOKEN`,
//! `SOURCEHUT_TOKEN`, `GOGS_TOKEN`, `ONEDEV_TOKEN`, `RADICLE_TOKEN`. These are
//! stubs with no wired transport, so `from_env` only checks that the secret is
//! present (proving the provider is "configured") — it never reads the value
//! itself; the value lookup itself is left to the real transport a future item
//! builds. No literal host/token ever appears in source. None of these are
//! added to `secrets_bootstrap::PAT_KEY_PREFIXES` — that multi-identity scan is
//! reserved for providers that actually need it, and a stub does not.

use std::env;

use async_trait::async_trait;

use crate::error::ToolError;

use super::capability::{CapabilityMap, ForgeEndpoint, SupportLevel};
use super::provider::{CredentialRef, ForgeProvider, ProviderId};

/// A stub forge adapter: an id, an honest capability map, and a reference to
/// the one vault key name it would resolve a token from. No adapter here
/// overrides `execute_endpoint`, so every declared endpoint honestly reports
/// `NotImplemented` rather than fabricating a result (see module docs).
pub struct StubForge {
    provider_id: ProviderId,
    caps: CapabilityMap,
    credential: CredentialRef,
}

impl std::fmt::Debug for StubForge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StubForge")
            .field("provider_id", &self.provider_id)
            .field("credential_key", &self.credential.key_name())
            .field("supported_endpoints", &self.caps.count(SupportLevel::Supported))
            .field("experimental_endpoints", &self.caps.count(SupportLevel::Experimental))
            .finish()
    }
}

impl StubForge {
    fn new(provider_id: ProviderId, caps: CapabilityMap, credential_key: &str) -> Self {
        Self { provider_id, caps, credential: CredentialRef::new(credential_key) }
    }

    /// The vault key name this stub would resolve a token from (never the
    /// value — see [`CredentialRef`]).
    pub fn credential_key(&self) -> &str {
        self.credential.key_name()
    }

    /// Build the **`bitbucket`** stub (git-public). Requires `BITBUCKET_TOKEN`
    /// to be present in the runtime secret store (materialized into env by
    /// `secrets_bootstrap`) — the value itself is never read here.
    pub fn bitbucket_from_env() -> Result<Self, ToolError> {
        require_env("BITBUCKET_TOKEN")?;
        Ok(Self::new("bitbucket", bitbucket_capabilities(), "BITBUCKET_TOKEN"))
    }

    /// Build the **`sourcehut`** stub (git-public, reduced surface). Requires
    /// `SOURCEHUT_TOKEN`.
    pub fn sourcehut_from_env() -> Result<Self, ToolError> {
        require_env("SOURCEHUT_TOKEN")?;
        Ok(Self::new("sourcehut", sourcehut_capabilities(), "SOURCEHUT_TOKEN"))
    }

    /// Build the **`gogs`** stub (git-private). Requires `GOGS_TOKEN`.
    pub fn gogs_from_env() -> Result<Self, ToolError> {
        require_env("GOGS_TOKEN")?;
        Ok(Self::new("gogs", gogs_capabilities(), "GOGS_TOKEN"))
    }

    /// Build the **`onedev`** stub (git-private). Requires `ONEDEV_TOKEN`.
    pub fn onedev_from_env() -> Result<Self, ToolError> {
        require_env("ONEDEV_TOKEN")?;
        Ok(Self::new("onedev", onedev_capabilities(), "ONEDEV_TOKEN"))
    }

    /// Build the **`radicle`** stub (experimental, p2p). Requires
    /// `RADICLE_TOKEN` (a `radicle-httpd` session/API token, when the seed
    /// node's read HTTP surface is auth-gated).
    pub fn radicle_from_env() -> Result<Self, ToolError> {
        require_env("RADICLE_TOKEN")?;
        Ok(Self::new("radicle", radicle_capabilities(), "RADICLE_TOKEN"))
    }
}

/// Confirm a secret key is present in the runtime secret store (materialized
/// into env by `secrets_bootstrap` at startup — an env read here IS the vault
/// read, per the established convention; see [`crate::github::adapter`]'s
/// module docs for the same pattern). A present-but-blank value is treated as
/// absent, matching `secrets_bootstrap`'s own "blank PAT is missing" rule.
fn require_env(key: &str) -> Result<(), ToolError> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(()),
        _ => Err(ToolError::NotConfigured(format!(
            "{key} environment variable is not set"
        ))),
    }
}

#[async_trait]
impl ForgeProvider for StubForge {
    fn id(&self) -> &str {
        self.provider_id
    }

    fn capabilities(&self) -> &CapabilityMap {
        &self.caps
    }

    // execute_endpoint is deliberately NOT overridden: the trait default
    // returns ForgeError::NotImplemented for any endpoint dispatch reaches,
    // which is exactly the honest "advertised but not yet wired" outcome a
    // stub is supposed to give (see module docs).
}

// ─── Capability maps ──────────────────────────────────────────────────────────

/// Bitbucket Cloud REST 2.0: broad surface, but no GitHub-style Releases
/// feature and no generic package registry — both honestly `Unsupported`.
fn bitbucket_capabilities() -> CapabilityMap {
    use ForgeEndpoint::*;
    let mut m = CapabilityMap::new();
    for ep in [
        ReposList, ReposGet, ReposCreate, ReposUpdate, ReposDelete, ReposFork,
        ReposMirrorConfig, ReposVisibility, ReposMetadata,
        BranchesList, BranchesGet, BranchesCreate, BranchesDelete, BranchesProtection,
        BranchesDefault, RefsList, RefsGet, RefsCreate, RefsDelete,
        CommitsList, CommitsGet, CommitsCompareDiff, CommitsStatus,
        PullRequestsList, PullRequestsGet, PullRequestsCreate, PullRequestsUpdate,
        PullRequestsReview, PullRequestsComment, PullRequestsMerge, PullRequestsClose,
        IssuesList, IssuesGet, IssuesCreate, IssuesUpdate, IssuesComment, IssuesLabel,
        IssuesAssign, IssuesClose,
        // Bitbucket Cloud has no "Releases" object; git tags exist independently.
        TagsList, TagsGet, TagsCreate, TagsDelete,
        WebhooksList, WebhooksCreate, WebhooksUpdate, WebhooksDelete, WebhooksTest,
        ContentReadFile, ContentWriteFile, ContentListTree, ContentRawFetch,
        // "Org" maps to Bitbucket workspaces/groups.
        OrgMembers, OrgTeams, OrgPermissions,
    ] {
        m.set(ep, SupportLevel::Supported);
    }
    for ep in [
        ReleasesList, ReleasesGet, ReleasesCreate, ReleasesUpdate, ReleasesDelete, ReleasesAssets,
        PackagesList, PackagesGet, PackagesPublish, PackagesDelete,
    ] {
        m.set(ep, SupportLevel::Unsupported);
    }
    m
}

/// SourceHut (sr.ht): patch-email workflow, not web PRs, and no package
/// registry — `PullRequests*` and `Packages*` are honestly `Unsupported`.
/// sr.ht's webhook model is per-service (todo.sr.ht, meta.sr.ht, ...) rather
/// than one generic surface, so `Webhooks*` is `Experimental`.
fn sourcehut_capabilities() -> CapabilityMap {
    use ForgeEndpoint::*;
    let mut m = CapabilityMap::new();
    for ep in [
        ReposList, ReposGet, ReposCreate, ReposUpdate, ReposDelete, ReposVisibility, ReposMetadata,
        BranchesList, BranchesGet, BranchesDefault,
        RefsList, RefsGet, RefsCreate, RefsDelete,
        CommitsList, CommitsGet, CommitsCompareDiff, CommitsStatus,
        // Issues via todo.sr.ht.
        IssuesList, IssuesGet, IssuesCreate, IssuesUpdate, IssuesComment, IssuesLabel,
        IssuesAssign, IssuesClose,
        TagsList, TagsGet, TagsCreate, TagsDelete,
        ContentReadFile, ContentListTree, ContentRawFetch,
        OrgMembers,
    ] {
        m.set(ep, SupportLevel::Supported);
    }
    for ep in [WebhooksList, WebhooksCreate, WebhooksUpdate, WebhooksDelete, WebhooksTest] {
        m.set(ep, SupportLevel::Experimental);
    }
    for ep in [
        // No web PR surface — patches go through the mailing-list workflow.
        PullRequestsList, PullRequestsGet, PullRequestsCreate, PullRequestsUpdate,
        PullRequestsReview, PullRequestsComment, PullRequestsMerge, PullRequestsClose,
        // No package/registry surface.
        PackagesList, PackagesGet, PackagesPublish, PackagesDelete,
        // No GitHub-style releases object.
        ReleasesList, ReleasesGet, ReleasesCreate, ReleasesUpdate, ReleasesDelete, ReleasesAssets,
        // No branch-protection or fork/mirror-config API, no team/permission API.
        BranchesCreate, BranchesDelete, BranchesProtection, ReposFork, ReposMirrorConfig,
        ContentWriteFile, OrgTeams, OrgPermissions,
    ] {
        m.set(ep, SupportLevel::Unsupported);
    }
    m
}

/// Gogs: a deliberately minimal Gitea-lineage fork. No branch-protection API,
/// no package registry, and no webhook test-delivery endpoint.
fn gogs_capabilities() -> CapabilityMap {
    use ForgeEndpoint::*;
    let mut m = CapabilityMap::new();
    for ep in [
        ReposList, ReposGet, ReposCreate, ReposUpdate, ReposDelete, ReposFork,
        ReposMirrorConfig, ReposVisibility, ReposMetadata,
        BranchesList, BranchesGet, BranchesCreate, BranchesDelete, BranchesDefault,
        RefsList, RefsGet, RefsCreate, RefsDelete,
        CommitsList, CommitsGet, CommitsCompareDiff, CommitsStatus,
        PullRequestsList, PullRequestsGet, PullRequestsCreate, PullRequestsUpdate,
        PullRequestsComment, PullRequestsMerge, PullRequestsClose,
        IssuesList, IssuesGet, IssuesCreate, IssuesUpdate, IssuesComment, IssuesLabel,
        IssuesAssign, IssuesClose,
        ReleasesList, ReleasesGet, ReleasesCreate, ReleasesUpdate, ReleasesDelete, ReleasesAssets,
        TagsList, TagsGet, TagsCreate, TagsDelete,
        WebhooksList, WebhooksCreate, WebhooksUpdate, WebhooksDelete,
        ContentReadFile, ContentWriteFile, ContentListTree, ContentRawFetch,
        OrgMembers, OrgTeams,
    ] {
        m.set(ep, SupportLevel::Supported);
    }
    // Older Gitea-lineage API; formal review sign-off is thinner than upstream
    // Gitea, so treat it as declared-but-partial rather than fully supported.
    m.set(PullRequestsReview, SupportLevel::Experimental);
    for ep in [
        BranchesProtection,
        WebhooksTest,
        PackagesList, PackagesGet, PackagesPublish, PackagesDelete,
        OrgPermissions,
    ] {
        m.set(ep, SupportLevel::Unsupported);
    }
    m
}

/// OneDev: modern self-hosted forge with a fairly complete REST surface,
/// including a real package-registry feature (Maven/npm/Docker), so its map
/// is close to the full vocabulary.
fn onedev_capabilities() -> CapabilityMap {
    let mut m = CapabilityMap::new();
    for ep in ForgeEndpoint::all() {
        m.set(*ep, SupportLevel::Supported);
    }
    // Generic instance-wide package publish is a heavier, protocol-specific
    // path (Maven/npm/Docker each differ) — declared but experimental until a
    // real adapter wires the specific registry protocols individually.
    m.set(ForgeEndpoint::PackagesPublish, SupportLevel::Experimental);
    m
}

/// Radicle: peer-to-peer, experimental. Writes happen over the `rad`/git
/// protocol, not a central REST API, and there is no org/collaboration
/// concept (p2p has no central membership list). The read-only
/// `radicle-httpd` HTTP surface is advertised `Experimental`; everything else
/// is honestly `Unsupported`.
fn radicle_capabilities() -> CapabilityMap {
    use ForgeEndpoint::*;
    let mut m = CapabilityMap::new();
    for ep in [
        ReposList, ReposGet, ReposMetadata,
        BranchesList, BranchesGet, BranchesDefault,
        RefsList, RefsGet,
        CommitsList, CommitsGet, CommitsCompareDiff, CommitsStatus,
        // Radicle "patches" are the PR analogue; read-only via httpd today.
        PullRequestsList, PullRequestsGet,
        // Radicle "issues" are collaborative objects (COBs); read-only via httpd.
        IssuesList, IssuesGet,
        TagsList, TagsGet,
        ContentReadFile, ContentListTree, ContentRawFetch,
    ] {
        m.set(ep, SupportLevel::Experimental);
    }
    for ep in [
        ReposCreate, ReposUpdate, ReposDelete, ReposFork, ReposMirrorConfig, ReposVisibility,
        BranchesCreate, BranchesDelete, BranchesProtection, RefsCreate, RefsDelete,
        PullRequestsCreate, PullRequestsUpdate, PullRequestsReview, PullRequestsComment,
        PullRequestsMerge, PullRequestsClose,
        IssuesCreate, IssuesUpdate, IssuesComment, IssuesLabel, IssuesAssign, IssuesClose,
        ReleasesList, ReleasesGet, ReleasesCreate, ReleasesUpdate, ReleasesDelete, ReleasesAssets,
        TagsCreate, TagsDelete,
        WebhooksList, WebhooksCreate, WebhooksUpdate, WebhooksDelete, WebhooksTest,
        PackagesList, PackagesGet, PackagesPublish, PackagesDelete,
        ContentWriteFile,
        // No central org/membership concept in a p2p network.
        OrgMembers, OrgTeams, OrgPermissions,
    ] {
        m.set(ep, SupportLevel::Unsupported);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::provider::{ForgeRequest, ForgeError};
    use serial_test::serial;
    use serde_json::json;

    fn clear_stub_env() {
        for k in [
            "BITBUCKET_TOKEN", "SOURCEHUT_TOKEN", "GOGS_TOKEN", "ONEDEV_TOKEN", "RADICLE_TOKEN",
        ] {
            env::remove_var(k);
        }
    }

    #[test]
    #[serial]
    fn from_env_requires_its_token() {
        clear_stub_env();
        assert!(StubForge::bitbucket_from_env().is_err());
        assert!(StubForge::sourcehut_from_env().is_err());
        assert!(StubForge::gogs_from_env().is_err());
        assert!(StubForge::onedev_from_env().is_err());
        assert!(StubForge::radicle_from_env().is_err());

        env::set_var("BITBUCKET_TOKEN", "  "); // blank-after-trim still missing
        assert!(StubForge::bitbucket_from_env().is_err());
        env::set_var("BITBUCKET_TOKEN", "tok");
        assert!(StubForge::bitbucket_from_env().is_ok());
        clear_stub_env();
    }

    #[test]
    #[serial]
    fn each_stub_advertises_its_id_and_credential_key() {
        clear_stub_env();
        let cases: Vec<(fn() -> Result<StubForge, ToolError>, &str, &str)> = vec![
            (StubForge::bitbucket_from_env, "bitbucket", "BITBUCKET_TOKEN"),
            (StubForge::sourcehut_from_env, "sourcehut", "SOURCEHUT_TOKEN"),
            (StubForge::gogs_from_env, "gogs", "GOGS_TOKEN"),
            (StubForge::onedev_from_env, "onedev", "ONEDEV_TOKEN"),
            (StubForge::radicle_from_env, "radicle", "RADICLE_TOKEN"),
        ];
        for (build, id, key) in cases {
            env::set_var(key, "tok");
            let forge = build().expect("configured stub should build");
            assert_eq!(forge.id(), id);
            assert_eq!(forge.credential_key(), key);
            env::remove_var(key);
        }
    }

    #[test]
    fn sourcehut_advertises_no_web_pr_and_no_registry() {
        let caps = sourcehut_capabilities();
        for ep in [
            ForgeEndpoint::PullRequestsCreate,
            ForgeEndpoint::PullRequestsList,
            ForgeEndpoint::PullRequestsMerge,
            ForgeEndpoint::PackagesPublish,
            ForgeEndpoint::PackagesList,
        ] {
            assert_eq!(caps.level(ep), SupportLevel::Unsupported, "{ep:?}");
        }
        // But issues (todo.sr.ht) and repos ARE real.
        assert_eq!(caps.level(ForgeEndpoint::IssuesCreate), SupportLevel::Supported);
        assert_eq!(caps.level(ForgeEndpoint::ReposGet), SupportLevel::Supported);
    }

    #[test]
    fn radicle_is_mostly_unsupported_or_experimental() {
        let caps = radicle_capabilities();
        assert_eq!(caps.level(ForgeEndpoint::ReposCreate), SupportLevel::Unsupported);
        assert_eq!(caps.level(ForgeEndpoint::OrgMembers), SupportLevel::Unsupported);
        assert_eq!(caps.level(ForgeEndpoint::ReposGet), SupportLevel::Experimental);
        // Nothing at all is fully Supported for the experimental p2p provider.
        assert_eq!(caps.count(SupportLevel::Supported), 0);
    }

    #[test]
    fn bitbucket_has_no_releases_or_packages() {
        let caps = bitbucket_capabilities();
        for ep in [
            ForgeEndpoint::ReleasesCreate,
            ForgeEndpoint::ReleasesList,
            ForgeEndpoint::PackagesPublish,
        ] {
            assert_eq!(caps.level(ep), SupportLevel::Unsupported, "{ep:?}");
        }
        // But tags and PRs ARE real on Bitbucket Cloud.
        assert_eq!(caps.level(ForgeEndpoint::TagsCreate), SupportLevel::Supported);
        assert_eq!(caps.level(ForgeEndpoint::PullRequestsCreate), SupportLevel::Supported);
    }

    #[test]
    fn gogs_lacks_branch_protection_and_packages_and_webhook_test() {
        let caps = gogs_capabilities();
        for ep in [
            ForgeEndpoint::BranchesProtection,
            ForgeEndpoint::WebhooksTest,
            ForgeEndpoint::PackagesPublish,
        ] {
            assert_eq!(caps.level(ep), SupportLevel::Unsupported, "{ep:?}");
        }
        assert_eq!(caps.level(ForgeEndpoint::IssuesCreate), SupportLevel::Supported);
    }

    #[test]
    fn onedev_advertises_near_full_surface() {
        let caps = onedev_capabilities();
        assert_eq!(caps.count(SupportLevel::Unsupported), 0);
        assert_eq!(caps.level(ForgeEndpoint::PackagesList), SupportLevel::Supported);
        assert_eq!(caps.level(ForgeEndpoint::PackagesPublish), SupportLevel::Experimental);
    }

    /// Negative test: an endpoint every stub advertises as `Unsupported`
    /// returns the clean `ForgeError::Unsupported` naming the provider, before
    /// any transport is attempted — never a fabricated response.
    #[tokio::test]
    #[serial]
    async fn sourcehut_pull_request_create_is_cleanly_unsupported() {
        env::set_var("SOURCEHUT_TOKEN", "tok");
        let forge = StubForge::sourcehut_from_env().expect("configured");
        let err = forge
            .dispatch(ForgeEndpoint::PullRequestsCreate, ForgeRequest::new(json!({})))
            .await
            .expect_err("sourcehut has no web PR surface");
        match err {
            ForgeError::Unsupported { provider, endpoint } => {
                assert_eq!(provider, "sourcehut");
                assert_eq!(endpoint, "pull_requests_create");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        env::remove_var("SOURCEHUT_TOKEN");
    }

    /// Negative test: an endpoint a stub DOES advertise (declared) but has not
    /// wired falls through the trait default to a clean `NotImplemented` —
    /// never a fabricated result. Bitbucket declares `ReposGet` as Supported,
    /// but no stub overrides `execute_endpoint`.
    #[tokio::test]
    #[serial]
    async fn advertised_but_unwired_endpoint_reports_not_implemented() {
        env::set_var("BITBUCKET_TOKEN", "tok");
        let forge = StubForge::bitbucket_from_env().expect("configured");
        assert!(forge.supports(ForgeEndpoint::ReposGet));
        let err = forge
            .dispatch(ForgeEndpoint::ReposGet, ForgeRequest::new(json!({})))
            .await
            .expect_err("declared-but-unwired endpoint should fail honestly");
        match err {
            ForgeError::NotImplemented { provider, endpoint } => {
                assert_eq!(provider, "bitbucket");
                assert_eq!(endpoint, "repos_get");
            }
            other => panic!("expected NotImplemented, got {other:?}"),
        }
        env::remove_var("BITBUCKET_TOKEN");
    }

    #[test]
    fn every_stub_capability_report_covers_full_vocabulary() {
        for caps in [
            bitbucket_capabilities(),
            sourcehut_capabilities(),
            gogs_capabilities(),
            onedev_capabilities(),
            radicle_capabilities(),
        ] {
            let total = caps.count(SupportLevel::Supported)
                + caps.count(SupportLevel::Experimental)
                + caps.count(SupportLevel::Unsupported);
            assert_eq!(total, ForgeEndpoint::all().len());
        }
    }
}
