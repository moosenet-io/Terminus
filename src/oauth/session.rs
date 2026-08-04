//! The short-lived login session that bridges the two halves of the interactive
//! flow (RMCP-03).
//!
//! ## What this is, and what it deliberately is not
//! The authorization flow is three HTTP requests: `GET /authorize` renders a
//! login form, `POST /login` authenticates, `POST /consent` mints the code.
//! Something has to carry "this browser proved it is account X" from the second
//! request to the third.
//!
//! This is NOT a general web session. It lives for [`TTL_SECS`], is scoped to
//! the OAuth path, and is consumed by the single act of issuing an
//! authorization code. Terminus has no logged-in web surface for it to leak
//! into, and giving it a longer life would create one by accident.
//!
//! ## Signed, not stored — except for the one thing a signature cannot say
//! The session itself is an HS256 JWT in a cookie rather than a row in a table:
//! the same mechanism, key convention and issuer-pinning discipline as
//! [`crate::mesh::person`], and for the same reasons — no shared state to keep
//! in step, no cleanup job, and nothing about the session survives its own
//! expiry.
//!
//! A signature cannot express "already used", though, and single use is
//! precisely what stops a replayed consent post from minting a second
//! authorization code for one human approval. That one bit IS durable: the
//! `jti` is claimed in `rmcp_login_session_use` via
//! [`crate::oauth::store::OauthStore::claim_login_session`], an `INSERT … ON
//! CONFLICT DO NOTHING` whose primary key picks a single winner across every
//! replica.
//!
//! An earlier revision kept those spent identifiers in a process-local map.
//! Review round 1 rejected it: the same signed cookie presented to two replicas
//! was unspent at both. The row is not a session store by another name — it
//! holds a digest and an expiry, never the session — but the claim genuinely
//! has to be somewhere both replicas can see.
//!
//! ## Cookie attributes, and why each one
//! - `HttpOnly` — no page on this origin runs script (the CSP forbids it), but
//!   the attribute costs nothing and removes the whole class.
//! - `Secure` — the authorization endpoint is reached over public HTTPS. Set
//!   unconditionally rather than "when the request looked secure": a
//!   conditional here reads the very headers a downgrade attack controls.
//! - `SameSite=Lax` — the login and consent posts are same-site form
//!   submissions, so `Lax` is sufficient and `Strict` would break the initial
//!   cross-site navigation from the client application into `/authorize`.
//! - `Path=/oauth` — the cookie is never sent to `/mcp` or any tool route.
//! - `Max-Age` — matches the token's own expiry so a stale cookie is not
//!   presented for a token that cannot possibly verify.
//!
//! `SameSite=Lax` is defence in depth, not the CSRF defence: the consent form
//! also carries a token bound to this session (see [`LoginSession::csrf`]).

use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ToolError;

/// Env name of the HS256 key that signs login sessions.
///
/// Materialized into the process environment from the runtime secret store at
/// startup — this crate has no separate `SecretManager::get()`, so an env read
/// here IS the vault read (see [`crate::oauth`]'s module docs and
/// [`crate::pg::conn`] for the established precedent). Absent means the login
/// surface refuses to start, never that it runs unsigned.
pub const SIGNING_KEY_ENV: &str = "RMCP_OAUTH_SIGNING_KEY";

/// Cookie name. Prefixed so it cannot collide with any other cookie the
/// operator's browser holds for this host.
pub const COOKIE_NAME: &str = "rmcp_login";

/// Path the cookie is scoped to. Everything this module's flow touches lives
/// under it, and nothing else does.
pub const COOKIE_PATH: &str = "/oauth";

/// Issuer claim, pinning these tokens to this purpose.
///
/// Without it, a token minted by any other HS256 consumer that happened to
/// share the key would decode here. That is not hypothetical in a fleet with
/// several JWT-signing subsystems, and the check costs one string comparison.
const ISSUER: &str = "terminus-rmcp-login-session";

/// How long a login session lives.
///
/// Long enough for a human to actually read the consent screen — which lists
/// concrete tool patterns and is meant to be read, not clicked through — and
/// short enough that a session left open on an unattended machine has expired
/// before it matters. It is not a "stay signed in" window; there is nothing to
/// stay signed in to.
const TTL_SECS: u64 = 300;

/// Minimum accepted signing-key length, in bytes.
///
/// HS256's security is bounded by the key, and `jsonwebtoken` will happily sign
/// with a four-character string. A short key here would make every login
/// session forgeable by anyone who can guess it, so a too-short key is treated
/// as a misconfiguration and refused at construction rather than silently
/// accepted.
const MIN_KEY_LEN: usize = 32;

/// The HS256 key, wrapped so it cannot be logged or compared by accident.
///
/// No `Debug`, no `Display`, no accessor returning the bytes to arbitrary
/// callers — the only thing that can be done with one is sign or verify inside
/// this module.
#[derive(Clone)]
pub struct SessionKey(String);

impl SessionKey {
    /// Read the key from the runtime-materialized environment.
    ///
    /// Blank reads as absent, matching the crate-wide rule that an empty
    /// materialized secret is a missing one rather than a valid empty
    /// credential. The error names the VARIABLE, never the value.
    pub fn from_env() -> Result<Self, ToolError> {
        let raw = std::env::var(SIGNING_KEY_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                ToolError::NotConfigured(format!(
                    "{SIGNING_KEY_ENV} not set — the RMCP login surface cannot sign sessions"
                ))
            })?;
        Self::from_secret(&raw)
    }

    /// Build a key from an already-obtained secret. Used by tests and by any
    /// caller that already holds the materialized value.
    ///
    /// Rejects a key shorter than [`MIN_KEY_LEN`]. The refusal does not echo
    /// the value or its length beyond the bound it failed.
    pub fn from_secret(raw: &str) -> Result<Self, ToolError> {
        if raw.len() < MIN_KEY_LEN {
            return Err(ToolError::NotConfigured(format!(
                "{SIGNING_KEY_ENV} is too short — HS256 needs at least {MIN_KEY_LEN} bytes of key \
                 material for a signature to mean anything"
            )));
        }
        Ok(Self(raw.to_string()))
    }
}

/// Claims of a login session.
///
/// `name` is carried alongside `sub` for one specific reason: the account must
/// be re-checked for the `disabled` flag at code-issuance time, not only at
/// login (an operator who disables an account mid-flow expects it to take
/// effect immediately). The store's active-account lookup is by NAME, so the
/// name has to travel. Both are verified together at re-lookup — the id must
/// still match the row the name resolves to — so a rename cannot be used to
/// swing a session onto a different account.
#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    sub: String,
    name: String,
    jti: String,
    csrf: String,
    exp: u64,
    iss: String,
}

/// A verified login session.
///
/// The only constructor is [`verify`], and the fields are read-only accessors,
/// so holding one of these is proof that the signature, the expiry and the
/// issuer were all checked. Modelled on [`crate::mesh::person`]'s
/// `VerifiedPerson` for the same reason: an "authenticated" struct that can be
/// built from parts is not evidence of anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSession {
    account_id: Uuid,
    account_name: String,
    jti: String,
    csrf: String,
}

impl LoginSession {
    /// The authenticated account's id.
    pub fn account_id(&self) -> Uuid {
        self.account_id
    }

    /// The authenticated account's name, for the disabled-mid-flow re-check.
    pub fn account_name(&self) -> &str {
        &self.account_name
    }

    /// The single-use identifier. The caller records this when it issues a
    /// code, and refuses a second presentation of the same session.
    pub fn jti(&self) -> &str {
        &self.jti
    }

    /// The CSRF token this session's consent form must echo.
    ///
    /// A signed double-submit: the value lives in the `HttpOnly` cookie's
    /// signed payload and in a hidden form field, and a cross-site attacker can
    /// cause the cookie to be sent but cannot read it to populate the field.
    pub fn csrf(&self) -> &str {
        &self.csrf
    }

    /// Constant-time-ish comparison of a submitted CSRF value against this
    /// session's.
    ///
    /// The token is not a credential an attacker can probe adaptively — a
    /// mismatch invalidates the whole submission and there is no oracle to
    /// iterate against — but comparing lengths first and then every byte
    /// without early exit costs nothing and keeps the habit intact.
    pub fn csrf_matches(&self, submitted: &str) -> bool {
        let expected = self.csrf.as_bytes();
        let actual = submitted.as_bytes();
        if expected.len() != actual.len() || expected.is_empty() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(actual.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

/// Mint a signed login session for an authenticated account.
///
/// `jti` and `csrf` are supplied by the caller so both come from the one
/// high-entropy generator this module's sibling uses
/// ([`crate::oauth::authorize::new_high_entropy_token`]) rather than from a
/// second source with its own strength to argue about.
pub fn mint(
    key: &SessionKey,
    account_id: Uuid,
    account_name: &str,
    jti: &str,
    csrf: &str,
) -> Result<String, ToolError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ToolError::Execution("system clock is before the epoch".into()))?
        .as_secs();

    let claims = SessionClaims {
        sub: account_id.to_string(),
        name: account_name.to_string(),
        jti: jti.to_string(),
        csrf: csrf.to_string(),
        exp: now + TTL_SECS,
        iss: ISSUER.to_string(),
    };

    encode(&Header::default(), &claims, &EncodingKey::from_secret(key.0.as_bytes()))
        .map_err(|_| ToolError::Execution("could not sign the login session".into()))
}

/// Verify a presented session token.
///
/// Returns `None` for every failure — bad signature, wrong issuer, expired,
/// malformed, or an unparseable account id. The caller's response to `None` is
/// always the same (re-render the login page), so distinguishing the causes
/// here would only create a channel for telling an attacker which part of a
/// forged token was wrong.
pub fn verify(key: &SessionKey, token: &str) -> Option<LoginSession> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&[ISSUER]);
    validation.validate_exp = true;
    // `jsonwebtoken` defaults `leeway` to SIXTY SECONDS, which would extend a
    // five-minute session by a fifth. That default exists to absorb clock skew
    // between organisations; this token is minted and verified by the same
    // process, so there is no skew to absorb and no reason to honour an expired
    // session for another minute. Zero, explicitly — the same override, for the
    // same reason, as `crate::mesh::person`.
    validation.leeway = 0;
    // `sub` is a UUID, not a registered audience/subject we pin here; it is
    // parsed below and re-checked against the live account by the caller.
    validation.required_spec_claims.clear();
    validation.required_spec_claims.insert("exp".to_string());

    let decoded =
        decode::<SessionClaims>(token, &DecodingKey::from_secret(key.0.as_bytes()), &validation)
            .ok()?;

    let account_id = Uuid::parse_str(&decoded.claims.sub).ok()?;
    // A session with no `jti` could not be marked consumed, so it would be
    // replayable; a session with no `csrf` would make every comparison against
    // it fail closed but pointlessly. Both are refused rather than tolerated.
    if decoded.claims.jti.is_empty() || decoded.claims.csrf.is_empty() {
        return None;
    }

    Some(LoginSession {
        account_id,
        account_name: decoded.claims.name,
        jti: decoded.claims.jti,
        csrf: decoded.claims.csrf,
    })
}

/// The `Set-Cookie` value that installs a session.
pub fn set_cookie(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path={COOKIE_PATH}; Max-Age={TTL_SECS}; HttpOnly; Secure; \
         SameSite=Lax"
    )
}

/// The `Set-Cookie` value that clears one.
///
/// Emitted on every terminal outcome — a successful redirect, a refusal, an
/// error — so a session cookie never outlives the flow it was minted for. The
/// attributes must match those used to set it or some browsers will refuse to
/// clear it.
pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path={COOKIE_PATH}; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
}

/// Pull the session token out of a raw `Cookie` request header.
///
/// Returns `None` when the header holds no cookie by that name. Matching is on
/// the exact name after trimming, so a `rmcp_login_other` cookie cannot be read
/// as this one.
pub fn token_from_cookie_header(raw: &str) -> Option<&str> {
    raw.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name.trim() == COOKIE_NAME {
            Some(value.trim())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key long enough to be accepted, built from a repeated pattern so this
    /// file holds nothing that looks like a real credential.
    fn test_key() -> SessionKey {
        SessionKey::from_secret(&"k".repeat(48)).expect("a 48-byte key must be accepted")
    }

    fn other_key() -> SessionKey {
        SessionKey::from_secret(&"j".repeat(48)).expect("a 48-byte key must be accepted")
    }

    /// HS256 with a four-character key is a signature nobody has to forge.
    #[test]
    fn a_short_key_is_refused_and_the_refusal_says_nothing_about_it() {
        // Matched rather than `expect_err`: that helper requires the OK type to
        // be `Debug`, and `SessionKey` deliberately implements no `Debug` at
        // all so a signing key cannot reach a log through a stray `{:?}`. The
        // test bends to the security property, not the other way round.
        //
        // The rejected value is a distinctive marker rather than a plausible
        // word: an earlier revision passed `"short"` and then asserted the
        // message did not contain `"short"`, which failed for the wrong reason
        // — the refusal legitimately says "too short" about the LENGTH. What
        // must never appear is the KEY MATERIAL, so the fixture is a string
        // that could only get into the message by being echoed.
        let rejected = "Qz7";
        let err = match SessionKey::from_secret(rejected) {
            Err(err) => err,
            Ok(_) => panic!("a key below the minimum length must be refused"),
        };
        let text = err.to_string();
        assert!(!text.contains(rejected), "the refusal must not echo the key: {text}");
        assert!(text.contains(SIGNING_KEY_ENV));
        assert!(SessionKey::from_secret(&"k".repeat(MIN_KEY_LEN)).is_ok());
    }

    #[test]
    fn a_minted_session_round_trips() {
        let key = test_key();
        let id = Uuid::new_v4();
        let token = mint(&key, id, "operator", "jti-value", "csrf-value").expect("mint");
        let session = verify(&key, &token).expect("a freshly minted session must verify");
        assert_eq!(session.account_id(), id);
        assert_eq!(session.account_name(), "operator");
        assert_eq!(session.jti(), "jti-value");
        assert!(session.csrf_matches("csrf-value"));
    }

    /// The whole point of signing: a session minted under a different key — or
    /// tampered with — must not verify.
    #[test]
    fn a_session_signed_with_another_key_or_tampered_with_does_not_verify() {
        let token = mint(&other_key(), Uuid::new_v4(), "operator", "j", "c").expect("mint");
        assert!(verify(&test_key(), &token).is_none(), "a foreign signature must not verify");

        let mine = mint(&test_key(), Uuid::new_v4(), "operator", "j", "c").expect("mint");
        let mut tampered = mine.clone();
        // Flip a character in the payload segment; the signature no longer covers it.
        let mid = tampered.len() / 2;
        let replacement = if tampered.as_bytes()[mid] == b'A' { 'B' } else { 'A' };
        tampered.replace_range(mid..mid + 1, &replacement.to_string());
        assert!(verify(&test_key(), &tampered).is_none(), "a tampered token must not verify");

        for junk in ["", "not-a-jwt", "a.b.c"] {
            assert!(verify(&test_key(), junk).is_none(), "must refuse {junk:?}");
        }
    }

    /// A session with no `jti` could never be marked consumed, so it would be
    /// infinitely replayable at the code-issuance step.
    #[test]
    fn a_session_without_a_jti_or_csrf_is_refused() {
        let key = test_key();
        let no_jti = mint(&key, Uuid::new_v4(), "operator", "", "csrf-value").expect("mint");
        assert!(verify(&key, &no_jti).is_none());
        let no_csrf = mint(&key, Uuid::new_v4(), "operator", "jti-value", "").expect("mint");
        assert!(verify(&key, &no_csrf).is_none());
    }

    /// The CSRF comparison must reject a mismatch, a prefix, and an empty
    /// submission — a length-only or prefix comparison would accept the latter
    /// two.
    #[test]
    fn csrf_comparison_rejects_mismatches_prefixes_and_blanks() {
        let key = test_key();
        let token = mint(&key, Uuid::new_v4(), "operator", "j", "abcdef").expect("mint");
        let session = verify(&key, &token).expect("verify");
        assert!(session.csrf_matches("abcdef"));
        assert!(!session.csrf_matches("abcdeg"));
        assert!(!session.csrf_matches("abc"));
        assert!(!session.csrf_matches("abcdefg"));
        assert!(!session.csrf_matches(""));
    }

    /// Every attribute here is load-bearing; a missing one is a real
    /// vulnerability rather than a style lapse, so they are asserted.
    #[test]
    fn the_cookie_carries_every_required_attribute() {
        let header = set_cookie("a-token-value");
        assert!(header.starts_with("rmcp_login=a-token-value;"));
        assert!(header.contains("HttpOnly"), "{header}");
        assert!(header.contains("Secure"), "{header}");
        assert!(header.contains("SameSite=Lax"), "{header}");
        assert!(header.contains("Path=/oauth"), "{header}");
        assert!(header.contains(&format!("Max-Age={TTL_SECS}")), "{header}");
        // Not host-wide: a cookie on `/` would ride along to `/mcp`.
        assert!(!header.contains("Path=/;"), "{header}");
    }

    /// A clearing cookie that omits the attributes it was set with is ignored
    /// by some browsers, leaving the session installed.
    #[test]
    fn the_clearing_cookie_matches_the_setting_cookie() {
        let header = clear_cookie();
        assert!(header.contains("Max-Age=0"), "{header}");
        for attribute in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/oauth"] {
            assert!(header.contains(attribute), "{attribute} missing from {header}");
        }
    }

    /// Cookie parsing must match the exact name — a prefix match would read a
    /// neighbouring cookie an attacker could set as this one.
    #[test]
    fn cookie_parsing_matches_the_exact_name() {
        assert_eq!(token_from_cookie_header("rmcp_login=abc"), Some("abc"));
        assert_eq!(token_from_cookie_header("other=1; rmcp_login=abc; more=2"), Some("abc"));
        assert_eq!(token_from_cookie_header("other=1;rmcp_login=abc"), Some("abc"));
        assert_eq!(token_from_cookie_header("rmcp_login_other=abc"), None);
        assert_eq!(token_from_cookie_header("xrmcp_login=abc"), None);
        assert_eq!(token_from_cookie_header(""), None);
        assert_eq!(token_from_cookie_header("novalue"), None);
    }
}
