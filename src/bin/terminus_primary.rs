//! `terminus-primary`: the aggregated-core-registry gateway binary (TGW-01 —
//! Terminus Primary Gateway sprint, S108).
//!
//! Per the operator-authorized Gateway architecture and the S108 spec's
//! orchestrator-resolved design decisions:
//! - **(1) ALONGSIDE.** This binary runs beside `terminus_personal` (the
//!   existing personal-registry deployment, serving `register_personal`'s
//!   fleet-app tool subset) and beside Chord's own `:8099`-style proxy port
//!   — it does NOT narrow or replace either. Narrowing Chord's
//!   client-facing surface is an explicitly deferred, separately-approved
//!   follow-up (TGW-05), not part of this item.
//! - **(2) Core registry only, here.** This binary registers ONLY
//!   `registry::register_all` (the core tool set — git-public, plane,
//!   gitea, github, etc.) into its `ToolRegistry`. It deliberately does
//!   **NOT** call `registry::register_personal` locally: personal-registry
//!   tools (the `terminus_personal` subset) are reached via federation,
//!   built in TGW-02, not by aggregating both registration functions into
//!   one `ToolRegistry` here. This also sidesteps a REAL, pre-existing
//!   collision — `register_all` and `register_personal` both register the
//!   `plane`/`gitea`/`github`/`sundry` tool modules under the same names
//!   (see `crate::registry::core_personal_name_collisions` and its test),
//!   so a single combined registry would immediately drop entries via each
//!   module's own silent `tracing::warn!`-and-drop duplicate handling. Not
//!   building that combined registry in the first place is the correct fix
//!   for TGW-01's scope; TGW-02 handles personal-tool reachability without
//!   ever registering `register_personal` into this process's registry.
//! - **(3) Independent auto-generated CA.** No code branch is needed for
//!   this: `crate::pki::ca()`'s existing load-or-generate precedence reads
//!   `TERMINUS_CA_CERT`/`TERMINUS_CA_KEY` from THIS process's own
//!   environment (or its own local store file, `TERMINUS_CA_STORE_PATH`),
//!   so deploying `terminus-primary` with its own independently provisioned
//!   CA material (never `terminus_personal`'s own) already yields an
//!   independent CA purely from separate provisioning at deploy time
//!   (TGW-05) — see `crate::pki` module docs for the precedence.
//!
//! ## TGW-02 update — personal-tool federation
//! This binary now also proxies personal-registry tool calls (any
//! `tools/call` whose name isn't in the local `register_all` registry) to
//! Chord's existing `/v1/personal/tools/call` relay, and includes the
//! personal-tool set in its `tools/list` — see `terminus_rs::federation`'s
//! module doc for the full contract (auth via a short-lived service JWT
//! signed with `TERMINUS_PRIMARY_CHORD_JWT_SECRET`, caller identity
//! forwarded from the mTLS cert, transport vs. tool-level error
//! classification). Core-tool dispatch is completely unchanged by this.
//!
//! ## TGW-03 update — inference proxy to Chord
//! This binary now also forwards `/v1/chat/completions`, `/v1/infer`,
//! `/v1/agent/execute`, and `/v1/coding/select` to the co-located Chord
//! process (the actual inference engine) over loopback, relaying Chord's
//! response back to the mTLS caller — including SSE streaming, unbuffered —
//! see `terminus_rs::inference_proxy`'s module doc for the full contract
//! (auth via the SAME short-lived service JWT TGW-02's federation client
//! mints, caller identity forwarded from the mTLS cert, Chord's own error
//! statuses relayed verbatim). Core-tool dispatch and personal-tool
//! federation are completely unchanged by this.
//!
//! ## What this item still does NOT add
//! Per the TGW-01 spec item's explicit scope boundary (now narrowed by
//! TGW-02/TGW-03 landing): no per-user auth/audit/rate-limit pipeline
//! (TGW-04) wraps these routes yet — they are reachable by any caller who
//! reaches the mTLS front door at all, same posture as the tool-call routes
//! before TGW-04. Reviewers should not expect TGW-04 behavior yet.
//!
//! ## Runtime configuration (env-sourced; NO literals)
//! - `TERMINUS_PRIMARY_PORT` — plain HTTP+JWT listener bind port. Defaults
//!   to `8310` — distinct from `terminus_personal`'s `TERMINUS_PERSONAL_PORT`
//!   default (`8300`) so both binaries can run side by side on the same
//!   host (design decision #1) with no collision.
//! - `TERMINUS_PRIMARY_BIND` — plain listener bind address. Defaults to
//!   `127.0.0.1`, same defense-in-depth posture as `terminus_personal`'s own
//!   default (`/mcp` is unauthenticated unless `TERMINUS_PRIMARY_TOKEN` is
//!   set, so this process binds loopback-only by default and relies on a
//!   reverse proxy / the mTLS listener for wider reachability).
//! - `TERMINUS_PRIMARY_TOKEN` — optional. If set, the plain `/mcp` listener
//!   requires `Authorization: Bearer <value>`.
//! - `TERMINUS_PRIMARY_MTLS_BIND` / `TERMINUS_PRIMARY_MTLS_PORT` /
//!   `TERMINUS_PRIMARY_MTLS_SERVER_IDENTITY` — the mTLS listener's own
//!   config (`crate::config::mtls_primary_bind_addr`/`mtls_primary_port`/
//!   `mtls_primary_server_identity`), a SEPARATE var family from
//!   `terminus_personal`'s `TERMINUS_MTLS_*`. See `crate::config`'s "TGW-01"
//!   section for the defaults and why they're distinct.
//! - CA/PKI material (`TERMINUS_CA_CERT`/`TERMINUS_CA_KEY`, or the local
//!   store at `TERMINUS_CA_STORE_PATH`) and the enrollment secrets
//!   (`TERMINUS_ENROLLMENT_SHARED_SECRET`, `TERMINUS_JWT_SIGNING_KEY`) are
//!   read the same way every other Terminus binary reads them — see
//!   `crate::pki` and `crate::pki::enroll` module docs. This binary does no
//!   startup secret-store-fetch bootstrap of its own (unlike // pii-test-fixture
//!   `terminus_personal`'s `fetch_downstream_secrets_from_infisical`) — // pii-test-fixture
//!   deployment (TGW-05) provisions its host environment directly; a
//!   startup secret-store fetch for this binary is out of this item's scope
//!   and can be added later without touching the shared `pki::server` setup
//!   this item builds.
//! - `TERMINUS_PRIMARY_CHORD_URL` — base URL of Chord's relay
//!   (`crate::config::chord_personal_federation_url`); defaults to Chord's
//!   loopback proxy port for a co-located deploy.
//!   `TERMINUS_PRIMARY_CHORD_FEDERATION_TIMEOUT_MS`
//!   bounds each federated call (default 30000ms).
//!   `TERMINUS_PRIMARY_CHORD_JWT_SECRET` — the shared HS256 secret this
//!   binary signs its outbound service JWT with; MUST match Chord's own
//!   `CHORD_JWT_SECRET` (provisioned identically on both hosts at deploy
//!   time — see `terminus_rs::federation`'s module doc). TGW-03's inference
//!   proxy reuses this same secret and `TERMINUS_PRIMARY_CHORD_URL` (Chord
//!   mounts both the personal-tool relay and the inference routes on one
//!   router) — `TERMINUS_PRIMARY_CHORD_INFERENCE_CONNECT_TIMEOUT_MS` bounds
//!   only the inference hop's initial connect (default 5000ms), deliberately
//!   NOT a total-response timeout, so a long/streamed generation is never
//!   cut off — see `crate::config`'s "TGW-03" section and
//!   `terminus_rs::inference_proxy`'s module doc.

use terminus_rs::pki::server::{build_gateway_router, spawn_mtls_listener, GatewayServerConfig};
use terminus_rs::registry::{register_all, ToolRegistry};

#[tokio::main]
async fn main() {
    terminus_rs::intake::init_tracing();

    let port: u16 = std::env::var("TERMINUS_PRIMARY_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8310);

    let bind_addr = std::env::var("TERMINUS_PRIMARY_BIND")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let auth_token = std::env::var("TERMINUS_PRIMARY_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());

    // TGW-01 design decision #2: core tools ONLY. Deliberately no
    // `register_personal` call here — see the module doc above.
    let mut registry = ToolRegistry::new();
    register_all(&mut registry);

    tracing::info!(
        "terminus_primary: {} tools registered, binding {bind_addr}:{port} (auth: {})",
        registry.len(),
        if auth_token.is_some() { "token" } else { "none" }
    );

    // TGW-02: any tool call whose name is NOT in the local core registry
    // (the register_personal-exclusive `git_private` tools, and any other
    // genuinely non-core tool name) is reached by federating to Chord's
    // existing `/v1/personal/tools/*` relay (per the S108 spec's RESOLVED
    // design decision (2)) rather than registering `register_personal`
    // locally (which would collide with `register_all` on the modules the
    // two share -- see the "(2) Core registry only, here" note in this
    // binary's module doc above, and
    // `terminus_rs::registry::core_personal_name_collisions`). See
    // `terminus_rs::federation`'s module doc for the full auth/
    // error-classification contract.
    let personal_federation = Some(terminus_rs::federation::PersonalFederationClient::from_env());

    // TGW-03: forward the inference/agent routes to the co-located Chord
    // process -- see `terminus_rs::inference_proxy`'s module doc for the
    // full contract (thin proxy, streaming preserved, same service-JWT auth
    // TGW-02's federation client uses).
    let inference_proxy = Some(terminus_rs::inference_proxy::InferenceProxyClient::from_env());

    let gateway_config = GatewayServerConfig {
        server_name: "terminus-primary".to_string(),
        server_version: terminus_rs::VERSION.to_string(),
        auth_token,
        mtls_bind: terminus_rs::config::mtls_primary_bind_addr(),
        mtls_port: terminus_rs::config::mtls_primary_port(),
        mtls_server_identity: terminus_rs::config::mtls_primary_server_identity(),
        personal_federation,
        inference_proxy,
    };

    // Same shared setup `terminus_personal` uses (TGW-01 extraction, see
    // `terminus_rs::pki::server` module docs): the `/mcp`+`/enroll` router,
    // then the background mTLS listener on this binary's own
    // `TERMINUS_PRIMARY_MTLS_*`-derived config.
    let router = build_gateway_router(registry, &gateway_config);
    spawn_mtls_listener(router.clone(), &gateway_config);

    let listener = tokio::net::TcpListener::bind(format!("{bind_addr}:{port}"))
        .await
        .unwrap_or_else(|e| panic!("terminus_primary: failed to bind {bind_addr}:{port}: {e}"));

    axum::serve(listener, router)
        .await
        .expect("terminus_primary: server error");
}

#[cfg(test)]
mod tests {
    use httpmock::MockServer;
    use serde_json::json;
    use serial_test::serial;
    use terminus_rs::federation::PersonalFederationClient;
    use terminus_rs::inference_proxy::InferenceProxyClient;
    use terminus_rs::pki::server::{build_gateway_router, GatewayServerConfig};
    use terminus_rs::registry::{register_all, ToolRegistry};

    /// `terminus_primary`'s registry-building step, exercised directly
    /// (mirrors the exact call `main()` makes) -- confirms core tools land
    /// and, per design decision #2, that this binary's registry is built
    /// from `register_all` alone (no `register_personal` mixed in, so no
    /// plane/gitea/github/sundry collision -- see
    /// `terminus_rs::registry::core_personal_name_collisions`).
    #[test]
    fn primary_registry_build_matches_main_and_has_core_tools() {
        let mut registry = ToolRegistry::new();
        register_all(&mut registry);

        assert!(registry.len() > 0, "register_all should populate the registry");
        // Spot-check a few representative core tools from different
        // modules, proving this is genuinely the core/`register_all` set.
        assert!(registry.contains("plane_list_projects"));
        assert!(registry.contains("gitea_list_identities"));
        assert!(registry.contains("github_list_repos"));
    }

    // ── TGW-02: personal-tool federation, exercised through the actual
    // router `terminus_primary`'s `main()` builds ─────────────────────────

    fn set_jwt_secret() {
        std::env::set_var("TERMINUS_PRIMARY_CHORD_JWT_SECRET", "test-chord-shared-secret");
    }
    fn clear_jwt_secret() {
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_JWT_SECRET");
    }

    fn primary_router_with_federation(
        chord_base_url: String,
    ) -> axum::Router {
        let mut registry = ToolRegistry::new();
        register_all(&mut registry);
        let config = GatewayServerConfig {
            server_name: "terminus-primary-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            mtls_bind: "127.0.0.1".to_string(),
            mtls_port: 0,
            mtls_server_identity: "terminus-primary-test".to_string(),
            personal_federation: Some(PersonalFederationClient::with_base_url(chord_base_url)),
            inference_proxy: None,
        };
        build_gateway_router(registry, &config)
    }

    /// TGW-03: same shape as `primary_router_with_federation`, but wires an
    /// `InferenceProxyClient` pointed at `chord_base_url` instead (personal
    /// federation left `None` — these tests exercise the inference-proxy
    /// routes specifically, not tool dispatch).
    fn primary_router_with_inference_proxy(chord_base_url: String) -> axum::Router {
        let mut registry = ToolRegistry::new();
        register_all(&mut registry);
        let config = GatewayServerConfig {
            server_name: "terminus-primary-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            mtls_bind: "127.0.0.1".to_string(),
            mtls_port: 0,
            mtls_server_identity: "terminus-primary-test".to_string(),
            personal_federation: None,
            inference_proxy: Some(InferenceProxyClient::with_base_url(chord_base_url)),
        };
        build_gateway_router(registry, &config)
    }

    async fn post_mcp(router: axum::Router, body: serde_json::Value) -> serde_json::Value {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let raw = String::from_utf8(bytes.to_vec()).unwrap();
        let json_str = raw
            .lines()
            .find(|l| l.starts_with("data:"))
            .map(|l| l.trim_start_matches("data:").trim())
            .unwrap_or(&raw);
        serde_json::from_str(json_str).unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn core_tool_call_dispatches_locally_no_federation_hit() {
        set_jwt_secret();
        let server = MockServer::start();
        // No mock registered -- if the router federated a core tool call by
        // mistake, this test would fail on `mock.assert_hits(0)` below.
        let hit_tracker = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/personal/tools/call");
            then.status(200).json_body(json!({"result": "should not be called"}));
        });

        let router = primary_router_with_federation(server.base_url());
        let body = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "gitea_list_identities", "arguments": {}}
            }),
        )
        .await;

        hit_tracker.assert_hits(0);
        // gitea_list_identities is a real registered core tool -- it should
        // execute (successfully or with its own tool error), never surface
        // as "Unknown tool" or a federation error.
        let text = body["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(!text.starts_with("Unknown tool"));
        assert!(!text.starts_with("federation error"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn personal_tool_call_is_proxied_to_chord_relay() {
        set_jwt_secret();
        let server = MockServer::start();
        // `git_private` is register_personal-EXCLUSIVE (register_all serves
        // `git_public` instead -- see
        // `terminus_rs::registry::personal_only_tool_metadata`), so it is
        // NOT in terminus-primary's local core registry and must federate.
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/personal/tools/call")
                .json_body_partial(r#"{"name": "git_private"}"#);
            then.status(200)
                .json_body(json!({"result": "cloned repo", "source": "terminus_personal"}));
        });

        let router = primary_router_with_federation(server.base_url());
        let body = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "git_private", "arguments": {"action": "list"}}
            }),
        )
        .await;

        mock.assert();
        assert_eq!(body["result"]["content"][0]["text"], "cloned repo");
        assert_eq!(body["result"]["isError"], false);
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn tools_list_aggregates_core_and_personal_when_federation_configured() {
        set_jwt_secret();
        let server = MockServer::start();
        let router = primary_router_with_federation(server.base_url());
        let body = post_mcp(
            router,
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        )
        .await;

        let tools = body["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        // A core tool (register_all) is present...
        assert!(names.contains(&"gitea_list_identities"));
        // core also serves git_public locally...
        assert!(names.contains(&"git_public"));
        // ...and the personal-EXCLUSIVE git_private tool is aggregated in via
        // federation metadata (it is NOT in register_all) -- proving the
        // aggregated surface, not just the local core catalog.
        assert!(names.contains(&"git_private"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn tools_list_is_core_only_when_federation_not_configured() {
        // terminus_personal's own posture (personal_federation: None) --
        // confirms this binary's aggregation is opt-in via the config field,
        // not an unconditional always-on behavior.
        let mut registry = ToolRegistry::new();
        register_all(&mut registry);
        let config = GatewayServerConfig {
            server_name: "terminus-primary-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            mtls_bind: "127.0.0.1".to_string(),
            mtls_port: 0,
            mtls_server_identity: "terminus-primary-test".to_string(),
            personal_federation: None,
            inference_proxy: None,
        };
        let router = build_gateway_router(registry, &config);
        let body = post_mcp(
            router,
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
        )
        .await;

        let tools = body["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"gitea_list_identities"));
        // No federation configured => core-only surface: git_public (core)
        // is present, git_private (personal-exclusive) is not.
        assert!(names.contains(&"git_public"));
        assert!(!names.contains(&"git_private"));
    }

    #[tokio::test]
    #[serial]
    async fn unknown_tool_not_found_at_chord_relay_is_tool_error_not_hang() {
        set_jwt_secret();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/personal/tools/call");
            then.status(404).json_body(json!({"error": "tool not found: bogus_thing"}));
        });

        let router = primary_router_with_federation(server.base_url());
        let body = post_mcp(
            router,
            json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": "bogus_thing", "arguments": {}}
            }),
        )
        .await;

        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("bogus_thing"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn chord_relay_unreachable_surfaces_federation_error_no_hang() {
        set_jwt_secret();
        // Nothing listening on this port -- an unreachable Chord.
        // git_private is personal-exclusive -> not local -> federates -> hits
        // the unreachable chord. (A core tool like ledger_accounts would
        // dispatch locally and never exercise the federation path.)
        let router = primary_router_with_federation("http://127.0.0.1:1".to_string());
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            post_mcp(
                router,
                json!({
                    "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                    "params": {"name": "git_private", "arguments": {"action": "list"}}
                }),
            ),
        )
        .await
        .expect("federation call to an unreachable chord must fail fast, not hang");

        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("federation error"));
        clear_jwt_secret();
    }

    // ── TGW-03: inference proxy to Chord, exercised through the actual
    // router `terminus_primary`'s `main()` builds ────────────────────────

    async fn post_json(
        router: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (axum::http::StatusCode, serde_json::Value, axum::http::HeaderMap) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, value, headers)
    }

    #[tokio::test]
    #[serial]
    async fn chat_completions_round_trips_to_mocked_chord() {
        set_jwt_secret();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"model": "test-model"}"#);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"id": "chatcmpl-abc", "choices": [{"index": 0}]}));
        });

        let router = primary_router_with_inference_proxy(server.base_url());
        let (status, body, _) = post_json(
            router,
            "/v1/chat/completions",
            json!({"model": "test-model", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;

        mock.assert();
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body["id"], "chatcmpl-abc");
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn chat_completions_streaming_passes_sse_chunks_through_unbuffered() {
        set_jwt_secret();
        let server = MockServer::start();
        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: [DONE]\n\n";
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse_body);
        });

        let router = primary_router_with_inference_proxy(server.base_url());
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"model": "test-model", "stream": true}).to_string(),
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), sse_body);
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn chat_completions_forwards_mtls_identity_and_service_jwt() {
        set_jwt_secret();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat/completions")
                .matches(|req| {
                    let auth = req
                        .headers
                        .as_ref()
                        .and_then(|hs| hs.iter().find(|(k, _)| k.eq_ignore_ascii_case("authorization")))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    auth.starts_with("Bearer ")
                });
            then.status(200).json_body(json!({"ok": true}));
        });

        // The plain HTTP+JWT listener never populates the `ClientIdentity`
        // extension (that only happens on the mTLS listener per
        // `crate::pki::mtls::run_listener`) -- so this test confirms the
        // service JWT is attached even with no caller identity present, and
        // (via the mock's own assert) that the route reaches Chord at all.
        let router = primary_router_with_inference_proxy(server.base_url());
        let (status, _, _) = post_json(
            router,
            "/v1/chat/completions",
            json!({"model": "test-model"}),
        )
        .await;

        mock.assert();
        assert_eq!(status, axum::http::StatusCode::OK);
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn infer_agent_execute_and_coding_select_are_all_proxied() {
        set_jwt_secret();
        let server = MockServer::start();
        for path in ["/v1/infer", "/v1/agent/execute", "/v1/coding/select"] {
            server.mock(|when, then| {
                when.method(httpmock::Method::POST);
                then.status(200).json_body(json!({"ok": true}));
            });
            let router = primary_router_with_inference_proxy(server.base_url());
            let (status, body, _) = post_json(router, path, json!({"model": "test-model"})).await;
            assert_eq!(status, axum::http::StatusCode::OK, "path {path} should proxy through");
            assert_eq!(body["ok"], true, "path {path} should relay chord's response body");
        }
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn inference_proxy_chord_unreachable_is_clean_502_no_hang() {
        set_jwt_secret();
        let router = primary_router_with_inference_proxy("http://127.0.0.1:1".to_string());
        let (status, body, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            post_json(router, "/v1/chat/completions", json!({"model": "test-model"})),
        )
        .await
        .expect("an unreachable chord must fail fast, not hang");

        assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY);
        assert!(body["error"].as_str().unwrap().contains("unreachable"));
        clear_jwt_secret();
    }

    #[tokio::test]
    #[serial]
    async fn inference_proxy_not_configured_returns_clean_503_not_404() {
        // terminus_personal's posture (inference_proxy: None) -- the routes
        // exist but this process isn't configured to serve them.
        let mut registry = ToolRegistry::new();
        register_all(&mut registry);
        let config = GatewayServerConfig {
            server_name: "terminus-primary-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            mtls_bind: "127.0.0.1".to_string(),
            mtls_port: 0,
            mtls_server_identity: "terminus-primary-test".to_string(),
            personal_federation: None,
            inference_proxy: None,
        };
        let router = build_gateway_router(registry, &config);
        let (status, _, _) = post_json(router, "/v1/chat/completions", json!({})).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
}
