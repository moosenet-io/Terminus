//! RMCP-11 — mounting the OAuth door (TERM #631).
//!
//! ## Why this module exists at all
//!
//! RMCP-02, RMCP-03, RMCP-04 and RMCP-09 each merged a piece of an OAuth
//! server: discovery documents, the authorize/login/consent pages, the token
//! endpoint, and a public edge listener with a per-path source policy. Each was
//! reviewed, tested, and correct. **None of them was reachable in a running
//! binary**, because every one of them shipped a `Router` and no item owned
//! binding those routers to the process.
//!
//! That is a specific and unpleasant failure mode: every individual item can
//! pass its own acceptance criteria while the feature does not exist. Review
//! round 5 (`gpt56`) refused RMCP-11 on exactly that ground — the controls this
//! item promises cannot protect a door that nothing serves — and it was right,
//! so the mounting became this item's work rather than a deferral.
//!
//! ## What is mounted, and where
//!
//! One `Router`, merged into the process's main router alongside `/mcp` and the
//! discovery documents, so it is served by BOTH the private listeners and (when
//! configured) RMCP-09's public edge. The edge wraps a clone of the same router,
//! which is what makes its per-path source policy the thing that decides who may
//! reach these paths — rather than this module inventing a second opinion.
//!
//! | Path | Owner | Edge class |
//! |---|---|---|
//! | `/oauth/authorize` | RMCP-03 [`authorize::router`] | interactive |
//! | `/oauth/login`, `/oauth/consent` | RMCP-03 | interactive |
//! | `/oauth/token` | RMCP-04 [`token::build_token_router`] | anthropic |
//! | `/oauth/revoke` | RMCP-11, here | anthropic |
//!
//! The authorize router is `nest`ed under `/oauth` because its own routes are
//! written relative (`/authorize`), and its cookie is scoped to `/oauth` — so
//! mounting it anywhere else would silently break session continuity between
//! the login POST and the consent POST. The token router already spells its
//! path absolutely, so it is merged rather than nested. Getting this backwards
//! produces `/oauth/oauth/token`, which fails as a 404 at exactly the moment a
//! client is trying to exchange a code, so both forms are asserted by tests.
//!
//! ## Fail-closed construction
//!
//! [`OauthEndpoints::from_env`] returns `Ok(None)` when the door is not
//! configured — a deployment with no connector must behave exactly as it did
//! before this item, opening no database pool and binding no routes. A
//! configuration that is PRESENT but unusable is an error, not a silent
//! degradation: a half-built auth surface that answers `/oauth/authorize` and
//! then fails at `/oauth/token` is worse than one that never came up, because
//! the operator's evidence points at the client.
//!
//! Mounting is deliberately gated on the schema being present, for the same
//! reason: every endpoint here reads the database on its first request, and
//! "the migration has not been applied" should be one clear refusal at startup
//! rather than an opaque `relation does not exist` per request.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::error::ToolError;
use crate::oauth::audit::{AuditDetail, DenialReason, OauthAuditRecord, OauthEvent};
use crate::oauth::authorize::{AuthorizeConfig, AuthorizeState};
use crate::oauth::edge::ResolvedClientIp;
use crate::oauth::limits::{throttled_response, OauthEndpoint, OauthRateLimiter};
use crate::oauth::revoke::{RevocationRequest, RevocationService, SessionStore};
use crate::oauth::session::SessionKey;
use crate::oauth::store::OauthStore;
use crate::oauth::token::TokenEndpoint;

/// RFC 7009's revocation path. Absolute, matching RMCP-04's `TOKEN_PATH` and
/// RMCP-09's policy table, which already classifies it.
pub const REVOKE_PATH: &str = "/oauth/revoke";

/// Bound on the token request body.
///
/// Mirrors RMCP-04's own private `MAX_BODY_BYTES`. Duplicated rather than
/// exported because this module re-builds the token route in order to rate-limit
/// it (see [`handle_token`]), and a route that dropped the bound while adding a
/// limiter would trade one pre-auth DoS vector for another. Pinned against
/// RMCP-04's documented value by a test.
const MAX_TOKEN_BODY_BYTES: usize = 8 * 1024;

/// The largest revocation request body accepted.
///
/// A revocation carries a token, an optional hint and an optional client id —
/// a few hundred bytes. The bound exists because this endpoint is reachable
/// from the internet and parses a form: an unbounded body on an
/// unauthenticated path is a memory amplifier regardless of what the parser
/// then does with it.
const MAX_REVOKE_BODY_BYTES: usize = 4 * 1024;

/// Everything needed to serve the OAuth door, built once at startup.
pub struct OauthEndpoints {
    authorize: Arc<AuthorizeState>,
    token: Arc<TokenEndpoint>,
    revocation: RevocationService,
    limiter: Arc<OauthRateLimiter>,
}

impl OauthEndpoints {
    /// Build from the runtime environment, or `Ok(None)` when the door is not
    /// configured.
    ///
    /// "Not configured" is keyed on the DATABASE URL, because that is the one
    /// value with no usable default and no meaning outside this feature: an
    /// operator who has not set it has not asked for an OAuth door. The signing
    /// key, issuer and resource are then REQUIRED — if the door is being asked
    /// for, a missing signing key is a broken door, not an absent one.
    pub async fn from_env() -> Result<Option<Self>, ToolError> {
        // Absence is decided HERE, on the variable itself — never inferred from
        // an error variant.
        //
        // An earlier revision asked `OauthConfig::from_env` and read
        // `Err(NotConfigured)` as "absent". That put two meanings on one variant
        // inside one function: absence, and (three lines later, for the schema
        // check) present-but-unusable. Review round 9 (`gpt56`) called it a
        // fail-open, and the shape is exactly that even though the current call
        // site happens to abort — any caller that reasonably reads
        // `NotConfigured` as "not configured, carry on" silently loses the door.
        //
        // So: the ONLY absent case is the connection URL being unset or blank,
        // tested directly. Everything after this line is a configured door, and
        // every failure below is therefore fatal — which is RMCP-02's rule,
        // reused rather than re-decided: ABSENT means not configured; PRESENT
        // means the value must be usable.
        let configured = std::env::var(crate::oauth::DATABASE_URL_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .is_some();
        if !configured {
            return Ok(None);
        }
        let config = crate::oauth::OauthConfig::from_env()?;

        let store = OauthStore::connect(&config).await?;
        if !store.schema_ready().await {
            // Deliberately NOT `NotConfigured`: the door IS configured, and this
            // is the one failure most likely to be met on a first deploy (the
            // S132 migrations are applied by hand, as a deploy step). Reporting
            // it in the variant that elsewhere means "absent" is how it would
            // come to be swallowed. `schema_not_ready_is_not_reported_as_absence`
            // pins that.
            return Err(ToolError::Database(
                "the RMCP OAuth door is configured but its schema is not present — apply the \
                 S132 migrations with pg_ddl before starting, or unset RMCP_DATABASE_URL. \
                 Refusing to serve an auth surface whose every request would fail at the \
                 database"
                    .into(),
            ));
        }

        // One limiter for the whole door (TERM #633). The authorize state holds
        // the same `Arc`, so the login budget and every other endpoint's budget
        // come from one table and one set of invariants.
        let limiter = Arc::new(OauthRateLimiter::from_env()?);

        let authorize = Arc::new(AuthorizeState::with_limiter(
            store.clone(),
            AuthorizeConfig::from_env()?,
            SessionKey::from_env()?,
            limiter.clone(),
        ));
        let token = Arc::new(TokenEndpoint::from_env(store.clone())?);
        let session_store: Arc<dyn SessionStore> = Arc::new(store);
        let revocation = RevocationService::new(session_store);

        Ok(Some(Self { authorize, token, revocation, limiter }))
    }

    /// Build from already-constructed parts. The tests' door, and the seam a
    /// future binary with its own pool would use.
    pub fn from_parts(
        authorize: Arc<AuthorizeState>,
        token: Arc<TokenEndpoint>,
        revocation: RevocationService,
        limiter: Arc<OauthRateLimiter>,
    ) -> Self {
        Self { authorize, token, revocation, limiter }
    }

    /// The router to merge into the process's main router.
    pub fn router(&self) -> Router {
        let revoke = Router::new()
            .route(REVOKE_PATH, axum::routing::post(handle_revoke))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_REVOKE_BODY_BYTES))
            .with_state(RevokeState {
                revocation: self.revocation.clone(),
                limiter: self.limiter.clone(),
            });

        // The token route is rebuilt here rather than taken from
        // `token::build_token_router`, for one reason: it has to pass through
        // the door's shared limiter.
        //
        // Review round 6 (`gpt56`) found the endpoint mounted UNLIMITED — the
        // limiter this item converged everything else onto did not reach
        // `/oauth/token` at all. That is the endpoint an attacker hammers to
        // brute-force an authorization code or grind refresh tokens, and the one
        // where the subject-vs-address split matters most, so it was the worst
        // possible omission: exactly the defect this item had just found in
        // RMCP-03's limiter (a control that exists and does not reach the path
        // that needs it), reintroduced one layer up.
        //
        // The handler delegates to RMCP-04's `TokenEndpoint::handle` and reuses
        // its `IntoResponse` impls, so no request or error shape is duplicated
        // here — only the limit is added.
        let token = Router::new()
            .route(
                crate::oauth::token::TOKEN_PATH,
                axum::routing::post(handle_token),
            )
            .layer(axum::extract::DefaultBodyLimit::max(MAX_TOKEN_BODY_BYTES))
            .with_state(TokenState {
                endpoint: self.token.clone(),
                limiter: self.limiter.clone(),
            });

        Router::new()
            // Nested: RMCP-03's routes are relative, and its cookie is scoped to
            // `/oauth`.
            .nest("/oauth", crate::oauth::authorize::router(self.authorize.clone()))
            .merge(token)
            .merge(revoke)
            // The per-address charge, ahead of every handler on this router.
            .route_layer(axum::middleware::from_fn_with_state(
                self.limiter.clone(),
                charge_address_budget,
            ))
    }

    /// A log-safe description of what was mounted.
    pub fn describe(&self) -> String {
        format!(
            "RMCP OAuth endpoints mounted: /oauth/authorize, /oauth/login, /oauth/consent, \
             {}, {REVOKE_PATH}",
            crate::oauth::token::TOKEN_PATH
        )
    }
}

/// Which endpoint budget a mounted path draws on.
///
/// The single source of truth for that mapping, consumed by the rate-limit
/// layer and asserted against the mounted route list by
/// `every_mounted_route_is_covered_by_the_shared_limiter`.
///
/// An unmapped path falls back to [`OauthEndpoint::Login`] — the TIGHTEST
/// budget — rather than to no limit. That arm is unreachable while the contract
/// test passes, and it is the safe direction to be wrong in: a route added
/// without updating this map gets throttled hard, which is noticed, instead of
/// silently unlimited, which is not.
fn endpoint_for_path(path: &str) -> OauthEndpoint {
    match path {
        "/oauth/authorize" | "/oauth/consent" => OauthEndpoint::Authorize,
        "/oauth/login" => OauthEndpoint::Login,
        crate::oauth::token::TOKEN_PATH => OauthEndpoint::Token,
        REVOKE_PATH => OauthEndpoint::Revoke,
        _ => OauthEndpoint::Login,
    }
}

/// Charge the per-address budget for every mounted OAuth route, BEFORE the
/// handler runs.
///
/// ## Why a layer and not a line in each handler
///
/// Review round 9 (`gpt56`) found that `handle_revoke` returned `400` for a bad
/// content type before charging anything, and that `post_login` and
/// `post_consent` parsed and validated a body first. An attacker could therefore
/// send malformed requests indefinitely at zero budget cost — and a limiter that
/// only counts well-formed traffic does not bound the traffic worth bounding.
///
/// The fix is not "remember to charge earlier in five handlers", because that is
/// the same shape as the three hand-written 429s and the two audit-redaction
/// passes this item has already had to delete: a rule every author must
/// remember is a rule some author will not. A layer runs before the handler by
/// construction, so no handler CAN parse first. What remains in the handlers is
/// the subject dimension only, which genuinely cannot be charged before the body
/// is read.
///
/// Applied with `route_layer`, so it runs only for paths this router actually
/// serves. An unmatched path is a `404` that costs no budget — deliberately,
/// because scanning for routes that do not exist is RMCP-09's edge limiter's
/// job, and charging a real endpoint's budget for a request that never reached
/// it would let a scanner exhaust the login budget without ever touching login.
async fn charge_address_budget(
    State(limiter): State<Arc<OauthRateLimiter>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let endpoint = endpoint_for_path(req.uri().path());
    let source = crate::oauth::authorize::resolved_source_for(req.extensions());

    match limiter.check_address(endpoint, source).await {
        Err(outcome) => throttled_response(&outcome),
        Ok(cleared) => {
            // The witness travels to the handler in the request extensions.
            // A handler that charges a subject extracts it; one that does not
            // simply ignores it. Either way it cannot be forged — see
            // `AddressCleared` for why that is the whole point.
            let mut req = req;
            req.extensions_mut().insert(cleared);
            next.run(req).await
        }
    }
}

#[derive(Clone)]
struct TokenState {
    endpoint: Arc<TokenEndpoint>,
    limiter: Arc<OauthRateLimiter>,
}

/// `POST /oauth/token`, rate-limited, then delegated to RMCP-04 unchanged.
async fn handle_token(
    State(state): State<TokenState>,
    cleared: crate::oauth::limits::AddressCleared,
    extensions: axum::http::Extensions,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let source = crate::oauth::authorize::resolved_source_for(&extensions);

    // The address dimension was charged by `charge_address_budget` before this
    // handler ran — so a malformed or empty body has already cost budget, which
    // is what stops a code being testable at line rate through requests this
    // handler would have rejected.
    //
    // The SUBJECT dimension needs the body, so it is charged here, still before
    // the grant runs. The key is the CLAIMED client id — claimed, not
    // authenticated, and that is correct for a rate-limit key: waiting until the
    // client is authenticated would mean the unauthenticated attempts, the ones
    // worth throttling, were never counted. It is emphatically not an
    // authentication input: nothing downstream sees this value, and
    // `TokenEndpoint::handle` re-extracts and verifies the client itself.
    if let Some(client) = presented_client_id(&headers, &body) {
        let outcome = state.limiter.check_subject(&cleared, &client).await;
        if outcome.is_limited() {
            OauthAuditRecord::new(OauthEvent::TokenDenied)
                .endpoint(OauthEndpoint::Token)
                .from_address(source)
                .reason(DenialReason::RateLimited)
                .emit();
            return throttled_response(&outcome);
        }
    }

    match state.endpoint.handle(&headers, &body).await {
        Ok(response) => response.into_response(),
        Err(error) => error.into_response(),
    }
}

/// The client id a token request CLAIMS, for use as a rate-limit key only.
///
/// Best-effort by design, and deliberately not RMCP-04's `extract_client_auth`:
/// that function decides authentication and rejects a request presenting two
/// credentials, which is the right rule for authenticating and the wrong one for
/// counting. A request this cannot attribute is still limited on its address —
/// the subject dimension narrows, it never gates.
fn presented_client_id(headers: &HeaderMap, body: &[u8]) -> Option<String> {
    if let Some((_, value)) = parse_form(body).into_iter().find(|(k, _)| k == "client_id") {
        if !value.is_empty() {
            return Some(value);
        }
    }
    // HTTP Basic: the username half is the client id.
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?
        .trim()
        .strip_prefix("Basic ")?;
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD.decode(raw.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (client_id, _) = decoded.split_once(':')?;
    // Percent-decoded per RFC 6749 §2.3.1, which specifies form-encoding the
    // two halves before base64.
    let client_id = percent_decode(client_id);
    (!client_id.is_empty()).then_some(client_id)
}

#[derive(Clone)]
struct RevokeState {
    revocation: RevocationService,
    limiter: Arc<OauthRateLimiter>,
}

/// `POST /oauth/revoke` (RFC 7009).
///
/// Thin by design: the semantics — 200 for an unknown token, 200-but-revoke-
/// nothing for a client presenting someone else's, 400 only for a request with
/// no token at all — live in
/// [`RevocationService::revoke_presented_token`], where they are tested without
/// a transport. This function is the form parsing, the rate limit, and the
/// status mapping, and nothing else.
async fn handle_revoke(
    State(state): State<RevokeState>,
    cleared: crate::oauth::limits::AddressCleared,
    extensions: axum::http::Extensions,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let source = crate::oauth::authorize::resolved_source_for(&extensions);

    // The address dimension is already charged (`charge_address_budget`), which
    // is what round 9 required: this handler used to return `400` for a bad
    // content type BEFORE charging anything, so an attacker could post junk here
    // for free. The refusal below now costs budget like any other request.
    //
    // Content type is still checked before the body is looked at, so a caller
    // that posts JSON gets a clear refusal rather than an empty form parse that
    // reads as "no token" and lands on the 400 path for the wrong reason.
    if !is_form_content_type(&headers) {
        // Audited before returning. An unaudited pre-auth refusal is a denial an
        // operator cannot see, and this one sits on the endpoint they reach for
        // during an incident — a burst of them means requests are being refused
        // at the door before they are even parsed, which is exactly the thing
        // worth noticing. The record names the endpoint and the reason and
        // carries nothing from the request: the content-type header is
        // caller-controlled text, and this door keeps no redaction pass to
        // trust with it.
        OauthAuditRecord::new(OauthEvent::Revoked)
            .endpoint(OauthEndpoint::Revoke)
            .from_address(source)
            .reason(DenialReason::MalformedRequest)
            .detail(AuditDetail::RefusedBeforeParsing)
            .emit();
        return (StatusCode::BAD_REQUEST, r#"{"error":"invalid_request"}"#).into_response();
    }

    let fields = parse_form(&body);
    let presented_client = fields
        .iter()
        .find(|(k, _)| k == "client_id")
        .map(|(_, v)| v.clone());

    // The SUBJECT dimension, charged before the token is looked up so this
    // endpoint cannot be used to probe harvested tokens at speed. The key is the
    // presented client id — never the token, which is a credential and would put
    // one in a bucket key.
    if let Some(client) = presented_client.as_deref() {
        let outcome = state.limiter.check_subject(&cleared, client).await;
        if outcome.is_limited() {
            return throttled_response(&outcome);
        }
    }

    let request = RevocationRequest {
        token: fields
            .iter()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.clone())
            .unwrap_or_default(),
        token_type_hint: fields
            .iter()
            .find(|(k, _)| k == "token_type_hint")
            .map(|(_, v)| v.clone()),
        client_id: presented_client,
        source: Some(source),
    };

    match state.revocation.revoke_presented_token(request).await {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, response.body).into_response()
        }
        // A store failure must not be reported as a successful revocation.
        // RFC 7009's 200-for-unknown-token rule is about a token this server
        // does not know; it is not about a server that could not look.
        //
        // Audited for the same reason as the refusal above, and with more
        // urgency: an operator who ran a revocation during an incident and got
        // a 500 needs the trail to agree that nothing was revoked.
        Err(_) => {
            OauthAuditRecord::new(OauthEvent::Revoked)
                .endpoint(OauthEndpoint::Revoke)
                .from_address(source)
                .reason(DenialReason::Revoked)
                .detail(AuditDetail::RevocationNotEffective { still_live: 0, matched: 0 })
                .emit();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"server_error"}"#,
            )
                .into_response()
        }
    }
}

fn is_form_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
        .unwrap_or(false)
}

/// Minimal `application/x-www-form-urlencoded` decode.
///
/// Returns pairs rather than a map: a duplicated key is left visible to the
/// caller, which takes the FIRST occurrence. A map would silently pick one, and
/// "which `token` wins when two are sent" is not a question to answer by
/// accident on a revocation endpoint.
fn parse_form(body: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(body);
    text.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mounted surface: (path, endpoint budget, does the subject dimension
    /// apply). One list, consumed by every contract test below, so "which routes
    /// exist" is answered in exactly one place.
    const MOUNTED_ROUTES: &[(&str, OauthEndpoint, bool)] = &[
        ("/oauth/authorize", OauthEndpoint::Authorize, false),
        ("/oauth/consent", OauthEndpoint::Authorize, false),
        ("/oauth/login", OauthEndpoint::Login, true),
        (crate::oauth::token::TOKEN_PATH, OauthEndpoint::Token, true),
        (REVOKE_PATH, OauthEndpoint::Revoke, true),
    ];


    /// The nesting rule, asserted because getting it backwards is a 404 at the
    /// exact moment a client exchanges a code — and a 404 on a route that
    /// "exists" is the kind of failure that gets blamed on the client.
    #[test]
    fn the_mounted_paths_are_the_ones_the_edge_policy_names() {
        // RMCP-09's policy table and RMCP-04's constant are the authorities on
        // these strings; this test pins that this module agrees with both.
        assert_eq!(crate::oauth::token::TOKEN_PATH, "/oauth/token");
        assert_eq!(REVOKE_PATH, "/oauth/revoke");
        // The authorize router is nested under `/oauth`, so its relative routes
        // become the interactive paths the policy classifies.
        for path in ["/oauth/authorize", "/oauth/login", "/oauth/consent"] {
            assert!(path.starts_with("/oauth/"), "{path} would fall outside the nest");
        }
    }

    /// The round-6 regression guard, and the answer to "is every mounted route
    /// limited?" in a form that cannot rot.
    ///
    /// `/oauth/token` was mounted UNLIMITED because the limiter was wired into
    /// two of the four handlers and nobody enumerated the rest. This table is
    /// the enumeration: every route this module mounts, and the budget that
    /// covers it. A route added to `router()` without a limiter is a route
    /// missing from this list, and the count assertion is what makes that fail
    /// here rather than in production.
    #[test]
    fn every_mounted_route_is_covered_by_the_shared_limiter() {
        let mounted = MOUNTED_ROUTES;

        assert_eq!(
            mounted.len(),
            5,
            "a route was added to or removed from `router()` without updating this table"
        );
        for &(path, endpoint, _) in mounted {
            assert!(path.starts_with("/oauth/"), "{path} falls outside the door");
            // Every budget named here exists and is armed.
            assert!(endpoint.default_budgets().per_address.burst > 0);
        }

        // The path->budget map the LAYER uses must agree with this table, or a
        // route would be charged against the wrong endpoint's budget — or, worse,
        // fall into the fail-closed default and be throttled at the login rate.
        for &(path, endpoint, _) in mounted {
            assert_eq!(
                endpoint_for_path(path),
                endpoint,
                "{path} is charged against the wrong budget by the rate-limit layer"
            );
        }

        // The three endpoint budgets the door actually draws on. `Register` has
        // no mounted route (RMCP-08 is not merged) and is therefore absent —
        // stated here so its absence reads as known rather than as another
        // unwired endpoint.
        let used: std::collections::BTreeSet<&str> =
            mounted.iter().map(|(_, e, _)| e.as_str()).collect();
        assert_eq!(
            used,
            ["authorize", "login", "revoke", "token"].into_iter().collect(),
            "the mounted surface no longer matches the budgets it draws on"
        );
    }

    /// A wrong content type is REFUSED and RECORDED. Round 14: this early
    /// return produced a 400 with no audit record — an unaudited pre-auth
    /// refusal on the endpoint an operator reaches for during an incident.
    ///
    /// Asserts the record exists and that it carries nothing from the request:
    /// the offending content-type value must not appear anywhere in it, because
    /// that header is caller-controlled and this door keeps no redaction pass
    /// to trust with it.
    #[tokio::test]
    async fn a_wrong_content_type_is_refused_and_recorded() {
        use crate::oauth::audit::{record_text, recent_records, AuditDetail, OauthEvent};
        use crate::oauth::revoke::fake::FakeSessionStore;

        let source = "203.0.113.44".parse::<std::net::IpAddr>().expect("literal");
        let store: Arc<dyn crate::oauth::revoke::SessionStore> =
            Arc::new(FakeSessionStore::new());
        let state = RevokeState {
            revocation: RevocationService::new(store),
            limiter: Arc::new(OauthRateLimiter::with_defaults()),
        };

        // The witness the layer would have produced, so the handler runs the
        // way it does in the mounted router.
        let cleared = state
            .limiter
            .check_address(OauthEndpoint::Revoke, source)
            .await
            .expect("under budget");
        let mut extensions = axum::http::Extensions::new();
        extensions.insert(crate::oauth::edge::ResolvedClientIp(source));

        let mut headers = HeaderMap::new();
        // A distinctive value, so the assertion below is about THIS request.
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/x-distinctive-wrong-type".parse().expect("header"),
        );

        let response = handle_revoke(
            State(state),
            cleared,
            extensions,
            headers,
            axum::body::Bytes::from_static(b"token=whatever"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let recorded = recent_records()
            .into_iter()
            .filter(|r| r.source_address().as_deref() == Some("203.0.113.44"))
            .find(|r| r.detail_kind() == Some(AuditDetail::RefusedBeforeParsing))
            .expect("the refusal was not recorded");
        assert_eq!(recorded.event_kind(), OauthEvent::Revoked);
        assert_eq!(recorded.endpoint_of(), Some(OauthEndpoint::Revoke));
        for text in record_text(&recorded) {
            assert!(
                !text.contains("distinctive-wrong-type"),
                "the offending header reached the audit record: {text}"
            );
        }
    }

    /// The property round 9 put in dispute: the limiter runs BEFORE parsing.
    ///
    /// The previous contract test proved the limiter was wired; it did not prove
    /// it ran first, and it did not: `handle_revoke` returned `400` for a bad
    /// content type before charging, and the login/consent handlers parsed and
    /// validated a body first. So malformed traffic was free.
    ///
    /// This drives a MALFORMED request at every mounted path through a real
    /// router carrying the real layer, and asserts budget was consumed — by
    /// showing the SECOND such request is refused with a 429 on a burst-1
    /// budget. A handler that parsed first would answer `400`/`404`/`415` twice
    /// and never reach the limiter.
    #[tokio::test]
    async fn a_malformed_request_costs_budget_at_every_mounted_route() {
        use tower::ServiceExt as _;

        for &(path, _endpoint, _has_subject) in MOUNTED_ROUTES {
            // Burst of 1 on every endpoint, negligible refill: the first request
            // is admitted, the second must be throttled if — and only if — the
            // first one was charged.
            let limiter = Arc::new(
                OauthRateLimiter::from_budgets(|_| crate::oauth::limits::EndpointBudgets {
                    per_address: crate::oauth::limits::Budget {
                        burst: 1,
                        refill_per_sec: 0.0001,
                    },
                    per_subject: crate::oauth::limits::Budget {
                        burst: 5,
                        refill_per_sec: 0.0002,
                    },
                })
                .expect("fixture budgets satisfy the invariant"),
            );

            // A router with the REAL layer in front of a handler that would
            // reject anything — standing in for the parse/validate refusals the
            // real handlers make.
            let router = Router::new()
                .route(path, axum::routing::any(|| async { StatusCode::BAD_REQUEST }))
                .route_layer(axum::middleware::from_fn_with_state(
                    limiter.clone(),
                    charge_address_budget,
                ));

            let malformed = || {
                axum::http::Request::builder()
                    .method("POST")
                    .uri(path)
                    // Deliberately the wrong content type and a junk body: the
                    // exact shape that used to be refused for free.
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from("{not-a-form"))
                    .expect("request")
            };

            let first = router.clone().oneshot(malformed()).await.expect("first");
            assert_eq!(
                first.status(),
                StatusCode::BAD_REQUEST,
                "{path}: the first malformed request should reach the handler"
            );

            let second = router.oneshot(malformed()).await.expect("second");
            assert_eq!(
                second.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "{path}: a malformed request did not consume budget, so an attacker can send \
                 them without limit"
            );
        }
    }

    /// An unmatched path costs no budget, and that is deliberate rather than an
    /// oversight — see `charge_address_budget`'s doc. Pinned so the choice
    /// between `layer` and `route_layer` cannot be flipped silently.
    #[tokio::test]
    async fn an_unmatched_path_does_not_spend_a_real_endpoints_budget() {
        use tower::ServiceExt as _;

        let limiter = Arc::new(
            OauthRateLimiter::from_budgets(|_| crate::oauth::limits::EndpointBudgets {
                per_address: crate::oauth::limits::Budget { burst: 1, refill_per_sec: 0.0001 },
                per_subject: crate::oauth::limits::Budget { burst: 5, refill_per_sec: 0.0002 },
            })
            .expect("fixture budgets satisfy the invariant"),
        );
        let router = Router::new()
            .route("/oauth/login", axum::routing::post(|| async { StatusCode::BAD_REQUEST }))
            .route_layer(axum::middleware::from_fn_with_state(
                limiter.clone(),
                charge_address_budget,
            ));

        for _ in 0..10 {
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/oauth/does-not-exist")
                .body(axum::body::Body::empty())
                .expect("request");
            let response = router.clone().oneshot(req).await.expect("response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        // The login budget is untouched: a scan of absent paths must not be able
        // to throttle a real endpoint.
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/oauth/login")
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(
            router.oneshot(req).await.expect("response").status(),
            StatusCode::BAD_REQUEST,
            "scanning absent paths drained the login budget"
        );
    }

    /// Rebuilding the token route must not have dropped RMCP-04's body bound
    /// while adding the limiter — that would trade one pre-auth DoS vector for
    /// another.
    #[test]
    fn the_rebuilt_token_route_keeps_the_documented_body_bound() {
        assert_eq!(MAX_TOKEN_BODY_BYTES, 8 * 1024, "RMCP-04's documented MAX_BODY_BYTES");
    }

    /// The subject key for a token request is the CLAIMED client id, from
    /// either carrier. Best-effort on purpose: an unattributable request is
    /// still limited on its address.
    #[test]
    fn the_token_subject_key_reads_a_claimed_client_id_from_either_carrier() {
        let headers = HeaderMap::new();
        assert_eq!(
            presented_client_id(&headers, b"grant_type=refresh_token&client_id=a-connector"),
            Some("a-connector".to_string())
        );

        // HTTP Basic, per RFC 6749 §2.3.1.
        let mut basic = HeaderMap::new();
        use base64::Engine as _;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode("a-connector:not-a-real-secret");
        basic.insert(
            axum::http::header::AUTHORIZATION,
            format!("Basic {encoded}").parse().expect("header"),
        );
        assert_eq!(presented_client_id(&basic, b""), Some("a-connector".to_string()));

        // Unattributable: no panic, no key — the address dimension still applies.
        assert_eq!(presented_client_id(&HeaderMap::new(), b"grant_type=refresh_token"), None);
        let mut junk = HeaderMap::new();
        junk.insert(axum::http::header::AUTHORIZATION, "Basic !!!".parse().expect("header"));
        assert_eq!(presented_client_id(&junk, b""), None);
    }

    /// A missing schema must NOT be reported in the variant that elsewhere means
    /// "not configured", because that is the variant a caller reads as "carry on
    /// without the door".
    ///
    /// This is the failure a real operator meets first: the S132 migrations are
    /// applied by hand as a deploy step, so "configured, no tables" is the
    /// ordinary state of a fresh deployment. Failing open there means the
    /// operator believes the door is up while nothing serves it.
    #[test]
    fn schema_not_ready_is_not_reported_as_absence() {
        // The exact error `from_env` returns for a present-but-unmigrated door.
        let err = ToolError::Database(
            "the RMCP OAuth door is configured but its schema is not present".into(),
        );
        assert!(
            !matches!(err, ToolError::NotConfigured(_)),
            "a present-but-unusable door must not share a variant with an absent one"
        );
        // And absence itself is decided on the variable, never on an error: the
        // only `Ok(None)` in `from_env` is guarded by reading the URL directly.
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/oauth/mount.rs"),
        )
        .expect("readable");
        let from_env = source
            .split("pub async fn from_env")
            .nth(1)
            .expect("from_env exists");
        let body = &from_env[..from_env.find("\n    /// Build from already").unwrap_or(from_env.len())];
        assert!(
            !body.contains("Err(ToolError::NotConfigured(_)) => return Ok(None)"),
            "absence is being inferred from an error variant again"
        );
        assert_eq!(
            body.matches("return Ok(None)").count(),
            1,
            "there must be exactly one absent path, guarded by the URL check"
        );
    }

    #[test]
    fn form_content_type_is_required_and_parameters_are_tolerated() {
        let mut headers = HeaderMap::new();
        assert!(!is_form_content_type(&headers), "an absent content type is not a form");
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().expect("header"),
        );
        assert!(!is_form_content_type(&headers));
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8".parse().expect("header"),
        );
        assert!(is_form_content_type(&headers), "a charset parameter must not change the type");
    }

    /// A duplicated key must not be resolved silently. The handler takes the
    /// first occurrence; this pins that both are still visible so the choice
    /// stays a decision rather than a map's arbitrary winner.
    #[test]
    fn duplicate_form_keys_are_preserved_not_collapsed() {
        let parsed = parse_form(b"token=first&token=second");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], ("token".into(), "first".into()));
    }

    #[test]
    fn form_values_are_percent_and_plus_decoded() {
        let parsed = parse_form(b"token=a%2Bb+c&token_type_hint=refresh_token");
        assert_eq!(parsed[0].1, "a+b c");
        assert_eq!(parsed[1].1, "refresh_token");
        // A malformed escape is passed through rather than dropped: losing
        // characters from a token would turn a valid revocation into a silent
        // no-op, which this endpoint must never do.
        assert_eq!(parse_form(b"token=a%zz")[0].1, "a%zz");
    }

    #[test]
    fn an_empty_body_yields_no_fields_rather_than_a_phantom_one() {
        assert!(parse_form(b"").is_empty());
        assert!(parse_form(b"&&").is_empty());
    }
}
