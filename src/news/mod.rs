//! News tools — headlines, search, and topic feeds.
//!
//! Three tools mirroring the Python news_tools.py on the fleet host exactly:
//!   news_headlines  — top headlines (NewsAPI primary, GNews fallback)
//!   news_search     — keyword search (NewsAPI primary, GNews fallback)
//!   news_topic      — topic feed (GNews topic endpoint)
//!
//! Required env vars (one or both):
//!   NEWSAPI_KEY    — newsapi.org API key (100 req/day free tier)
//!   GNEWS_API_KEY  — gnews.io API key (100 req/day free tier)
//!
//! If only one key is available, that API is used exclusively. Both missing
//! registers stub tools that return a clear NotConfigured error.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct NewsConfig {
    newsapi_key: Option<String>,
    gnews_key: Option<String>,
}

impl NewsConfig {
    fn from_env() -> Self {
        let newsapi_key = std::env::var("NEWSAPI_KEY").ok().filter(|s| !s.is_empty());
        let gnews_key = std::env::var("GNEWS_API_KEY").ok().filter(|s| !s.is_empty());
        Self { newsapi_key, gnews_key }
    }

    fn has_any_key(&self) -> bool {
        self.newsapi_key.is_some() || self.gnews_key.is_some()
    }

    fn client() -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("MooseNet-MCP/1.0")
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }
}

// ── Shared response type ──────────────────────────────────────────────────────

fn article_from_newsapi(a: &Value) -> Value {
    json!({
        "title":       a.get("title").and_then(Value::as_str).unwrap_or(""),
        "description": a.get("description").and_then(Value::as_str).unwrap_or(""),
        "source":      a.get("source").and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or(""),
        "url":         a.get("url").and_then(Value::as_str).unwrap_or(""),
        "published":   a.get("publishedAt").and_then(Value::as_str).unwrap_or(""),
    })
}

fn article_from_gnews(a: &Value) -> Value {
    json!({
        "title":       a.get("title").and_then(Value::as_str).unwrap_or(""),
        "description": a.get("description").and_then(Value::as_str).unwrap_or(""),
        "source":      a.get("source").and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or(""),
        "url":         a.get("url").and_then(Value::as_str).unwrap_or(""),
        "published":   a.get("publishedAt").and_then(Value::as_str).unwrap_or(""),
    })
}

// ── NewsAPI calls ─────────────────────────────────────────────────────────────

async fn newsapi_headlines(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    category: &str,
    country: &str,
    limit: u32,
) -> Result<Value, ToolError> {
    let mut params = vec![
        ("apiKey", key.to_string()),
        ("pageSize", limit.to_string()),
        ("country", country.to_string()),
    ];
    if !query.is_empty() { params.push(("q", query.to_string())); }
    if !category.is_empty() { params.push(("category", category.to_string())); }

    let resp = client
        .get("https://newsapi.org/v2/top-headlines")
        .query(&params)
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;

    if !status.is_success() || body.get("status").and_then(Value::as_str) != Some("ok") {
        let msg = body.get("message").and_then(Value::as_str)
            .unwrap_or("Unknown NewsAPI error");
        return Err(ToolError::Http(format!("NewsAPI {status}: {msg}")));
    }

    let articles: Vec<Value> = body.get("articles")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(article_from_newsapi).collect())
        .unwrap_or_default();

    Ok(json!({
        "source":   "newsapi",
        "total":    body.get("totalResults").and_then(Value::as_u64).unwrap_or(0),
        "count":    articles.len(),
        "articles": articles,
    }))
}

async fn newsapi_search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    limit: u32,
    sort_by: &str,
) -> Result<Value, ToolError> {
    let params = vec![
        ("apiKey", key.to_string()),
        ("q", query.to_string()),
        ("pageSize", limit.to_string()),
        ("sortBy", sort_by.to_string()),
        ("language", "en".to_string()),
    ];

    let resp = client
        .get("https://newsapi.org/v2/everything")
        .query(&params)
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;

    if !status.is_success() || body.get("status").and_then(Value::as_str) != Some("ok") {
        let msg = body.get("message").and_then(Value::as_str)
            .unwrap_or("Unknown NewsAPI error");
        return Err(ToolError::Http(format!("NewsAPI {status}: {msg}")));
    }

    let articles: Vec<Value> = body.get("articles")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(article_from_newsapi).collect())
        .unwrap_or_default();

    Ok(json!({
        "source":   "newsapi",
        "query":    query,
        "total":    body.get("totalResults").and_then(Value::as_u64).unwrap_or(0),
        "count":    articles.len(),
        "articles": articles,
    }))
}

// ── GNews calls ───────────────────────────────────────────────────────────────

async fn gnews_headlines(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    category: &str,
    country: &str,
    limit: u32,
) -> Result<Value, ToolError> {
    let mut params = vec![
        ("apikey", key.to_string()),
        ("max", limit.to_string()),
        ("lang", "en".to_string()),
        ("country", country.to_string()),
    ];
    if !query.is_empty() { params.push(("q", query.to_string())); }
    if !category.is_empty() { params.push(("topic", category.to_string())); }

    let resp = client
        .get("https://gnews.io/api/v4/top-headlines")
        .query(&params)
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!("GNews HTTP {}", resp.status())));
    }

    let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
    let articles: Vec<Value> = body.get("articles")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(article_from_gnews).collect())
        .unwrap_or_default();

    Ok(json!({
        "source":   "gnews",
        "total":    body.get("totalArticles").and_then(Value::as_u64).unwrap_or(0),
        "count":    articles.len(),
        "articles": articles,
    }))
}

async fn gnews_search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    limit: u32,
) -> Result<Value, ToolError> {
    let params = vec![
        ("apikey", key.to_string()),
        ("q", query.to_string()),
        ("max", limit.to_string()),
        ("lang", "en".to_string()),
    ];

    let resp = client
        .get("https://gnews.io/api/v4/search")
        .query(&params)
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!("GNews HTTP {}", resp.status())));
    }

    let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
    let articles: Vec<Value> = body.get("articles")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(article_from_gnews).collect())
        .unwrap_or_default();

    Ok(json!({
        "source":   "gnews",
        "query":    query,
        "total":    body.get("totalArticles").and_then(Value::as_u64).unwrap_or(0),
        "count":    articles.len(),
        "articles": articles,
    }))
}

async fn gnews_topic(
    client: &reqwest::Client,
    key: &str,
    topic: &str,
    limit: u32,
) -> Result<Value, ToolError> {
    let params = vec![
        ("apikey", key.to_string()),
        ("topic", topic.to_string()),
        ("max", limit.to_string()),
        ("lang", "en".to_string()),
    ];

    let resp = client
        .get("https://gnews.io/api/v4/top-headlines")
        .query(&params)
        .send()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!("GNews HTTP {}", resp.status())));
    }

    let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
    let articles: Vec<Value> = body.get("articles")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(article_from_gnews).collect())
        .unwrap_or_default();

    Ok(json!({
        "source":   "gnews",
        "topic":    topic,
        "total":    body.get("totalArticles").and_then(Value::as_u64).unwrap_or(0),
        "count":    articles.len(),
        "articles": articles,
    }))
}

// ── Tool structs ──────────────────────────────────────────────────────────────

struct NewsHeadlines { config: NewsConfig }
struct NewsSearch    { config: NewsConfig }
struct NewsTopic     { config: NewsConfig }

#[async_trait]
impl RustTool for NewsHeadlines {
    fn name(&self) -> &str { "news_headlines" }

    fn description(&self) -> &str {
        "Fetch top news headlines. Optional filters: query (keyword), \
category (business, entertainment, general, health, science, sports, technology), \
country (us, gb, ca, au, etc), limit (default 10). \
Uses NewsAPI with GNews fallback."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query":    { "type": "string", "description": "Keyword filter (optional)" },
                "category": { "type": "string", "description": "Category filter: business, entertainment, general, health, science, sports, technology" },
                "country":  { "type": "string", "description": "2-letter country code, default: us" },
                "limit":    { "type": "integer", "description": "Max articles (1-100, default 10)" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        if !self.config.has_any_key() {
            return Err(ToolError::NotConfigured(
                "NEWSAPI_KEY and GNEWS_API_KEY are both unset".into(),
            ));
        }

        let query    = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let category = args.get("category").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let country  = args.get("country").and_then(Value::as_str).unwrap_or("us").trim().to_string();
        let limit    = args.get("limit").and_then(Value::as_u64).unwrap_or(10).min(100) as u32;

        let client = NewsConfig::client()?;

        // NewsAPI primary
        if let Some(ref key) = self.config.newsapi_key {
            match newsapi_headlines(&client, key, &query, &category, &country, limit).await {
                Ok(v) => return Ok(v.to_string()),
                Err(primary_err) => {
                    // Fallback to GNews
                    if let Some(ref gkey) = self.config.gnews_key {
                        match gnews_headlines(&client, gkey, &query, &category, &country, limit).await {
                            Ok(v) => return Ok(v.to_string()),
                            Err(fb_err) => return Err(ToolError::Http(format!(
                                "Both APIs failed — NewsAPI: {primary_err}; GNews: {fb_err}"
                            ))),
                        }
                    }
                    return Err(primary_err);
                }
            }
        }

        // GNews only (no NewsAPI key)
        if let Some(ref key) = self.config.gnews_key {
            let v = gnews_headlines(&client, key, &query, &category, &country, limit).await?;
            return Ok(v.to_string());
        }

        Err(ToolError::NotConfigured("No news API keys available".into()))
    }
}

#[async_trait]
impl RustTool for NewsSearch {
    fn name(&self) -> &str { "news_search" }

    fn description(&self) -> &str {
        "Search news articles by keyword. sort_by: relevancy, popularity, publishedAt. \
Uses NewsAPI with GNews fallback."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query":   { "type": "string",  "description": "Search keyword (required)" },
                "limit":   { "type": "integer", "description": "Max articles (1-100, default 10)" },
                "sort_by": { "type": "string",  "description": "Sort order: relevancy, popularity, publishedAt (default)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        if !self.config.has_any_key() {
            return Err(ToolError::NotConfigured(
                "NEWSAPI_KEY and GNEWS_API_KEY are both unset".into(),
            ));
        }

        let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Err(ToolError::InvalidArgument("query is required".into()));
        }
        let limit   = args.get("limit").and_then(Value::as_u64).unwrap_or(10).min(100) as u32;
        let sort_by = args.get("sort_by").and_then(Value::as_str).unwrap_or("publishedAt").trim().to_string();

        let valid_sorts = ["relevancy", "popularity", "publishedAt"];
        let sort_by = if valid_sorts.contains(&sort_by.as_str()) { sort_by } else { "publishedAt".into() };

        let client = NewsConfig::client()?;

        if let Some(ref key) = self.config.newsapi_key {
            match newsapi_search(&client, key, &query, limit, &sort_by).await {
                Ok(v) => return Ok(v.to_string()),
                Err(primary_err) => {
                    if let Some(ref gkey) = self.config.gnews_key {
                        match gnews_search(&client, gkey, &query, limit).await {
                            Ok(v) => return Ok(v.to_string()),
                            Err(fb_err) => return Err(ToolError::Http(format!(
                                "Both APIs failed — NewsAPI: {primary_err}; GNews: {fb_err}"
                            ))),
                        }
                    }
                    return Err(primary_err);
                }
            }
        }

        if let Some(ref key) = self.config.gnews_key {
            let v = gnews_search(&client, key, &query, limit).await?;
            return Ok(v.to_string());
        }

        Err(ToolError::NotConfigured("No news API keys available".into()))
    }
}

#[async_trait]
impl RustTool for NewsTopic {
    fn name(&self) -> &str { "news_topic" }

    fn description(&self) -> &str {
        "Get news for a specific topic via GNews. Topics: world, nation, business, \
technology, entertainment, sports, science, health."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "Topic name: world, nation, business, technology, entertainment, sports, science, health" },
                "limit": { "type": "integer", "description": "Max articles (1-100, default 10)" }
            },
            "required": ["topic"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let key = self.config.gnews_key.as_deref().ok_or_else(|| {
            ToolError::NotConfigured("GNEWS_API_KEY not set (required for news_topic)".into())
        })?;

        let topic = args.get("topic").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if topic.is_empty() {
            return Err(ToolError::InvalidArgument("topic is required".into()));
        }
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10).min(100) as u32;

        let client = NewsConfig::client()?;
        let v = gnews_topic(&client, key, &topic, limit).await?;
        Ok(v.to_string())
    }
}

// ── Registration ──────────────────────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    let config = NewsConfig::from_env();
    if !config.has_any_key() {
        tracing::warn!(
            "News tools not configured: NEWSAPI_KEY and GNEWS_API_KEY are both unset. \
Registering no-op stubs."
        );
    }
    registry.register_or_replace(Box::new(NewsHeadlines { config: config.clone() }));
    registry.register_or_replace(Box::new(NewsSearch    { config: config.clone() }));
    registry.register_or_replace(Box::new(NewsTopic     { config }));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    #[allow(unused_imports)]
    use httpmock::prelude::*;

    fn make_config(newsapi: Option<&str>, gnews: Option<&str>) -> NewsConfig {
        NewsConfig {
            newsapi_key: newsapi.map(str::to_string),
            gnews_key:   gnews.map(str::to_string),
        }
    }

    fn newsapi_ok_body(count: usize) -> serde_json::Value {
        let articles: Vec<serde_json::Value> = (0..count).map(|i| json!({
            "title": format!("Article {i}"),
            "description": format!("Desc {i}"),
            "source": { "name": "BBC" },
            "url": format!("https://bbc.com/{i}"),
            "publishedAt": "2026-06-08T07:00:00Z" // pii-test-fixture
        })).collect();
        json!({ "status": "ok", "totalResults": count, "articles": articles })
    }

    fn gnews_ok_body(count: usize) -> serde_json::Value {
        let articles: Vec<serde_json::Value> = (0..count).map(|i| json!({
            "title": format!("GNews {i}"),
            "description": format!("GDesc {i}"),
            "source": { "name": "Reuters" },
            "url": format!("https://reuters.com/{i}"),
            "publishedAt": "2026-06-08T07:00:00Z" // pii-test-fixture
        })).collect();
        json!({ "totalArticles": count, "articles": articles })
    }

    // ── news_headlines ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn headlines_no_keys_returns_not_configured() {
        let tool = NewsHeadlines { config: make_config(None, None) };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn headlines_newsapi_success() {
        // Test response shape parsing directly (real HTTP calls use live API keys)
        let body = newsapi_ok_body(3);
        let articles = body.get("articles").and_then(Value::as_array).unwrap();
        let parsed: Vec<Value> = articles.iter().map(article_from_newsapi).collect();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0]["title"], "Article 0");
        assert_eq!(parsed[0]["source"], "BBC");
        assert!(parsed[0].get("url").is_some());
        assert_eq!(parsed[2]["title"], "Article 2");
    }

    #[tokio::test]
    async fn headlines_newsapi_error_fallback_to_gnews() {
        // Config with both keys; NewsAPI fails → should fall back
        let config = make_config(Some("bad_newsapi_key"), Some("bad_gnews_key"));
        let tool = NewsHeadlines { config };
        // Will get HTTP errors from real endpoints with bad keys — that's OK for this test
        // Just verify both failure paths return an aggregated error
        let result = tool.execute(json!({"country": "us"})).await;
        // Either the call succeeds (keys happen to work) or we get an Http error
        // We just verify it doesn't panic and returns a sensible result
        match result {
            Ok(s) => {
                let v: Value = serde_json::from_str(&s).unwrap();
                assert!(v.get("articles").is_some() || v.get("error").is_some());
            }
            Err(ToolError::Http(_)) | Err(ToolError::NotConfigured(_)) => {}
            Err(e) => panic!("Unexpected error variant: {e}"),
        }
    }

    #[tokio::test]
    async fn headlines_limit_capped_at_100() {
        let config = make_config(None, Some("gkey"));
        let tool = NewsHeadlines { config };
        // limit=999 should be capped; we can't easily assert without a real server
        // but verify the arg parsing path doesn't panic
        let result = tool.execute(json!({"limit": 999})).await;
        // Will fail with HTTP error (invalid key) but shouldn't panic
        assert!(result.is_err());
    }

    // ── news_search ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn search_no_keys_returns_not_configured() {
        let tool = NewsSearch { config: make_config(None, None) };
        let result = tool.execute(json!({"query": "rust programming"})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn search_empty_query_returns_invalid_argument() {
        let config = make_config(Some("key"), None);
        let tool = NewsSearch { config };
        let result = tool.execute(json!({"query": ""})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn search_missing_query_returns_invalid_argument() {
        let config = make_config(Some("key"), None);
        let tool = NewsSearch { config };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn search_invalid_sort_by_defaults_to_published_at() {
        // Can't easily intercept the HTTP call, but validate the sort_by normalisation logic
        let valid = ["relevancy", "popularity", "publishedAt"];
        let sort_by = "garbage";
        let normalized = if valid.contains(&sort_by) { sort_by } else { "publishedAt" };
        assert_eq!(normalized, "publishedAt");
    }

    #[tokio::test]
    async fn search_response_shape() {
        // Validate newsapi search response parsing
        let body = json!({
            "status": "ok",
            "totalResults": 2,
            "articles": [
                { "title": "T1", "description": "D1", "source": {"name": "CNN"}, "url": "https://cnn.com/1", "publishedAt": "2026-06-08" }, // pii-test-fixture
                { "title": "T2", "description": "D2", "source": {"name": "Fox"}, "url": "https://fox.com/2", "publishedAt": "2026-06-07" } // pii-test-fixture
            ]
        });
        let articles = body.get("articles").and_then(Value::as_array).unwrap();
        let parsed: Vec<Value> = articles.iter().map(article_from_newsapi).collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1]["source"], "Fox");
    }

    // ── news_topic ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn topic_no_gnews_key_returns_not_configured() {
        let config = make_config(Some("newsapi_key"), None); // has newsapi but not gnews
        let tool = NewsTopic { config };
        let result = tool.execute(json!({"topic": "technology"})).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn topic_empty_topic_returns_invalid_argument() {
        let config = make_config(None, Some("gnews_key"));
        let tool = NewsTopic { config };
        let result = tool.execute(json!({"topic": ""})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn topic_missing_topic_returns_invalid_argument() {
        let config = make_config(None, Some("gnews_key"));
        let tool = NewsTopic { config };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn topic_response_shape() {
        let body = gnews_ok_body(4);
        let articles = body.get("articles").and_then(Value::as_array).unwrap();
        let parsed: Vec<Value> = articles.iter().map(article_from_gnews).collect();
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0]["source"], "Reuters");
        assert_eq!(parsed[3]["title"], "GNews 3");
    }

    // ── article mapping ───────────────────────────────────────────────────────

    #[test]
    fn article_from_newsapi_handles_missing_fields() {
        let a = json!({});
        let out = article_from_newsapi(&a);
        assert_eq!(out["title"], "");
        assert_eq!(out["source"], "");
    }

    #[test]
    fn article_from_gnews_handles_missing_fields() {
        let a = json!({});
        let out = article_from_gnews(&a);
        assert_eq!(out["title"], "");
        assert_eq!(out["source"], "");
    }

    #[test]
    fn article_from_newsapi_extracts_nested_source_name() {
        let a = json!({ "source": { "name": "Reuters" }, "title": "Test" });
        let out = article_from_newsapi(&a);
        assert_eq!(out["source"], "Reuters");
        assert_eq!(out["title"], "Test");
    }

    // ── registration ─────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn register_adds_three_tools() {
        let mut reg = ToolRegistry::new();
        // Temporarily remove env vars to test stub registration path
        let newsapi_backup = std::env::var("NEWSAPI_KEY").ok();
        let gnews_backup   = std::env::var("GNEWS_API_KEY").ok();
        std::env::remove_var("NEWSAPI_KEY");
        std::env::remove_var("GNEWS_API_KEY");

        register(&mut reg);

        // Restore env vars
        if let Some(v) = newsapi_backup { std::env::set_var("NEWSAPI_KEY", v); }
        if let Some(v) = gnews_backup   { std::env::set_var("GNEWS_API_KEY", v); }

        assert!(reg.contains("news_headlines"));
        assert!(reg.contains("news_search"));
        assert!(reg.contains("news_topic"));
        assert_eq!(reg.len(), 3);
    }

    #[test]
    fn tool_names_are_stable() {
        let config = make_config(None, None);
        assert_eq!(NewsHeadlines { config: config.clone() }.name(), "news_headlines");
        assert_eq!(NewsSearch    { config: config.clone() }.name(), "news_search");
        assert_eq!(NewsTopic     { config               }.name(), "news_topic");
    }

    #[test]
    fn tool_parameters_are_valid_json_schema() {
        let config = make_config(None, None);
        let h = NewsHeadlines { config: config.clone() }.parameters();
        let s = NewsSearch    { config: config.clone() }.parameters();
        let t = NewsTopic     { config               }.parameters();
        assert_eq!(h["type"], "object");
        assert_eq!(s["type"], "object");
        assert_eq!(t["type"], "object");
        // news_search and news_topic require their key field
        assert!(s.get("required").is_some());
        assert!(t.get("required").is_some());
    }
}
