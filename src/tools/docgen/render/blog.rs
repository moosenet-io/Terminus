//! Blog-post renderer (DOCGEN-06).
//!
//! Renders generated content into a blog-post-shaped Markdown artifact
//! (front-matter-free -- most blog platforms' publish APIs take title +
//! body separately rather than YAML front matter) that the calling harness
//! would later POST to the configured blog platform's publish API. This
//! renderer itself never makes that publish call -- same write-model
//! inversion as [`super::notion`]; see `render/mod.rs`'s doc comment.
//!
//! Mirrors [`super::notion`]'s seam pattern exactly: a read-only
//! `BlogClient::validate()` credential/connectivity check gates rendering,
//! a real HTTP-backed [`HttpBlogClient`] implements it, and tests inject a
//! mock so no test ever makes a real network call.

use async_trait::async_trait;

use super::{RenderContext, RenderedArtifact};
use crate::tools::docgen::config::DocTargetType;

/// Seam between this renderer and the configured blog platform's API.
#[async_trait]
pub trait BlogClient: Send + Sync {
    /// Read-only credential/connectivity check. Must never publish
    /// anything.
    async fn validate(&self) -> Result<(), String>;
}

/// Real `BlogClient`, authenticated with an already-resolved token value
/// (resolved by `render/mod.rs::resolve_credential("DOCGEN_BLOG_API_TOKEN")`
/// -- never a literal here). The concrete blog platform's base URL is
/// itself vault/config-driven (`DOCGEN_BLOG_API_URL`, resolved the same
/// way as the token, never hardcoded), so this client works against
/// whichever platform the operator has configured rather than assuming
/// one.
#[derive(Debug, Clone)]
pub struct HttpBlogClient {
    token: String,
    http: reqwest::Client,
}

impl HttpBlogClient {
    pub fn new(token: String) -> Self {
        Self { token, http: reqwest::Client::new() }
    }

    fn base_url(&self) -> Option<String> {
        super::resolve_credential("DOCGEN_BLOG_API_URL")
    }
}

#[async_trait]
impl BlogClient for HttpBlogClient {
    async fn validate(&self) -> Result<(), String> {
        let Some(base) = self.base_url() else {
            return Err(
                "blog platform base URL not configured (DOCGEN_BLOG_API_URL unset)".to_string(),
            );
        };
        let resp = self
            .http
            .get(format!("{}/whoami", base.trim_end_matches('/')))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("blog platform connectivity check failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("blog platform credential rejected: HTTP {}", resp.status()));
        }
        Ok(())
    }
}

pub async fn render(ctx: &RenderContext<'_>, client: &dyn BlogClient) -> RenderedArtifact {
    if let Err(e) = client.validate().await {
        return RenderedArtifact::skipped(
            DocTargetType::Blog,
            "blog-markdown",
            format!("blog target skipped: {e}"),
        );
    }

    let title = ctx.module;
    let body = ctx.content.trim();
    let rendered = format!(
        "# {title}\n\n{body}\n\n---\n_Generated {} from {} in {}._\n",
        ctx.generated_at, ctx.source_commit, ctx.project
    );
    RenderedArtifact::rendered(DocTargetType::Blog, "blog-markdown", rendered)
}

/// Test-only mock clients shared with `render/mod.rs`'s cross-target tests.
#[cfg(test)]
pub mod tests_support {
    use super::*;

    pub struct AlwaysOkBlogClient;
    #[async_trait]
    impl BlogClient for AlwaysOkBlogClient {
        async fn validate(&self) -> Result<(), String> {
            Ok(())
        }
    }

    pub struct AlwaysFailBlogClient;
    #[async_trait]
    impl BlogClient for AlwaysFailBlogClient {
        async fn validate(&self) -> Result<(), String> {
            Err("blog platform credential rejected: HTTP 403".to_string())
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
    async fn renders_valid_blog_markdown_from_sample_content() {
        let client = AlwaysOkBlogClient;
        let artifact = render(&ctx("The widget does A."), &client).await;
        assert!(artifact.was_rendered());
        let content = artifact.content.unwrap();
        assert!(content.starts_with("# src/widget"));
        assert!(content.contains("The widget does A."));
    }

    /// Negative test: validation failure skips with a clear note, not a
    /// fabricated artifact.
    #[tokio::test]
    async fn validation_failure_skips_with_clear_note() {
        let client = AlwaysFailBlogClient;
        let artifact = render(&ctx("Body."), &client).await;
        assert!(!artifact.was_rendered());
        assert!(artifact.note.unwrap().contains("403"));
    }
}
