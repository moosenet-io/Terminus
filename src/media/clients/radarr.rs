//! Radarr client — thin typed wrapper for movie search/status lookups.
//!
//! Radarr owns the "movies" side of the media stack: it holds the movie
//! library, drives quality-profile-based acquisition, and hands searches off
//! to Prowlarr/indexers and the download client (qtor). This module is
//! deliberately thin — MEDIA-01 only establishes the client + config +
//! one representative operation (`lookup_movie`, Radarr's own
//! `/api/v3/movie/lookup`) so later items (MEDIA-02 search, MEDIA-03
//! request/download) have a typed, mock-tested foundation to build on. It is
//! not a full Radarr API mirror.
//!
//! ## Configuration
//! - `RADARR_URL`     — base URL, e.g. `http://<radarr-host>:7878`
//! - `RADARR_API_KEY` — sent as the `X-Api-Key` header
//!
//! Both must be set for [`RadarrClient::from_env`] to succeed; otherwise it
//! returns `ToolError::NotConfigured` naming the missing variable(s) so a
//! caller can surface a clear setup hint rather than a generic failure.

use serde_json::Value;

use crate::error::ToolError;

#[derive(Clone)]
pub struct RadarrClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl RadarrClient {
    /// Build a client from `RADARR_URL` + `RADARR_API_KEY`. Never panics;
    /// missing/empty config maps to a clear `NotConfigured` error.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("RADARR_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("RADARR_URL not set".into()))?;
        let api_key = std::env::var("RADARR_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("RADARR_API_KEY not set".into()))?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self { base_url, api_key, http })
    }

    /// Build a client directly from parts (used by tests / callers wiring a
    /// mock server; production code should use [`Self::from_env`]).
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            http,
        }
    }

    /// `GET /api/v3/movie/lookup?term={term}` — resolve a free-text title to
    /// candidate movies Radarr/its indexers know about. Thin: returns the raw
    /// parsed JSON array; response shaping for narration is MEDIA-02's job.
    pub async fn lookup_movie(&self, term: &str) -> Result<Value, ToolError> {
        let url = format!("{}/api/v3/movie/lookup", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.api_key)
            .query(&[("term", term)])
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Radarr unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /api/v3/movie` — the current library (used by later items for
    /// presence/status checks). Thin passthrough of the parsed JSON.
    pub async fn library(&self) -> Result<Value, ToolError> {
        let url = format!("{}/api/v3/movie", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.api_key)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Radarr unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `POST /api/v3/movie` — add a movie to the library, driving Radarr's
    /// own indexer search + grab (which hands the completed download to the
    /// configured download client, qtor) when `addOptions.searchForMovie` is
    /// `true` in `body`. MEDIA-03: this is the only mutation this client
    /// exposes; it stays a thin passthrough of a caller-built body -- the
    /// tiering/confirmation decision about whether to call this at all lives
    /// in `crate::media::request`, not here.
    pub async fn add_movie(&self, body: Value) -> Result<Value, ToolError> {
        let url = format!("{}/api/v3/movie", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-Api-Key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Radarr unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `PUT /api/v3/movie/{id}` — update an existing movie resource (tags,
    /// `monitored`, `qualityProfileId`, collection membership, ...). MEDIA-04:
    /// thin passthrough of a caller-built full resource body -- Radarr's PUT
    /// expects the complete updated resource, not a partial patch, so callers
    /// must round-trip through a prior `library()`/lookup read. Used for both
    /// non-destructive organize actions (tag, monitor toggle) and for the
    /// high-impact quality-profile-change path, which `organize.rs` treats as
    /// destructive and hard-gates before ever calling this.
    pub async fn update_movie(&self, id: i64, body: Value) -> Result<Value, ToolError> {
        let url = format!("{}/api/v3/movie/{id}", self.base_url);
        let resp = self
            .http
            .put(&url)
            .header("X-Api-Key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Radarr unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `DELETE /api/v3/movie/{id}?deleteFiles=true&addImportExclusion=false`
    /// — remove a movie from the Radarr library **and** delete its files on
    /// disk. MEDIA-04: this is the one truly destructive operation this
    /// client exposes; the hard-typed-confirmation gate lives entirely in
    /// `crate::media::organize`, never here -- this method stays a thin,
    /// unconditional passthrough so the safety logic has exactly one place
    /// to live and be tested. Returns `Ok(false)` (not `Err`) when Radarr
    /// reports the id doesn't exist, so callers can render a clean no-op
    /// message instead of a false error.
    pub async fn delete_movie(&self, id: i64) -> Result<bool, ToolError> {
        let url = format!("{}/api/v3/movie/{id}", self.base_url);
        let resp = self
            .http
            .delete(&url)
            .header("X-Api-Key", &self.api_key)
            .query(&[("deleteFiles", "true"), ("addImportExclusion", "false")])
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Radarr unavailable: {e}")))?;

        match map_response(resp).await {
            Ok(_) => Ok(true),
            Err(ToolError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Shared status-mapping: 404 -> NotFound, other 4xx -> api-error Http,
/// 5xx -> unavailable Http, else parse the body as JSON.
async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::NotFound("Radarr resource not found".into()));
    }
    if status.is_client_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "Radarr API error (HTTP {status}): {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    if status.is_server_error() {
        return Err(ToolError::Http(format!("Radarr unavailable (HTTP {status})")));
    }

    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from Radarr: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_client(base_url: &str) -> RadarrClient {
        RadarrClient::new(base_url, "testkey", reqwest::Client::new())
    }

    #[test]
    #[serial]
    fn from_env_missing_url_is_not_configured() {
        let url = std::env::var("RADARR_URL").ok();
        let key = std::env::var("RADARR_API_KEY").ok();
        std::env::remove_var("RADARR_URL");
        std::env::remove_var("RADARR_API_KEY");

        let result = RadarrClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("RADARR_URL", u); }
        if let Some(k) = key { std::env::set_var("RADARR_API_KEY", k); }
    }

    #[test]
    #[serial]
    fn from_env_missing_key_is_not_configured() {
        let url = std::env::var("RADARR_URL").ok();
        let key = std::env::var("RADARR_API_KEY").ok();
        std::env::set_var("RADARR_URL", "http://radarr.test:7878");
        std::env::remove_var("RADARR_API_KEY");

        let result = RadarrClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("RADARR_URL", u); } else { std::env::remove_var("RADARR_URL"); }
        if let Some(k) = key { std::env::set_var("RADARR_API_KEY", k); }
    }

    #[test]
    #[serial]
    fn from_env_builds_when_both_set() {
        let url = std::env::var("RADARR_URL").ok();
        let key = std::env::var("RADARR_API_KEY").ok();
        std::env::set_var("RADARR_URL", "http://radarr.test:7878/");
        std::env::set_var("RADARR_API_KEY", "abc123");

        let client = RadarrClient::from_env().expect("should construct");
        assert_eq!(client.base_url, "http://radarr.test:7878");

        if let Some(u) = url { std::env::set_var("RADARR_URL", u); } else { std::env::remove_var("RADARR_URL"); }
        if let Some(k) = key { std::env::set_var("RADARR_API_KEY", k); } else { std::env::remove_var("RADARR_API_KEY"); }
    }

    #[tokio::test]
    async fn lookup_movie_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie/lookup").query_param("term", "dune");
            then.status(200).json_body(json!([{ "title": "Dune", "tmdbId": 438631 }]));
        });

        let client = test_client(&server.base_url());
        let result = client.lookup_movie("dune").await.unwrap();
        mock.assert();
        assert_eq!(result[0]["title"], "Dune");
    }

    #[tokio::test]
    async fn library_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "title": "Arrival", "hasFile": true }]));
        });

        let client = test_client(&server.base_url());
        let result = client.library().await.unwrap();
        mock.assert();
        assert_eq!(result[0]["title"], "Arrival");
    }

    #[tokio::test]
    async fn add_movie_posts_body_and_parses_mocked_201() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v3/movie")
                .json_body(json!({ "tmdbId": 438631, "title": "Dune", "qualityProfileId": 1, "rootFolderPath": "/movies", "monitored": true, "addOptions": { "searchForMovie": true } }));
            then.status(201).json_body(json!({ "id": 7, "title": "Dune" }));
        });

        let client = test_client(&server.base_url());
        let body = json!({ "tmdbId": 438631, "title": "Dune", "qualityProfileId": 1, "rootFolderPath": "/movies", "monitored": true, "addOptions": { "searchForMovie": true } });
        let result = client.add_movie(body).await.unwrap();
        mock.assert();
        assert_eq!(result["id"], 7);
    }

    #[tokio::test]
    async fn add_movie_server_error_maps_to_http() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/v3/movie");
            then.status(500);
        });

        let client = test_client(&server.base_url());
        let result = client.add_movie(json!({})).await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    async fn not_found_maps_to_not_found_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie/lookup");
            then.status(404);
        });

        let client = test_client(&server.base_url());
        let result = client.lookup_movie("nothing").await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    async fn server_error_maps_to_http_unavailable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie/lookup");
            then.status(500);
        });

        let client = test_client(&server.base_url());
        let result = client.lookup_movie("x").await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    async fn client_error_maps_to_http_api_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie/lookup");
            then.status(401).body("Unauthorized");
        });

        let client = test_client(&server.base_url());
        let result = client.lookup_movie("x").await;
        match result {
            Err(ToolError::Http(msg)) => assert!(msg.contains("401")),
            other => panic!("expected Http error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_movie_puts_body_and_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(PUT).path("/api/v3/movie/7");
            then.status(200).json_body(json!({ "id": 7, "monitored": false }));
        });

        let client = test_client(&server.base_url());
        let result = client.update_movie(7, json!({ "id": 7, "monitored": false })).await.unwrap();
        mock.assert();
        assert_eq!(result["id"], 7);
    }

    #[tokio::test]
    async fn delete_movie_present_returns_true() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/v3/movie/7")
                .query_param("deleteFiles", "true");
            then.status(200);
        });

        let client = test_client(&server.base_url());
        let deleted = client.delete_movie(7).await.unwrap();
        mock.assert();
        assert!(deleted);
    }

    #[tokio::test]
    async fn delete_movie_not_present_returns_false_not_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/999");
            then.status(404);
        });

        let client = test_client(&server.base_url());
        let deleted = client.delete_movie(999).await.unwrap();
        assert!(!deleted, "a 404 from Radarr must map to Ok(false), not an error");
    }

    #[tokio::test]
    async fn delete_movie_server_error_maps_to_http() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/7");
            then.status(500);
        });

        let client = test_client(&server.base_url());
        let result = client.delete_movie(7).await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    async fn unreachable_server_maps_to_http_error_not_panic() {
        // Port 1 is reliably closed/unreachable in test sandboxes.
        let client = RadarrClient::new("http://127.0.0.1:1", "k", reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .unwrap());
        let result = client.lookup_movie("x").await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }
}
