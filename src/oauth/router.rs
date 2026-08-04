//! RMCP-02 — the HTTP surface for OAuth discovery.
//!
//! Four routes, two documents, no state beyond an [`Arc<Discovery>`] built at
//! startup. It is deliberately small, and deliberately separate from
//! `crate::mcp_server`'s router: these paths are the ONLY part of the OAuth
//! door that must answer while everything else is broken.
//!
//! ## Why both the bare and the path-suffixed well-known
//! RFC 9728 defines the protected-resource metadata URL by inserting the
//! well-known segment after the resource's authority and keeping its path as a
//! suffix — `https://host/.well-known/oauth-protected-resource/mcp` for a
//! connector at `https://host/mcp`. Clients probe that form first and fall back
//! to the bare `/.well-known/oauth-protected-resource`. Serving only one of the
//! two produces the worst possible outcome: it works against whichever client
//! the developer tested with and fails against the next one, with a generic
//! "couldn't reach the MCP server" as the only symptom. Both are mounted, and
//! both return the SAME document, because a resource server has exactly one
//! identity no matter which URL was used to ask about it.
//!
//! The same reasoning mounts the suffixed form of the RFC 8414
//! authorization-server well-known: an issuer configured WITH a path (a
//! deployment behind a shared host, say) is only discoverable at the suffixed
//! URL, and mounting it costs one route.
//!
//! ## Why the suffix is not validated
//! A request to `/.well-known/oauth-protected-resource/anything-at-all` is
//! answered with this server's one document rather than a `404`. The suffix
//! carries no authority — it is a hint about which resource the client is
//! asking after, and this process serves exactly one. Matching on it would turn
//! a client that normalizes a path slightly differently (a stripped trailing
//! segment, an encoded character) into a hard discovery failure, in exchange
//! for no security property: the document contains only public information, and
//! the `resource` field inside it is what actually binds the answer to an
//! identity.
//!
//! ## `HEAD` and caching
//! axum's `get` handler also answers `HEAD`, which matters because probes and
//! uptime checks use it and a `405` there reads as a broken endpoint. Both
//! documents are served with a `Cache-Control` that permits shared caching:
//! they change only when the process is reconfigured and restarted, and a
//! cached discovery response is one fewer thing between the client and its
//! 10-second budget.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::oauth::metadata::{
    Discovery, AUTHORIZATION_SERVER_WELL_KNOWN, PROTECTED_RESOURCE_WELL_KNOWN,
};

/// How long a client or intermediary may cache a discovery document.
///
/// One hour: long enough that discovery is effectively free after the first
/// connection, short enough that an operator who fixes a misconfigured
/// canonical URL and restarts does not spend a day explaining why the old value
/// is still being used.
const CACHE_CONTROL: &str = "public, max-age=3600";

/// Build the unauthenticated OAuth discovery router.
///
/// Every route here is deliberately unauthenticated. The documents contain only
/// values that are already public by construction — the connector URL the
/// operator pastes into a third-party web form, and the endpoint paths that URL
/// implies — and requiring credentials to discover how to obtain credentials is
/// a loop with no entry point.
pub fn oauth_router(discovery: Arc<Discovery>) -> Router {
    // `format!` rather than a literal so the suffixed route and the bare route
    // can never drift apart. axum 0.7 spells a wildcard capture `*name`; the
    // `{name}` brace form is an axum 0.8 spelling that does NOT match on this
    // version — it would be treated as a literal path segment, taking the
    // path-suffixed probe (the one clients try FIRST) off the air without any
    // build or startup error. Asserted by `both_well_known_forms_...` below.
    let protected_resource_suffixed = format!("{PROTECTED_RESOURCE_WELL_KNOWN}/*suffix");
    let authorization_server_suffixed = format!("{AUTHORIZATION_SERVER_WELL_KNOWN}/*suffix");

    Router::new()
        .route(PROTECTED_RESOURCE_WELL_KNOWN, get(serve_protected_resource))
        .route(&protected_resource_suffixed, get(serve_protected_resource))
        .route(
            AUTHORIZATION_SERVER_WELL_KNOWN,
            get(serve_authorization_server),
        )
        .route(&authorization_server_suffixed, get(serve_authorization_server))
        .with_state(discovery)
}

/// RFC 9728 protected-resource metadata. Served from the body rendered at
/// startup — no allocation of the document, no database, no fallible work of
/// any kind on this path.
async fn serve_protected_resource(State(discovery): State<Arc<Discovery>>) -> Response {
    document(discovery.protected_resource_json())
}

/// RFC 8414 authorization-server metadata (see [`serve_protected_resource`]).
async fn serve_authorization_server(State(discovery): State<Arc<Discovery>>) -> Response {
    document(discovery.authorization_server_json())
}

fn document(body: &str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, CACHE_CONTROL),
        ],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::metadata::CanonicalUri;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    fn test_discovery() -> Arc<Discovery> {
        Arc::new(
            Discovery::new(
                CanonicalUri::parse("TEST", "https://connector.test/mcp").expect("valid"),
                CanonicalUri::parse("TEST", "https://connector.test").expect("valid"),
                false,
                vec!["mcp".to_string(), "offline_access".to_string()],
                "mcp".to_string(),
                false,
            )
            .expect("valid"),
        )
    }

    async fn fetch(method: &str, path: &str) -> (StatusCode, String, Option<String>) {
        let router = oauth_router(test_discovery());
        let response = router
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let cache = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned(), cache)
    }

    /// Both probe forms must work. A server that serves only one passes against
    /// whichever client the developer happened to test with.
    #[tokio::test]
    async fn both_well_known_forms_serve_the_same_document() {
        let (bare_status, bare_body, _) = fetch("GET", "/.well-known/oauth-protected-resource").await;
        let (suffixed_status, suffixed_body, _) =
            fetch("GET", "/.well-known/oauth-protected-resource/mcp").await;
        assert_eq!(bare_status, StatusCode::OK);
        assert_eq!(suffixed_status, StatusCode::OK);
        assert_eq!(bare_body, suffixed_body);

        let doc: serde_json::Value = serde_json::from_str(&bare_body).expect("valid JSON");
        assert_eq!(doc["resource"], serde_json::json!("https://connector.test/mcp"));

        let (as_bare, as_bare_body, _) =
            fetch("GET", "/.well-known/oauth-authorization-server").await;
        let (as_suffixed, as_suffixed_body, _) =
            fetch("GET", "/.well-known/oauth-authorization-server/mcp").await;
        assert_eq!(as_bare, StatusCode::OK);
        assert_eq!(as_suffixed, StatusCode::OK);
        assert_eq!(as_bare_body, as_suffixed_body);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&as_bare_body).expect("valid JSON")
                ["code_challenge_methods_supported"],
            serde_json::json!(["S256"])
        );
    }

    /// The URL advertised in the `401` challenge must actually resolve to the
    /// document. This is the link in the discovery chain that fails silently:
    /// a challenge pointing at an unmounted path produces the same generic
    /// client error as no challenge at all.
    #[tokio::test]
    async fn the_advertised_metadata_url_resolves() {
        let discovery = test_discovery();
        let advertised = discovery.protected_resource_url();
        let path = advertised
            .strip_prefix(discovery.resource().origin())
            .expect("the advertised URL is on this origin");
        let (status, body, _) = fetch("GET", path).await;
        assert_eq!(status, StatusCode::OK, "advertised path {path} must be mounted");
        assert_eq!(body, discovery.protected_resource_json());
    }

    /// A suffix this server does not recognise still gets the document — see
    /// the module doc for why matching on it would buy nothing and cost
    /// discovery failures.
    #[tokio::test]
    async fn an_unrecognised_suffix_still_serves_the_document() {
        let (status, body, _) =
            fetch("GET", "/.well-known/oauth-protected-resource/some/other/path").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("https://connector.test/mcp"));
    }

    /// Probes and uptime checks use `HEAD`; a `405` there reads as a broken
    /// endpoint.
    #[tokio::test]
    async fn head_is_answered_not_rejected() {
        for path in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server",
        ] {
            let (status, body, _) = fetch("HEAD", path).await;
            assert_eq!(status, StatusCode::OK, "HEAD {path}");
            assert!(body.is_empty(), "HEAD must not carry a body");
        }
    }

    /// Discovery is the first thing a client does and has a ~10s budget, so the
    /// answer must be cacheable.
    #[tokio::test]
    async fn documents_are_cacheable_json() {
        let (_, _, cache) = fetch("GET", "/.well-known/oauth-protected-resource").await;
        assert_eq!(cache.as_deref(), Some(CACHE_CONTROL));
    }
}
