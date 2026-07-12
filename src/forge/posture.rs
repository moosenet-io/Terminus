//! Endpoint posture classification for the git-private / git-public tool
//! assembly (S106 / GITX-05).
//!
//! The shared [`ForgeEndpoint`] vocabulary is constant, but the two tools apply
//! DIFFERENT governance postures to it:
//! - git-private: full operator R/W; a small set of DESTRUCTIVE endpoints
//!   (repo delete, branch/ref/tag/release/webhook/package delete, and any
//!   write carrying an explicit `force`/history-rewrite flag) require an
//!   explicit confirmation.
//! - git-public: every WRITE endpoint (everything that is not a pure read) is
//!   unconditionally PII-gated, first-publish-gated per repo/provider, and
//!   restricted from carrying a per-call host override (egress isolation).
//!
//! This module only classifies; [`super::git_private`] / [`super::git_public`]
//! enforce.

use super::capability::ForgeEndpoint;

/// True for endpoints that only read state — never mutate the forge. Every
/// other endpoint in the vocabulary is a write for posture purposes.
pub fn is_read_endpoint(endpoint: ForgeEndpoint) -> bool {
    use ForgeEndpoint::*;
    matches!(
        endpoint,
        ReposList
            | ReposGet
            | ReposMetadata
            | BranchesList
            | BranchesGet
            | RefsList
            | RefsGet
            | CommitsList
            | CommitsGet
            | CommitsCompareDiff
            | CommitsStatus
            | PullRequestsList
            | PullRequestsGet
            | PullRequestsListComments
            | IssuesList
            | IssuesGet
            | ReleasesList
            | ReleasesGet
            | TagsList
            | TagsGet
            | WebhooksList
            | PackagesList
            | PackagesGet
            | ContentReadFile
            | ContentListTree
            | ContentRawFetch
            | OrgMembers
            | OrgTeams
            | OrgPermissions
    )
}

/// True for endpoints that mutate forge state. The complement of
/// [`is_read_endpoint`] — kept as its own named predicate so call sites read
/// naturally (`is_write_endpoint(ep)` at a write-gate check).
pub fn is_write_endpoint(endpoint: ForgeEndpoint) -> bool {
    !is_read_endpoint(endpoint)
}

/// True for git-private endpoints that are DESTRUCTIVE by themselves
/// (irreversible or history-altering), independent of any request parameter.
/// git-private posture requires explicit confirmation for these.
pub fn is_destructive_endpoint(endpoint: ForgeEndpoint) -> bool {
    use ForgeEndpoint::*;
    matches!(
        endpoint,
        ReposDelete
            | BranchesDelete
            | RefsDelete
            | ReleasesDelete
            | TagsDelete
            | WebhooksDelete
            | PackagesDelete
    )
}

/// Whether a request's params carry an explicit force / history-rewrite
/// intent, regardless of endpoint. Any such request is treated as destructive
/// on git-private (force-push and history rewrite are the spec's named
/// examples of ops that always require human confirmation).
pub fn requests_force_or_rewrite(params: &serde_json::Value) -> bool {
    for key in ["force", "force_push", "rewrite_history", "history_rewrite"] {
        if params.get(key).and_then(serde_json::Value::as_bool) == Some(true) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_are_not_writes() {
        assert!(is_read_endpoint(ForgeEndpoint::ReposList));
        assert!(!is_write_endpoint(ForgeEndpoint::ReposList));
        assert!(is_read_endpoint(ForgeEndpoint::ContentRawFetch));
    }

    #[test]
    fn creates_and_updates_are_writes() {
        assert!(is_write_endpoint(ForgeEndpoint::ReposCreate));
        assert!(is_write_endpoint(ForgeEndpoint::IssuesComment));
        assert!(is_write_endpoint(ForgeEndpoint::ContentWriteFile));
        assert!(is_write_endpoint(ForgeEndpoint::PullRequestsMerge));
    }

    #[test]
    fn deletes_are_destructive() {
        assert!(is_destructive_endpoint(ForgeEndpoint::ReposDelete));
        assert!(is_destructive_endpoint(ForgeEndpoint::BranchesDelete));
        assert!(!is_destructive_endpoint(ForgeEndpoint::ReposCreate));
        assert!(!is_destructive_endpoint(ForgeEndpoint::ReposList));
    }

    #[test]
    fn force_flag_is_detected_independent_of_endpoint() {
        assert!(requests_force_or_rewrite(&json!({"force": true})));
        assert!(requests_force_or_rewrite(&json!({"rewrite_history": true})));
        assert!(!requests_force_or_rewrite(&json!({"force": false})));
        assert!(!requests_force_or_rewrite(&json!({})));
    }

    #[test]
    fn every_endpoint_is_read_xor_write() {
        for ep in ForgeEndpoint::all() {
            assert_ne!(is_read_endpoint(*ep), is_write_endpoint(*ep), "{ep:?}");
        }
    }
}
