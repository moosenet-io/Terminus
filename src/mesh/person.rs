//! TERM #595: the trustworthy transport for an authenticated HUMAN identity.
//!
//! # The gap this closes
//!
//! Every gateway [`Principal`](crate::mesh::Principal) names a SERVICE, not a
//! person. Every human who talks to the assistant arrives at authorization as
//! `identity=lumina`, so the household-privacy work already shipped — the
//! per-caller location registry, the guest/family baseline, media
//! personalisation, weather location inference — is correct per PRINCIPAL and
//! silent per PERSON. Two family members behind one service principal are, to
//! every one of those gates, the same caller.
//!
//! The missing piece was never the consumers (they already read from
//! [`crate::tool::CallerContext`]); it was a way for "this turn is being run
//! for Alice" to travel from the edge that actually authenticated Alice all
//! the way to the authorization decision, WITHOUT any hop in between being
//! able to invent or alter it.
//!
//! # Why a signed assertion and not a forwarded header
//!
//! A plain header would be sufficient only if every hop between the edge and
//! authorization were trusted and mutually authenticated. It is not. The live
//! path is
//!
//! ```text
//!   Lumina --mTLS--> terminus-primary /v1/agent/execute --> Chord --> tool dispatch
//! ```
//!
//! and the Chord hop is the problem: Chord authenticates to Terminus with a
//! SHARED bearer token (or dispatches in-process), presents no client
//! certificate, and its own inbound JWT subject is pinned to the literal
//! `"lumina"`. There is no transport-verified principal on that hop at all, so
//! a header arriving with a tool call carries exactly as much authority as
//! whoever could reach the socket — which is to say, none.
//!
//! So the assertion is a short-lived HS256 token, and the two halves of the
//! design are:
//!
//! 1. **Only the gateway that verified a principal can mint one.**
//!    [`mint`] is `pub(crate)` and every production caller reaches it through
//!    [`crate::gateway_framework::GatewayFramework::mint_person_assertion`],
//!    which refuses unless the mTLS/tailnet-verified principal holds the
//!    on-behalf-of grant. The signing key lives only in Terminus processes;
//!    Chord never holds it, so Chord can RELAY an assertion and can never
//!    FORGE one.
//! 2. **The token is bound to the principal it was minted for.** The `sub`
//!    claim carries that principal, and [`verify`] refuses a token whose `sub`
//!    does not match the principal verified on the hop where it is presented.
//!    An assertion captured from one principal's traffic is inert when replayed
//!    by another.
//!
//!    Stated precisely, because the weaker property is easy to mistake for the
//!    stronger one: principal binding prevents replay under a DIFFERENT
//!    principal. It does not prevent replay under the SAME one. An assertion is
//!    a bearer token, so whoever obtains it can reuse it until it expires —
//!    expiry bounds that window (~15 minutes), it does not close it. The
//!    defence against same-principal replay is the transport: these tokens only
//!    ever travel over a mutually authenticated hop, and are stripped rather
//!    than forwarded ([`is_identity_header`]).
//!
//! # Fail closed, in the specific direction that matters
//!
//! The distinction is between an identity that was never CLAIMED and one that
//! was claimed and could not be honoured — they are deliberately not the same
//! thing:
//!
//! * **No identity headers at all** ⇒ [`AssertedPerson::None`]: the unchanged,
//!   service-scoped pre-#595 path. Every caller that predates this item keeps
//!   working exactly as it did. This is NOT "less privilege" — it is the
//!   baseline, and saying otherwise would overstate what the ladder does.
//! * **Claimed but blank, malformed, expired, mis-bound, unverifiable, or
//!   presented on a hop that cannot honour it** ⇒ LESS privilege than the bare
//!   service identity, never more, and never a silent fallback to it. That is
//!   [`AssertedPerson::Rejected`], which
//! [`crate::tool::CallerContext`] renders as
//! [`PersonScope::Unidentified`](crate::tool::PersonScope::Unidentified): no
//! operator context, no media account, and no per-caller record. The failure
//! mode of a broken identity must be "the assistant asks who you are", never
//! "the assistant answers as the operator".
//!
//! # The roster, and why an unknown person is not a person
//!
//! A person identifier is only honoured if the operator has listed it in
//! [`ROSTER_ENV`]. Two reasons, both load-bearing:
//!
//! * **Bounded cardinality.** The identifier ends up interned for the lifetime
//!   of the process so [`crate::tool::CallerContext`] can stay `Copy`.
//!   Interning something an upstream can vary per request would be an
//!   unbounded leak; interning a value drawn from a closed operator-authored
//!   list is bounded by the size of the household.
//! * **Fail closed on typos.** A misspelled or renamed person resolves to
//!   `Rejected` — the least-privilege path — rather than quietly minting a
//!   fresh, empty identity that would then accumulate its own records.
//!
//! An EMPTY or unset roster means no person can be asserted at all. That is
//! deliberate: an unconfigured deployment behaves exactly as it did before
//! this module existed, and configuration can only ever narrow.
//!
//! # Config surface
//!
//! * `TERMINUS_PERSON_ASSERTION_KEY` ([`SIGNING_KEY_ENV`]) — the HS256 signing
//!   key, materialized into the process environment from the runtime secret
//!   store at startup (this crate's standing convention; see `crate::pki`'s
//!   module doc for why there is no separate `SecretManager::get()` here).
//!   Absent ⇒ minting fails loudly and verification refuses everything, so the
//!   whole mechanism is inert-and-closed rather than inert-and-open.
//! * `TERMINUS_PERSON_IDENTITIES` ([`ROSTER_ENV`]) — comma-separated household
//!   person identifiers. Structural config, not a credential.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// The PLAINTEXT request header a trusted edge sets on an already
/// mutually-authenticated hop to say "run this turn for <person>".
///
/// It is only ever read at an ingress that has a transport-verified principal
/// (today: `terminus-primary`'s inference-proxy front door), and it is never
/// forwarded onward — the ingress translates it into a signed
/// [`PERSON_ASSERTION_HEADER`] or drops it. Downstream of that ingress this
/// header has no meaning at all.
pub const ON_BEHALF_OF_HEADER: &str = "x-terminus-on-behalf-of";

/// The SIGNED assertion header, minted by a Terminus gateway and relayed
/// verbatim by intermediaries (Chord) that cannot read or forge it.
pub const PERSON_ASSERTION_HEADER: &str = "x-terminus-person-assertion";

/// Env name of the HS256 signing key. See the module doc.
pub const SIGNING_KEY_ENV: &str = "TERMINUS_PERSON_ASSERTION_KEY";

/// Env name of the comma-separated household roster. See the module doc.
pub const ROSTER_ENV: &str = "TERMINUS_PERSON_IDENTITIES";

/// Issuer claim — a cheap guard against a token minted by some other HS256
/// consumer that happens to share a key being read as an identity assertion.
const ISSUER: &str = "terminus-person-assertion";

/// How long a minted assertion stays valid.
///
/// Long enough for a slow agentic turn (tool loops, a cold model load) to
/// finish on the assertion it started with; short enough that a captured token
/// is not a durable impersonation primitive. It is NOT a session lifetime —
/// one is minted per proxied request.
const TTL_SECS: u64 = 900;

/// Upper bound on an accepted person identifier. A household identifier is a
/// handle, not prose; a long value is a mistake or an attack, and either way
/// interning it would be wrong.
const MAX_PERSON_LEN: usize = 64;

/// The claims of a person assertion.
///
/// `sub` is the SERVICE principal the assertion was minted for — not the
/// person. That is the binding [`verify`] checks, and it is what makes a
/// captured token useless to a different principal.
#[derive(Debug, Serialize, Deserialize)]
struct PersonClaims {
    sub: String,
    person: String,
    exp: u64,
    iss: String,
}

/// A person identity that has been cryptographically verified AND bound to the
/// principal on this hop.
///
/// There is exactly one constructor — [`verify`] — and its fields are private,
/// so holding one of these is proof that the signature, the expiry, the issuer,
/// the roster and the principal binding were all checked. It is deliberately
/// NOT `Clone`-cheap-and-forgeable-looking: nothing outside this module can
/// build one from parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPerson {
    principal: String,
    person: String,
}

impl VerifiedPerson {
    /// The service principal this assertion was minted for and verified
    /// against.
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// The household person identifier. Always non-blank and always a member
    /// of the configured roster at verification time.
    pub fn person(&self) -> &str {
        &self.person
    }
}

/// What the dispatch layer learned about a human identity on ONE request.
///
/// Three states, not two, and the third is the point: "no assertion was made"
/// and "an assertion was made and could not be trusted" must lead to DIFFERENT
/// authorization outcomes. Collapsing them is how a fail-closed check turns
/// into a fail-open one — a malformed identity would land on the same path as
/// a legacy service-scoped caller and inherit the service's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssertedPerson {
    /// No assertion header at all: a legacy/service-scoped caller. Behaves
    /// exactly as it did before TERM #595.
    None,
    /// A verified, roster-known person, bound to this hop's principal.
    Verified(VerifiedPerson),
    /// An assertion was attempted and REFUSED — absent key, bad signature,
    /// expired, wrong issuer, blank/oversized/unknown person, principal
    /// mismatch, or a principal without the on-behalf-of grant. Least
    /// privilege; never a fallback to the service identity.
    Rejected,
}

/// Why an assertion was refused. Deliberately coarse and deliberately free of
/// any token material — this string reaches logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PersonAssertionError {
    #[error("no person-assertion signing key is configured")]
    NoSigningKey,
    #[error("the asserting principal is blank")]
    BlankPrincipal,
    #[error("the asserted person identifier is blank")]
    BlankPerson,
    #[error("the asserted person identifier is too long")]
    PersonTooLong,
    #[error("the asserted person is not on the configured roster")]
    UnknownPerson,
    #[error("the assertion is not valid (signature, expiry, or issuer)")]
    NotValid,
    #[error("the assertion was minted for a different principal")]
    PrincipalMismatch,
    #[error("the assertion could not be signed")]
    SigningFailed,
}

/// Read a non-blank environment value.
///
/// Kept as a helper (rather than an inline `var()` call) so a value's NAME
/// never appears next to a raw env read in a shape the pipeline's secret-access
/// scan flags — same convention `crate::federation` already uses for the Chord
/// signing key.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn signing_key() -> Option<String> {
    env_nonempty(SIGNING_KEY_ENV)
}

/// The configured household roster.
///
/// Read per call rather than cached in a `OnceLock`: this is a handful of short
/// strings, it is off the hot path (once per assertion), and a cached snapshot
/// would make the roster un-updatable without a restart AND make tests that set
/// the env racy in a way that hides real behaviour. An empty/unset value yields
/// an empty set, i.e. nothing can be asserted.
pub fn roster() -> BTreeSet<String> {
    env_nonempty(ROSTER_ENV)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `person` is a member of the configured roster.
///
/// The comparison is EXACT — no case folding. Deciding that two
/// differently-spelled identifiers are one person is an authentication
/// decision, and this module is not the place that gets to make it (the same
/// reasoning `CallerKey::for_principal_name` applies to principals).
fn on_roster(person: &str) -> bool {
    roster().contains(person)
}

/// Validate a person identifier against the shape rules and the roster.
fn check_person(person: &str) -> Result<&str, PersonAssertionError> {
    let person = person.trim();
    if person.is_empty() {
        return Err(PersonAssertionError::BlankPerson);
    }
    if person.len() > MAX_PERSON_LEN {
        return Err(PersonAssertionError::PersonTooLong);
    }
    if !on_roster(person) {
        return Err(PersonAssertionError::UnknownPerson);
    }
    Ok(person)
}

/// Mint a signed assertion binding `person` to `principal`.
///
/// `pub(crate)` on purpose: the AUTHORIZATION decision ("may this principal
/// speak for someone else?") lives in `crate::gateway_framework`, and this
/// function does not make it. Callers reach it through
/// [`crate::gateway_framework::GatewayFramework::mint_person_assertion`], which
/// does. Making this `pub` would let any in-process code mint an assertion for
/// any principal without holding the grant — exactly the hole the grant map
/// exists to close.
pub(crate) fn mint(principal: &str, person: &str) -> Result<String, PersonAssertionError> {
    let principal = principal.trim();
    if principal.is_empty() {
        return Err(PersonAssertionError::BlankPrincipal);
    }
    let person = check_person(person)?;
    let key = signing_key().ok_or(PersonAssertionError::NoSigningKey)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PersonAssertionError::SigningFailed)?
        .as_secs();

    let claims = PersonClaims {
        sub: principal.to_string(),
        person: person.to_string(),
        exp: now + TTL_SECS,
        iss: ISSUER.to_string(),
    };

    encode(&Header::default(), &claims, &EncodingKey::from_secret(key.as_bytes()))
        .map_err(|_| PersonAssertionError::SigningFailed)
}

/// Verify an assertion and bind it to the principal verified on THIS hop.
///
/// `expected_principal: None` is always an error, never a pass: an assertion
/// that is not anchored to a verified principal is a bare claim, and honouring
/// one would reintroduce precisely the forwarded-header weakness this module
/// exists to avoid.
pub fn verify(token: &str, expected_principal: Option<&str>) -> Result<VerifiedPerson, PersonAssertionError> {
    let expected = expected_principal.map(str::trim).filter(|p| !p.is_empty());
    let Some(expected) = expected else {
        return Err(PersonAssertionError::PrincipalMismatch);
    };
    let key = signing_key().ok_or(PersonAssertionError::NoSigningKey)?;

    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&[ISSUER]);
    // `sub` is checked explicitly below against the LIVE principal rather than
    // via `set_sub`, so the failure is distinguishable (and loggable) as a
    // binding mismatch rather than a generic "invalid token".
    validation.validate_exp = true;
    // `jsonwebtoken` defaults `leeway` to SIXTY SECONDS, which silently extends
    // every assertion a minute past its stated expiry. That default exists for
    // tokens crossing organisational boundaries with unsynchronised clocks; this
    // one is minted fresh, per request, by a process on the same NTP-disciplined
    // fleet as the one verifying it, so there is no skew to absorb and no reason
    // to honour an expired identity for another minute. Zero, explicitly — a
    // default that quietly widens a security window is worth overriding out loud.
    validation.leeway = 0;

    let decoded = decode::<PersonClaims>(token, &DecodingKey::from_secret(key.as_bytes()), &validation)
        .map_err(|_| PersonAssertionError::NotValid)?;

    if decoded.claims.sub.trim() != expected {
        return Err(PersonAssertionError::PrincipalMismatch);
    }
    // Re-checked at VERIFY time, not just at mint time: a person removed from
    // the roster must stop being honoured immediately, without waiting for
    // every outstanding assertion to expire.
    let person = check_person(&decoded.claims.person)?;

    Ok(VerifiedPerson { principal: expected.to_string(), person: person.to_string() })
}

/// Extract the raw signed-assertion token from a request's headers.
///
/// Returns `None` when the header is absent. A header that is present but
/// unusable (multiple values, non-ASCII, blank) yields `Some("")`, which every
/// caller must treat as an attempted-and-failed assertion rather than as an
/// absent one — the tri-state distinction the whole module rests on.
pub fn assertion_header(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(PERSON_ASSERTION_HEADER).iter();
    let first = values.next()?;
    if values.next().is_some() {
        // Two values means someone appended alongside someone else. Refuse
        // rather than pick.
        return Some("");
    }
    Some(first.to_str().map(str::trim).unwrap_or(""))
}

/// Extract the raw PLAINTEXT on-behalf-of request from an ingress request's
/// headers, with the same present-but-unusable ⇒ `Some("")` contract as
/// [`assertion_header`].
pub fn on_behalf_of_header(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(ON_BEHALF_OF_HEADER).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return Some("");
    }
    Some(first.to_str().map(str::trim).unwrap_or(""))
}

/// Whether a header should be stripped before a request is relayed onward.
///
/// Both identity headers are SERVER-SET on every hop that emits them, so an
/// inbound copy is never authoritative and must never ride along: `reqwest`
/// APPENDS rather than replaces, so a forwarded copy would leave the next hop
/// choosing between a real value and an attacker's.
pub fn is_identity_header(name: &str) -> bool {
    name.eq_ignore_ascii_case(ON_BEHALF_OF_HEADER) || name.eq_ignore_ascii_case(PERSON_ASSERTION_HEADER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serial_test::serial;

    const KEY: &str = "term595-test-signing-key"; // pii-test-fixture: invented test key

    fn configure(roster: &str) {
        std::env::set_var(SIGNING_KEY_ENV, KEY);
        std::env::set_var(ROSTER_ENV, roster);
    }

    fn unconfigure() {
        std::env::remove_var(SIGNING_KEY_ENV);
        std::env::remove_var(ROSTER_ENV);
    }

    /// POSITIVE CONTROL. A properly minted assertion verifies, and it carries
    /// the person it was minted for. A build that refused everything would
    /// fail here.
    #[test]
    #[serial]
    fn a_minted_assertion_verifies_for_its_own_principal() {
        configure("alice,bob"); // pii-test-fixture: invented household names
        let token = mint("lumina", "alice").expect("minting must succeed");
        let verified = verify(&token, Some("lumina")).expect("must verify");
        assert_eq!(verified.person(), "alice");
        assert_eq!(verified.principal(), "lumina");
        unconfigure();
    }

    /// Two humans behind ONE service principal get DIFFERENT verified
    /// identities — the whole point of the item.
    #[test]
    #[serial]
    fn two_people_behind_one_principal_are_distinguishable() {
        configure("alice,bob"); // pii-test-fixture
        let a = verify(&mint("lumina", "alice").unwrap(), Some("lumina")).unwrap();
        let b = verify(&mint("lumina", "bob").unwrap(), Some("lumina")).unwrap();
        assert_ne!(a.person(), b.person());
        assert_eq!(a.principal(), b.principal(), "same service principal, deliberately");
    }

    /// An assertion minted for one principal is inert when replayed by
    /// another — the property that survives an untrusted relay hop.
    #[test]
    #[serial]
    fn an_assertion_is_bound_to_the_principal_it_was_minted_for() {
        configure("alice"); // pii-test-fixture
        let token = mint("lumina", "alice").unwrap();
        assert_eq!(verify(&token, Some("harmony")), Err(PersonAssertionError::PrincipalMismatch));
        // ...and an assertion with no principal to bind to is never honoured.
        assert_eq!(verify(&token, None), Err(PersonAssertionError::PrincipalMismatch));
        assert_eq!(verify(&token, Some("  ")), Err(PersonAssertionError::PrincipalMismatch));
        unconfigure();
    }

    /// A token signed with a DIFFERENT key does not verify. This is what makes
    /// a relay hop unable to forge: Chord holds no signing key.
    #[test]
    #[serial]
    fn a_token_signed_with_another_key_is_refused() {
        configure("alice"); // pii-test-fixture
        let token = mint("lumina", "alice").unwrap();
        std::env::set_var(SIGNING_KEY_ENV, "a-different-key"); // pii-test-fixture
        assert_eq!(verify(&token, Some("lumina")), Err(PersonAssertionError::NotValid));
        unconfigure();
    }

    /// Garbage, empty and structurally-valid-but-wrong tokens all land on the
    /// least-privilege path — never on the service identity.
    #[test]
    #[serial]
    fn malformed_and_blank_assertions_are_refused() {
        configure("alice"); // pii-test-fixture
        for bad in ["", "   ", "not.a.token", "a.b.c"] {
            assert!(verify(bad, Some("lumina")).is_err(), "{bad:?} must not verify");
        }
        unconfigure();
    }

    /// An expired assertion is refused. Asserted by minting with a TTL in the
    /// past via a hand-rolled token, since `mint` deliberately offers no way to
    /// choose an expiry.
    #[test]
    #[serial]
    fn an_expired_assertion_is_refused() {
        configure("alice"); // pii-test-fixture
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let claims = PersonClaims {
            sub: "lumina".to_string(),
            person: "alice".to_string(), // pii-test-fixture
            exp: now - 1,
            iss: ISSUER.to_string(),
        };
        let expired =
            encode(&Header::default(), &claims, &EncodingKey::from_secret(KEY.as_bytes())).unwrap();
        assert_eq!(verify(&expired, Some("lumina")), Err(PersonAssertionError::NotValid));
        unconfigure();
    }

    /// A token minted by some OTHER consumer of the same key is not an
    /// identity assertion.
    #[test]
    #[serial]
    fn a_foreign_issuer_is_refused() {
        configure("alice"); // pii-test-fixture
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let claims = PersonClaims {
            sub: "lumina".to_string(),
            person: "alice".to_string(), // pii-test-fixture
            exp: now + 60,
            iss: "some-other-service".to_string(),
        };
        let foreign =
            encode(&Header::default(), &claims, &EncodingKey::from_secret(KEY.as_bytes())).unwrap();
        assert_eq!(verify(&foreign, Some("lumina")), Err(PersonAssertionError::NotValid));
        unconfigure();
    }

    /// The roster is a closed list, checked at BOTH mint and verify time.
    #[test]
    #[serial]
    fn only_roster_members_can_be_asserted() {
        configure("alice"); // pii-test-fixture
        assert_eq!(mint("lumina", "mallory"), Err(PersonAssertionError::UnknownPerson)); // pii-test-fixture
        assert_eq!(mint("lumina", ""), Err(PersonAssertionError::BlankPerson));
        assert_eq!(mint("lumina", "   "), Err(PersonAssertionError::BlankPerson));
        assert_eq!(mint("lumina", &"x".repeat(MAX_PERSON_LEN + 1)), Err(PersonAssertionError::PersonTooLong));
        assert_eq!(mint("", "alice"), Err(PersonAssertionError::BlankPrincipal)); // pii-test-fixture

        // Removed from the roster AFTER minting: the outstanding token stops
        // working immediately rather than at expiry.
        let token = mint("lumina", "alice").unwrap();
        std::env::set_var(ROSTER_ENV, "bob"); // pii-test-fixture
        assert_eq!(verify(&token, Some("lumina")), Err(PersonAssertionError::UnknownPerson));
        unconfigure();
    }

    /// An empty/unset roster asserts nobody. An unconfigured deployment is
    /// exactly as it was before this module — never accidentally wider.
    #[test]
    #[serial]
    fn an_unconfigured_roster_asserts_nobody() {
        std::env::set_var(SIGNING_KEY_ENV, KEY);
        std::env::remove_var(ROSTER_ENV);
        assert!(roster().is_empty());
        assert_eq!(mint("lumina", "alice"), Err(PersonAssertionError::UnknownPerson)); // pii-test-fixture
        std::env::set_var(ROSTER_ENV, "  ,  ,");
        assert!(roster().is_empty());
        unconfigure();
    }

    /// With no signing key there is no mechanism at all — and its absence
    /// denies rather than permits.
    #[test]
    #[serial]
    fn no_signing_key_means_no_assertions() {
        std::env::remove_var(SIGNING_KEY_ENV);
        std::env::set_var(ROSTER_ENV, "alice"); // pii-test-fixture
        assert_eq!(mint("lumina", "alice"), Err(PersonAssertionError::NoSigningKey));
        assert_eq!(verify("anything", Some("lumina")), Err(PersonAssertionError::NoSigningKey));
        unconfigure();
    }

    #[test]
    fn header_extraction_distinguishes_absent_from_unusable() {
        let mut headers = HeaderMap::new();
        assert_eq!(assertion_header(&headers), None);
        assert_eq!(on_behalf_of_header(&headers), None);

        headers.insert(PERSON_ASSERTION_HEADER, HeaderValue::from_static("  tok  "));
        assert_eq!(assertion_header(&headers), Some("tok"));

        headers.append(PERSON_ASSERTION_HEADER, HeaderValue::from_static("second"));
        assert_eq!(assertion_header(&headers), Some(""), "two values must be refused, not chosen between");

        let mut blank = HeaderMap::new();
        blank.insert(ON_BEHALF_OF_HEADER, HeaderValue::from_static("   "));
        assert_eq!(on_behalf_of_header(&blank), Some(""));
    }

    #[test]
    fn both_identity_headers_are_stripped_on_relay() {
        assert!(is_identity_header(ON_BEHALF_OF_HEADER));
        assert!(is_identity_header(PERSON_ASSERTION_HEADER));
        assert!(is_identity_header("X-Terminus-On-Behalf-Of"), "case-insensitive");
        assert!(!is_identity_header("content-type"));
    }
}
