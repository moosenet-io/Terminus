//! Plane CE API response types for deserialization.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub identifier: String,
    #[serde(default)]
    pub network: u8,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Issue {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_html: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub state_detail: Option<StateDetail>,
    #[serde(default)]
    pub priority: Option<String>,
    pub project: String,
    pub workspace: String,
    #[serde(default)]
    pub sequence_id: Option<u64>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub label_ids: Vec<String>,
    #[serde(default)]
    pub cycle_id: Option<String>,
    #[serde(default)]
    pub module_ids: Vec<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StateDetail {
    pub id: String,
    pub name: String,
    pub color: String,
    pub group: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct State {
    pub id: String,
    pub name: String,
    pub color: String,
    pub group: String,
    pub project: String,
    #[serde(default)]
    pub sequence: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    pub project: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Member {
    pub id: String,
    #[serde(default)]
    pub member: Option<MemberDetail>,
    pub role: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemberDetail {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Cycle {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub project: String,
    pub workspace: String,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Module {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub project: String,
    pub workspace: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub target_date: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Comment {
    pub id: String,
    #[serde(default)]
    pub comment_html: Option<String>,
    #[serde(default)]
    pub comment_stripped: Option<String>,
    pub issue: String,
    pub project: String,
    #[serde(default)]
    pub actor_detail: Option<MemberDetail>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Activity {
    pub id: String,
    #[serde(default)]
    pub verb: Option<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub old_value: Option<String>,
    #[serde(default)]
    pub new_value: Option<String>,
    #[serde(default)]
    pub issue: Option<String>,
    pub project: String,
    #[serde(default)]
    pub actor_detail: Option<MemberDetail>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Plane paginated list response wrapper.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaginatedList<T> {
    pub count: Option<u64>,
    pub next: Option<String>,
    pub previous: Option<String>,
    pub results: Vec<T>,
}

/// Plane API may return arrays directly or paginated objects.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ApiList<T> {
    Paginated(PaginatedList<T>),
    Array(Vec<T>),
}

impl<T> ApiList<T> {
    pub fn into_items(self) -> Vec<T> {
        match self {
            ApiList::Paginated(p) => p.results,
            ApiList::Array(a) => a,
        }
    }

    pub fn total_count(&self) -> usize {
        match self {
            ApiList::Paginated(p) => p.count.unwrap_or(0) as usize,
            ApiList::Array(a) => a.len(),
        }
    }
}

/// Fields for creating a work item (issue).
#[derive(Debug, Clone, Serialize)]
pub struct CreateIssueRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub label_ids: Vec<String>,
}

/// Fields for updating a work item (issue).
#[derive(Debug, Clone, Serialize)]
pub struct UpdateIssueRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
}

/// Fields for creating a module.
#[derive(Debug, Clone, Serialize)]
pub struct CreateModuleRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_date: Option<String>,
}

/// Fields for creating a comment.
#[derive(Debug, Clone, Serialize)]
pub struct CreateCommentRequest {
    pub comment_html: String,
}
