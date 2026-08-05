//! RMCP-08 — `POST /oauth/register`, RFC 7591 dynamic client registration.
//!
//! ## Off by default, and never an unauthenticated write
//!
//! This endpoint is mounted only when `RMCP_OAUTH_DCR_ENABLED` is on, and that
//! is the SAME VALUE [`crate::oauth::metadata`] uses to decide whether to
//! advertise `registration_endpoint`. The flag is read ONCE, at startup in the
//! binary, and the boolean is passed to both — round 1 of review found an
//! earlier version calling the reader twice, which centralises the parsing and
//! not the value, and two reads of a mutable environment can disagree about the
//! one thing that must not differ: whether the endpoint is advertised versus
//! whether it is served. An
//! advertised endpoint that refuses everything is worse than an absent one: the
//! client reads the key as a supported path, attempts it, and reports the
//! refusal as a server fault instead of falling back to the pre-registered
//! `client_id` the operator already pasted in.
//!
//! When it IS on, a registration must present an operator-issued **initial
//! access token** (RFC 7591 §3.1) as `Authorization: Bearer <iat>`. An
//! unauthenticated registration is refused. That is what keeps "only the
//! operator can create a client here" true with the endpoint open: without it,
//! an internet-reachable registration endpoint is an anonymous write to the
//! table that decides which connectors exist.
//!
//! ## Why the handler re-checks a flag the mounting already decided
//!
//! [`Registration::router`] returns an empty router when DCR is off, so the
//! path 404s. The handler checks anyway, and the check is not decoration: it is
//! the arm that holds if a future mounting change, a test harness, or a second
//! caller ever builds this route without consulting the flag. A guard whose
//! only protection is that no one currently calls it wrongly is not a guard,
//! and `a_disabled_endpoint_refuses_and_creates_nothing` drives the handler
//! DIRECTLY so the check is exercised rather than assumed.
//!
//! ## What a registered client can do
//!
//! Nothing, until an operator scopes it. It lands with no scope rows and
//! RMCP-07 reads absence as the empty set — see [`crate::oauth::clients`]'s
//! module docs for why that, rather than the `disabled` column, is the control
//! that delivers it.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::oauth::audit::{AuditDetail, DenialReason, OauthAuditRecord, OauthEvent};
use crate::oauth::clients::{
    validate, ClientService, FieldFault, MintedClient, SubmittedMetadata, COSMETIC_METADATA,
    SUPPORTED_METADATA, UNIMPLEMENTED_CRITICAL_METADATA,
};
use crate::oauth::limits::OauthEndpoint;
use crate::oauth::metadata::REGISTER_PATH;

/// The largest registration body accepted.
///
/// Registration metadata is a name, a handful of URIs and two short arrays —
/// well under a kilobyte in practice. The bound exists because this endpoint is
/// reachable from the internet and parses JSON: an unbounded body on a
/// pre-auth path is a memory amplifier whatever the parser then does, and it is
/// also what makes "deeply nested JSON" a bounded problem rather than a
/// recursion one, since 4 KiB cannot express a nesting depth worth worrying
/// about.
pub(crate) const MAX_REGISTER_BODY_BYTES: usize = 4 * 1024;

/// Everything `POST /oauth/register` needs.
#[derive(Clone)]
pub struct Registration {
    service: ClientService,
    dcr_enabled: bool,
}

impl Registration {
    /// The rate limiter is deliberately NOT a field here. `/oauth/register`
    /// draws on the door's one shared budget table through
    /// [`crate::oauth::mount`]'s layer, like every other mounted route; a
    /// limiter owned by this endpoint would be a second budget that could drift
    /// from the first, which is the duplication TERM #633 already had to undo
    /// once for the login POST.
    pub fn new(service: ClientService, dcr_enabled: bool) -> Self {
        Self { service, dcr_enabled }
    }

    /// Whether this deployment registers clients dynamically.
    pub fn enabled(&self) -> bool {
        self.dcr_enabled
    }

    /// The route, or an EMPTY router when DCR is off.
    ///
    /// Empty rather than "mounted and refusing", so a disabled deployment has
    /// no registration surface at all: the path 404s exactly as it does on a
    /// build that never had the feature, and the metadata document omits the
    /// key. Those two facts come from one flag, which is the point.
    pub fn router(&self) -> Router {
        if !self.dcr_enabled {
            return Router::new();
        }
        Router::new()
            .route(REGISTER_PATH, axum::routing::post(handle_register))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_REGISTER_BODY_BYTES))
            .with_state(self.clone())
    }
}

/// `POST /oauth/register`.
///
/// The address dimension of the rate limit was already charged by
/// [`crate::oauth::mount`]'s shared layer, before this handler ran — so a
/// malformed or unauthenticated attempt costs budget, which is what stops an
/// initial access token being guessed at line rate through requests this
/// handler would have refused.
pub async fn handle_register(
    State(state): State<Registration>,
    cleared: crate::oauth::limits::AddressCleared,
    extensions: axum::http::Extensions,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let source = crate::oauth::authorize::resolved_source_for(&extensions);

    // ── The kill switch, re-asserted where the work happens ─────────────────
    if !state.dcr_enabled {
        OauthAuditRecord::new(OauthEvent::RegistrationDenied)
            .endpoint(OauthEndpoint::Register)
            .from_address(source)
            .reason(DenialReason::RegistrationNotPermitted)
            .emit();
        // 404, identical to the unmounted case. A 403 here would tell a
        // scanner the feature exists and is merely switched off, which is one
        // more thing than "there is nothing at this path".
        return not_found();
    }

    if !is_json_content_type(&headers) {
        OauthAuditRecord::new(OauthEvent::RegistrationDenied)
            .endpoint(OauthEndpoint::Register)
            .from_address(source)
            .reason(DenialReason::MalformedRequest)
            .detail(AuditDetail::RefusedBeforeParsing)
            .emit();
        return error_response(StatusCode::BAD_REQUEST, "invalid_client_metadata", &[]);
    }

    // ── The operator-issued initial access token ────────────────────────────
    //
    // Read, and SPENT, before the body is parsed. An unauthenticated caller
    // must not be able to make this process do JSON work, and it must not be
    // able to learn anything about its metadata by varying it.
    let Some(presented) = bearer_token(&headers) else {
        return refuse_unauthenticated(source);
    };

    // The subject dimension, keyed on nothing the caller chose: the token is a
    // credential and must never become a bucket key. Registration is keyed on
    // the ENDPOINT alone here, which the address dimension already covers, so
    // this deliberately charges nothing further rather than inventing a key out
    // of caller-controlled bytes. `cleared` is held to prove the address charge
    // happened.
    let _ = &cleared;

    // The authority is re-derived from the store on THIS request. An initial
    // access token can be revoked, can expire, and can run out of uses after it
    // was issued — a check made at minting time would never see any of that.
    let issued_by = match state.service.claim_registration_token(presented).await {
        Ok(Some(account)) => account,
        Ok(None) => return refuse_unauthenticated(source),
        Err(_) => {
            // A store failure must not be reported as an accepted or a rejected
            // registration. Neither happened.
            OauthAuditRecord::new(OauthEvent::RegistrationDenied)
                .endpoint(OauthEndpoint::Register)
                .from_address(source)
                .reason(DenialReason::RegistrationNotPermitted)
                .emit();
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                &[],
            );
        }
    };

    // ── The metadata ────────────────────────────────────────────────────────
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        OauthAuditRecord::new(OauthEvent::RegistrationDenied)
            .endpoint(OauthEndpoint::Register)
            .from_address(source)
            .reason(DenialReason::MalformedRequest)
            .detail(AuditDetail::RefusedBeforeParsing)
            .emit();
        return error_response(StatusCode::BAD_REQUEST, "invalid_client_metadata", &[]);
    };

    let submitted = submitted_from_json(&parsed);
    let validated = match validate(&submitted) {
        Ok(validated) => validated,
        Err(faults) => {
            OauthAuditRecord::new(OauthEvent::RegistrationDenied)
                .endpoint(OauthEndpoint::Register)
                .from_address(source)
                .reason(DenialReason::MalformedRequest)
                .emit();
            let redirect_fault = faults.iter().any(|f| f.field == "redirect_uris");
            let code = if redirect_fault { "invalid_redirect_uri" } else { "invalid_client_metadata" };
            return error_response(StatusCode::BAD_REQUEST, code, &faults);
        }
    };

    match state.service.register_dynamic(issued_by, &validated).await {
        Ok(minted) => {
            OauthAuditRecord::new(OauthEvent::RegistrationAccepted)
                .endpoint(OauthEndpoint::Register)
                .from_address(source)
                // The row id, which is not a credential and is not the public
                // `client_id`. Nothing caller-chosen and nothing presentable
                // enters the record.
                .client_uuid(minted.client.id)
                .account(issued_by)
                .detail(AuditDetail::ClientRegistered)
                .emit();
            (StatusCode::CREATED, registration_response(&minted)).into_response()
        }
        Err(ToolError::Conflict(_)) | Err(ToolError::Database(_)) | Err(ToolError::Execution(_)) => {
            OauthAuditRecord::new(OauthEvent::RegistrationDenied)
                .endpoint(OauthEndpoint::Register)
                .from_address(source)
                .reason(DenialReason::RegistrationNotPermitted)
                .emit();
            error_response(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable", &[])
        }
        Err(_) => {
            OauthAuditRecord::new(OauthEvent::RegistrationDenied)
                .endpoint(OauthEndpoint::Register)
                .from_address(source)
                .reason(DenialReason::MalformedRequest)
                .emit();
            error_response(StatusCode::BAD_REQUEST, "invalid_client_metadata", &[])
        }
    }
}

/// The one refusal for every unusable initial access token: absent, malformed,
/// unknown, expired, revoked, or exhausted.
///
/// All six answer identically, and that is deliberate. A response that
/// distinguished "no such token" from "that token is spent" would tell an
/// attacker which of their guesses had once been real — the same
/// existence-oracle reasoning that collapses "no such account" into "bad
/// credentials" everywhere else in this subsystem.
fn refuse_unauthenticated(source: std::net::IpAddr) -> Response {
    OauthAuditRecord::new(OauthEvent::RegistrationDenied)
        .endpoint(OauthEndpoint::Register)
        .from_address(source)
        .reason(DenialReason::RegistrationNotPermitted)
        .emit();
    let mut response =
        error_response(StatusCode::UNAUTHORIZED, "invalid_token", &[]);
    response.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Bearer error=\"invalid_token\""),
    );
    response
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, [(axum::http::header::CONTENT_TYPE, "application/json")], "{}")
        .into_response()
}

/// An RFC 7591 §3.2.2 error body.
///
/// `error_description` is assembled from [`FieldFault::render`], which is a
/// static field name, an integer index and a static message — so this body
/// cannot echo a submitted value back at the caller, at any length. That is not
/// merely tidy: this response goes to an unauthenticated-ish caller over the
/// public internet and lands in logs on both sides.
fn error_response(status: StatusCode, code: &str, faults: &[FieldFault]) -> Response {
    let mut body = json!({ "error": code });
    if !faults.is_empty() {
        let described: Vec<String> = faults.iter().map(FieldFault::render).collect();
        body["error_description"] = json!(described.join("; "));
    }
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// RFC 7591 §3.2.1's success body.
///
/// `client_secret` appears here and NOWHERE else in the system: no read path
/// carries one, and `client_secret_expires_at: 0` states that it does not
/// expire, per the RFC. `client_id_issued_at` is seconds since the epoch.
fn registration_response(minted: &MintedClient) -> Response {
    let client = &minted.client;
    let mut body = json!({
        "client_id": client.client_id,
        "client_id_issued_at": client.created_at.timestamp(),
        "client_name": client.name,
        "redirect_uris": client.redirect_uris,
        "grant_types": client.grant_types,
        "response_types": ["code"],
        "token_endpoint_auth_method": client.token_endpoint_auth_method,
    });
    if let Some(secret) = &minted.secret {
        body["client_secret"] = json!(secret);
        body["client_secret_expires_at"] = json!(0);
    }
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Read submitted metadata out of a JSON body.
///
/// Nothing here judges; every rule lives in [`validate`]. What this DOES decide
/// is how a member is CLASSIFIED, and two classifications are load-bearing:
///
/// **Present-but-wrong-typed is MALFORMED, never absent.** Round 1 (`gpt56`)
/// found the first version returning `None` for a present non-string and a
/// present non-array, which fed straight into the "absent means the default"
/// arm — so `grant_types: "password"` registered a client with the supported
/// grants it did not ask for, and `token_endpoint_auth_method: 42` registered
/// it on the WEAKEST auth method. RMCP-02's rule is applied verbatim rather
/// than re-decided: ABSENT means not configured; PRESENT means the value must
/// be usable; present-but-unusable is refused.
///
/// **An unrecognised member is refused, not ignored.** Understood
/// ([`SUPPORTED_METADATA`]) and deliberately-cosmetic ([`COSMETIC_METADATA`])
/// members pass; everything else fails the request. Members named in
/// [`UNIMPLEMENTED_CRITICAL_METADATA`] are reported individually because their
/// names are `&'static str`s we own; anything else contributes only a BOOLEAN,
/// because an unrecognised key is caller-chosen text and this value is on the
/// path to an error body.
///
/// A JSON `null` reads as ABSENT throughout, matching the crate-wide rule that
/// a blank value is an unset one — a client that serializes its whole struct
/// must not be refused for the fields it left empty.
/// What a JSON body says about one metadata member.
///
/// **The three-way rule, stated once.** Every reader below branches on exactly
/// these three cases, so no call site re-decides what `null` or `""` means:
///
/// | Body | Meaning | Outcome |
/// |---|---|---|
/// | key not present | not configured | the documented default applies |
/// | key present, value usable | the client said this | the submitted value |
/// | key present, anything else | present-but-unusable | **refused** |
///
/// This is RMCP-02's rule — *ABSENT means not configured; PRESENT means the
/// value must be USABLE* — applied to JSON members rather than environment
/// variables. In JSON a `null` is a PRESENT key with a null value: the client
/// sent the member. A blank string is more obviously present still.
///
/// ## Why this needed a round 5
///
/// Round 2 fixed wrong-TYPED values (`grant_types: "password"`,
/// `token_endpoint_auth_method: 42`) falling through to defaults, and I stated
/// at the time that "`null` still reads as absent" as though that were settled.
/// It was not, and it is not defensible: treating `42` as malformed while
/// treating `null` as absent draws a line the rule does not draw. The
/// consequence was the same one round 2 had just removed —
/// `token_endpoint_auth_method: null` and `""` both selected `"none"`, so a
/// client that submitted something meaningless was registered as PUBLIC, with
/// no client authentication at all.
enum Member<'a> {
    /// The key is not in the object.
    Absent,
    /// The key is present with a value worth examining.
    Present(&'a Value),
    /// The key is present and carries `null` — the client named the member and
    /// gave nothing usable for it.
    Unusable,
}

/// Classify one member. The ONLY place `null` is interpreted.
fn member<'a>(body: &'a Value, name: &str) -> Member<'a> {
    match body.get(name) {
        None => Member::Absent,
        Some(Value::Null) => Member::Unusable,
        Some(present) => Member::Present(present),
    }
}

/// Read submitted metadata out of a JSON body.
///
/// Nothing here judges the CONTENT; every such rule lives in [`validate`]. What
/// this decides is presence and usability — see [`Member`] for the rule — and
/// how a member is classified:
///
/// - **Members this server READS** ([`SUPPORTED_METADATA`]): the three-way rule
///   applies in full. Absent takes the default; present must be usable;
///   `null`, blank, wrong type, or a wrong-typed array element are all
///   malformed.
/// - **Members it REFUSES** ([`UNIMPLEMENTED_CRITICAL_METADATA`]): presence
///   alone refuses, whatever the value — including `null`. The refusal is not
///   about the value, it is that this server cannot honour the member at all,
///   so a null one is still the client naming it. Omit the key.
/// - **Members it IGNORES** ([`COSMETIC_METADATA`]): the value is never
///   examined, so "usable" has no meaning for it. This is the one class where
///   `null` changes nothing, and it is a named exception with a reason rather
///   than a general one: we already ignore every other value these carry, so
///   singling out `null` would be incoherent.
/// - **Anything else**: unrecognised, and refused — contributing a BOOLEAN, not
///   the key, because an unrecognised name is caller-chosen text and this value
///   is on the path to an error body.
fn submitted_from_json(value: &Value) -> SubmittedMetadata {
    let mut malformed_members: Vec<&'static str> = Vec::new();

    let mut string_array = |name: &'static str| -> Option<Vec<String>> {
        match member(value, name) {
            Member::Absent => None,
            Member::Unusable => {
                malformed_members.push(name);
                None
            }
            Member::Present(raw) => {
                let Some(items) = raw.as_array() else {
                    malformed_members.push(name);
                    return None;
                };
                // A wrong-typed ELEMENT makes the MEMBER malformed. Turning a
                // type error into a value error produced a confusing fault and
                // — for `redirect_uris` — could read as "no redirect URIs"
                // rather than "that is not a list of URIs".
                if items.iter().any(|item| !item.is_string()) {
                    malformed_members.push(name);
                    return None;
                }
                Some(
                    items
                        .iter()
                        .map(|item| item.as_str().unwrap_or_default().to_string())
                        .collect(),
                )
            }
        }
    };

    let redirect_uris = string_array("redirect_uris");
    let grant_types = string_array("grant_types");
    let response_types = string_array("response_types");

    let mut string = |name: &'static str| -> Option<String> {
        match member(value, name) {
            Member::Absent => None,
            Member::Unusable => {
                malformed_members.push(name);
                None
            }
            Member::Present(raw) => match raw.as_str() {
                // A BLANK string is present-but-unusable, not absent. This is
                // the arm that stops `token_endpoint_auth_method: ""` selecting
                // `none` — the weakest method — on behalf of a client that said
                // nothing meaningful.
                Some(text) if !text.trim().is_empty() => Some(text.to_string()),
                _ => {
                    malformed_members.push(name);
                    None
                }
            },
        }
    };
    let name = string("client_name");
    let token_endpoint_auth_method = string("token_endpoint_auth_method");

    // Classification of every member actually sent. Note there is no
    // null-skipping here: a `null` critical member is still the client naming
    // a member this server cannot honour.
    let mut critical_members_present: Vec<&'static str> = Vec::new();
    let mut unrecognised_member_present = false;
    if let Some(members) = value.as_object() {
        for key in members.keys() {
            // Read by the readers above, which already applied the rule.
            if SUPPORTED_METADATA.contains(&key.as_str()) {
                continue;
            }
            // Never examined at all — see this function's doc for why `null` is
            // not special here.
            if COSMETIC_METADATA.contains(&key.as_str()) {
                continue;
            }
            // The `&'static str` from OUR constant, never the key as it
            // arrived — so no submitted text can reach a fault message.
            match UNIMPLEMENTED_CRITICAL_METADATA
                .iter()
                .find(|known| *known == key)
            {
                Some(known) => critical_members_present.push(known),
                None => unrecognised_member_present = true,
            }
        }
    }

    SubmittedMetadata {
        name,
        redirect_uris: redirect_uris.unwrap_or_default(),
        grant_types,
        response_types,
        token_endpoint_auth_method,
        critical_members_present,
        unrecognised_member_present,
        malformed_members,
    }
}

/// The bearer token from `Authorization`, if there is a well-formed one.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim();
    // The scheme is case-insensitive per RFC 7235; the token is not.
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn headers(auth: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().expect("header"),
        );
        if let Some(auth) = auth {
            headers.insert(
                axum::http::header::AUTHORIZATION,
                auth.parse().expect("header"),
            );
        }
        headers
    }

    #[test]
    fn a_bearer_token_is_read_case_insensitively_on_the_scheme_only() {
        assert_eq!(bearer_token(&headers(Some("Bearer abc"))), Some("abc"));
        assert_eq!(bearer_token(&headers(Some("bearer abc"))), Some("abc"));
        assert_eq!(bearer_token(&headers(Some("BEARER abc"))), Some("abc"));
        // Not a bearer, or nothing after the scheme.
        assert_eq!(bearer_token(&headers(Some("Basic abc"))), None);
        assert_eq!(bearer_token(&headers(Some("Bearer"))), None);
        assert_eq!(bearer_token(&headers(Some("Bearer   "))), None);
        assert_eq!(bearer_token(&headers(None)), None);
    }

    #[test]
    fn only_a_json_content_type_is_accepted() {
        assert!(is_json_content_type(&headers(None)));
        let mut form = HeaderMap::new();
        form.insert(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded".parse().expect("header"),
        );
        assert!(!is_json_content_type(&form));
        let mut charset = HeaderMap::new();
        charset.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8".parse().expect("header"),
        );
        assert!(is_json_content_type(&charset), "a charset parameter is not a different type");
        assert!(!is_json_content_type(&HeaderMap::new()), "an absent type is not JSON");
    }

    /// The JSON reader must not be able to smuggle a caller-chosen key into a
    /// fault message: only the `&'static str`s from our own constants are
    /// recorded, and anything else contributes a boolean.
    #[test]
    fn only_static_member_names_are_recorded_as_critical() {
        let body = json!({
            "client_name": "A connector",
            "redirect_uris": ["https://connector.test/cb"],
            "software_statement": "ey.distinctive-marker",
            "SOFTWARE_STATEMENT": "not the same key",
        });
        let submitted = submitted_from_json(&body);
        assert_eq!(submitted.critical_members_present, vec!["software_statement"]);
        // The differently-cased key is not the same member; it is unrecognised,
        // and it contributes a boolean rather than its own text.
        assert!(submitted.unrecognised_member_present);

        let faults = validate(&submitted).expect_err("must refuse");
        for fault in &faults {
            let rendered = fault.render();
            assert!(!rendered.contains("distinctive-marker"), "{rendered}");
            assert!(!rendered.contains("SOFTWARE_STATEMENT"), "{rendered}");
        }
    }

    /// **A `null` member is PRESENT and unusable — refused, not defaulted.**
    ///
    /// This test replaces one that asserted the opposite. Round 2 fixed
    /// wrong-TYPED values falling through to defaults and I recorded "`null`
    /// still reads as absent" as settled; round 5 (`gpt56`) reopened it and was
    /// right. In JSON a `null` is a present key — the client sent the member —
    /// so RMCP-02's rule, which this item applies everywhere else, refuses it.
    ///
    /// The consequence was concrete: `token_endpoint_auth_method: null`
    /// selected `"none"`, registering a client as PUBLIC with no client
    /// authentication because it had said nothing meaningful.
    #[test]
    fn a_null_member_is_present_and_unusable_rather_than_absent() {
        for name in SUPPORTED_METADATA {
            let mut body = serde_json::Map::new();
            body.insert("client_name".into(), json!("A connector"));
            body.insert("redirect_uris".into(), json!(["https://connector.test/cb"]));
            body.insert((*name).to_string(), Value::Null);
            let submitted = submitted_from_json(&Value::Object(body));

            assert!(
                submitted.malformed_members.contains(name),
                "{name}: null was read as absent instead of present-and-unusable"
            );
            assert!(
                validate(&submitted).is_err(),
                "{name}: a null member must be refused"
            );
        }
    }

    /// **A BLANK string is present and unusable too** — the other half of the
    /// round-5 finding, and the one that reached `"none"` most easily.
    #[test]
    fn a_blank_string_member_is_refused_rather_than_defaulted() {
        for name in ["client_name", "token_endpoint_auth_method"] {
            for blank in [json!(""), json!("   "), json!("\t")] {
                let mut body = serde_json::Map::new();
                body.insert("client_name".into(), json!("A connector"));
                body.insert("redirect_uris".into(), json!(["https://connector.test/cb"]));
                body.insert(name.to_string(), blank.clone());
                let submitted = submitted_from_json(&Value::Object(body));

                assert!(
                    submitted.malformed_members.contains(&name),
                    "{name} = {blank} was read as absent"
                );
                assert!(validate(&submitted).is_err(), "{name} = {blank} must be refused");
            }
        }
    }

    /// The specific consequence, pinned on its own: a meaningless auth method
    /// must never register a PUBLIC client.
    ///
    /// The mutation target for the whole finding — restore either the
    /// null-is-absent reading or the `filter(|m| !m.is_empty())` and this goes
    /// red, because the client would come away registered with no client
    /// authentication it never asked to drop.
    #[test]
    fn a_meaningless_auth_method_never_registers_a_public_client() {
        for meaningless in [json!(Value::Null), json!(""), json!("  "), json!(42), json!([])] {
            let body = json!({
                "client_name": "A connector",
                "redirect_uris": ["https://connector.test/cb"],
                "token_endpoint_auth_method": meaningless,
            });
            let submitted = submitted_from_json(&body);
            assert!(
                submitted.token_endpoint_auth_method.is_none(),
                "{meaningless} produced a usable auth method"
            );
            assert!(
                validate(&submitted).is_err(),
                "{meaningless} was accepted and would register a PUBLIC client"
            );
        }

        // …while a genuinely ABSENT member still takes the documented default,
        // which is the behaviour that had to survive the fix.
        let absent = json!({
            "client_name": "A connector",
            "redirect_uris": ["https://connector.test/cb"],
        });
        let validated = validate(&submitted_from_json(&absent)).expect("absent is fine");
        assert_eq!(
            validated.token_endpoint_auth_method,
            crate::oauth::clients::DEFAULT_AUTH_METHOD
        );
        assert!(!validated.wants_secret());
    }

    /// A `null` on a CRITICAL member is still the client naming a member this
    /// server cannot honour, so it is refused like any other value of it.
    #[test]
    fn a_null_critical_member_is_still_refused() {
        for critical in UNIMPLEMENTED_CRITICAL_METADATA {
            let mut body = serde_json::Map::new();
            body.insert("client_name".into(), json!("A connector"));
            body.insert("redirect_uris".into(), json!(["https://connector.test/cb"]));
            body.insert((*critical).to_string(), Value::Null);
            let submitted = submitted_from_json(&Value::Object(body));
            assert!(
                submitted.critical_members_present.contains(critical),
                "{critical}: a null value let it through"
            );
            assert!(validate(&submitted).is_err());
        }
    }

    /// **The one named exception: a COSMETIC member's value is never examined,
    /// so `null` changes nothing for it.**
    ///
    /// Stated as its own test rather than left implicit, because a general
    /// null-is-absent exception is what created the round-5 finding. The reason
    /// this class is different is narrow and checkable: we already ignore every
    /// value these members carry, so singling out `null` would be incoherent
    /// rather than stricter.
    #[test]
    fn a_null_cosmetic_member_is_ignored_because_its_value_is_never_read() {
        for cosmetic in COSMETIC_METADATA {
            for value in [Value::Null, json!(""), json!(42), json!({"nested": true})] {
                let mut body = serde_json::Map::new();
                body.insert("client_name".into(), json!("A connector"));
                body.insert("redirect_uris".into(), json!(["https://connector.test/cb"]));
                body.insert((*cosmetic).to_string(), value.clone());
                let submitted = submitted_from_json(&Value::Object(body));
                assert!(
                    submitted.malformed_members.is_empty()
                        && !submitted.unrecognised_member_present,
                    "{cosmetic} = {value} must be ignored, not classified"
                );
                assert!(
                    validate(&submitted).is_ok(),
                    "{cosmetic} = {value} must not refuse the registration"
                );
            }
        }
    }

    /// **Understood and cosmetic members pass; everything else is refused.**
    ///
    /// The round-1 restructure: the control is an allowlist, so a
    /// security-significant member nobody anticipated is refused BY DEFAULT
    /// rather than by somebody having thought to list it.
    #[test]
    fn cosmetic_members_are_ignored_and_unrecognised_ones_are_refused() {
        let with = |extra: serde_json::Map<String, Value>| {
            let mut body = serde_json::Map::new();
            body.insert("client_name".into(), json!("A connector"));
            body.insert("redirect_uris".into(), json!(["https://connector.test/cb"]));
            body.extend(extra);
            submitted_from_json(&Value::Object(body))
        };

        // Every cosmetic member, together, still registers cleanly.
        let mut cosmetic = serde_json::Map::new();
        for name in COSMETIC_METADATA {
            cosmetic.insert((*name).to_string(), json!("something"));
        }
        let submitted = with(cosmetic);
        assert!(!submitted.unrecognised_member_present);
        assert!(validate(&submitted).is_ok(), "cosmetic members must be ignored, not refused");

        // An unrecognised member is refused.
        let mut unknown = serde_json::Map::new();
        unknown.insert("some_future_member".into(), json!(true));
        let submitted = with(unknown);
        assert!(submitted.unrecognised_member_present);
        let kinds: Vec<_> = validate(&submitted)
            .expect_err("must refuse")
            .into_iter()
            .map(|f| f.fault)
            .collect();
        assert!(kinds.contains(&crate::oauth::clients::MetadataFault::UnrecognisedMember));

        // And every named security member is refused individually, naming itself.
        for member in UNIMPLEMENTED_CRITICAL_METADATA {
            let mut one = serde_json::Map::new();
            one.insert((*member).to_string(), json!("x"));
            let faults = validate(&with(one)).expect_err("must refuse");
            assert!(
                faults.iter().any(|f| f.field == *member),
                "{member} was not refused by name"
            );
        }

        // The three classes must not overlap — a member in two lists would have
        // an order-dependent meaning.
        for name in SUPPORTED_METADATA {
            assert!(!COSMETIC_METADATA.contains(name));
            assert!(!UNIMPLEMENTED_CRITICAL_METADATA.contains(name));
        }
        for name in COSMETIC_METADATA {
            assert!(!UNIMPLEMENTED_CRITICAL_METADATA.contains(name));
        }
    }

    /// **A present-but-wrong-typed member is MALFORMED, never absent.**
    ///
    /// The round-1 fail-open, one case per member. Each of these used to read
    /// as absence and take the default — and for `token_endpoint_auth_method`
    /// the default is the WEAKEST method, chosen by a malformed request rather
    /// than by the caller.
    #[test]
    fn a_present_but_wrong_typed_member_is_refused_rather_than_defaulted() {
        use crate::oauth::clients::MetadataFault;

        let cases = [
            ("client_name", json!(42)),
            ("redirect_uris", json!("https://connector.test/cb")),
            ("grant_types", json!("password")),
            ("response_types", json!(42)),
            ("token_endpoint_auth_method", json!(42)),
            // Wrong element type inside a well-formed array.
            ("redirect_uris", json!([42])),
            ("grant_types", json!([{ "nested": true }])),
        ];

        for (member, bad) in cases {
            let mut body = serde_json::Map::new();
            body.insert("client_name".into(), json!("A connector"));
            body.insert("redirect_uris".into(), json!(["https://connector.test/cb"]));
            body.insert(member.to_string(), bad.clone());
            let submitted = submitted_from_json(&Value::Object(body));

            assert!(
                submitted.malformed_members.contains(&member),
                "{member} = {bad} was read as absent instead of malformed"
            );
            let faults = validate(&submitted).expect_err("must refuse");
            assert!(
                faults
                    .iter()
                    .any(|f| f.field == member && f.fault == MetadataFault::MalformedMember),
                "{member} = {bad} did not produce a malformed-member fault"
            );
        }
    }

    /// The specific consequence worth naming: a malformed auth method must not
    /// land on `none`. This is the mutation target for the fix — restore the
    /// `None`-for-wrong-type reading and this registers a client on the weakest
    /// method it never asked for.
    #[test]
    fn a_malformed_auth_method_never_silently_becomes_a_public_client() {
        let body = json!({
            "client_name": "A connector",
            "redirect_uris": ["https://connector.test/cb"],
            "token_endpoint_auth_method": 42,
        });
        let submitted = submitted_from_json(&body);
        assert!(submitted.token_endpoint_auth_method.is_none());
        assert!(validate(&submitted).is_err(), "a malformed auth method was defaulted to `none`");

        // …while a genuinely ABSENT one still defaults, which is the behaviour
        // that must survive the fix.
        let absent = json!({
            "client_name": "A connector",
            "redirect_uris": ["https://connector.test/cb"],
        });
        let validated = validate(&submitted_from_json(&absent)).expect("absent is fine");
        assert_eq!(validated.token_endpoint_auth_method, "none");
        assert!(!validated.wants_secret());
    }

    /// The body bound is the same order as the token endpoint's and is what
    /// makes deeply nested JSON a bounded parse rather than a recursion
    /// problem.
    #[test]
    fn the_registration_body_is_bounded() {
        assert_eq!(MAX_REGISTER_BODY_BYTES, 4 * 1024);
        // A deeply nested body within the byte bound is still refused by the
        // parser's own recursion limit — never by exhausting this process.
        let deep = format!("{}{}", "[".repeat(600), "]".repeat(600));
        assert!(deep.len() < MAX_REGISTER_BODY_BYTES);
        assert!(
            serde_json::from_slice::<Value>(deep.as_bytes()).is_err(),
            "serde_json must refuse the nesting rather than recurse into it"
        );
    }

    /// The mounted surface is EMPTY when DCR is off. This is the fact that has
    /// to agree with the metadata document.
    #[tokio::test]
    async fn the_route_is_absent_entirely_when_dcr_is_off() {
        use tower::ServiceExt as _;

        for (enabled, expected) in [(false, StatusCode::NOT_FOUND), (true, StatusCode::UNAUTHORIZED)]
        {
            let registration = Registration {
                // A service is needed to build the router; the disabled arm
                // never reaches it, and the enabled arm is refused at the
                // token check before any store call.
                service: ClientService::new(crate::oauth::store::OauthStore::from_pool(
                    lazy_pool(),
                )),
                dcr_enabled: enabled,
            };
            let limiter = Arc::new(crate::oauth::limits::OauthRateLimiter::with_defaults());
            let router = registration
                .router()
                .layer(axum::middleware::from_fn_with_state(limiter, charge_for_test));
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(REGISTER_PATH)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from("{}"))
                .expect("request");
            let response = router.oneshot(request).await.expect("response");
            assert_eq!(
                response.status(),
                expected,
                "dcr_enabled={enabled} produced the wrong surface"
            );
        }
    }

    /// The in-handler kill switch, driven DIRECTLY — the mutation target for
    /// "a guard that cannot fail for the case it exists to catch". Delete the
    /// `if !state.dcr_enabled` block in `handle_register` and this goes red,
    /// while the routing test above stays green.
    #[tokio::test]
    async fn a_disabled_endpoint_refuses_and_creates_nothing() {
        use crate::oauth::audit::{recent_records, OauthEvent};

        let source = "203.0.113.19".parse::<std::net::IpAddr>().expect("literal");
        let limiter = Arc::new(crate::oauth::limits::OauthRateLimiter::with_defaults());
        let state = Registration {
            service: ClientService::new(crate::oauth::store::OauthStore::from_pool(lazy_pool())),
            dcr_enabled: false,
        };
        let cleared = limiter
            .check_address(OauthEndpoint::Register, source)
            .await
            .expect("under budget");
        let mut extensions = axum::http::Extensions::new();
        extensions.insert(crate::oauth::edge::ResolvedClientIp(source));

        // A well-formed, fully-authenticated-looking request. If the switch
        // were absent this would proceed to the store.
        let response = handle_register(
            State(state),
            cleared,
            extensions,
            headers(Some("Bearer an-initial-access-token")),
            axum::body::Bytes::from_static(
                br#"{"client_name":"x","redirect_uris":["https://connector.test/cb"]}"#,
            ),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "a disabled registration endpoint must refuse"
        );
        let recorded = recent_records()
            .into_iter()
            .filter(|r| r.source_address().as_deref() == Some("203.0.113.19"))
            .find(|r| r.event_kind() == OauthEvent::RegistrationDenied)
            .expect("the refusal was not audited");
        assert_eq!(recorded.endpoint_of(), Some(OauthEndpoint::Register));
        assert_eq!(
            recorded.denial_reason(),
            Some(DenialReason::RegistrationNotPermitted)
        );
    }

    /// An enabled endpoint with NO initial access token refuses, audits, and
    /// never reaches the store — the acceptance criterion that DCR is never an
    /// unauthenticated write.
    #[tokio::test]
    async fn an_unauthenticated_registration_is_refused() {
        use crate::oauth::audit::{record_text, recent_records};

        let source = "203.0.113.21".parse::<std::net::IpAddr>().expect("literal");
        let limiter = Arc::new(crate::oauth::limits::OauthRateLimiter::with_defaults());
        let state = Registration {
            service: ClientService::new(crate::oauth::store::OauthStore::from_pool(lazy_pool())),
            dcr_enabled: true,
        };
        let cleared = limiter
            .check_address(OauthEndpoint::Register, source)
            .await
            .expect("under budget");
        let mut extensions = axum::http::Extensions::new();
        extensions.insert(crate::oauth::edge::ResolvedClientIp(source));

        let response = handle_register(
            State(state),
            cleared,
            extensions,
            headers(None),
            axum::body::Bytes::from_static(
                br#"{"client_name":"x","redirect_uris":["https://connector.test/cb"]}"#,
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response.headers().contains_key(axum::http::header::WWW_AUTHENTICATE),
            "the refusal must say what it wanted"
        );
        let recorded = recent_records()
            .into_iter()
            .filter(|r| r.source_address().as_deref() == Some("203.0.113.21"))
            .last()
            .expect("the refusal was not audited");
        assert_eq!(
            recorded.denial_reason(),
            Some(DenialReason::RegistrationNotPermitted)
        );
        // Nothing from the request reaches the record.
        for text in record_text(&recorded) {
            assert!(!text.contains("connector.test"));
        }
    }

    /// A pool that is never connected.
    ///
    /// `connect_lazy_with` performs no I/O and touches no filesystem path — the
    /// database is only opened when a query runs, and no test here runs one,
    /// which is itself the assertion: every refusal below happens BEFORE the
    /// store is consulted.
    ///
    /// S132/RMCP-SQLITE also removed a small hazard from this fixture. Its
    /// Postgres predecessor needed an invented DSN carrying a user and a host,
    /// which had to be spelled carefully — a DOTLESS host — because the repo's
    /// own `no_pii_in_own_source_tree` scanner reads that shape as an email
    /// address. Writing the shape out HERE trips the same scanner, which is
    /// worth knowing and is why this sentence describes it rather than quoting
    /// it. An in-memory SQLite handle names no user and no host at all.
    fn lazy_pool() -> sqlx::SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy_with(sqlx::sqlite::SqliteConnectOptions::new().in_memory(true))
    }

    /// Stands in for `mount`'s shared charge layer, so the router test exercises
    /// the handler's `AddressCleared` extractor the way the mounted door does.
    async fn charge_for_test(
        State(limiter): State<Arc<crate::oauth::limits::OauthRateLimiter>>,
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> Response {
        let source = crate::oauth::authorize::resolved_source_for(req.extensions());
        match limiter.check_address(OauthEndpoint::Register, source).await {
            Err(outcome) => crate::oauth::limits::throttled_response(&outcome),
            Ok(cleared) => {
                let mut req = req;
                req.extensions_mut().insert(cleared);
                next.run(req).await
            }
        }
    }
}
