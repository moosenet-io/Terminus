//! RMCP-05 — the resource-server half of the OAuth door: turn a bearer token
//! into a [`Principal`] the existing gateway already knows how to authorize.
//!
//! ## What this module is responsible for, and what it deliberately is not
//! This is the only place in the crate that decides whether a bearer token
//! authenticates anybody. It answers exactly one question — *whose* request is
//! this — and hands the answer to [`crate::gateway_framework`], which answers
//! the separate question of what that principal may do. The new door changes
//! how a caller proves who they are; it introduces no second way to decide
//! what they may do, and in particular it mints no entitlements: a
//! [`crate::tool::CallerContext`] is still constructible only inside
//! `crate::gateway_framework`, from the resolved principal's grants (see that
//! module's `caller_context.rs` for why that boundary is compiler-checked
//! rather than conventional).
//!
//! ## Why the audience check is the load-bearing one
//! Signature verification proves a token was minted by a holder of the signing
//! key. In a federated fleet that is not the same as proving the token was
//! minted *for this server*: peers can share an issuer, a key can be reused
//! across deployments by an operator who did not think about it, and a token
//! handed to a peer is a token that peer can replay. So the check that
//! actually stops cross-server replay is `aud == this server's canonical
//! resource` — a token whose audience is a federated peer is REJECTED here
//! even though its signature verifies perfectly. RFC 8707 exists so that
//! audience is pinned at issuance; this module is the half that makes pinning
//! mean something.
//!
//! For the same reason a MULTI-valued `aud` is refused rather than searched for
//! a match. A token that is valid at two audiences is replayable at the second
//! by whoever holds it at the first, which is precisely the property
//! audience-binding exists to remove. That refusal is now STRUCTURAL rather
//! than a check: [`crate::oauth::jwt::AccessClaims`] types `aud` as a single
//! `String`, so an array fails to deserialize and never reaches a comparison at
//! all — a property that cannot be forgotten by a later edit.
//!
//! ## One verifier, not two
//! The cryptography here is RMCP-04's ([`crate::oauth::jwt::JwtSigner`]) — the
//! same code that MINTS these tokens verifies them. Until review round 3 this
//! module carried its own parallel implementation: its own key loading, its own
//! `Validation`, its own minimum key length, and its own env var for the
//! audience. That last divergence was not hypothetical damage — the minting
//! side reads `RMCP_OAUTH_RESOURCE` and the copy here read
//! `RMCP_CANONICAL_RESOURCE`, so under the documented configuration every token
//! the fleet issued would have been refused here as wrong-audience, and the
//! door would have failed in exactly the silent way this item keeps having to
//! design against. Two implementations of one cryptographic contract do not
//! stay equal; these two were already unequal on the single value the whole
//! door turns on.
//!
//! ## Header only — a token in a URL is a leaked token
//! [`BEARER_QUERY_PARAM`] in a query string is rejected and audited BEFORE the
//! Authorization header is even looked at, and regardless of whether the token
//! itself would have validated. The MCP specification prohibits it, and the
//! reason is operational rather than cryptographic: URLs are written to access
//! logs, forwarded in `Referer`, cached by proxies, and kept in shell history.
//! A credential that has been through any of those is compromised whether or
//! not this particular request would have succeeded, so accepting it "just
//! this once" would be accepting a credential already known to have leaked —
//! and rejecting it silently would deny the operator the one signal that it
//! happened.
//!
//! ## Live state, not just a signature
//! An access token is a bearer credential with a fixed lifetime, so anything
//! checked only at issuance stays true until the token expires. Consent
//! revocation and client disablement are exactly the things an operator
//! reaches for when something is wrong, and "it takes effect within fifteen
//! minutes" is not what they mean by revoke. So every request re-reads the
//! client row, the account row, and the consent row: revoking consent or
//! disabling a client denies the NEXT call, not the next token refresh. The
//! cost is three indexed primary-key reads on the dispatch path, which is the
//! right trade for a revocation that actually revokes.
//!
//! ## Fail-closed, including when the store is unreachable
//! A database that cannot be read cannot confirm that consent is live, so it
//! resolves to [`ResourceRejection::Unavailable`] and a `503` — never to "we
//! could not check, so proceed". That is the one rejection that deliberately
//! omits the `WWW-Authenticate` challenge: a challenge tells the client its
//! credential is bad and to go get another one, which would send the user
//! through a whole re-authorization for what is a server-side outage that a
//! retry will fix.
//!
//! ## Secret access (S7/S8)
//! The signing key is read from the process environment, which in this crate
//! IS the vault read — see [`crate::oauth`]'s module doc for the full
//! rationale. It is never logged, never interpolated into an error, and never
//! returned; [`ResourceServerConfig`] does not derive `Debug` for that reason.

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use uuid::Uuid;

use crate::error::ToolError;
use crate::gateway_framework::audit::{AuditEntry, AuditResult};
use crate::gateway_framework::{ActionKind, ANONYMOUS_IDENTITY};
use crate::mesh::{AuthError, OauthAccount, Principal, PrincipalResolver};
use crate::oauth::jwt::{AccessClaims, JwtSigner, VerifyFailure};
use crate::oauth::store::OauthStore;

/// The canonical resource identifier this server answers to — the connector
/// URL exactly as the user types it into the client, and the value a token's
/// `aud` must equal.
///
/// Re-exported from [`crate::oauth::authorize`] rather than defined again.
/// RMCP-03 binds this value to the authorization code and RMCP-04 mints it as
/// the token's audience; if this module read a DIFFERENT variable, every token
/// the fleet issued would be rejected here as wrong-audience, and the door
/// would fail in exactly the silent way this item keeps having to design
/// against. One name, one meaning, one place it is defined.
pub use crate::oauth::authorize::RESOURCE_ENV as CANONICAL_RESOURCE_ENV;

/// The switch that opens the door.
///
/// An explicit flag, not "configured means enabled". This door is the only one
/// reachable from the public internet, so which deployments have it open must
/// be a sentence an operator wrote, not a side effect of a variable being
/// present in an environment file that got copied between hosts. It mirrors
/// `TERMINUS_MESH_ENABLED`, the fleet's existing convention for a feature that
/// changes what the process exposes.
pub const ENABLED_ENV: &str = "RMCP_OAUTH_ENABLED";

/// The query parameter RFC 6750 defines for URI-borne access tokens, and which
/// this server refuses. Named as a constant so the refusal and its test cannot
/// drift apart.
pub const BEARER_QUERY_PARAM: &str = "access_token";

/// The path segment RFC 9728 reserves for the protected-resource metadata
/// document, inserted between authority and path to build the challenge's
/// `resource_metadata` URL.
const PROTECTED_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

/// Hard ceiling on the Authorization header before any parsing happens.
///
/// The parse below is bounded and allocation-free either way; this exists so
/// an oversized header is refused by a length comparison rather than by
/// whatever the first parsing step would have done with it. Comfortably above
/// any real HS256 JWT.
const MAX_AUTHORIZATION_HEADER_BYTES: usize = 8 * 1024;

/// Ceiling on the credential itself, after the scheme is stripped.
const MAX_TOKEN_BYTES: usize = 4 * 1024;

/// Ceiling on the individual claim strings this module carries forward
/// (`sub`, `client_id`, `scope`). They end up in a principal-map lookup and in
/// audit records, so they are bounded before either.
const MAX_CLAIM_BYTES: usize = 512;

/// Why a request failed resource-server validation.
///
/// Deliberately field-less. Every variant's operator-facing text and
/// client-facing description are fixed strings chosen here, so there is no
/// path by which a value from the request — a claim, a header, a client id —
/// reaches a response header or the audit log through this type. That closes
/// header injection into the quoted `WWW-Authenticate` parameters by
/// construction rather than by remembering to escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRejection {
    /// No `Authorization` header at all. Per RFC 6750 this gets a challenge
    /// with NO `error` parameter — nothing was presented, so nothing was
    /// invalid, and Claude reads the bare challenge as "begin discovery".
    NoCredential,
    /// An `Authorization` header that is not a well-formed single Bearer
    /// credential: duplicated, oversized, non-ASCII, wrong scheme, or not
    /// shaped like a JWT.
    MalformedAuthorization,
    /// A token was presented in the query string. Refused unconditionally.
    TokenInQueryString,
    /// Signature, issuer, `nbf`, or claim shape failed. One variant on
    /// purpose: distinguishing "bad signature" from "wrong issuer" on the wire
    /// tells a prober how far their forgery got.
    InvalidToken,
    /// The signature verified but the token has expired. Kept distinct because
    /// it is the one token failure that is both safe to disclose and useful:
    /// it is how a client knows to refresh rather than to re-authorize.
    ExpiredToken,
    /// The signature verified but `aud` is not this server. The headline
    /// rejection — see this module's doc.
    WrongAudience,
    /// The token names a client that no longer exists or has been disabled.
    UnknownClient,
    /// The token names an account that no longer exists or has been disabled.
    InactiveAccount,
    /// The account has revoked its consent for this client.
    ConsentRevoked,
    /// EVERY session for this (account, client) pair has been revoked or has
    /// expired, while the presented token itself is still valid and unexpired.
    ///
    /// Deliberately not named `SessionRevoked`: this server cannot tell which
    /// session a token belongs to (TERM #635), so it can only observe that the
    /// pair has none left. Revoking one of several sessions produces no
    /// rejection at all.
    AllSessionsRevoked,
    /// The account authenticated, but no `oauth_account` entry maps it to a
    /// canonical principal. Fail-closed exactly as an unmapped mTLS CN is —
    /// the OAuth door is not a way around the principal map.
    UnmappedAccount,
    /// The store could not be read, so live state could not be confirmed.
    Unavailable,
}

impl ResourceRejection {
    /// The HTTP status this rejection produces.
    pub fn status(self) -> StatusCode {
        match self {
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::UNAUTHORIZED,
        }
    }

    /// The RFC 6750 `error` code, or `None` when no credential was presented
    /// (in which case the challenge carries no `error` parameter at all).
    fn error_code(self) -> Option<&'static str> {
        match self {
            Self::NoCredential | Self::Unavailable => None,
            Self::MalformedAuthorization | Self::TokenInQueryString => Some("invalid_request"),
            _ => Some("invalid_token"),
        }
    }

    /// The client-facing description. Coarse on purpose for everything that
    /// depends on stored state: "no such client", "account disabled" and
    /// "consent revoked" share one description so that a caller holding a
    /// token cannot use the response to enumerate which of the three is true.
    /// The operator gets the precise reason in the audit log instead.
    fn description(self) -> &'static str {
        match self {
            Self::NoCredential => "authentication required",
            Self::MalformedAuthorization => "malformed Authorization header",
            Self::TokenInQueryString => "the access token must be sent in the Authorization header",
            Self::InvalidToken | Self::WrongAudience => "the access token is not valid for this resource",
            Self::ExpiredToken => "the access token has expired",
            Self::UnknownClient
            | Self::InactiveAccount
            | Self::ConsentRevoked
            | Self::AllSessionsRevoked
            | Self::UnmappedAccount => "the access token is no longer accepted",
            Self::Unavailable => "authorization state is temporarily unavailable",
        }
    }

    /// The precise reason, for the audit log only. This is where the
    /// distinctions the wire deliberately blurs are recorded.
    fn audit_reason(self) -> &'static str {
        match self {
            Self::NoCredential => "no Authorization header presented",
            Self::MalformedAuthorization => "malformed or oversized Authorization header",
            Self::TokenInQueryString => "access token presented in the query string (prohibited)",
            Self::InvalidToken => "token signature, issuer, nbf or claim shape rejected",
            Self::ExpiredToken => "token expired",
            Self::WrongAudience => "token audience is not this server's canonical resource",
            Self::UnknownClient => "token names an unknown or disabled client",
            Self::InactiveAccount => "token names an unknown or disabled account",
            Self::ConsentRevoked => "consent for this client has been revoked",
            Self::AllSessionsRevoked => "every session for this account and client has been revoked",
            Self::UnmappedAccount => "account has no oauth_account entry in the principal map",
            Self::Unavailable => "OAuth store unreachable; live state could not be confirmed",
        }
    }

    /// Whether this rejection should carry a `WWW-Authenticate` challenge.
    ///
    /// Everything except [`Self::Unavailable`]: a challenge is an instruction
    /// to go and get a new credential, and a store outage is not a reason to
    /// send the user through re-authorization for a token that is fine.
    fn carries_challenge(self) -> bool {
        !matches!(self, Self::Unavailable)
    }
}

/// The validated identity of one OAuth-authenticated request.
///
/// The principal and the `client_id` are two SEPARATE values, and that
/// separation is the point. The principal is who the caller is — the same
/// canonical name an mTLS or tailnet caller resolves to, feeding the same
/// grant lookup. The `client_id` is which connector they came through, which
/// is the second axis RMCP-07 intersects the grant against. Folding the client
/// into the principal name would make one person several principals and turn
/// an intersection into a widening; keeping them apart means a connector can
/// only ever narrow what its account could already do.
#[derive(Debug, Clone)]
pub struct OauthCaller {
    principal: Principal,
    client_id: String,
    account: String,
    scope: String,
}

impl OauthCaller {
    /// The resolved caller — authorize with this exactly as with any other.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Take ownership of the principal for the request path.
    pub fn into_principal(self) -> Principal {
        self.principal
    }

    /// The OAuth `client_id` this token was issued to. NOT part of the
    /// caller's identity — see the type doc.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The account name the token's `sub` carried, for audit.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// The space-delimited scope string the token carries.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// TEST-ONLY: build a caller without standing up a signing key, a resolver
    /// and a store, so `crate::mcp_server` can assert what the request path
    /// does WITH one. `cfg(test)` only — it exists in no shipped binary, so it
    /// cannot become a way to mint an authenticated caller in production.
    #[cfg(test)]
    pub(crate) fn for_test(
        principal: Principal,
        client_id: impl Into<String>,
        account: impl Into<String>,
    ) -> Self {
        Self {
            principal,
            client_id: client_id.into(),
            account: account.into(),
            scope: String::new(),
        }
    }
}

/// The live rows a validated token still depends on.
///
/// A trait rather than a direct [`OauthStore`] call so the ORDER and the
/// FAIL-CLOSED direction of these three checks are testable without a
/// database — this crate stands up no Postgres in tests, and "revoked consent
/// denies the next call" is exactly the kind of property that must not be
/// asserted only by reading the code.
#[async_trait::async_trait]
pub trait TokenState: Send + Sync {
    /// The row id of the client, or `None` when it is unknown OR disabled.
    async fn active_client_row(&self, client_id: &str) -> Result<Option<Uuid>, ToolError>;
    /// The NAME of the account, or `None` when it is unknown OR disabled.
    ///
    /// Keyed by the row id because that is what RMCP-04 puts in `sub`, and it
    /// returns the name because that is what the operator-authored
    /// `oauth_account` principal map is keyed on. One query does both jobs, and
    /// — the part that matters — the name used for authorization comes from
    /// the DATABASE on every request rather than from the token, so renaming or
    /// disabling an account cannot be outlived by a credential that still
    /// carries the old value.
    async fn active_account_name(&self, account_id: Uuid) -> Result<Option<String>, ToolError>;
    /// Whether this account still consents to this client at all.
    async fn consent_is_live(&self, account_row: Uuid, client_row: Uuid) -> Result<bool, ToolError>;
    /// Whether the (account, client) pair has ANY live session.
    ///
    /// Named for what it asks. It is NOT "is the session behind this token
    /// live" — nothing in an access token identifies a session (see
    /// `OauthStore::any_session_is_live`, and TERM #635). So it catches
    /// revoking EVERY session for a pair and does NOT catch revoking one of
    /// several: a token from the revoked one keeps working until it expires.
    ///
    /// Still distinct from [`Self::consent_is_live`], and both are needed.
    /// Consent is the standing permission for a connector; a session is one
    /// issuance of it. Revoking every session leaves consent intact, so a check
    /// that consulted only consent would admit a pair whose sessions are all
    /// gone.
    async fn any_session_is_live(&self, account_row: Uuid, client_row: Uuid) -> Result<bool, ToolError>;
}

#[async_trait::async_trait]
impl TokenState for OauthStore {
    async fn active_client_row(&self, client_id: &str) -> Result<Option<Uuid>, ToolError> {
        Ok(self.find_active_client(client_id).await?.map(|c| c.id))
    }

    async fn active_account_name(&self, account_id: Uuid) -> Result<Option<String>, ToolError> {
        Ok(self.find_active_account_by_id(account_id).await?.map(|a| a.name))
    }

    async fn consent_is_live(&self, account_row: Uuid, client_row: Uuid) -> Result<bool, ToolError> {
        self.has_live_consent(account_row, client_row).await
    }

    async fn any_session_is_live(&self, account_row: Uuid, client_row: Uuid) -> Result<bool, ToolError> {
        // See `OauthStore::any_session_is_live` for the exact guarantee and the
        // per-session gap it does not close.
        OauthStore::any_session_is_live(self, account_row, client_row).await
    }
}

/// Non-secret-shaped configuration plus the verification keys.
///
/// No `Debug`: it holds HMAC key material, and a stray `{:?}` in a log line is
/// how that leaks. Same reasoning as [`crate::oauth::OauthConfig`].
pub struct ResourceServerConfig {
    canonical_resource: String,
    /// RMCP-04's signer, used here ONLY to verify.
    ///
    /// Not a second verifier built from the same secrets, which is what this
    /// module carried until review round 3. That copy loaded its own keys, ran
    /// its own `Validation`, enforced its own minimum key length — and read a
    /// DIFFERENT env var for the audience than the one RMCP-03 binds to the
    /// authorization code and RMCP-04 mints into the token. Two
    /// implementations of one cryptographic contract do not stay equal; these
    /// two were already unequal on the single value the whole door turns on,
    /// so every token the fleet issued would have been rejected here as
    /// wrong-audience. One verifier, one place.
    signer: JwtSigner,
}

impl ResourceServerConfig {
    /// Build and validate a configuration.
    ///
    /// The canonical resource is validated here and the signing material by
    /// [`JwtSigner`], each in the one place that owns it. Every failure is a
    /// hard error rather than a degraded default — the opposite of
    /// `AllowlistPolicy::from_env`'s convention, and for the documented
    /// reason: that type degrades because it guards a per-request decision
    /// that must never panic a running process, whereas this one is built once
    /// at startup, and a resource server running with the wrong audience is
    /// worse than one that refused to start. A canonical-resource typo in
    /// particular produces a failure the user experiences as "the connector
    /// just does not work", with nothing in any log pointing at the cause.
    pub fn new(canonical_resource: &str, signer: JwtSigner) -> Result<Self, ToolError> {
        Ok(Self {
            canonical_resource: validate_resource_uri(canonical_resource, CANONICAL_RESOURCE_ENV)?,
            signer,
        })
    }

    /// Read the configuration from the environment (the vault read, S7/S8 —
    /// see this module's doc).
    ///
    /// The signing key, the issuer, the previous-key rotation window and the
    /// clock-skew leeway are all read by [`JwtSigner::from_env`]. This function
    /// deliberately reads exactly ONE variable of its own: the resource, and
    /// even that name is RMCP-03's.
    pub fn from_env() -> Result<Self, ToolError> {
        let resource = env_nonempty(CANONICAL_RESOURCE_ENV)
            .ok_or_else(|| not_set(CANONICAL_RESOURCE_ENV))?;
        Self::new(&resource, JwtSigner::from_env()?)
    }

    /// The canonical resource a token's `aud` must equal.
    pub fn canonical_resource(&self) -> &str {
        &self.canonical_resource
    }

    /// The RFC 9728 protected-resource metadata URL for
    /// [`Self::canonical_resource`] — the path-suffixed form, which is the one
    /// Claude probes first.
    ///
    /// Derived rather than separately configured: it must agree with the
    /// canonical resource for discovery to work at all, and two env vars that
    /// must agree are two env vars that will eventually disagree. RMCP-02
    /// serves the document at exactly this URL.
    pub fn resource_metadata_url(&self) -> String {
        let rest = self
            .canonical_resource
            .strip_prefix("https://")
            .unwrap_or(&self.canonical_resource);
        match rest.find('/') {
            Some(slash) => {
                let (authority, path) = rest.split_at(slash);
                format!("https://{authority}{PROTECTED_RESOURCE_METADATA_PATH}{path}")
            }
            None => format!("https://{rest}{PROTECTED_RESOURCE_METADATA_PATH}"),
        }
    }

    /// The `WWW-Authenticate` value for a rejection, or `None` when the
    /// rejection deliberately carries no challenge.
    ///
    /// Every interpolated piece is either a fixed string from
    /// [`ResourceRejection`] or a startup-validated URI with no quote or
    /// backslash in it, so the quoted parameters cannot be broken out of.
    pub fn challenge(&self, rejection: ResourceRejection) -> Option<String> {
        if !rejection.carries_challenge() {
            return None;
        }
        let metadata = self.resource_metadata_url();
        Some(match rejection.error_code() {
            Some(code) => format!(
                "Bearer error=\"{code}\", error_description=\"{}\", resource_metadata=\"{metadata}\"",
                rejection.description()
            ),
            None => format!("Bearer resource_metadata=\"{metadata}\""),
        })
    }

    /// Verify a raw token string, delegating to RMCP-04's [`JwtSigner`].
    ///
    /// Signature (HS256, pinned by construction so `alg: none` and RS/HS
    /// confusion are non-issues), `iss`, `nbf`/`exp` with the configured
    /// leeway, key rotation, and the RFC 8707 audience binding are all that
    /// verifier's job — this function adds only what a RESOURCE server needs
    /// on top of a valid token, and translates the outcome into the vocabulary
    /// this door answers in.
    ///
    /// ## What happened to the multi-audience refusal
    /// The previous revision parsed `aud` as string-or-array so it could refuse
    /// a token naming several audiences (replayable at the second by whoever
    /// holds it at the first). [`AccessClaims`] types `aud` as a plain
    /// `String`, so an array now fails to DESERIALIZE and the token is rejected
    /// before any check runs. The property is stronger for being structural —
    /// it cannot be forgotten — and it costs only the ability to report that
    /// particular shape distinctly, which no caller acted on.
    ///
    /// The byte-equality check below is still worth its two lines: `jsonwebtoken`
    /// matches an audience by SET MEMBERSHIP, so it is the exact-match
    /// guarantee, and it is the assertion that fails first if a future change
    /// widens `set_audience` to several values.
    pub fn verify_token(&self, token: &str) -> Result<AccessClaims, ResourceRejection> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(ResourceRejection::MalformedAuthorization);
        }

        let claims = self
            .signer
            .verify_with_reason(token, &self.canonical_resource)
            .map_err(|failure| match failure {
                VerifyFailure::Expired => ResourceRejection::ExpiredToken,
                VerifyFailure::Audience => ResourceRejection::WrongAudience,
                VerifyFailure::Invalid => ResourceRejection::InvalidToken,
            })?;

        if claims.aud != self.canonical_resource {
            return Err(ResourceRejection::WrongAudience);
        }
        if !bounded_claim(&claims.sub)
            || !bounded_claim(&claims.client_id)
            || claims.scope.len() > MAX_CLAIM_BYTES
        {
            return Err(ResourceRejection::InvalidToken);
        }

        Ok(claims)
    }

    /// The whole resource-server decision for one request.
    ///
    /// Order is load-bearing and is asserted by tests:
    /// 1. A query-string token is refused FIRST, so it is refused even when it
    ///    would otherwise have validated — see this module's doc.
    /// 2. The Authorization header is parsed under a length bound.
    /// 3. The token is verified, audience included.
    /// 4. The client, the account and the consent are re-read LIVE.
    /// 5. The account is mapped to a canonical principal, fail-closed.
    ///
    /// Every rejection is audited here, in one place, so there is no path by
    /// which a refused request is silently dropped.
    pub async fn authenticate(
        &self,
        state: &dyn TokenState,
        resolver: &PrincipalResolver,
        headers: &HeaderMap,
        query: Option<&str>,
    ) -> Result<OauthCaller, ResourceRejection> {
        match self.authenticate_inner(state, resolver, headers, query).await {
            Ok(caller) => Ok(caller),
            Err(rejection) => {
                audit_rejection(rejection);
                Err(rejection)
            }
        }
    }

    async fn authenticate_inner(
        &self,
        state: &dyn TokenState,
        resolver: &PrincipalResolver,
        headers: &HeaderMap,
        query: Option<&str>,
    ) -> Result<OauthCaller, ResourceRejection> {
        if query_carries_token(query) {
            return Err(ResourceRejection::TokenInQueryString);
        }

        let token = extract_bearer(headers)?;
        let claims = self.verify_token(token)?;

        // RMCP-04 puts the account's UUID in `sub`. A token whose `sub` is not
        // a UUID was not minted by this fleet's token endpoint, whatever else
        // survived verification, so it is refused rather than looked up.
        let account_id = Uuid::parse_str(claims.sub.trim())
            .map_err(|_| ResourceRejection::InvalidToken)?;

        let client_row = state
            .active_client_row(&claims.client_id)
            .await
            .map_err(|_| ResourceRejection::Unavailable)?
            .ok_or(ResourceRejection::UnknownClient)?;
        // The NAME comes back from the database, never from the token — see
        // `TokenState::active_account_name`. A renamed or disabled account is
        // therefore not something a still-valid credential can outlive.
        let account_name = state
            .active_account_name(account_id)
            .await
            .map_err(|_| ResourceRejection::Unavailable)?
            .ok_or(ResourceRejection::InactiveAccount)?;
        if !state
            .consent_is_live(account_id, client_row)
            .await
            .map_err(|_| ResourceRejection::Unavailable)?
        {
            return Err(ResourceRejection::ConsentRevoked);
        }
        // A signature is a point-in-time authority; revocation happens after it
        // was issued, so session state is re-derived on the READ path rather
        // than trusted from the token. This catches the pair losing ALL of its
        // sessions; it does not catch one of several being revoked, because no
        // claim identifies a session (TERM #635).
        if !state
            .any_session_is_live(account_id, client_row)
            .await
            .map_err(|_| ResourceRejection::Unavailable)?
        {
            return Err(ResourceRejection::AllSessionsRevoked);
        }

        let account = OauthAccount(account_name.clone());
        // No cert and no tailnet identity is passed, and that is not a
        // simplification: this branch is only ever reached for a request that
        // presented neither (see `crate::mcp_server`), and the resolver
        // enforces the same precedence independently.
        let principal = resolver
            .resolve_with_oauth(None, None, Some(&account))
            .map_err(|e| match e {
                AuthError::UnmappedIdentity(_) => ResourceRejection::UnmappedAccount,
                _ => ResourceRejection::InvalidToken,
            })?;

        Ok(OauthCaller {
            principal,
            client_id: claims.client_id,
            account: account_name,
            scope: claims.scope,
        })
    }
}

/// A resource server bound to a live [`OauthStore`].
///
/// The split between this and [`ResourceServerConfig`] is what lets the whole
/// validation path be tested against a fake [`TokenState`] while production
/// uses the real store.
pub struct OauthResourceServer {
    config: ResourceServerConfig,
    /// The live-state source, held as the CAPABILITY rather than as the
    /// concrete [`OauthStore`].
    ///
    /// Not indirection for its own sake: it is what lets `crate::mcp_server`
    /// drive the whole request path — token in, principal and connector out —
    /// against a fake in a unit test. This crate stands up no Postgres in
    /// tests, so a concrete store here would have made the end-to-end
    /// behaviour of the new door assertable only by reading the code, which
    /// for an internet-facing authentication path is not good enough.
    state: Arc<dyn TokenState>,
    /// The SAME store the `state` above is, when this door was built the
    /// production way — kept as its concrete type so the one thing that needs
    /// more than [`TokenState`] can have it.
    ///
    /// TERM #631 item 5: RMCP-07's scope resolver needs an `OauthStore`, and
    /// the previous shape gave the store away to `state` and left no handle,
    /// so a process could only have had a resolver by opening a SECOND pool
    /// against the same database — two connection budgets, two failure modes,
    /// and two answers to "is the door up" for one door. Sharing the handle
    /// keeps that a single fact.
    ///
    /// `None` for a door built by [`Self::with_state`] over a fake, which has
    /// no store at all. Callers must read that absence as "no scope source" —
    /// which resolves to the EMPTY scope, never to an unscoped door. See
    /// [`crate::oauth::scope::scope_source_for_door`], the one place that
    /// reading is made.
    store: Option<Arc<OauthStore>>,
}

impl OauthResourceServer {
    /// The production constructor: live state comes from the OAuth store.
    ///
    /// Takes the store as an `Arc` so the handle can be SHARED rather than
    /// consumed — see the `store` field's doc for why that matters.
    pub fn new(config: ResourceServerConfig, store: Arc<OauthStore>) -> Self {
        Self {
            config,
            state: Arc::clone(&store) as Arc<dyn TokenState>,
            store: Some(store),
        }
    }

    /// This door's own store handle, if it has one.
    ///
    /// `None` for a door built over a fake [`TokenState`]. There is
    /// deliberately no fallback that opens a store here: a caller that needs a
    /// store and finds none must fail closed, not manufacture one.
    pub fn store(&self) -> Option<&Arc<OauthStore>> {
        self.store.as_ref()
    }

    /// Build one over any [`TokenState`]. Production uses [`Self::new`]; this
    /// exists so the request path can be exercised end to end without a
    /// database (see the field doc).
    pub fn with_state(config: ResourceServerConfig, state: Arc<dyn TokenState>) -> Self {
        Self { config, state, store: None }
    }

    pub fn config(&self) -> &ResourceServerConfig {
        &self.config
    }

    /// See [`ResourceServerConfig::authenticate`].
    pub async fn authenticate(
        &self,
        resolver: &PrincipalResolver,
        headers: &HeaderMap,
        query: Option<&str>,
    ) -> Result<OauthCaller, ResourceRejection> {
        self.config.authenticate(self.state.as_ref(), resolver, headers, query).await
    }
}

/// Whether the operator has switched the OAuth door on.
///
/// Deliberately strict about what counts as "on": the usual truthy spellings
/// and nothing else. A value the operator meant as an enable but this function
/// does not recognise (`"yes please"`, `"On "` with junk after it) returns
/// `false` — which is the safe direction for a switch that exposes a public
/// door, and is why [`resource_server_from_env`] logs at INFO when it decides
/// the door is closed rather than staying silent about it.
pub fn door_enabled(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// THE way an [`OauthResourceServer`] comes into existence in production.
///
/// `Ok(None)` — the door is switched off; the process runs exactly as it did
/// before this item and no bearer token is ever inspected.
/// `Ok(Some(..))` — the door is on and fully constructed.
/// `Err(..)` — the door is ON and could not be built.
///
/// ## Why an enabled-but-broken door is a hard error and never a quiet `None`
/// This is the failure this function exists to make impossible. A door that is
/// configured, believed to be open, and silently closed does not present as an
/// error anywhere: the operator sees a connector that will not link, with no
/// log line, no failed request, and nothing to grep for — because the code
/// that would have produced the error is never reached. That is strictly worse
/// than not starting, and it is the same shape of bug this item shipped in its
/// own first revision (a validator that was never constructed).
///
/// So once [`ENABLED_ENV`] is set, EVERY remaining failure — a malformed
/// canonical resource, an absent signing key, an unreachable OAuth database —
/// is returned as an `Err` for the binary's `main` to abort on. The cost is
/// real and worth naming: it couples the whole gateway's startup to Postgres
/// for deployments that turn this on. That coupling is the operator's own
/// choice, made by setting one variable, and the alternative is a gateway that
/// starts healthy while its authentication door is dead.
///
/// The mirror-image rule is just as deliberate: with the flag OFF, nothing
/// here is read at all, so a half-configured host that has not opted in cannot
/// be prevented from starting by this item.
///
/// Callers: `terminus_primary`'s `main` (hard-fails on `Err`).
/// `crate::pki::server::build_gateway_router` deliberately does NOT call this
/// — it takes an already-built value, keeping its documented "a library
/// function does not abort someone's `main`" policy intact while the abort
/// happens where a process is allowed to abort.
pub async fn resource_server_from_env() -> Result<Option<Arc<OauthResourceServer>>, ToolError> {
    if !door_enabled(env_nonempty(ENABLED_ENV).as_deref()) {
        tracing::info!(
            "oauth: RMCP connector door is CLOSED ({ENABLED_ENV} not set) — no bearer token \
             will be validated on /mcp"
        );
        return Ok(None);
    }

    // From here on every failure is fatal, by design: the operator has said
    // this door should be open, so "open" is the only acceptable outcome.
    let config = ResourceServerConfig::from_env()?;
    let store = OauthStore::connect(&crate::oauth::OauthConfig::from_env()?).await?;
    if !store.schema_ready().await {
        return Err(ToolError::NotConfigured(
            "the RMCP OAuth schema is not present — apply the S132 migration before enabling \
             the connector door, or the door would accept tokens it cannot check consent for"
                .into(),
        ));
    }

    tracing::info!(
        "oauth: RMCP connector door is OPEN for resource {}",
        config.canonical_resource()
    );
    Ok(Some(Arc::new(OauthResourceServer::new(
        config,
        Arc::new(store),
    ))))
}

/// RMCP-05: refuse — and audit — an access token carried in the URL.
///
/// This is a gate for EVERY request, not just OAuth-authenticated ones, and
/// that is the whole point of it living outside [`ResourceServerConfig`]. The
/// harm is not "this request authenticated wrongly"; it is that the credential
/// has already been written to an access log, a `Referer`, a proxy cache and a
/// shell history by the time it arrives. That is equally true of a request
/// that presented a client certificate, and equally true of one that would
/// have been refused anyway — so the check cannot be a branch of the OAuth
/// path, or it silently stops applying to exactly the requests that took a
/// different door.
///
/// An earlier revision of this item had it inside the OAuth branch and
/// therefore skipped it whenever a cert or tailnet identity was present.
/// Review round 1 caught that. It is now unconditional, and it runs BEFORE any
/// identity source is selected.
pub fn refuse_query_string_token(query: Option<&str>) -> Result<(), ResourceRejection> {
    if query_carries_token(query) {
        audit_rejection(ResourceRejection::TokenInQueryString);
        return Err(ResourceRejection::TokenInQueryString);
    }
    Ok(())
}

/// Whether the query string carries an access token.
///
/// Bounded, allocation-free, and deliberately does not care whether the value
/// is well-formed or even present: `?access_token` with no value is still a
/// caller that put a credential parameter in a URL, and the point of the
/// refusal is the URL, not the token.
fn query_carries_token(query: Option<&str>) -> bool {
    let Some(query) = query else { return false };
    query.split('&').any(|pair| {
        let key = pair.split('=').next().unwrap_or(pair);
        key == BEARER_QUERY_PARAM
    })
}

/// THE definition of "this request carries a Bearer credential", used by every
/// code path that needs to ask.
///
/// There must be exactly one of these. Review round 1 found the seam that
/// appears when there are two: this module matched the scheme
/// case-insensitively (RFC 7235 says the scheme is case-insensitive) while
/// `crate::mcp_server`'s candidate check used a case-SENSITIVE
/// `strip_prefix("Bearer ")`, so a `bearer …` header was a credential to one
/// function and not to the other. A disagreement about what counts as
/// authentication is precisely what an attacker probes for, and the fix is not
/// to make the second copy match — it is to delete the second copy.
///
/// `None` means "not a single well-formed Bearer credential": absent,
/// duplicated, oversized, non-ASCII, or another scheme. Duplicates are refused
/// rather than resolved to the first, because which one an intermediary
/// forwards is unspecified — accepting one means validating a credential that
/// may not be the one the next hop sees.
///
/// The returned value is the raw credential; it is NOT yet known to be a JWT
/// (see [`extract_bearer`], which adds that).
pub fn bearer_credential(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all("authorization").iter();
    let raw = values.next()?;
    if values.next().is_some() {
        return None;
    }
    if raw.len() > MAX_AUTHORIZATION_HEADER_BYTES {
        return None;
    }
    let (scheme, rest) = raw.to_str().ok()?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(rest.trim())
}

/// Whether an `Authorization` header is present at all, whatever its shape.
///
/// Distinct from [`bearer_credential`] because "nothing was presented" and
/// "something was presented and it is not usable" must lead to different
/// answers: the first gets a bare challenge (begin discovery), the second an
/// `invalid_request`. Collapsing them would tell a client to re-authorize when
/// the real problem is a malformed header it will send again next time.
pub fn authorization_header_present(headers: &HeaderMap) -> bool {
    headers.get("authorization").is_some()
}

/// Pull the single Bearer credential out of the headers and require it to be
/// JWT-shaped. Bounded, with no panicking path.
fn extract_bearer(headers: &HeaderMap) -> Result<&str, ResourceRejection> {
    if !authorization_header_present(headers) {
        return Err(ResourceRejection::NoCredential);
    }
    let token = bearer_credential(headers).ok_or(ResourceRejection::MalformedAuthorization)?;
    if !is_jwt_shaped(token) {
        return Err(ResourceRejection::MalformedAuthorization);
    }
    Ok(token)
}

/// Whether `token` is shaped like a compact-serialization JWT: three non-empty
/// base64url segments and nothing else.
///
/// A cheap structural gate before the cryptographic one. Its real job is to
/// keep anything that is not a JWT — a stray header value, a pasted URL, a
/// credential from another system — out of the decoder and out of the audit
/// log's blast radius, with a bounded scan and no allocation.
fn is_jwt_shaped(token: &str) -> bool {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
        return false;
    }
    let mut segments = 0usize;
    for segment in token.split('.') {
        segments += 1;
        if segments > 3 || segment.is_empty() {
            return false;
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return false;
        }
    }
    segments == 3
}

/// A claim string that is present and within bounds.
fn bounded_claim(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_CLAIM_BYTES
}

/// Write the one audit record for a refused request.
///
/// Uses [`ANONYMOUS_IDENTITY`] because no principal was established — the
/// whole point of the refusal — matching how `crate::gateway_framework`
/// already audits a no-identity denial, so an operator reads one convention
/// rather than two.
fn audit_rejection(rejection: ResourceRejection) {
    let detail = format!("oauth: {}", rejection.audit_reason());
    AuditEntry::new(
        ANONYMOUS_IDENTITY,
        "oauth_bearer",
        ActionKind::Tool,
        AuditResult::DeniedNoIdentity,
        Some(detail.as_str()),
    )
    .log();
}

/// Validate an absolute HTTPS URI with no fragment and no trailing slash.
///
/// Not a general URI parser, and deliberately not: the only thing that matters
/// is that the value can be compared BYTE FOR BYTE against a token's `aud` and
/// against what the user typed into the client. Normalizing anything here
/// would make this server's idea of its own name differ from the issuer's,
/// which is the failure mode the strictness exists to prevent.
fn validate_resource_uri(raw: &str, var: &str) -> Result<String, ToolError> {
    let value = raw.trim();
    let bad = |why: &str| {
        ToolError::NotConfigured(format!(
            "{var} must be an absolute https:// URI with no fragment and no trailing slash ({why})"
        ))
    };
    let Some(rest) = value.strip_prefix("https://") else {
        return Err(bad("not https://"));
    };
    if rest.is_empty() || rest.starts_with('/') {
        return Err(bad("no host"));
    }
    if value.contains('#') {
        return Err(bad("contains a fragment"));
    }
    if value.contains('?') {
        return Err(bad("contains a query string"));
    }
    if value.ends_with('/') {
        return Err(bad("trailing slash"));
    }
    if value.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') {
        return Err(bad("contains whitespace or a quoting character"));
    }
    Ok(value.to_string())
}

fn not_set(var: &str) -> ToolError {
    ToolError::NotConfigured(format!("{var} not set — the RMCP resource server requires it"))
}

/// Read an env var, trimmed; `None` when unset or blank. Same convention as
/// [`crate::mesh::principal`]'s copy.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::PrincipalSource;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use serde_json::json;

    // pii-test-fixture: invented connector/issuer URIs and an invented HMAC
    // key, none of which name any real host or credential.
    const RESOURCE: &str = "https://connector.example.test/mcp"; // pii-test-fixture
    const ISSUER: &str = "https://connector.example.test"; // pii-test-fixture
    const KEY: &str = "test-signing-key-of-at-least-32-bytes"; // pii-test-fixture
    const OTHER_KEY: &str = "other-signing-key-of-at-least-32-bytes"; // pii-test-fixture

    /// The account UUID RMCP-04 puts in `sub`. Fixed rather than random so a
    /// failure names the same value every run.
    const ACCOUNT_ID: &str = "11111111-2222-3333-4444-555555555555";

    fn signer(previous: Option<&str>) -> JwtSigner {
        JwtSigner::new(KEY.into(), previous.map(str::to_string), ISSUER.into(), 900, 30)
            .expect("valid signer")
    }

    fn config() -> ResourceServerConfig {
        ResourceServerConfig::new(RESOURCE, signer(None)).expect("valid config")
    }

    fn resolver() -> PrincipalResolver {
        PrincipalResolver::new(
            serde_json::from_value(json!({"oauth_account": {"operator": "moose"}}))
                .expect("map fixture"),
        )
    }

    /// Mint a token the way RMCP-04 will, so the tests exercise the real
    /// verification path rather than a hand-built claims struct.
    fn mint(key: &str, overrides: serde_json::Value) -> String {
        let now = chrono::Utc::now().timestamp();
        let mut claims = json!({
            "iss": ISSUER,
            "sub": ACCOUNT_ID,
            "aud": RESOURCE,
            "client_id": "client-abc",
            "scope": "mcp offline_access",
            "jti": "jti-1",
            "iat": now,
            "nbf": now - 1,
            "exp": now + 900,
        });
        for (k, v) in overrides.as_object().expect("overrides object") {
            claims[k] = v.clone();
        }
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(key.as_bytes()),
        )
        .expect("mint")
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {token}").parse().expect("header value"),
        );
        headers
    }

    /// A [`TokenState`] whose three answers are set per test, so the live
    /// checks can be driven independently of a database.
    struct FakeState {
        client: Option<Uuid>,
        /// The live account NAME this id resolves to — `None` for unknown or
        /// disabled, matching the real store.
        account: Option<&'static str>,
        consent: bool,
        /// Whether the pair has ANY live session. `false` models EVERY session
        /// for the pair having been revoked or expired — not one of several.
        any_session: bool,
        fail: bool,
    }

    impl FakeState {
        fn healthy() -> Self {
            Self {
                client: Some(Uuid::nil()),
                account: Some("operator"),
                consent: true,
                any_session: true,
                fail: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl TokenState for FakeState {
        async fn active_client_row(&self, _client_id: &str) -> Result<Option<Uuid>, ToolError> {
            if self.fail {
                return Err(ToolError::Database("down".into()));
            }
            Ok(self.client)
        }
        async fn active_account_name(&self, _id: Uuid) -> Result<Option<String>, ToolError> {
            if self.fail {
                return Err(ToolError::Database("down".into()));
            }
            Ok(self.account.map(str::to_string))
        }
        async fn consent_is_live(&self, _a: Uuid, _c: Uuid) -> Result<bool, ToolError> {
            if self.fail {
                return Err(ToolError::Database("down".into()));
            }
            Ok(self.consent)
        }
        async fn any_session_is_live(&self, _a: Uuid, _c: Uuid) -> Result<bool, ToolError> {
            if self.fail {
                return Err(ToolError::Database("down".into()));
            }
            Ok(self.any_session)
        }
    }

    async fn authenticate(
        state: &FakeState,
        headers: &HeaderMap,
        query: Option<&str>,
    ) -> Result<OauthCaller, ResourceRejection> {
        config().authenticate(state, &resolver(), headers, query).await
    }

    // ── The happy path, so every negative below has a positive control ────

    #[tokio::test]
    async fn a_valid_token_resolves_to_the_mapped_principal_and_carries_its_client_separately() {
        let caller = authenticate(&FakeState::healthy(), &bearer(&mint(KEY, json!({}))), None)
            .await
            .expect("a well-formed, in-audience, consented token must authenticate");
        assert_eq!(caller.principal().name(), "moose");
        assert_eq!(caller.principal().source(), PrincipalSource::OAuth);
        assert_eq!(caller.account(), "operator");
        // The second axis: present, and NOT folded into the identity.
        assert_eq!(caller.client_id(), "client-abc");
        assert_ne!(caller.principal().name(), caller.client_id());
        assert!(!caller.principal().name().contains("client-abc"));
    }

    // ── The headline: audience binding ───────────────────────────────────

    /// A token minted by THIS server, with a valid signature and a live
    /// account, for a FEDERATED PEER. It must not be accepted here — this is
    /// the single property that stops a peer replaying its own tokens at us.
    #[tokio::test]
    async fn a_token_audienced_at_a_peer_is_rejected_despite_a_valid_signature() {
        let token = mint(KEY, json!({"aud": "https://peer.example.test/mcp"})); // pii-test-fixture
        // Positive control that the signature really is good: the same token
        // differs from the accepted one ONLY in `aud`.
        assert!(config().verify_token(&mint(KEY, json!({}))).is_ok());
        assert_eq!(
            config().verify_token(&token).expect_err("must be refused"),
            ResourceRejection::WrongAudience
        );
    }

    /// A token valid at two audiences is replayable at the second by whoever
    /// holds it at the first, so containing our name is not enough.
    ///
    /// Since the collapse onto RMCP-04's verifier this is enforced by the claim
    /// TYPE — `AccessClaims::aud` is a `String`, so any array fails to
    /// deserialize — which is why the rejection is `InvalidToken` rather than
    /// `WrongAudience`. That is a stronger guarantee than the check it
    /// replaced: it cannot be edited away without changing the shared claims
    /// struct, and it holds for every consumer of that struct, not just this
    /// one. The test asserts the OUTCOME (refused) rather than the mechanism.
    #[test]
    fn a_multi_audience_token_is_rejected_even_though_it_names_this_server() {
        let token = mint(KEY, json!({"aud": [RESOURCE, "https://peer.example.test/mcp"]})); // pii-test-fixture
        assert_eq!(
            config().verify_token(&token).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
        // A single-element array is refused for the same structural reason —
        // asserted so the behaviour is documented rather than discovered.
        assert!(config().verify_token(&mint(KEY, json!({"aud": [RESOURCE]}))).is_err());
        // POSITIVE CONTROL: the string form, which is what RMCP-04 mints, IS
        // accepted — so this test cannot pass by rejecting everything.
        assert!(config().verify_token(&mint(KEY, json!({}))).is_ok());
    }

    #[test]
    fn a_token_signed_by_an_unknown_key_is_rejected() {
        assert_eq!(
            config().verify_token(&mint(OTHER_KEY, json!({}))).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
    }

    #[test]
    fn a_token_from_a_foreign_issuer_is_rejected() {
        let token = mint(KEY, json!({"iss": "https://elsewhere.example.test"})); // pii-test-fixture
        assert_eq!(
            config().verify_token(&token).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
    }

    /// Expiry is reported distinctly, and it still carries a challenge — that
    /// is what makes Claude refresh reactively instead of stranding the user.
    #[test]
    fn an_expired_token_is_reported_as_expired_and_still_challenges() {
        let now = chrono::Utc::now().timestamp();
        let token = mint(KEY, json!({"exp": now - 3600, "nbf": now - 7200, "iat": now - 7200}));
        let rejection = config().verify_token(&token).expect_err("expired");
        assert_eq!(rejection, ResourceRejection::ExpiredToken);
        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);
        let challenge = config().challenge(rejection).expect("expiry must challenge");
        assert!(challenge.contains("error=\"invalid_token\""));
        assert!(challenge.contains("resource_metadata="));
    }

    #[test]
    fn a_not_yet_valid_token_is_rejected() {
        let now = chrono::Utc::now().timestamp();
        let token = mint(KEY, json!({"nbf": now + 3600, "iat": now + 3600, "exp": now + 7200}));
        assert_eq!(
            config().verify_token(&token).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
    }

    /// Rotation: the previous key verifies, the current key still mints.
    #[test]
    fn a_previous_signing_key_verifies_during_a_rotation_window() {
        let rotating = ResourceServerConfig::new(RESOURCE, signer(Some(OTHER_KEY)))
            .expect("valid config");
        assert!(rotating.verify_token(&mint(KEY, json!({}))).is_ok());
        assert!(rotating.verify_token(&mint(OTHER_KEY, json!({}))).is_ok());
        // And a third key is still nobody.
        let stranger = "stranger-signing-key-of-at-least-32-bytes"; // pii-test-fixture
        assert_eq!(
            rotating.verify_token(&mint(stranger, json!({}))).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
    }

    // ── Header only ──────────────────────────────────────────────────────

    /// The token in the query string is OTHERWISE VALID — the same token that
    /// authenticates from the header. It is still refused, and audited.
    #[tokio::test]
    async fn an_otherwise_valid_token_in_the_query_string_is_refused() {
        let token = mint(KEY, json!({}));
        assert!(authenticate(&FakeState::healthy(), &bearer(&token), None).await.is_ok());

        let query = format!("{BEARER_QUERY_PARAM}={token}");
        // Refused even with the valid header ALSO present, so this cannot be
        // read as "the header was simply missing".
        let rejection = authenticate(&FakeState::healthy(), &bearer(&token), Some(query.as_str()))
            .await
            .expect_err("a token in a URL is a leaked token");
        assert_eq!(rejection, ResourceRejection::TokenInQueryString);
    }

    // ── The door's own switch (review round 2) ───────────────────────────

    #[test]
    fn the_enable_flag_accepts_the_usual_truthy_spellings_and_nothing_else() {
        for on in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert!(door_enabled(Some(on)), "{on:?} should open the door");
        }
        for off in ["0", "false", "no", "off", "", "  ", "maybe", "true-ish"] {
            assert!(!door_enabled(Some(off)), "{off:?} must NOT open the door");
        }
        assert!(!door_enabled(None), "an unset flag leaves the door closed");
    }

    /// The whole point of review round 2's finding: an ENABLED door that
    /// cannot be built must be an `Err` for `main` to abort on — never
    /// `Ok(None)`, which would leave the process running with an
    /// internet-facing door that looks configured and silently accepts
    /// nothing. `Ok(None)` is reserved for "the operator did not ask for it".
    #[tokio::test]
    #[serial_test::serial]
    async fn an_enabled_but_misconfigured_door_is_an_error_not_a_silent_none() {
        std::env::remove_var(CANONICAL_RESOURCE_ENV);

        // Flag off: not our business, and nothing else is even read.
        std::env::remove_var(ENABLED_ENV);
        assert!(
            matches!(resource_server_from_env().await, Ok(None)),
            "an un-opted-in host must start normally"
        );

        // Flag on, configuration missing: hard error, and it names the
        // variable the operator has to fix.
        std::env::set_var(ENABLED_ENV, "1");
        let err = match resource_server_from_env().await {
            Err(e) => e,
            Ok(_) => panic!("an enabled-but-unconfigured door must not resolve to a closed one"),
        };
        assert!(
            err.to_string().contains(CANONICAL_RESOURCE_ENV),
            "the error must name the missing variable: {err}"
        );

        std::env::remove_var(ENABLED_ENV);
    }

    /// Review round 1: the refusal is a gate for EVERY request, so it must be
    /// reachable — and audited — without going anywhere near a resource-server
    /// config, a token, or an identity. That is what lets `handle_mcp` run it
    /// before it has selected an identity source at all.
    #[test]
    fn the_query_string_refusal_is_a_standalone_gate() {
        assert_eq!(
            refuse_query_string_token(Some("access_token=whatever")),
            Err(ResourceRejection::TokenInQueryString)
        );
        assert_eq!(refuse_query_string_token(Some("session=1")), Ok(()));
        assert_eq!(refuse_query_string_token(None), Ok(()));
    }

    /// Review round 1: there must be exactly ONE definition of what counts as
    /// a Bearer header, because a second copy elsewhere had drifted to a
    /// case-SENSITIVE `strip_prefix("Bearer ")` — so `bearer …` authenticated
    /// on one path and not the other. RFC 7235 makes the scheme
    /// case-insensitive; this asserts the shared matcher agrees.
    #[test]
    fn the_shared_bearer_matcher_is_case_insensitive_and_strict_about_everything_else() {
        for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("{scheme} a.b.c").parse().unwrap());
            assert_eq!(
                bearer_credential(&headers),
                Some("a.b.c"),
                "scheme {scheme:?} must be recognised"
            );
            // And the strict path agrees with the lenient one, so the two can
            // never disagree about the same header.
            assert_eq!(extract_bearer(&headers), Ok("a.b.c"), "scheme {scheme:?}");
        }

        // Everything that is NOT a single well-formed Bearer credential.
        let empty = HeaderMap::new();
        assert_eq!(bearer_credential(&empty), None);
        assert!(!authorization_header_present(&empty));

        let mut other_scheme = HeaderMap::new();
        other_scheme.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(bearer_credential(&other_scheme), None);
        // ...but the header IS present, which is the distinction that keeps a
        // malformed credential from being treated as "nothing was sent".
        assert!(authorization_header_present(&other_scheme));

        let mut dup = HeaderMap::new();
        dup.append("authorization", "Bearer a.b.c".parse().unwrap());
        dup.append("authorization", "Bearer d.e.f".parse().unwrap());
        assert_eq!(bearer_credential(&dup), None);

        let mut oversized = HeaderMap::new();
        let huge = format!("Bearer {}", "a".repeat(MAX_AUTHORIZATION_HEADER_BYTES + 1));
        oversized.insert("authorization", huge.parse().unwrap());
        assert_eq!(bearer_credential(&oversized), None);
    }

    #[test]
    fn query_token_detection_does_not_fire_on_unrelated_parameters() {
        assert!(!query_carries_token(None));
        assert!(!query_carries_token(Some("session=1&other=2")));
        assert!(!query_carries_token(Some("my_access_token=x")));
        assert!(query_carries_token(Some("a=1&access_token=x")));
        // A valueless parameter is still a credential in a URL.
        assert!(query_carries_token(Some("access_token")));
    }

    // ── Bounded, non-panicking header parsing ────────────────────────────

    #[test]
    fn malformed_and_oversized_authorization_headers_are_refused_without_panicking() {
        let mut none = HeaderMap::new();
        assert_eq!(extract_bearer(&none), Err(ResourceRejection::NoCredential));

        none.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(extract_bearer(&none), Err(ResourceRejection::MalformedAuthorization));

        for raw in [
            "Bearer",
            "Bearer ",
            "Bearer   ",
            "Bearer not.a.jwt!",
            "Bearer a.b",
            "Bearer a.b.c.d",
            "Bearer ..",
            "BearerNoSpace",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", raw.parse().unwrap());
            assert_eq!(
                extract_bearer(&headers),
                Err(ResourceRejection::MalformedAuthorization),
                "should refuse {raw:?}"
            );
        }

        let mut oversized = HeaderMap::new();
        let huge = format!("Bearer {}", "a".repeat(MAX_AUTHORIZATION_HEADER_BYTES + 1));
        oversized.insert("authorization", huge.parse().unwrap());
        assert_eq!(
            extract_bearer(&oversized),
            Err(ResourceRejection::MalformedAuthorization)
        );
    }

    /// Two credentials, one request: which one the next hop sees is not
    /// specified, so neither is accepted.
    #[test]
    fn duplicate_authorization_headers_are_refused() {
        let mut headers = HeaderMap::new();
        headers.append("authorization", format!("Bearer {}", mint(KEY, json!({}))).parse().unwrap());
        headers.append("authorization", "Bearer a.b.c".parse().unwrap());
        assert_eq!(extract_bearer(&headers), Err(ResourceRejection::MalformedAuthorization));
    }

    #[test]
    fn the_bearer_scheme_is_matched_case_insensitively() {
        let token = mint(KEY, json!({}));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("bearer {token}").parse().unwrap());
        assert_eq!(extract_bearer(&headers), Ok(token.as_str()));
    }

    // ── Live state: revocation takes effect at the next call ─────────────

    #[tokio::test]
    async fn a_disabled_or_deleted_client_denies_a_perfectly_valid_token() {
        let state = FakeState { client: None, ..FakeState::healthy() };
        assert_eq!(
            authenticate(&state, &bearer(&mint(KEY, json!({}))), None).await.err(),
            Some(ResourceRejection::UnknownClient)
        );
    }

    #[tokio::test]
    async fn a_disabled_account_denies_a_perfectly_valid_token() {
        let state = FakeState { account: None, ..FakeState::healthy() };
        assert_eq!(
            authenticate(&state, &bearer(&mint(KEY, json!({}))), None).await.err(),
            Some(ResourceRejection::InactiveAccount)
        );
    }

    #[tokio::test]
    async fn revoked_consent_denies_at_dispatch_not_at_the_next_token_expiry() {
        let state = FakeState { consent: false, ..FakeState::healthy() };
        assert_eq!(
            authenticate(&state, &bearer(&mint(KEY, json!({}))), None).await.err(),
            Some(ResourceRejection::ConsentRevoked)
        );
    }

    /// TERM #631: when a pair loses EVERY session, that is refused at the NEXT
    /// DISPATCH rather than at the token's expiry.
    ///
    /// The token here is fully valid in every respect the cryptography can see
    /// — correct signature, correct issuer, correct audience, `exp` comfortably
    /// in the future, live client, live account, consent intact. The ONLY thing
    /// that changed is store state written after it was minted. So if the
    /// read-path check were removed this test fails, and it cannot pass for an
    /// unrelated reason: the positive control is the same token with the
    /// sessions left alone.
    ///
    /// Named for what it proves. The narrower per-session case is NOT covered —
    /// see `one_revoked_session_among_several_is_still_accepted_term_635`.
    #[tokio::test]
    async fn revoking_every_session_for_a_pair_is_refused_at_the_next_dispatch() {
        let token = mint(KEY, json!({}));

        // POSITIVE CONTROL: signature, audience and expiry are all good, and
        // the call is admitted while a session lives.
        assert!(
            authenticate(&FakeState::healthy(), &bearer(&token), None).await.is_ok(),
            "the control must pass, or the assertion below proves nothing"
        );

        // Now every session is gone — nothing about the TOKEN changes.
        let revoked = FakeState { any_session: false, ..FakeState::healthy() };
        let rejection = authenticate(&revoked, &bearer(&token), None)
            .await
            .expect_err("a pair with no live session must be refused");
        assert_eq!(rejection, ResourceRejection::AllSessionsRevoked);

        // And it is still an unexpired, verifiable token — so the refusal came
        // from live state, not from the token going stale mid-test.
        assert!(
            config().verify_token(&token).is_ok(),
            "the token must still verify; otherwise this test would pass even with the \
             revocation check deleted"
        );
    }

    /// THE DOCUMENTED GAP, pinned so it is visible rather than implied.
    ///
    /// Revoking ONE session while another remains active leaves the pair with a
    /// live session, so `any_session_is_live` answers `true` and an access
    /// token minted for the REVOKED session is still accepted — until it
    /// expires, which is at most the access-token TTL.
    ///
    /// `any_session: true` is exactly what the real query returns for that
    /// store state: one row revoked, one row unrevoked and unexpired, for the
    /// same (account, client) pair.
    ///
    /// This is not a bug in this module and must not be "fixed" here by denying
    /// more — denying whenever ANY session was ever revoked would permanently
    /// break every surviving session for the pair, since revoked rows persist.
    /// It is closed by giving the access token a family claim so the session
    /// can be identified at all: **TERM #635**. When that lands, this test
    /// should start failing and be replaced by its inverse.
    #[tokio::test]
    async fn one_revoked_session_among_several_is_still_accepted_term_635() {
        let state = FakeState { any_session: true, ..FakeState::healthy() };
        assert!(
            authenticate(&state, &bearer(&mint(KEY, json!({}))), None).await.is_ok(),
            "documenting the TERM #635 gap: with a sibling session still live, a token from \
             a revoked session is accepted"
        );
    }

    /// Consent and sessions are INDEPENDENT axes: losing every session leaves
    /// consent intact, so a check that only consulted consent would admit it.
    #[tokio::test]
    async fn losing_every_session_is_refused_even_though_consent_is_still_granted() {
        let state = FakeState { any_session: false, consent: true, ..FakeState::healthy() };
        let rejection = authenticate(&state, &bearer(&mint(KEY, json!({}))), None)
            .await
            .expect_err("a live consent must not rescue a pair with no live session");
        assert_eq!(rejection, ResourceRejection::AllSessionsRevoked);
    }

    /// A store outage cannot confirm consent, so it denies — and does so
    /// WITHOUT a challenge, since telling the client its credential is bad
    /// would send the user through a re-authorization for a server problem.
    #[tokio::test]
    async fn an_unreachable_store_fails_closed_and_does_not_challenge() {
        let state = FakeState { fail: true, ..FakeState::healthy() };
        let rejection = authenticate(&state, &bearer(&mint(KEY, json!({}))), None)
            .await
            .expect_err("unconfirmable state must deny");
        assert_eq!(rejection, ResourceRejection::Unavailable);
        assert_eq!(rejection.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(config().challenge(rejection), None);
    }

    // ── Fail-closed principal mapping ────────────────────────────────────

    /// The OAuth door is not a way around the principal map: a token this
    /// server signed, for an account it created, that the operator has not
    /// mapped, resolves to NOBODY — exactly as an unmapped mTLS CN does.
    #[tokio::test]
    async fn an_account_with_no_principal_mapping_fails_closed() {
        // A live, perfectly ordinary account whose NAME the operator never put
        // in the `oauth_account` map. The token is valid in every other way.
        let state = FakeState { account: Some("stranger"), ..FakeState::healthy() };
        let rejection = config()
            .authenticate(&state, &resolver(), &bearer(&mint(KEY, json!({}))), None)
            .await
            .expect_err("an unmapped account must be denied");
        assert_eq!(rejection, ResourceRejection::UnmappedAccount);
    }

    /// `sub` is an account UUID (RMCP-04 mints it that way). A token that
    /// survived signature and audience checks but whose `sub` is not a UUID did
    /// not come from this fleet's token endpoint, so it is refused rather than
    /// used as a lookup key.
    #[tokio::test]
    async fn a_sub_that_is_not_an_account_uuid_is_refused() {
        let token = mint(KEY, json!({"sub": "operator"}));
        let rejection = config()
            .authenticate(&FakeState::healthy(), &resolver(), &bearer(&token), None)
            .await
            .expect_err("a non-uuid sub must be refused");
        assert_eq!(rejection, ResourceRejection::InvalidToken);
    }

    /// And with NO map authored at all, the door is inert rather than open.
    #[tokio::test]
    async fn an_unconfigured_principal_map_admits_nobody() {
        let rejection = config()
            .authenticate(
                &FakeState::healthy(),
                &PrincipalResolver::default(),
                &bearer(&mint(KEY, json!({}))),
                None,
            )
            .await
            .expect_err("an empty map must admit nobody");
        assert_eq!(rejection, ResourceRejection::UnmappedAccount);
    }

    // ── Challenge and metadata URL ───────────────────────────────────────

    #[test]
    fn the_metadata_url_is_the_rfc_9728_path_suffixed_form() {
        assert_eq!(
            config().resource_metadata_url(),
            "https://connector.example.test/.well-known/oauth-protected-resource/mcp" // pii-test-fixture
        );
        let rootish = ResourceServerConfig::new(ISSUER, signer(None)).expect("valid");
        assert_eq!(
            rootish.resource_metadata_url(),
            "https://connector.example.test/.well-known/oauth-protected-resource" // pii-test-fixture
        );
    }

    /// No credential ⇒ a challenge with NO `error`, which is what tells a
    /// client to start discovery rather than to assume its token is bad.
    #[test]
    fn a_missing_credential_challenges_without_an_error_code() {
        let challenge = config()
            .challenge(ResourceRejection::NoCredential)
            .expect("must challenge");
        assert!(!challenge.contains("error="), "{challenge}");
        assert!(challenge.contains("resource_metadata="));
    }

    /// The stored-state rejections are indistinguishable on the wire, so a
    /// token holder cannot enumerate accounts or clients from the response.
    #[test]
    fn stored_state_rejections_are_indistinguishable_to_the_client() {
        let a = config().challenge(ResourceRejection::UnknownClient);
        let b = config().challenge(ResourceRejection::InactiveAccount);
        let c = config().challenge(ResourceRejection::ConsentRevoked);
        assert_eq!(a, b);
        assert_eq!(b, c);
        // But the OPERATOR still gets the distinction, in the audit log.
        assert_ne!(
            ResourceRejection::UnknownClient.audit_reason(),
            ResourceRejection::ConsentRevoked.audit_reason()
        );
    }

    /// Nothing caller-controlled reaches a quoted challenge parameter, and the
    /// startup validation is what guarantees it: a resource URI carrying a
    /// quote or a backslash is refused before it can be interpolated.
    #[test]
    fn a_resource_uri_that_could_break_out_of_a_quoted_parameter_is_refused() {
        for bad in [
            "https://host.example.test/a\"b",   // pii-test-fixture
            "https://host.example.test/a\\b",   // pii-test-fixture
            "https://host.example.test/a b",    // pii-test-fixture
        ] {
            assert!(
                ResourceServerConfig::new(bad, signer(None)).is_err(),
                "should refuse {bad}"
            );
        }
    }

    // ── Startup validation ───────────────────────────────────────────────

    #[test]
    fn a_malformed_canonical_resource_is_a_hard_startup_error() {
        for bad in [
            "http://connector.example.test/mcp",  // pii-test-fixture: not https
            "https://connector.example.test/mcp/", // pii-test-fixture: trailing slash
            "https://connector.example.test/mcp#f", // pii-test-fixture: fragment
            "https://connector.example.test/mcp?x=1", // pii-test-fixture: query
            "https:///mcp",
            "connector.example.test/mcp", // pii-test-fixture
            "",
        ] {
            assert!(
                ResourceServerConfig::new(bad, signer(None)).is_err(),
                "should refuse {bad:?}"
            );
        }
    }

    // The minimum signing-key length and the clock-skew clamp were asserted
    // here until review round 3. They are RMCP-04's contract now, tested in
    // `crate::oauth::jwt` alongside the code that enforces them — re-asserting
    // them through a constructor this module no longer owns would be testing
    // someone else's invariant in a place nobody would think to update.

    #[test]
    fn oversized_claims_are_rejected() {
        let long = "x".repeat(MAX_CLAIM_BYTES + 1);
        assert_eq!(
            config().verify_token(&mint(KEY, json!({"sub": long.clone()}))).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
        assert_eq!(
            config().verify_token(&mint(KEY, json!({"client_id": long.clone()}))).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
        assert_eq!(
            config().verify_token(&mint(KEY, json!({"scope": long}))).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
        assert_eq!(
            config().verify_token(&mint(KEY, json!({"sub": "   "}))).expect_err("must be refused"),
            ResourceRejection::InvalidToken
        );
    }
}
