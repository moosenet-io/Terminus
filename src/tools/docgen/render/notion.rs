//! Notion renderer (DOCGEN-06).
//!
//! Converts generated content into a Notion "blocks" JSON structure -- the
//! shape the calling harness would later POST to Notion's Pages API to
//! actually create/update a page. This renderer itself NEVER makes that
//! placement call (see `render/mod.rs`'s write-model-inversion doc
//! comment); it returns the blocks JSON as a plain artifact string.
//!
//! ## The one read-only network touch: credential validation
//! Before rendering, this module asks its [`NotionClient`] to `validate()`
//! the configured credential -- a read-only check (real implementation:
//! Notion's `GET /v1/users/me`, which never creates/updates/publishes
//! anything). A validation failure (bad/expired token, Notion unreachable)
//! skips the target with a clear note while every other declared target
//! still renders (spec EDGE CASE: "Notion/Obsidian/blog API failure -> that
//! target skipped + noted, others succeed"). This mirrors the seam pattern
//! `crate::tools::docgen::generate::DocGenerator` already established for
//! DOCGEN-05 (a trait + a real HTTP impl + test mocks), so tests never make
//! a real network call.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{RenderContext, RenderedArtifact};
use crate::tools::docgen::config::DocTargetType;

/// Seam between this renderer and Notion's API. [`HttpNotionClient`] is the
/// real implementation; tests inject a mock instead.
#[async_trait]
pub trait NotionClient: Send + Sync {
    /// Read-only credential/connectivity check. Must never create, update,
    /// or publish anything.
    async fn validate(&self) -> Result<(), String>;
}

/// Real `NotionClient`, authenticated with an already-resolved token value
/// (resolved by `render/mod.rs::resolve_credential("NOTION_TOKEN")` --
/// never a literal here; this struct never reads the environment itself).
#[derive(Debug, Clone)]
pub struct HttpNotionClient {
    token: String,
    http: reqwest::Client,
}

impl HttpNotionClient {
    pub fn new(token: String) -> Self {
        Self { token, http: reqwest::Client::new() }
    }
}

#[async_trait]
impl NotionClient for HttpNotionClient {
    async fn validate(&self) -> Result<(), String> {
        let resp = self
            .http
            .get("https://api.notion.com/v1/users/me")
            .bearer_auth(&self.token)
            .header("Notion-Version", "2022-06-28")
            .send()
            .await
            .map_err(|e| format!("notion connectivity check failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("notion credential rejected: HTTP {}", resp.status()));
        }
        Ok(())
    }
}

/// Convert markdown-ish generated content into a minimal, valid Notion
/// blocks array: ATX headings become `heading_1`/`heading_2`/`heading_3`
/// blocks (levels 4-6 fold into `heading_3`, Notion's deepest heading
/// block), everything else becomes `paragraph` blocks. Blank lines are
/// skipped (Notion has no "empty paragraph" convention worth preserving
/// here).
fn content_to_blocks(content: &str) -> Value {
    let blocks: Vec<Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let trimmed = line.trim_start();
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            if level > 0 && trimmed.as_bytes().get(level) == Some(&b' ') {
                let text = trimmed[level + 1..].to_string();
                let heading_type = match level.min(3) {
                    1 => "heading_1",
                    2 => "heading_2",
                    _ => "heading_3",
                };
                json!({
                    "object": "block",
                    "type": heading_type,
                    heading_type: { "rich_text": [{"type": "text", "text": {"content": text}}] }
                })
            } else {
                json!({
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": { "rich_text": [{"type": "text", "text": {"content": line}}] }
                })
            }
        })
        .collect();
    json!(blocks)
}

pub async fn render(ctx: &RenderContext<'_>, client: &dyn NotionClient) -> RenderedArtifact {
    if let Err(e) = client.validate().await {
        return RenderedArtifact::skipped(
            DocTargetType::Notion,
            "notion-blocks-json",
            format!("notion target skipped: {e}"),
        );
    }

    let blocks = content_to_blocks(ctx.content);
    let payload = json!({
        "title": ctx.module,
        "source_commit": ctx.source_commit,
        "generated_at": ctx.generated_at,
        "children": blocks,
    });
    let rendered = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
    RenderedArtifact::rendered(DocTargetType::Notion, "notion-blocks-json", rendered)
}

/// Test-only mock clients shared with `render/mod.rs`'s integration-style
/// tests (which exercise `render_all` across all six targets at once and
/// need a Notion mock without duplicating one).
#[cfg(test)]
pub mod tests_support {
    use super::*;

    pub struct AlwaysOkNotionClient;
    #[async_trait]
    impl NotionClient for AlwaysOkNotionClient {
        async fn validate(&self) -> Result<(), String> {
            Ok(())
        }
    }

    pub struct AlwaysFailNotionClient;
    #[async_trait]
    impl NotionClient for AlwaysFailNotionClient {
        async fn validate(&self) -> Result<(), String> {
            Err("notion credential rejected: HTTP 401".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;

    fn ctx<'a>(content: &'a str) -> RenderContext<'a> {
        RenderContext {
            project: "widget-factory",
            module: "src/widget",
            source_commit: "abc123",
            generated_at: "2026-07-11T00:00:00Z",
            content,
        }
    }

    #[tokio::test]
    async fn renders_valid_notion_blocks_json_from_sample_content() {
        let client = AlwaysOkNotionClient;
        let artifact = render(&ctx("# Widget\n\nThe widget does A."), &client).await;
        assert!(artifact.was_rendered());
        let content = artifact.content.unwrap();
        let parsed: Value = serde_json::from_str(&content).expect("must be valid JSON");
        assert_eq!(parsed["children"][0]["type"], json!("heading_1"));
        assert_eq!(parsed["children"][1]["type"], json!("paragraph"));
    }

    /// Negative test: validation failure (bad credential / API down) skips
    /// with a clear note, not a fabricated artifact.
    #[tokio::test]
    async fn validation_failure_skips_with_clear_note() {
        let client = AlwaysFailNotionClient;
        let artifact = render(&ctx("# Widget\n\nBody."), &client).await;
        assert!(!artifact.was_rendered());
        assert!(artifact.note.unwrap().contains("401"));
    }

    #[test]
    fn content_to_blocks_produces_an_array() {
        let blocks = content_to_blocks("# Title\n\nBody line.");
        assert!(blocks.is_array());
        assert_eq!(blocks.as_array().unwrap().len(), 2);
    }
}
