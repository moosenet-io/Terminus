//! Gitea API response types for serde deserialization.

use serde::{Deserialize, Serialize};

/// Repository metadata returned by Gitea.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaRepo {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    pub description: String,
    pub private: bool,
    pub html_url: String,
    pub clone_url: String,
    pub default_branch: String,
    pub stars_count: u64,
    pub forks_count: u64,
    pub open_issues_count: u64,
    pub updated: Option<String>,
}

/// File content returned by Gitea GET /repos/{owner}/{repo}/contents/{path}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaFileContent {
    #[serde(rename = "type")]
    pub file_type: String,
    pub encoding: Option<String>,
    pub size: u64,
    pub name: String,
    pub path: String,
    /// Base64-encoded file content.
    pub content: Option<String>,
    pub sha: String,
    pub url: String,
    pub html_url: String,
}

/// Response from file create/update operations.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaFileResponse {
    pub content: Option<GiteaFileContent>,
    pub commit: GiteaCommit,
}

/// Commit metadata embedded in file responses.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaCommit {
    pub sha: String,
    pub url: String,
    pub html_url: String,
    pub message: String,
}

/// Pull request returned by Gitea.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaPullRequest {
    pub id: u64,
    pub number: u64,
    pub state: String,
    pub title: String,
    pub body: Option<String>,
    pub html_url: String,
    pub user: GiteaUser,
    pub head: GiteaBranch,
    pub base: GiteaBranch,
    pub mergeable: Option<bool>,
    pub merged: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Gitea user (minimal — only fields we use).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaUser {
    pub login: String,
    pub full_name: Option<String>,
}

/// Branch reference in a pull request.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaBranch {
    pub label: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    pub repo: Option<GiteaRepoBrief>,
}

/// Minimal repo context used inside PR branch refs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaRepoBrief {
    pub name: String,
    pub full_name: String,
}

/// Branch information returned by branch list endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaBranchInfo {
    pub name: String,
    pub commit: GiteaBranchCommit,
    pub protected: bool,
}

/// Commit reference within a branch listing.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaBranchCommit {
    pub id: String,
    pub message: Option<String>,
    pub timestamp: Option<String>,
}

/// Request body for creating or updating a file.
#[derive(Debug, Serialize)]
pub struct GiteaFileRequest {
    pub message: String,
    pub content: String, // base64-encoded
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(rename = "new_branch", skip_serializing_if = "Option::is_none")]
    pub new_branch: Option<String>,
}

/// Request body for deleting a file.
#[derive(Debug, Serialize)]
pub struct GiteaDeleteFileRequest {
    pub message: String,
    pub sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Request body for creating a pull request.
#[derive(Debug, Serialize)]
pub struct GiteaCreatePrRequest {
    pub title: String,
    pub head: String,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Response from merge endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteaMergeResponse {
    pub merged: Option<bool>,
    pub message: Option<String>,
}

/// Outcome of a successful pull-request merge via
/// [`crate::gitea::GiteaClient::merge_pull`] — the single merge code path
/// shared by the `gitea_merge_pr` tool and any future queue worker (GMQ-02+).
///
/// `base` is the pull request's REAL base branch, fetched from Gitea via
/// `GET /repos/{owner}/{repo}/pulls/{pr}` before the merge POST (Gitea's merge
/// endpoint itself returns `200` with no useful body on success, per
/// [`GiteaMergeResponse`]'s doc comment — there is no other source for it).
/// This replaces the pre-GMQ-01 bug where the tool's success string reported
/// the merge `style` (`merge`/`rebase`/`squash`) in the base branch's place.
#[derive(Debug, Clone, Serialize)]
pub struct GiteaMergeOutcome {
    /// Always `true` when this value exists (an `Err` is returned instead of
    /// a "not merged" outcome) — kept explicit for forward-compat with the
    /// stale-base guard's idempotent "already merged" success (GMQ-03).
    pub merged: bool,
    /// The pull request's real base branch (e.g. `"main"`).
    pub base: String,
    /// The pull request's head branch (e.g. `"feature/x"`), for callers that
    /// want to log/report the full `head -> base` picture.
    pub head: String,
}
