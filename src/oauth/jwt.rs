//! Access-token minting and verification for the RMCP OAuth door (RMCP-04).
//!
//! ## Why the token is a JWT and not an opaque handle
//! The resource server (RMCP-05) has to decide, on every `/mcp` request, which
//! account and which client a bearer token stands for. An opaque handle would
//! make that a database round trip on the hot dispatch path; a signed token
//! carries the answer with it. The trade is that a minted token cannot be
//! withdrawn before it expires, which is why the access TTL is short
//! ([`DEFAULT_ACCESS_TTL_SECONDS`], capped by [`MAX_ACCESS_TTL_SECONDS`]) and
//! why the long-lived half of the credential — the refresh token — is opaque,
//! stored hashed, and revocable per family.
//!
//! ## The audience binding is the point
//! Every token carries `aud` = the RFC 8707 `resource` the authorization code
//! was bound to, and [`JwtSigner::verify`] REQUIRES the caller to state which
//! audience it is. A federated peer and this server can therefore be signed by
//! the same key without a token minted for one being replayable at the other.
//! `validate_aud` is left on and the audience is never defaulted: a token with
//! no `aud`, or the wrong one, fails.
//!
//! The token also carries BOTH `sub` (the account) and `client_id`, because
//! RMCP-07's effective permission is the intersection of the two. A token that
//! named only the human would make the connector's own scope unenforceable at
//! the resource server.
//!
//! ## Key rotation
//! Verification accepts a PREVIOUS key for a grace window ([`PREVIOUS_SIGNING_KEY_ENV`])
//! so rotating the signing secret does not invalidate every live token at the
//! instant of the restart. Minting always uses the current key, so the grace
//! window drains on its own: once the longest access TTL has elapsed since the
//! rotation, no token signed by the previous key can still be valid, and the
//! operator removes it.
//!
//! ## Clocks
//! Unlike [`crate::oauth::store`], which deliberately does every expiry
//! comparison against the DATABASE clock, a JWT's `exp`/`nbf` can only be
//! written and checked against the process clock — that is what a bearer token
//! means to any other verifier. The compensation is a configurable verification
//! leeway ([`LEEWAY_ENV`]) applied on VERIFY ONLY. Minting never pre-dates or
//! extends a token to accommodate skew, because that would make the leeway
//! compound across two hosts.
//!
//! ## Secret access (S7/S8)
//! As with the rest of this module, the runtime secret store is materialized
//! into the process environment at startup, so the env read here IS the vault
//! read (see [`crate::oauth`]'s module docs). The key is never logged, never
//! returned, and never interpolated into an error.

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ToolError;

/// Env var holding the CURRENT HS256 signing secret. Minting always uses this.
pub const SIGNING_KEY_ENV: &str = "RMCP_OAUTH_SIGNING_KEY";

/// Env var holding the PREVIOUS signing secret, accepted on verification only
/// for the duration of a rotation. Absent is the normal steady state.
pub const PREVIOUS_SIGNING_KEY_ENV: &str = "RMCP_OAUTH_SIGNING_KEY_PREVIOUS";

/// Env var naming this authorization server's issuer identifier. It must match
/// the `issuer` field of the RMCP-02 metadata document, because a client that
/// discovered one issuer and received a token claiming another has been
/// redirected somewhere.
pub const ISSUER_ENV: &str = "RMCP_OAUTH_ISSUER";

/// Env var overriding [`DEFAULT_ACCESS_TTL_SECONDS`].
pub const ACCESS_TTL_ENV: &str = "RMCP_OAUTH_ACCESS_TOKEN_TTL_SECONDS";

/// Env var overriding [`DEFAULT_LEEWAY_SECONDS`] (verification only).
pub const LEEWAY_ENV: &str = "RMCP_OAUTH_CLOCK_SKEW_SECONDS";

/// Fifteen minutes. Short because a minted JWT cannot be recalled: the window
/// between "operator revokes" and "the last token stops working" is exactly
/// this value.
pub const DEFAULT_ACCESS_TTL_SECONDS: i64 = 900;

/// An hour is the longest an operator may stretch the un-revocable window.
/// A tuning knob that can be set to a day is a revocation hole with a
/// configuration file in front of it.
pub const MAX_ACCESS_TTL_SECONDS: i64 = 3600;

/// Half a minute of skew tolerance on `exp`/`nbf`, applied on verification.
pub const DEFAULT_LEEWAY_SECONDS: u64 = 30;

/// The largest leeway an operator may configure. Beyond a few minutes the
/// leeway, not the TTL, decides how long a token lives.
const MAX_LEEWAY_SECONDS: u64 = 300;

/// Minimum signing-secret length. HS256's security is the key's entropy, and a
/// short shared secret on an internet-facing token issuer is forgeable offline.
/// This refuses at configuration time rather than issuing weak tokens: the door
/// is new, so there is no existing deployment for a fail-closed check to break.
const MIN_SIGNING_KEY_BYTES: usize = 32;

/// The claims carried by an RMCP access token.
///
/// Every field is deliberately present on every token — there is no
/// `skip_serializing_if` and no `Option` — because each one is checked
/// somewhere downstream, and an absent claim that merely fails "open" at one
/// call site is the shape of a bypass.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessClaims {
    /// This authorization server's issuer identifier.
    pub iss: String,
    /// The ACCOUNT this token acts for, as the `rmcp_account` UUID.
    pub sub: String,
    /// The RFC 8707 resource the authorization code was bound to.
    pub aud: String,
    /// The public `client_id` of the connector. Paired with `sub` by RMCP-07.
    pub client_id: String,
    /// Space-delimited granted scope.
    pub scope: String,
    /// Unique token identifier, for replay tracing in audit logs.
    pub jti: String,
    pub exp: i64,
    pub iat: i64,
    pub nbf: i64,
}

/// A freshly minted access token and the facts a token response needs about it.
///
/// No `Debug`: the `token` field IS the bearer credential, and a `{:?}` on this
/// struct in a handler is the most likely way it would ever reach a log.
pub struct MintedAccessToken {
    /// The encoded JWT. This is presentable material — never log it.
    pub token: String,
    /// The `jti`, which is safe to log and is what an audit trail records.
    pub jti: String,
    /// Seconds until expiry, for the `expires_in` field of the token response.
    pub expires_in: i64,
}

impl std::fmt::Debug for MintedAccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedAccessToken")
            .field("token", &"<redacted>")
            .field("jti", &self.jti)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Mints and verifies RMCP access tokens.
///
/// Holds no `Debug` impl for the same reason [`crate::oauth::OauthConfig`] has
/// none: the two most interesting fields are signing secrets.
#[derive(Clone)]
pub struct JwtSigner {
    current_key: String,
    previous_key: Option<String>,
    issuer: String,
    access_ttl_seconds: i64,
    leeway_seconds: u64,
}

impl JwtSigner {
    /// Build a signer from explicit values.
    ///
    /// Separate from [`Self::from_env`] so tests can exercise minting,
    /// verification and key rotation without mutating process-global
    /// environment state — which would make them order-dependent against every
    /// other test in the crate.
    pub fn new(
        current_key: String,
        previous_key: Option<String>,
        issuer: String,
        access_ttl_seconds: i64,
        leeway_seconds: u64,
    ) -> Result<Self, ToolError> {
        if current_key.len() < MIN_SIGNING_KEY_BYTES {
            // Names the variable and the requirement, never the value.
            return Err(ToolError::NotConfigured(format!(
                "{SIGNING_KEY_ENV} must be at least {MIN_SIGNING_KEY_BYTES} bytes — a short \
                 HS256 secret on an internet-facing token issuer is forgeable offline"
            )));
        }
        if let Some(previous) = previous_key.as_ref() {
            if previous.len() < MIN_SIGNING_KEY_BYTES {
                return Err(ToolError::NotConfigured(format!(
                    "{PREVIOUS_SIGNING_KEY_ENV} must meet the same {MIN_SIGNING_KEY_BYTES}-byte \
                     minimum as the current key — a rotation must not be a way to keep a weak \
                     key alive"
                )));
            }
            // A "rotation" to the same value is almost certainly a copy-paste
            // during a rotation that then looks complete but is not. Refusing
            // is cheap; the alternative is an operator believing they rotated.
            if previous == &current_key {
                return Err(ToolError::NotConfigured(format!(
                    "{PREVIOUS_SIGNING_KEY_ENV} is identical to {SIGNING_KEY_ENV} — that is not \
                     a rotation, and leaving it set hides the fact that one has not happened"
                )));
            }
        }
        if issuer.trim().is_empty() {
            return Err(ToolError::NotConfigured(format!(
                "{ISSUER_ENV} must name this authorization server's issuer identifier, matching \
                 the discovery document"
            )));
        }
        if access_ttl_seconds <= 0 || access_ttl_seconds > MAX_ACCESS_TTL_SECONDS {
            return Err(ToolError::NotConfigured(format!(
                "{ACCESS_TTL_ENV} must be between 1 and {MAX_ACCESS_TTL_SECONDS} seconds — a \
                 minted JWT cannot be recalled, so its lifetime is the revocation delay"
            )));
        }
        if leeway_seconds > MAX_LEEWAY_SECONDS {
            return Err(ToolError::NotConfigured(format!(
                "{LEEWAY_ENV} must not exceed {MAX_LEEWAY_SECONDS} seconds — past that the \
                 leeway, not the TTL, decides how long a token lives"
            )));
        }
        Ok(Self {
            current_key,
            previous_key,
            issuer: issuer.trim().to_string(),
            access_ttl_seconds,
            leeway_seconds,
        })
    }

    /// Read the signer's configuration from the environment (the vault read,
    /// see the module docs).
    pub fn from_env() -> Result<Self, ToolError> {
        let current = env_nonempty(SIGNING_KEY_ENV).ok_or_else(|| {
            ToolError::NotConfigured(format!(
                "{SIGNING_KEY_ENV} not set — the RMCP token endpoint cannot sign access tokens"
            ))
        })?;
        let issuer = env_nonempty(ISSUER_ENV).ok_or_else(|| {
            ToolError::NotConfigured(format!("{ISSUER_ENV} not set"))
        })?;
        // A malformed TTL or leeway falls back to the default rather than
        // taking the door offline: neither value grants anything, so the
        // fail-closed rule that governs PERMISSIONS does not apply to them.
        // Out-of-RANGE values are still rejected by `new`, because those are a
        // deliberate setting rather than a typo.
        let ttl = env_nonempty(ACCESS_TTL_ENV)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(DEFAULT_ACCESS_TTL_SECONDS);
        let leeway = env_nonempty(LEEWAY_ENV)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_LEEWAY_SECONDS);
        Self::new(current, env_nonempty(PREVIOUS_SIGNING_KEY_ENV), issuer, ttl, leeway)
    }

    /// The configured access-token lifetime, for the `expires_in` field.
    pub fn access_ttl_seconds(&self) -> i64 {
        self.access_ttl_seconds
    }

    /// Mint an access token bound to `audience`.
    ///
    /// `audience` is the resource the authorization code (or the refresh
    /// token's family) was bound to — never a value taken from the token
    /// request, which is why this takes it as a parameter rather than reading
    /// one. The caller having to produce the bound value is what makes it
    /// impossible to mint a token for an audience the human never approved.
    pub fn mint(
        &self,
        account_id: Uuid,
        client_id: &str,
        audience: &str,
        scope: &str,
    ) -> Result<MintedAccessToken, ToolError> {
        if audience.trim().is_empty() {
            // Refuse rather than mint an unbound token: `validate_aud` would
            // then be the only thing standing between this token and every
            // federated peer, and a token with no audience is exactly the
            // artifact the binding exists to prevent.
            return Err(ToolError::InvalidArgument(
                "refusing to mint an access token with no audience".into(),
            ));
        }
        let now = chrono::Utc::now().timestamp();
        let jti = super::random_token(16)?;
        let claims = AccessClaims {
            iss: self.issuer.clone(),
            sub: account_id.to_string(),
            aud: audience.to_string(),
            client_id: client_id.to_string(),
            scope: scope.to_string(),
            jti: jti.clone(),
            exp: now + self.access_ttl_seconds,
            iat: now,
            nbf: now,
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(self.current_key.as_bytes()),
        )
        .map_err(|e| ToolError::Execution(format!("access-token signing failed: {e}")))?;
        Ok(MintedAccessToken { token, jti, expires_in: self.access_ttl_seconds })
    }

    /// Verify a presented access token against `expected_audience`.
    ///
    /// The caller states the audience; there is no "any audience" mode, and
    /// adding one would silently undo the RFC 8707 binding for every call site
    /// at once.
    ///
    /// An EMPTY `expected_audience` is refused outright rather than passed to
    /// the validator. Review round 1 caught this: an empty expected audience is
    /// a caller whose own resource configuration is missing, and `jsonwebtoken`
    /// would compare against an empty set — fail-OPEN on the single property
    /// that stops a token minted for a federated peer being replayed here. The
    /// symmetric refusal in [`Self::mint`] means neither end of the binding can
    /// be blank, which is the whole reason the parameter exists.
    ///
    /// Every failure — bad signature, expired, not yet valid, wrong issuer,
    /// wrong audience, malformed — collapses into one error. A resource server
    /// only needs valid-or-not, and distinguishing the reasons to an
    /// unauthenticated caller is an oracle.
    ///
    /// RMCP-05 needs two of those reasons for its OWN bookkeeping — never for
    /// the wire — so it calls [`Self::verify_with_reason`], of which this is a
    /// thin wrapper. There is still exactly one verification implementation;
    /// only the error channel is richer.
    pub fn verify(&self, token: &str, expected_audience: &str) -> Result<AccessClaims, ToolError> {
        self.verify_with_reason(token, expected_audience)
            .map_err(|_| ToolError::InvalidArgument("access token is not valid".into()))
    }

    /// [`Self::verify`] with the failure CLASSIFIED.
    ///
    /// Same decision, same code path — the classification changes nothing about
    /// which tokens are accepted. It exists because a resource server has two
    /// obligations this one error cannot serve:
    ///
    /// - **An expired token must be answerable with a challenge that tells a
    ///   hosted client to REFRESH**, not to start a fresh authorization. Both
    ///   are `invalid_token` on the wire (RFC 6750 gives no separate code), so
    ///   the difference lives in the description — and getting it wrong strands
    ///   a user in a re-consent loop for a token that only needed renewing.
    /// - **A wrong AUDIENCE is the single most important thing to be able to
    ///   find in an audit log.** It is the signal that someone is replaying a
    ///   federated peer's token here, and it is indistinguishable from an
    ///   ordinary bad signature once both have collapsed to "not valid".
    ///
    /// The oracle concern in [`Self::verify`]'s doc still governs the WIRE:
    /// `crate::oauth::resource` maps every one of these to the same coarse
    /// client-facing description and keeps the distinction for the operator.
    pub fn verify_with_reason(
        &self,
        token: &str,
        expected_audience: &str,
    ) -> Result<AccessClaims, VerifyFailure> {
        self.verify_inner(token, expected_audience)
    }

    fn verify_inner(&self, token: &str, expected_audience: &str) -> Result<AccessClaims, VerifyFailure> {
        if expected_audience.trim().is_empty() {
            // Unchanged refusal, unchanged reason (see this method's doc): a
            // caller with no resource configured must deny, not match
            // everything. Classified as `Invalid` because it is a fault on THIS
            // side — nothing about the presented token is known to be wrong.
            return Err(VerifyFailure::Invalid);
        }
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&[expected_audience]);
        validation.set_issuer(&[self.issuer.as_str()]);
        // `jsonwebtoken` requires only `exp` by default and does NOT check
        // `nbf` unless asked. Both are demanded explicitly, along with the
        // three claims that carry the binding, so a token missing one is
        // rejected rather than accepted for lacking it. (`iat` is deliberately
        // absent: the library ignores unknown names here, and listing one
        // would imply an enforcement that does not happen.)
        validation.set_required_spec_claims(&["exp", "nbf", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = self.leeway_seconds;

        let mut keys = vec![self.current_key.as_str()];
        keys.extend(self.previous_key.as_deref());
        // The LAST failure is what gets classified, and the key order makes
        // that the right one: a token signed by neither key fails identically
        // under both, while a token signed by one of them fails under the other
        // for signature reasons only — so whichever key actually matched
        // decides the reported reason.
        let mut failure = VerifyFailure::Invalid;
        for key in keys {
            match decode::<AccessClaims>(
                token,
                &DecodingKey::from_secret(key.as_bytes()),
                &validation,
            ) {
                Ok(data) => return Ok(data.claims),
                Err(e) => {
                    let classified = VerifyFailure::classify(&e);
                    // Never let a mere signature mismatch against the FIRST key
                    // overwrite a substantive reason learned from another.
                    if !matches!(classified, VerifyFailure::Invalid) {
                        failure = classified;
                    }
                }
            }
        }
        Err(failure)
    }
}

/// Why [`JwtSigner::verify_with_reason`] refused a token.
///
/// Three variants, not ten: only the two that a resource server must ACT on
/// differently are named, and everything else stays collapsed. Adding a variant
/// here is adding a distinction someone can leak, so it should need an argument
/// as concrete as the two below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyFailure {
    /// Signature and issuer were fine; the token is past `exp` (allowing for
    /// the configured leeway). The one failure a client can fix by REFRESHING.
    Expired,
    /// The token names a different audience — most importantly, a federated
    /// peer's. This is the replay signal, and the reason it is worth naming.
    Audience,
    /// Everything else: bad signature, wrong issuer, not yet valid, malformed,
    /// missing a required claim. Deliberately undifferentiated.
    Invalid,
}

impl VerifyFailure {
    fn classify(e: &jsonwebtoken::errors::Error) -> Self {
        match e.kind() {
            jsonwebtoken::errors::ErrorKind::ExpiredSignature => Self::Expired,
            jsonwebtoken::errors::ErrorKind::InvalidAudience => Self::Audience,
            _ => Self::Invalid,
        }
    }
}

/// Read an env var, treating blank as absent (the same rule
/// [`crate::oauth::OauthConfig::from_env`] applies, and for the same reason: a
/// materialized-but-empty secret is a missing one).
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // TEST DOUBLES, not credentials: these two strings exist only in the test
    // binary, authenticate nothing, and are named and shaped so that is obvious
    // at a glance — a label followed by a repeated byte, padded to clear the
    // 32-byte minimum `JwtSigner::new` enforces. Their only job is to be two
    // DISTINCT values, so the rotation tests below can tell "signed by the
    // current key" from "signed by the previous one".
    //
    // They deliberately do NOT come from a runtime secret accessor. S7/S8
    // governs how the RUNNING service obtains real credentials; routing a test
    // double through that path would test the vault rather than the minting and
    // verification logic under test, and would make these tests depend on
    // process-global environment state they otherwise touch nowhere at all —
    // which is also why `JwtSigner::new` exists alongside `from_env`.
    const TEST_DOUBLE_KEY_CURRENT: &str = // pii-test-fixture: invented test key
        "test-double-not-a-secret-aaaaaaaaaaaaaaaa";
    const TEST_DOUBLE_KEY_PREVIOUS: &str = // pii-test-fixture: invented test key
        "test-double-not-a-secret-bbbbbbbbbbbbbbbb";
    const ISSUER: &str = "https://connector.test";
    const RESOURCE: &str = "https://connector.test/mcp";

    /// Build a signer over the test doubles above. A helper rather than a
    /// repeated `JwtSigner::new(...)` so each test below reads as the property
    /// it is asserting rather than as five constructor arguments.
    fn signer_with(
        current: &str,
        previous: Option<&str>,
        issuer: &str,
    ) -> Result<JwtSigner, ToolError> {
        JwtSigner::new(current.into(), previous.map(str::to_string), issuer.into(), 900, 30)
    }

    fn signer() -> JwtSigner {
        signer_with(TEST_DOUBLE_KEY_CURRENT, None, ISSUER).expect("valid config")
    }

    /// The headline property: a token minted for one resource must not verify
    /// at another. This is what stops a token issued for a federated peer being
    /// replayed here.
    #[test]
    fn audience_binding_is_enforced() {
        let signer = signer();
        let minted = signer
            .mint(Uuid::nil(), "a-client", RESOURCE, "mcp offline_access")
            .expect("mint");

        let claims = signer.verify(&minted.token, RESOURCE).expect("same audience verifies");
        assert_eq!(claims.aud, RESOURCE);

        assert!(
            signer.verify(&minted.token, "https://peer.test/mcp").is_err(),
            "a token minted for one resource must not verify at another"
        );
    }

    /// RMCP-07 intersects the account's grant with the client's scope, so a
    /// token naming only one of them would make the other unenforceable.
    #[test]
    fn token_carries_both_account_and_client() {
        let signer = signer();
        let account = Uuid::new_v4();
        let minted = signer.mint(account, "a-client", RESOURCE, "mcp").expect("mint");
        let claims = signer.verify(&minted.token, RESOURCE).expect("verify");
        assert_eq!(claims.sub, account.to_string());
        assert_eq!(claims.client_id, "a-client");
        assert_eq!(claims.scope, "mcp");
        assert_eq!(claims.iss, ISSUER);
        assert!(!claims.jti.is_empty());
        assert_eq!(claims.exp - claims.iat, 900);
        assert_eq!(claims.nbf, claims.iat);
    }

    /// Each token gets its own `jti`, or an audit trail cannot tell two
    /// issuances apart.
    #[test]
    fn each_token_has_a_unique_jti() {
        let signer = signer();
        let one = signer.mint(Uuid::nil(), "c", RESOURCE, "mcp").expect("mint");
        let two = signer.mint(Uuid::nil(), "c", RESOURCE, "mcp").expect("mint");
        assert_ne!(one.jti, two.jti);
        assert_ne!(one.token, two.token);
    }

    /// A token signed by a key this server does not hold must fail, and the
    /// PREVIOUS key must be accepted so a rotation does not invalidate every
    /// live token at the instant of the restart.
    #[test]
    fn verification_accepts_the_previous_key_but_not_a_foreign_one() {
        let old = signer_with(TEST_DOUBLE_KEY_PREVIOUS, None, ISSUER).expect("valid");
        let minted_under_old = old.mint(Uuid::nil(), "c", RESOURCE, "mcp").expect("mint");

        // Rotated: the current key is new, the previous one is still accepted.
        let rotated = signer_with(TEST_DOUBLE_KEY_CURRENT, Some(TEST_DOUBLE_KEY_PREVIOUS), ISSUER)
            .expect("valid");
        assert!(
            rotated.verify(&minted_under_old.token, RESOURCE).is_ok(),
            "the grace window must accept a token signed by the previous key"
        );

        // Grace window over: previous key removed.
        let after = signer();
        assert!(
            after.verify(&minted_under_old.token, RESOURCE).is_err(),
            "once the previous key is removed its tokens must stop verifying"
        );
    }

    /// Minting must always use the CURRENT key, never fall back to the
    /// previous one — otherwise a rotation would never actually take effect.
    #[test]
    fn minting_always_uses_the_current_key() {
        let rotated = signer_with(TEST_DOUBLE_KEY_CURRENT, Some(TEST_DOUBLE_KEY_PREVIOUS), ISSUER)
            .expect("valid");
        let minted = rotated.mint(Uuid::nil(), "c", RESOURCE, "mcp").expect("mint");

        // A verifier holding ONLY the old key must reject it.
        let old_only = signer_with(TEST_DOUBLE_KEY_PREVIOUS, None, ISSUER).expect("valid");
        assert!(old_only.verify(&minted.token, RESOURCE).is_err());
    }

    /// A token from a different issuer means the client was pointed somewhere
    /// else during discovery.
    #[test]
    fn a_foreign_issuer_is_rejected() {
        let mine = signer();
        let theirs =
            signer_with(TEST_DOUBLE_KEY_CURRENT, None, "https://elsewhere.test").expect("valid");
        let minted = theirs.mint(Uuid::nil(), "c", RESOURCE, "mcp").expect("mint");
        assert!(mine.verify(&minted.token, RESOURCE).is_err());
    }

    /// An expired token must fail even with the leeway applied, and the leeway
    /// must not be big enough to resurrect one.
    #[test]
    fn an_expired_token_is_rejected() {
        let signer = signer();
        // Claims are forged with a past expiry rather than sleeping out a
        // short TTL: a test that sleeps to observe expiry is slow and
        // eventually flakes on a loaded build host.
        let now = chrono::Utc::now().timestamp();
        let stale = AccessClaims {
            iss: ISSUER.into(),
            sub: Uuid::nil().to_string(),
            aud: RESOURCE.into(),
            client_id: "c".into(),
            scope: "mcp".into(),
            jti: "j".into(),
            exp: now - 600,
            iat: now - 1200,
            nbf: now - 1200,
        };
        let forged = encode(
            &Header::new(Algorithm::HS256),
            &stale,
            &EncodingKey::from_secret(TEST_DOUBLE_KEY_CURRENT.as_bytes()),
        )
        .expect("encode");
        assert!(signer.verify(&forged, RESOURCE).is_err());
    }

    /// A token whose `nbf` is in the future is not usable yet. `jsonwebtoken`
    /// does NOT check this by default, so the assertion guards the explicit
    /// `validate_nbf` above rather than library behaviour.
    #[test]
    fn a_not_yet_valid_token_is_rejected() {
        let signer = signer();
        let now = chrono::Utc::now().timestamp();
        let future = AccessClaims {
            iss: ISSUER.into(),
            sub: Uuid::nil().to_string(),
            aud: RESOURCE.into(),
            client_id: "c".into(),
            scope: "mcp".into(),
            jti: "j".into(),
            exp: now + 3600,
            iat: now,
            nbf: now + 3600,
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &future,
            &EncodingKey::from_secret(TEST_DOUBLE_KEY_CURRENT.as_bytes()),
        )
        .expect("encode");
        assert!(signer.verify(&token, RESOURCE).is_err());
    }

    /// Configuration that would weaken the door is refused at construction,
    /// not accepted with a warning.
    #[test]
    fn weak_or_nonsensical_configuration_is_refused() {
        let current = TEST_DOUBLE_KEY_CURRENT;
        // Too-short keys, on either side of a rotation.
        assert!(signer_with("short", None, ISSUER).is_err());
        assert!(signer_with(current, Some("short"), ISSUER).is_err());
        assert!(
            signer_with(current, Some(current), ISSUER).is_err(),
            "a previous key equal to the current one is not a rotation"
        );
        assert!(signer_with(current, None, "  ").is_err());
        // Lifetimes and leeway are checked by the full constructor.
        assert!(JwtSigner::new(current.into(), None, ISSUER.into(), 0, 30).is_err());
        assert!(
            JwtSigner::new(current.into(), None, ISSUER.into(), MAX_ACCESS_TTL_SECONDS + 1, 30)
                .is_err(),
            "the un-revocable window must not be configurable to an arbitrary length"
        );
        assert!(JwtSigner::new(current.into(), None, ISSUER.into(), 900, 3600).is_err());
    }

    /// An unbound token would defeat the whole audience design, so minting one
    /// is refused rather than left to the verifier to catch.
    #[test]
    fn minting_without_an_audience_is_refused() {
        assert!(signer().mint(Uuid::nil(), "c", "", "mcp").is_err());
        assert!(signer().mint(Uuid::nil(), "c", "   ", "mcp").is_err());
    }

    /// The other half of the same rule, and the more dangerous one: verifying
    /// against an empty expected audience would fail OPEN — a caller whose
    /// resource configuration is missing would accept a token minted for
    /// anything. A perfectly valid token must still be refused when the caller
    /// cannot say what it should be for.
    #[test]
    fn verifying_against_an_empty_audience_is_refused() {
        let signer = signer();
        let minted = signer.mint(Uuid::nil(), "c", RESOURCE, "mcp").expect("mint");
        assert!(signer.verify(&minted.token, RESOURCE).is_ok(), "the control case verifies");
        assert!(signer.verify(&minted.token, "").is_err());
        assert!(signer.verify(&minted.token, "   ").is_err());
    }

    /// The bearer credential must not be reachable through `Debug`.
    #[test]
    fn minted_token_debug_is_redacted() {
        let minted = signer().mint(Uuid::nil(), "c", RESOURCE, "mcp").expect("mint");
        let rendered = format!("{minted:?}");
        assert!(!rendered.contains(&minted.token));
        assert!(rendered.contains("<redacted>"));
    }
}
