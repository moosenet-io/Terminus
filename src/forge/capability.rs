//! Forge capability vocabulary + per-adapter support map (S106 / GITX-01).
//!
//! A forge is a forge: the set of endpoints a git forge can expose is treated
//! as a CONSTANT vocabulary shared by every provider. Both the git-private and
//! git-public tools (assembled later, GITX-05) speak this same vocabulary. What
//! VARIES between providers is which endpoints a given adapter actually
//! implements. This module makes the vocabulary machine-enumerable
//! ([`ForgeEndpoint::all`]) and lets each adapter advertise its own support
//! level per endpoint ([`CapabilityMap`]), so the surface can report
//! "unsupported by provider X" cleanly rather than fake a call it cannot make.
//!
//! No adapters live here (they arrive in GITX-02/03/04/06). This is only the
//! shared vocabulary + the capability-advertisement model.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// A functional grouping of forge endpoints. Every [`ForgeEndpoint`] belongs to
/// exactly one domain; the capability report groups its per-endpoint levels by
/// domain for readability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeDomain {
    Repos,
    Branches,
    Commits,
    PullRequests,
    Issues,
    Releases,
    Webhooks,
    Packages,
    Content,
    Org,
}

impl ForgeDomain {
    /// Stable snake_case identifier used in the capability report and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            ForgeDomain::Repos => "repos",
            ForgeDomain::Branches => "branches",
            ForgeDomain::Commits => "commits",
            ForgeDomain::PullRequests => "pull_requests",
            ForgeDomain::Issues => "issues",
            ForgeDomain::Releases => "releases",
            ForgeDomain::Webhooks => "webhooks",
            ForgeDomain::Packages => "packages",
            ForgeDomain::Content => "content",
            ForgeDomain::Org => "org",
        }
    }

    /// Every domain, in declaration order.
    pub fn all() -> &'static [ForgeDomain] {
        &[
            ForgeDomain::Repos,
            ForgeDomain::Branches,
            ForgeDomain::Commits,
            ForgeDomain::PullRequests,
            ForgeDomain::Issues,
            ForgeDomain::Releases,
            ForgeDomain::Webhooks,
            ForgeDomain::Packages,
            ForgeDomain::Content,
            ForgeDomain::Org,
        ]
    }
}

/// The full shared endpoint vocabulary. This enum IS "one surface": the complete
/// set of operations both forge tools can name. It is constant across providers;
/// only availability varies (advertised per adapter via [`CapabilityMap`]).
///
/// Mirrors the spec's shared endpoint surface: repos, branches/refs, commits,
/// pull/merge requests, issues, releases/tags, webhooks, packages/registry,
/// content, and org/collaboration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeEndpoint {
    // ── Repos ────────────────────────────────────────────────────────────────
    ReposList,
    ReposGet,
    ReposCreate,
    ReposUpdate,
    ReposDelete,
    ReposFork,
    ReposMirrorConfig,
    ReposVisibility,
    ReposMetadata,
    // ── Branches / refs ───────────────────────────────────────────────────────
    BranchesList,
    BranchesGet,
    BranchesCreate,
    BranchesDelete,
    BranchesProtection,
    BranchesDefault,
    // Generic refs (non-branch refs: `refs/tags/*`, custom refs) — distinct from
    // the branch-specific operations above.
    RefsList,
    RefsGet,
    RefsCreate,
    RefsDelete,
    // ── Commits ───────────────────────────────────────────────────────────────
    CommitsList,
    CommitsGet,
    CommitsCompareDiff,
    CommitsStatus,
    // ── Pull / merge requests ─────────────────────────────────────────────────
    PullRequestsList,
    PullRequestsGet,
    PullRequestsCreate,
    PullRequestsUpdate,
    PullRequestsReview,
    PullRequestsComment,
    PullRequestsMerge,
    PullRequestsClose,
    // ── Issues ────────────────────────────────────────────────────────────────
    IssuesList,
    IssuesGet,
    IssuesCreate,
    IssuesUpdate,
    IssuesComment,
    IssuesLabel,
    IssuesAssign,
    IssuesClose,
    // ── Releases / tags ───────────────────────────────────────────────────────
    ReleasesList,
    ReleasesGet,
    ReleasesCreate,
    ReleasesUpdate,
    ReleasesDelete,
    ReleasesAssets,
    // Tag operations independent of releases (forge APIs create/list/delete tags
    // without an associated release).
    TagsList,
    TagsGet,
    TagsCreate,
    TagsDelete,
    // ── Webhooks ──────────────────────────────────────────────────────────────
    WebhooksList,
    WebhooksCreate,
    WebhooksUpdate,
    WebhooksDelete,
    WebhooksTest,
    // ── Packages / registry ───────────────────────────────────────────────────
    PackagesList,
    PackagesGet,
    PackagesPublish,
    PackagesDelete,
    // ── Content ───────────────────────────────────────────────────────────────
    ContentReadFile,
    ContentWriteFile,
    ContentListTree,
    ContentRawFetch,
    // ── Org / collaboration ───────────────────────────────────────────────────
    OrgMembers,
    OrgTeams,
    OrgPermissions,
}

impl ForgeEndpoint {
    /// The complete, ordered vocabulary. Capability introspection iterates this
    /// so a report always covers every endpoint (constant vocabulary), marking
    /// each as supported/experimental/unsupported per the adapter.
    pub fn all() -> &'static [ForgeEndpoint] {
        use ForgeEndpoint::*;
        &[
            ReposList, ReposGet, ReposCreate, ReposUpdate, ReposDelete, ReposFork,
            ReposMirrorConfig, ReposVisibility, ReposMetadata,
            BranchesList, BranchesGet, BranchesCreate, BranchesDelete,
            BranchesProtection, BranchesDefault,
            RefsList, RefsGet, RefsCreate, RefsDelete,
            CommitsList, CommitsGet, CommitsCompareDiff, CommitsStatus,
            PullRequestsList, PullRequestsGet, PullRequestsCreate, PullRequestsUpdate,
            PullRequestsReview, PullRequestsComment, PullRequestsMerge, PullRequestsClose,
            IssuesList, IssuesGet, IssuesCreate, IssuesUpdate, IssuesComment,
            IssuesLabel, IssuesAssign, IssuesClose,
            ReleasesList, ReleasesGet, ReleasesCreate, ReleasesUpdate, ReleasesDelete,
            ReleasesAssets,
            TagsList, TagsGet, TagsCreate, TagsDelete,
            WebhooksList, WebhooksCreate, WebhooksUpdate, WebhooksDelete, WebhooksTest,
            PackagesList, PackagesGet, PackagesPublish, PackagesDelete,
            ContentReadFile, ContentWriteFile, ContentListTree, ContentRawFetch,
            OrgMembers, OrgTeams, OrgPermissions,
        ]
    }

    /// The domain this endpoint belongs to.
    pub fn domain(&self) -> ForgeDomain {
        use ForgeEndpoint::*;
        match self {
            ReposList | ReposGet | ReposCreate | ReposUpdate | ReposDelete | ReposFork
            | ReposMirrorConfig | ReposVisibility | ReposMetadata => ForgeDomain::Repos,
            BranchesList | BranchesGet | BranchesCreate | BranchesDelete
            | BranchesProtection | BranchesDefault | RefsList | RefsGet | RefsCreate
            | RefsDelete => ForgeDomain::Branches,
            CommitsList | CommitsGet | CommitsCompareDiff | CommitsStatus => ForgeDomain::Commits,
            PullRequestsList | PullRequestsGet | PullRequestsCreate | PullRequestsUpdate
            | PullRequestsReview | PullRequestsComment | PullRequestsMerge | PullRequestsClose => {
                ForgeDomain::PullRequests
            }
            IssuesList | IssuesGet | IssuesCreate | IssuesUpdate | IssuesComment | IssuesLabel
            | IssuesAssign | IssuesClose => ForgeDomain::Issues,
            ReleasesList | ReleasesGet | ReleasesCreate | ReleasesUpdate | ReleasesDelete
            | ReleasesAssets | TagsList | TagsGet | TagsCreate | TagsDelete => {
                ForgeDomain::Releases
            }
            WebhooksList | WebhooksCreate | WebhooksUpdate | WebhooksDelete | WebhooksTest => {
                ForgeDomain::Webhooks
            }
            PackagesList | PackagesGet | PackagesPublish | PackagesDelete => ForgeDomain::Packages,
            ContentReadFile | ContentWriteFile | ContentListTree | ContentRawFetch => {
                ForgeDomain::Content
            }
            OrgMembers | OrgTeams | OrgPermissions => ForgeDomain::Org,
        }
    }

    /// Stable snake_case identifier (e.g. `"repos_create"`). Used in errors, the
    /// capability report, and audit logs — a stable dispatch/label key.
    pub fn as_str(&self) -> &'static str {
        use ForgeEndpoint::*;
        match self {
            ReposList => "repos_list",
            ReposGet => "repos_get",
            ReposCreate => "repos_create",
            ReposUpdate => "repos_update",
            ReposDelete => "repos_delete",
            ReposFork => "repos_fork",
            ReposMirrorConfig => "repos_mirror_config",
            ReposVisibility => "repos_visibility",
            ReposMetadata => "repos_metadata",
            BranchesList => "branches_list",
            BranchesGet => "branches_get",
            BranchesCreate => "branches_create",
            BranchesDelete => "branches_delete",
            BranchesProtection => "branches_protection",
            BranchesDefault => "branches_default",
            RefsList => "refs_list",
            RefsGet => "refs_get",
            RefsCreate => "refs_create",
            RefsDelete => "refs_delete",
            CommitsList => "commits_list",
            CommitsGet => "commits_get",
            CommitsCompareDiff => "commits_compare_diff",
            CommitsStatus => "commits_status",
            PullRequestsList => "pull_requests_list",
            PullRequestsGet => "pull_requests_get",
            PullRequestsCreate => "pull_requests_create",
            PullRequestsUpdate => "pull_requests_update",
            PullRequestsReview => "pull_requests_review",
            PullRequestsComment => "pull_requests_comment",
            PullRequestsMerge => "pull_requests_merge",
            PullRequestsClose => "pull_requests_close",
            IssuesList => "issues_list",
            IssuesGet => "issues_get",
            IssuesCreate => "issues_create",
            IssuesUpdate => "issues_update",
            IssuesComment => "issues_comment",
            IssuesLabel => "issues_label",
            IssuesAssign => "issues_assign",
            IssuesClose => "issues_close",
            ReleasesList => "releases_list",
            ReleasesGet => "releases_get",
            ReleasesCreate => "releases_create",
            ReleasesUpdate => "releases_update",
            ReleasesDelete => "releases_delete",
            ReleasesAssets => "releases_assets",
            TagsList => "tags_list",
            TagsGet => "tags_get",
            TagsCreate => "tags_create",
            TagsDelete => "tags_delete",
            WebhooksList => "webhooks_list",
            WebhooksCreate => "webhooks_create",
            WebhooksUpdate => "webhooks_update",
            WebhooksDelete => "webhooks_delete",
            WebhooksTest => "webhooks_test",
            PackagesList => "packages_list",
            PackagesGet => "packages_get",
            PackagesPublish => "packages_publish",
            PackagesDelete => "packages_delete",
            ContentReadFile => "content_read_file",
            ContentWriteFile => "content_write_file",
            ContentListTree => "content_list_tree",
            ContentRawFetch => "content_raw_fetch",
            OrgMembers => "org_members",
            OrgTeams => "org_teams",
            OrgPermissions => "org_permissions",
        }
    }
}

/// How well an adapter supports a given endpoint. A missing entry in a
/// [`CapabilityMap`] reads as [`SupportLevel::Unsupported`] — the conservative
/// default, so a provider never claims a capability it hasn't declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportLevel {
    /// Fully implemented and expected to work against a live instance.
    Supported,
    /// Declared but experimental/partial (e.g. a p2p or reduced-surface forge).
    Experimental,
    /// Not offered by this provider. Dispatch returns a clean "unsupported"
    /// error naming the provider — never a fabricated result.
    Unsupported,
}

impl SupportLevel {
    /// Whether a call may be attempted (supported or experimental). Unsupported
    /// endpoints are rejected before any transport is attempted.
    pub fn is_available(&self) -> bool {
        matches!(self, SupportLevel::Supported | SupportLevel::Experimental)
    }

    /// Stable snake_case label for the report.
    pub fn as_str(&self) -> &'static str {
        match self {
            SupportLevel::Supported => "supported",
            SupportLevel::Experimental => "experimental",
            SupportLevel::Unsupported => "unsupported",
        }
    }
}

/// An adapter's advertised support for the shared vocabulary: a per-endpoint
/// [`SupportLevel`]. Endpoints absent from the map default to
/// [`SupportLevel::Unsupported`]. Built once per adapter (typically at
/// construction) and returned by [`crate::forge::ForgeProvider::capabilities`].
#[derive(Debug, Clone, Default)]
pub struct CapabilityMap {
    levels: HashMap<ForgeEndpoint, SupportLevel>,
}

impl CapabilityMap {
    /// An empty map: every endpoint is [`SupportLevel::Unsupported`] until set.
    pub fn new() -> Self {
        Self { levels: HashMap::new() }
    }

    /// Builder: set one endpoint's level, returning `self` for chaining.
    pub fn with(mut self, endpoint: ForgeEndpoint, level: SupportLevel) -> Self {
        self.levels.insert(endpoint, level);
        self
    }

    /// Builder convenience: mark an endpoint fully [`SupportLevel::Supported`].
    pub fn supported(self, endpoint: ForgeEndpoint) -> Self {
        self.with(endpoint, SupportLevel::Supported)
    }

    /// Builder convenience: mark an endpoint [`SupportLevel::Experimental`].
    pub fn experimental(self, endpoint: ForgeEndpoint) -> Self {
        self.with(endpoint, SupportLevel::Experimental)
    }

    /// Set an endpoint's level in place.
    pub fn set(&mut self, endpoint: ForgeEndpoint, level: SupportLevel) {
        self.levels.insert(endpoint, level);
    }

    /// The declared level for an endpoint (defaults to `Unsupported`).
    pub fn level(&self, endpoint: ForgeEndpoint) -> SupportLevel {
        self.levels
            .get(&endpoint)
            .copied()
            .unwrap_or(SupportLevel::Unsupported)
    }

    /// Whether the endpoint may be attempted (supported or experimental).
    pub fn supports(&self, endpoint: ForgeEndpoint) -> bool {
        self.level(endpoint).is_available()
    }

    /// The per-adapter support map as JSON, grouped by domain. Every endpoint in
    /// the constant vocabulary appears exactly once, so the report is a complete
    /// picture regardless of how sparse the map is:
    ///
    /// ```json
    /// { "repos": { "repos_list": "supported", "repos_delete": "unsupported" }, ... }
    /// ```
    pub fn report(&self) -> Value {
        let mut root = Map::new();
        for domain in ForgeDomain::all() {
            let mut group = Map::new();
            for ep in ForgeEndpoint::all() {
                if ep.domain() == *domain {
                    group.insert(ep.as_str().to_string(), json!(self.level(*ep).as_str()));
                }
            }
            root.insert(domain.as_str().to_string(), Value::Object(group));
        }
        Value::Object(root)
    }

    /// Count of endpoints at a given level — handy for a one-line summary.
    pub fn count(&self, level: SupportLevel) -> usize {
        ForgeEndpoint::all()
            .iter()
            .filter(|ep| self.level(**ep) == level)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn vocabulary_covers_every_domain_uniquely() {
        let all = ForgeEndpoint::all();
        // No duplicate variants in all().
        let unique: HashSet<&ForgeEndpoint> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "all() must not repeat endpoints");
        // Every declared domain is represented by at least one endpoint.
        let domains: HashSet<ForgeDomain> = all.iter().map(|e| e.domain()).collect();
        for d in ForgeDomain::all() {
            assert!(domains.contains(d), "domain {d:?} has no endpoints");
        }
        // Reasonable breadth (the full shared surface is dozens of endpoints).
        assert!(all.len() >= 55, "vocabulary unexpectedly small: {}", all.len());
    }

    #[test]
    fn endpoint_labels_are_unique_and_stable() {
        let mut seen = HashSet::new();
        for ep in ForgeEndpoint::all() {
            assert!(seen.insert(ep.as_str()), "duplicate label {}", ep.as_str());
        }
    }

    #[test]
    fn missing_entry_defaults_to_unsupported() {
        let map = CapabilityMap::new();
        assert_eq!(map.level(ForgeEndpoint::ReposList), SupportLevel::Unsupported);
        assert!(!map.supports(ForgeEndpoint::ReposList));
    }

    #[test]
    fn report_covers_full_vocabulary_grouped_by_domain() {
        let map = CapabilityMap::new()
            .supported(ForgeEndpoint::ReposList)
            .experimental(ForgeEndpoint::PackagesPublish);
        let report = map.report();
        // One group per domain.
        assert_eq!(report.as_object().unwrap().len(), ForgeDomain::all().len());
        // Every endpoint present under its domain.
        let mut total = 0;
        for domain in ForgeDomain::all() {
            total += report[domain.as_str()].as_object().unwrap().len();
        }
        assert_eq!(total, ForgeEndpoint::all().len());
        assert_eq!(report["repos"]["repos_list"], "supported");
        assert_eq!(report["packages"]["packages_publish"], "experimental");
        assert_eq!(report["repos"]["repos_delete"], "unsupported");
    }

    #[test]
    fn counts_partition_the_vocabulary() {
        let map = CapabilityMap::new()
            .supported(ForgeEndpoint::ReposList)
            .supported(ForgeEndpoint::ReposGet)
            .experimental(ForgeEndpoint::ReposFork);
        let total = map.count(SupportLevel::Supported)
            + map.count(SupportLevel::Experimental)
            + map.count(SupportLevel::Unsupported);
        assert_eq!(total, ForgeEndpoint::all().len());
        assert_eq!(map.count(SupportLevel::Supported), 2);
        assert_eq!(map.count(SupportLevel::Experimental), 1);
    }
}
