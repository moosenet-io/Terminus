//! CONST-02: the constellation aggregation API layer.
//!
//! Spec S97 (Plane `TERM CONST-02`, #319) decided the aggregation layer is a
//! **compiled-in module of the primary/gateway binary**, NOT a broker
//! worker (`docs/architecture/broker.md`): the broker exists to extract MCP
//! TOOL DOMAINS reached over a UDS/mTLS transport, whereas this layer is an
//! operator-facing HTTP API + static-asset host for `constellation-web` —
//! there is no tool domain here to extract, so it is added as plain routes
//! merged into the existing `axum::Router` from
//! `crate::mcp_server::build_router`, exactly like the existing `/mcp` and
//! inference-proxy routes are.
//!
//! ## What lives here
//! - [`constellation_router`] — the `Router` this module contributes:
//!   `/api/auth/*` (`crate::constellation::auth`), `/api/health`,
//!   `/api/terminus/config`, the three namespaced backend proxies
//!   (`crate::constellation::proxy`), a `/ws` scaffold, and — when
//!   `CONSTELLATION_WEB_DIST_DIR` is configured — a `ServeDir` static-asset
//!   fallback serving the built `constellation-web` SPA.
//! - [`mask`] — secret-masking applied to every `/api/*` response body
//!   before egress (the layer's load-bearing security property).
//! - [`proxy`] — the namespaced backend-proxy handlers.
//! - [`audit`] — the S6-sanitized mutating-request audit trail.
//! - [`auth`] — the CONST-02 auth SEAM (stub; CONST-03 replaces its
//!   internals without changing its shape — see that module's doc).
//!
//! ## Contract with `constellation-web`
//! The endpoint shapes here are pinned to (and tested against)
//! `constellation-web/src/lib/aggregationClient.ts`'s `httpAdapter`
//! contract — see that file's own doc comment for the endpoint list this
//! module must satisfy byte-for-byte.

pub mod audit;
pub mod auth;
pub mod mask;
pub mod proxy;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use crate::config;
use crate::mcp_server::McpServerState;

/// Build the `Router` this module contributes. The caller (`build_router`
/// in `crate::mcp_server`) is expected to `.merge()` this into the shared
/// router alongside `/mcp`, `/healthz`, etc.
///
// CONST-03: no auth-guard middleware layer is attached to `api` below yet --
// `crate::constellation::auth`'s seam is consulted by handlers for AUDIT
// attribution only (see `proxy::principal_from_headers`), it does not yet
// DENY an unauthenticated mutating request. Add a `axum::middleware::from_fn`
// (or an extractor-based guard) here, over the `/api/*` routes only (never
// `/api/auth/login` itself, which must stay reachable pre-auth), once real
// verification exists.
pub fn constellation_router(state: Arc<McpServerState>) -> Router {
    let api = Router::new()
        .route("/api/auth/me", get(auth::auth_me))
        .route("/api/auth/login", post(auth::auth_login))
        .route("/api/auth/logout", post(auth::auth_logout))
        .route("/api/health", get(handle_health))
        .route("/api/terminus/config", get(handle_terminus_config))
        .route("/api/harmony/*path", any(proxy::proxy_harmony))
        .route("/api/chord/*path", any(proxy::proxy_chord))
        .route("/api/lumina/*path", any(proxy::proxy_lumina))
        // CONST-04/CONST-*: `/ws` is scaffolded only -- it accepts the
        // request and returns a clean, typed "not yet implemented" instead
        // of a raw 404, so `constellation-web`'s `ws.connect()` fails
        // predictably rather than silently. The full same-origin,
        // session-cookie-authenticated WebSocket proxy (harmony-web's
        // engine/ralph-loop/log event stream) is a follow-up item -- axum's
        // `ws` extractor + a real upstream WS relay is out of this item's
        // scope (CONST-02 is the HTTP aggregation surface).
        .route("/ws", get(handle_ws_stub))
        .with_state(state);

    match config::constellation_web_dist_dir() {
        Some(dist_dir) => {
            let index = format!("{}/index.html", dist_dir.trim_end_matches('/'));
            let serve_dir = tower_http::services::ServeDir::new(&dist_dir)
                .not_found_service(tower_http::services::ServeFile::new(index));
            // `fallback_service`, not `nest_service("/", ..)`: this only
            // kicks in when nothing else in the merged router matched, so
            // it can never shadow `/api/*`/`/mcp`/`/healthz` regardless of
            // merge order.
            api.fallback_service(serve_dir)
        }
        // No web bundle configured (e.g. an API-only / dev-box deployment)
        // -- mount the API surface only, no static-asset host at all.
        None => api,
    }
}

async fn handle_ws_stub() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        [("content-type", "application/json")],
        json!({
            "error": "constellation /ws event stream is not yet implemented",
            "note": "CONST-02 ships the HTTP aggregation surface only; the WebSocket relay is a follow-up item"
        })
        .to_string(),
    )
        .into_response()
}

/// `GET /api/health` — one entry per known system
/// (`harmony`/`chord`/`lumina`/`terminus`), matching
/// `aggregationClient.ts`'s `HealthStatus[]` shape exactly. `terminus` is
/// this process itself, so it is always `available: true`; the other three
/// are cheap-probed against their configured base URL.
async fn handle_health(State(state): State<Arc<McpServerState>>) -> Response {
    let entries = json!([
        probe_system("harmony", config::constellation_harmony_url()).await,
        probe_system("chord", config::constellation_chord_url()).await,
        probe_system("lumina", config::constellation_lumina_url()).await,
        terminus_self_health(&state),
    ]);
    let masked = mask::mask_response(entries);
    (StatusCode::OK, [("content-type", "application/json")], masked.to_string()).into_response()
}

fn terminus_self_health(_state: &McpServerState) -> Value {
    json!({"system": "terminus", "available": true, "detail": "self"})
}

/// Cheap reachability probe for one backend: a bare GET to its configured
/// base URL with a SHORT bounded timeout (this backs the health summary,
/// not a full proxied call, so it must stay fast even when polled
/// frequently by the UI). Any failure (unconfigured, unreachable, timeout,
/// non-2xx) reports `available: false` with a short reason -- never
/// propagates an error to the caller.
async fn probe_system(system: &'static str, base_url: Option<String>) -> Value {
    let Some(base) = base_url else {
        return json!({"system": system, "available": false, "detail": format!("{system} backend not configured")});
    };
    // A short, fixed probe timeout independent of the general proxy
    // timeout -- a health check must be snappy even when the configured
    // per-call timeout is generous.
    let probe_timeout = Duration::from_millis(config::constellation_backend_timeout_ms().min(3_000));
    match proxy::http_client().get(&base).timeout(probe_timeout).send().await {
        Ok(resp) if resp.status().is_success() || resp.status().is_client_error() => {
            // A reachable server that answers ANYTHING (even a 404 on the
            // bare base URL) counts as "available" -- this probe is about
            // reachability, not about the base URL itself being a valid
            // health endpoint.
            json!({"system": system, "available": true, "detail": "reachable"})
        }
        Ok(resp) => json!({
            "system": system,
            "available": false,
            "detail": format!("upstream status {}", resp.status().as_u16())
        }),
        Err(e) if e.is_timeout() => {
            json!({"system": system, "available": false, "detail": format!("{system} probe timed out")})
        }
        Err(e) => json!({"system": system, "available": false, "detail": format!("{system} unreachable: {e}")}),
    }
}

/// `GET /api/terminus/config` — `{modules, workerCount}` matching
/// `aggregationClient.ts`'s `TerminusConfigSummary` shape. `modules` is
/// derived from the compiled-in registry's tool catalog: every registered
/// tool name follows this crate's established `{module}_{action}` naming
/// convention (e.g. `gitea_list_repos`, `plane_create_work_item` -- see
/// `crate::registry::register_all`'s per-domain `register()` calls), so the
/// distinct name PREFIXES are exactly the set of active domain modules.
/// `workerCount` is `state.broker_routes`'s current snapshot length (the
/// number of tool NAMES currently routed to an out-of-process worker, per
/// `crate::broker::routes::RouteTable` -- zero until a domain is extracted,
/// matching that module's "empty table is behavior-preserving" contract).
async fn handle_terminus_config(State(state): State<Arc<McpServerState>>) -> Response {
    let registry = state.registry.load();
    let mut module_names: Vec<String> = registry
        .list()
        .into_iter()
        .filter_map(|t| t.name.split('_').next().map(str::to_string))
        .collect();
    module_names.sort();
    module_names.dedup();

    let modules: Vec<Value> = module_names
        .into_iter()
        .map(|name| json!({"name": name, "enabled": true, "version": Value::Null}))
        .collect();

    let worker_count = state.broker_routes.load().len();

    let body = json!({"modules": modules, "workerCount": worker_count});
    let masked = mask::mask_response(body);
    (StatusCode::OK, [("content-type", "application/json")], masked.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::http::Request;
    use serial_test::serial;
    use tower::ServiceExt;

    fn test_state() -> Arc<McpServerState> {
        let mut registry = crate::registry::ToolRegistry::new();
        struct DummyTool;
        #[async_trait::async_trait]
        impl crate::tool::RustTool for DummyTool {
            fn name(&self) -> &str {
                "gitea_list_repos"
            }
            fn description(&self) -> &str {
                "dummy"
            }
            fn parameters(&self) -> Value {
                json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _args: Value) -> Result<String, crate::error::ToolError> {
                Ok("ok".to_string())
            }
        }
        registry.register(Box::new(DummyTool)).unwrap();
        Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(registry),
            server_name: "constellation-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: crate::mesh::PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
        })
    }

    async fn get_json(router: Router, path: &str) -> (StatusCode, Value) {
        let req = Request::builder().method("GET").uri(path).body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    #[serial]
    async fn health_reports_all_four_systems() {
        std::env::remove_var("CONSTELLATION_HARMONY_URL");
        std::env::remove_var("CONSTELLATION_CHORD_URL");
        std::env::remove_var("CONSTELLATION_LUMINA_URL");
        let router = constellation_router(test_state());
        let (status, body) = get_json(router, "/api/health").await;
        assert_eq!(status, StatusCode::OK);
        let systems: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["system"].as_str().unwrap().to_string())
            .collect();
        assert!(systems.contains(&"harmony".to_string()));
        assert!(systems.contains(&"chord".to_string()));
        assert!(systems.contains(&"lumina".to_string()));
        assert!(systems.contains(&"terminus".to_string()));
        let terminus = body.as_array().unwrap().iter().find(|v| v["system"] == "terminus").unwrap();
        assert_eq!(terminus["available"], true);
    }

    #[tokio::test]
    async fn terminus_config_derives_modules_from_registered_tool_prefixes() {
        let router = constellation_router(test_state());
        let (status, body) = get_json(router, "/api/terminus/config").await;
        assert_eq!(status, StatusCode::OK);
        let modules = body["modules"].as_array().unwrap();
        assert!(modules.iter().any(|m| m["name"] == "gitea"));
        assert_eq!(body["workerCount"], 0);
    }

    #[tokio::test]
    async fn ws_stub_returns_not_implemented_not_a_bare_404() {
        let router = constellation_router(test_state());
        let req = Request::builder().method("GET").uri("/ws").body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn auth_me_route_is_wired_through_the_router() {
        let router = constellation_router(test_state());
        let (status, body) = get_json(router, "/api/auth/me").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["authenticated"], false);
    }

    #[test]
    #[serial]
    fn no_dist_dir_configured_means_api_only_router_still_builds() {
        std::env::remove_var("CONSTELLATION_WEB_DIST_DIR");
        // Must not panic building the router with no static-asset host
        // configured -- this is the documented "API-only deployment" path.
        let _router = constellation_router(test_state());
    }
}
