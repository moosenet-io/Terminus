//! Plex client — thin typed wrapper for library/history reads.
//!
//! Plex is the consumption/history layer: library sections, watch history,
//! and on-deck/continue-watching all live here. MEDIA-01 scaffolded config +
//! `library_sections` + `history`; MEDIA-05 (`crate::media::recommend`) adds
//! `on_deck` (continue-watching) and `recently_added` (engagement surface)
//! on top of that same thin-passthrough shape.
//!
//! MUSEL-LIVE (MUSE #111 phase 1) adds the one read that was missing: the
//! LIVE one — [`PlexClient::sessions`] over `/status/sessions`, plus the
//! cheap [`PlexClient::identity`] server header. Everything above it is
//! historical; nothing in this tree read currently-playing state before.
//!
//! ## Configuration
//! - `PLEX_URL`   — base URL, e.g. `http://<plex-host>:32400`
//! - `PLEX_TOKEN` — sent as the `X-Plex-Token` header (also accepted as a
//!   query param by the real Plex API, but the header form avoids leaking
//!   the token into access logs / URLs).
//!
//! ## Why the live read has its OWN error type
//! The historical reads collapse every failure into [`ToolError::Http`],
//! which is fine when the answer is "no history to show". It is NOT fine
//! for live sessions: "I could not reach Plex", "Plex refused my token" and
//! "nobody is watching" are three different facts about the household, and a
//! dashboard that renders them identically is actively misleading. So
//! `sessions()` returns [`PlexSessionsError`], which keeps the transport
//! failure and the credential rejection apart at the type level rather than
//! by string-matching a message downstream.

use serde_json::Value;

use crate::error::ToolError;

/// Why a LIVE session read did not produce a session list (MUSEL-LIVE).
///
/// Deliberately NOT folded into [`ToolError`]: the whole point of this item is
/// that the caller can tell these apart, and a shared `Http(String)` variant
/// would force the tool layer to re-derive the distinction from prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlexSessionsError {
    /// No usable HTTP answer: connection refused, DNS failure, TLS error,
    /// timeout — or a 5xx, which means the server is there but cannot serve.
    /// Either way we do not know what is playing, which is NOT the same as
    /// knowing that nothing is.
    Unreachable(String),
    /// Plex answered and rejected the credential (401/403). The server is up;
    /// `PLEX_TOKEN` is wrong, expired, or lacks access. An operator fixes this
    /// by rotating a secret, not by restarting anything.
    TokenRejected(String),
    /// Plex answered 2xx but the body was not the JSON we can read (or the
    /// status was an unexpected 4xx). A protocol/compatibility problem.
    Malformed(String),
}

impl PlexSessionsError {
    /// Stable machine-readable discriminant for the tool/GUI contract.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unreachable(_) => "unreachable",
            Self::TokenRejected(_) => "unauthorized",
            Self::Malformed(_) => "malformed",
        }
    }

    /// The human-readable detail. Never contains the token: the token travels
    /// in a header, and no branch here echoes a request header back.
    pub fn detail(&self) -> &str {
        match self {
            Self::Unreachable(d) | Self::TokenRejected(d) | Self::Malformed(d) => d,
        }
    }
}

impl std::fmt::Display for PlexSessionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind(), self.detail())
    }
}

#[derive(Clone)]
pub struct PlexClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl PlexClient {
    /// Build a client from `PLEX_URL` + `PLEX_TOKEN`. Never panics;
    /// missing/empty config maps to a clear `NotConfigured` error.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("PLEX_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("PLEX_URL not set".into()))?;
        let token = std::env::var("PLEX_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("PLEX_TOKEN not set".into()))?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self { base_url, token, http })
    }

    pub fn new(base_url: impl Into<String>, token: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http,
        }
    }

    /// `GET /library/sections` — the configured library sections.
    pub async fn library_sections(&self) -> Result<Value, ToolError> {
        let url = format!("{}/library/sections", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Plex unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /status/sessions/history/all` — recent watch history (thin
    /// passthrough; used by MEDIA-05's recommendation rationale).
    pub async fn history(&self) -> Result<Value, ToolError> {
        let url = format!("{}/status/sessions/history/all", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Plex unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /library/onDeck` — Plex's own "continue watching" surface
    /// (in-progress + next-up episodes). Thin passthrough; used by
    /// MEDIA-05's `media_on_deck` tool.
    pub async fn on_deck(&self) -> Result<Value, ToolError> {
        let url = format!("{}/library/onDeck", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Plex unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /library/recentlyAdded` — items recently added across the
    /// configured library sections. Thin passthrough; used by MEDIA-05's
    /// `media_recently_added` tool.
    pub async fn recently_added(&self) -> Result<Value, ToolError> {
        let url = format!("{}/library/recentlyAdded", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Plex unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /status/sessions` — what is playing RIGHT NOW (MUSEL-LIVE).
    ///
    /// The only live read on this client. Uses [`PlexSessionsError`] rather
    /// than [`ToolError`] so "unreachable" and "token rejected" stay distinct
    /// all the way to the caller; an empty session list is a perfectly normal
    /// `Ok` — "nobody is watching" is an ANSWER, not a failure.
    pub async fn sessions(&self) -> Result<Value, PlexSessionsError> {
        let url = format!("{}/status/sessions", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| PlexSessionsError::Unreachable(transport_detail(&e)))?;

        map_live_response(resp).await
    }

    /// `GET /identity` — server version/machine id. Unauthenticated on a real
    /// Plex server (verified live), cheap, and useful as a GUI header.
    ///
    /// Same error type as [`Self::sessions`] so a caller can decide per-call
    /// whether to treat it as fatal; the `media_now_playing` tool treats it as
    /// strictly best-effort and never lets it change the session outcome.
    pub async fn identity(&self) -> Result<Value, PlexSessionsError> {
        let url = format!("{}/identity", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| PlexSessionsError::Unreachable(transport_detail(&e)))?;

        map_live_response(resp).await
    }
}

/// Describe a transport failure WITHOUT echoing the URL back.
///
/// `reqwest::Error`'s `Display` includes the request URL, and while the token
/// travels in a header (not the query string) the base URL is still internal
/// infrastructure that ends up in tool output and logs. Classify instead.
fn transport_detail(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out waiting for Plex".to_string()
    } else if e.is_connect() {
        "could not connect to Plex".to_string()
    } else if e.is_request() {
        "the request to Plex could not be sent".to_string()
    } else {
        "no response from Plex".to_string()
    }
}

/// Status mapping for the LIVE read. The 401/403 branch is the load-bearing
/// one: a real Plex server answers an unauthenticated `/status/sessions` with
/// an HTML 401 body (verified live), so this must classify on STATUS and must
/// not attempt to parse the body as JSON first.
async fn map_live_response(resp: reqwest::Response) -> Result<Value, PlexSessionsError> {
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(PlexSessionsError::TokenRejected(format!(
            "Plex rejected the configured token (HTTP {})",
            status.as_u16()
        )));
    }
    if status.is_server_error() {
        return Err(PlexSessionsError::Unreachable(format!(
            "Plex could not serve the request (HTTP {})",
            status.as_u16()
        )));
    }
    if !status.is_success() {
        return Err(PlexSessionsError::Malformed(format!(
            "unexpected response from Plex (HTTP {})",
            status.as_u16()
        )));
    }

    let text = resp
        .text()
        .await
        .map_err(|_| PlexSessionsError::Unreachable("the response from Plex was cut short".into()))?;
    if text.trim().is_empty() {
        return Err(PlexSessionsError::Malformed("Plex returned an empty body".into()));
    }
    serde_json::from_str(&text)
        .map_err(|e| PlexSessionsError::Malformed(format!("Plex did not return JSON: {e}")))
}

async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::NotFound("Plex resource not found".into()));
    }
    if status.is_client_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "Plex API error (HTTP {status}): {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    if status.is_server_error() {
        return Err(ToolError::Http(format!("Plex unavailable (HTTP {status})")));
    }

    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from Plex: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_client(base_url: &str) -> PlexClient {
        PlexClient::new(base_url, "testtoken", reqwest::Client::new())
    }

    #[test]
    #[serial]
    fn from_env_missing_token_is_not_configured() {
        let url = std::env::var("PLEX_URL").ok();
        let token = std::env::var("PLEX_TOKEN").ok();
        std::env::set_var("PLEX_URL", "http://plex.test:32400");
        std::env::remove_var("PLEX_TOKEN");

        let result = PlexClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("PLEX_URL", u); } else { std::env::remove_var("PLEX_URL"); }
        if let Some(t) = token { std::env::set_var("PLEX_TOKEN", t); }
    }

    #[test]
    #[serial]
    fn from_env_builds_when_both_set() {
        let url = std::env::var("PLEX_URL").ok();
        let token = std::env::var("PLEX_TOKEN").ok();
        std::env::set_var("PLEX_URL", "http://plex.test:32400/");
        std::env::set_var("PLEX_TOKEN", "tok");

        let client = PlexClient::from_env().expect("should construct");
        assert_eq!(client.base_url, "http://plex.test:32400");

        if let Some(u) = url { std::env::set_var("PLEX_URL", u); } else { std::env::remove_var("PLEX_URL"); }
        if let Some(t) = token { std::env::set_var("PLEX_TOKEN", t); } else { std::env::remove_var("PLEX_TOKEN"); }
    }

    #[tokio::test]
    async fn library_sections_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/library/sections");
            then.status(200).json_body(json!({ "MediaContainer": { "size": 2 } }));
        });

        let client = test_client(&server.base_url());
        let result = client.library_sections().await.unwrap();
        mock.assert();
        assert_eq!(result["MediaContainer"]["size"], 2);
    }

    #[tokio::test]
    async fn history_maps_server_error_to_unavailable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(503);
        });

        let client = test_client(&server.base_url());
        let result = client.history().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    async fn on_deck_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/library/onDeck");
            then.status(200).json_body(json!({
                "MediaContainer": { "Metadata": [{ "title": "Foundation", "viewOffset": 120000 }] }
            }));
        });

        let client = test_client(&server.base_url());
        let result = client.on_deck().await.unwrap();
        mock.assert();
        assert_eq!(result["MediaContainer"]["Metadata"][0]["title"], "Foundation");
    }

    #[tokio::test]
    async fn recently_added_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/library/recentlyAdded");
            then.status(200).json_body(json!({
                "MediaContainer": { "Metadata": [{ "title": "New Arrival" }] }
            }));
        });

        let client = test_client(&server.base_url());
        let result = client.recently_added().await.unwrap();
        mock.assert();
        assert_eq!(result["MediaContainer"]["Metadata"][0]["title"], "New Arrival");
    }

    #[tokio::test]
    async fn on_deck_maps_server_error_to_unavailable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/onDeck");
            then.status(503);
        });

        let client = test_client(&server.base_url());
        let result = client.on_deck().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    // ── MUSEL-LIVE: the live read's three distinguishable outcomes ─────────

    #[tokio::test]
    async fn sessions_zero_is_ok_not_an_error() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/status/sessions");
            then.status(200).json_body(json!({ "MediaContainer": { "size": 0 } }));
        });

        let client = test_client(&server.base_url());
        let raw = client.sessions().await.expect("zero sessions is an answer, not a failure");
        mock.assert();
        assert_eq!(raw["MediaContainer"]["size"], 0);
    }

    #[tokio::test]
    async fn sessions_401_is_token_rejected_not_unreachable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/status/sessions");
            // A real Plex server answers an unauthenticated request with an
            // HTML body, NOT JSON (verified against the live server) — so the
            // classification must come from the status, before any parse.
            then.status(401)
                .header("content-type", "text/html")
                .body("<html><head><title>Unauthorized</title></head><body></body></html>");
        });

        let client = test_client(&server.base_url());
        let err = client.sessions().await.unwrap_err();
        assert_eq!(err.kind(), "unauthorized");
        assert!(matches!(err, PlexSessionsError::TokenRejected(_)));
    }

    #[tokio::test]
    async fn sessions_5xx_is_unreachable_not_token_rejected() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/status/sessions");
            then.status(503);
        });

        let client = test_client(&server.base_url());
        let err = client.sessions().await.unwrap_err();
        assert_eq!(err.kind(), "unreachable");
    }

    #[tokio::test]
    async fn sessions_connection_refused_is_unreachable() {
        // Port 1 on loopback: nothing listens, so this is a genuine transport
        // failure rather than a mocked status code.
        let client = test_client("http://127.0.0.1:1");
        let err = client.sessions().await.unwrap_err();
        assert_eq!(err.kind(), "unreachable");
        // The detail must never leak the base URL back to the caller.
        assert!(!err.detail().contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn sessions_non_json_200_is_malformed_not_zero_sessions() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/status/sessions");
            then.status(200).header("content-type", "text/html").body("<html></html>");
        });

        let client = test_client(&server.base_url());
        let err = client.sessions().await.unwrap_err();
        assert_eq!(err.kind(), "malformed");
    }

    #[tokio::test]
    async fn identity_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/identity");
            then.status(200).json_body(json!({
                "MediaContainer": { "machineIdentifier": "MACHINE_ID_PLACEHOLDER", "version": "1.42.0.0" }
            }));
        });

        let client = test_client(&server.base_url());
        let raw = client.identity().await.unwrap();
        mock.assert();
        assert_eq!(raw["MediaContainer"]["version"], "1.42.0.0");
    }

    #[test]
    fn sessions_error_kinds_are_all_distinct() {
        let kinds = [
            PlexSessionsError::Unreachable("a".into()).kind(),
            PlexSessionsError::TokenRejected("b".into()).kind(),
            PlexSessionsError::Malformed("c".into()).kind(),
        ];
        let unique: std::collections::HashSet<_> = kinds.iter().collect();
        assert_eq!(unique.len(), 3, "the three failure kinds must never collapse");
    }

    #[tokio::test]
    async fn unauthorized_maps_to_http_api_error_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/sections");
            then.status(401);
        });

        let client = test_client(&server.base_url());
        let result = client.library_sections().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }
}
