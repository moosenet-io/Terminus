//! RMCP — the OAuth 2.1 remote-MCP connector door (S132).
//!
//! ## Why this module exists
//! Terminus has three ways in today, and all three are private: the loopback
//! plain listener, the mTLS listener ([`crate::pki::mtls`]), and the tailnet
//! listener ([`crate::mesh::tailnet`]). Each binds a caller's identity to a
//! transport artifact — a client certificate CN, or a tailnet WhoIs result.
//! That works well for the fleet's own services and for machines the operator
//! enrolled by hand.
//!
//! It does not work for a hosted third-party client. Anthropic's Claude
//! surfaces reach an external MCP server over public HTTPS, authenticate with
//! OAuth 2.1, and arrive from a fixed egress range — they cannot present an
//! mTLS certificate this fleet issued, and they are not on the tailnet. This
//! module is the fourth door: an OAuth 2.1 authorization server plus
//! resource-server validation, whose output is a [`crate::mesh::Principal`]
//! that the EXISTING [`crate::gateway_framework`] authorization already
//! understands. The new door changes how a caller proves who they are; it does
//! not introduce a second way to decide what they may do.
//!
//! ## The scoping model, stated once
//! An internet-facing door onto 400 fleet tools is only safe if the door is
//! narrower than the room behind it. Every request through this module resolves
//! to an intersection (RMCP-07):
//!
//! ```text
//! effective = grant_of(account)          // what the HUMAN may do  (existing)
//!           ∩ tools_of(client.groups)    // what THIS connector may do
//!           ∩ namespaces(client.servers) // which federated servers it sees
//! ```
//!
//! The intersection can only ever REMOVE. There is deliberately no code path by
//! which a client scoping record grants a tool the account's own grant would
//! have denied — the same anti-widening discipline as
//! [`crate::gateway_framework`]'s guest clamp, and for the same reason: the
//! dangerous failure in an authorization change is never a spurious denial, it
//! is a silent widening that nobody notices until it is used.
//!
//! ## What RMCP-01 delivers
//! The persistence layer and nothing else. There is no HTTP surface here yet —
//! the metadata documents (RMCP-02), the authorize/token endpoints (RMCP-03,
//! RMCP-04), resource-server validation (RMCP-05) and the scoping resolver
//! (RMCP-07) each land as their own item on top of these types. This item is
//! deliberately unreachable from the network so the schema and its fail-closed
//! contracts can be reviewed on their own.
//!
//! ## Credential storage — nothing here is presentable
//! No table in this schema stores a usable credential:
//! - Authorization codes and refresh tokens are high-entropy machine-generated
//!   values, stored as SHA-256 hashes ([`secret_hash`]). They need no salt or
//!   work factor precisely because they are full-entropy and short-lived;
//!   argon2 on a 256-bit random value buys nothing and costs latency on the
//!   token endpoint, which has a 10-second budget.
//! - Client secrets and account passwords are stored as argon2id PHC strings,
//!   written by RMCP-03/RMCP-08 which own the verification path.
//!
//! ## Secret access (S7/S8)
//! This crate has no separate `SecretManager::get()` API; the runtime secret
//! store is materialized into the process environment at startup, so an env
//! read here IS the vault read. See [`crate::pki`]'s module docs for the full
//! rationale and [`crate::pg::conn`] for the established precedent this
//! mirrors. The connection URL is read in exactly one place
//! ([`OauthConfig::from_env`]) and is never logged, returned, or embedded in an
//! error.

pub mod model;
pub mod store;

use crate::error::ToolError;

/// Env var naming the Postgres connection this module's own data plane uses.
///
/// This is the S9-pg "application service owns its own data plane" case: the
/// OAuth store is Terminus's own state, not ad-hoc fleet-database access, so it
/// holds a pool rather than routing through the `pg_*` tools. Fleet queries by
/// an agent still go through those tools.
pub const DATABASE_URL_ENV: &str = "RMCP_DATABASE_URL";

/// Non-secret configuration for the OAuth door.
///
/// Deliberately does NOT derive `Debug`: the only field is a connection URL
/// with an embedded password, and a stray `{:?}` in a log line is exactly how
/// that leaks. Callers that want to describe this value get
/// [`OauthConfig::describe`], which names the source and never the value.
#[derive(Clone)]
pub struct OauthConfig {
    database_url: String,
}

impl OauthConfig {
    /// Read the configuration from the environment.
    ///
    /// Returns [`ToolError::NotConfigured`] when the URL is absent or blank —
    /// blank is treated as absent, matching `secrets_bootstrap`'s own rule that
    /// an empty materialized secret is a missing one rather than a valid empty
    /// credential. The error text names the VARIABLE, never its value.
    pub fn from_env() -> Result<Self, ToolError> {
        let database_url = std::env::var(DATABASE_URL_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                ToolError::NotConfigured(format!(
                    "{DATABASE_URL_ENV} not set — the RMCP OAuth door requires its own \
                     Postgres connection"
                ))
            })?;
        Ok(Self { database_url })
    }

    /// The connection URL, for the one caller that opens the pool.
    pub(crate) fn database_url(&self) -> &str {
        &self.database_url
    }

    /// A log-safe description. Names where the value came from; never the value.
    pub fn describe(&self) -> String {
        format!("RMCP OAuth store configured from {DATABASE_URL_ENV}")
    }
}

/// Proof that a caller has performed the ownership check before writing a
/// client's scope.
///
/// ## Why this type exists
/// Three review rounds objected — reasonably — that
/// [`store::OauthStore::set_client_tool_groups_unchecked`] and its namespace
/// counterpart will attach one account's tool groups, or an arbitrary
/// namespace, to another account's client. The reviewers' point was that a doc
/// comment and an `_unchecked` suffix are advisory: a caller added later can
/// still simply not do the check.
///
/// The ownership RULE still belongs in one place — RMCP-12 puts it in a single
/// guard that every write path calls, so it cannot be implemented two ways or
/// drift. What this type fixes is the other half: the scope-writing methods now
/// REQUIRE a value that can only be produced by explicitly claiming the check
/// was done. "Forgot to authorize" is no longer expressible; the worst a caller
/// can do is lie in a way that names itself at the call site and shows up in a
/// grep for the constructor.
///
/// This is the same idiom as [`crate::tool::CallerContext`]'s entitlement
/// constructors, which exist because the compiler is a better enforcer of an
/// authorization contract than a comment.
///
/// The field is private and carries no data: the value IS the claim.
#[derive(Debug, Clone, Copy)]
pub struct ScopeWriteAuthorization(());

impl ScopeWriteAuthorization {
    /// Assert that the caller has verified the actor owns both the client being
    /// scoped and every namespace and tool group being attached.
    ///
    /// RMCP-12's single ownership guard is the intended — and, once it lands,
    /// the only — caller. Anything else calling this is claiming an audit it
    /// did not perform, which is a reviewable defect rather than an accident.
    #[must_use]
    pub fn ownership_verified() -> Self {
        Self(())
    }
}

/// Hash a high-entropy machine-generated secret (an authorization code or a
/// refresh token) for storage and lookup.
///
/// SHA-256, unsalted and unstretched, and that is the correct choice here — not
/// a shortcut. These values are 256-bit random strings this server generated;
/// there is no dictionary to attack and no low-entropy input to stretch, so a
/// work factor would only add latency to the token endpoint. A salt would break
/// the property this function exists for: the store looks a token UP by its
/// hash, which requires the mapping to be deterministic.
///
/// Passwords and client secrets are the opposite case — attacker-chosen or
/// human-chosen, hence argon2id — and deliberately do NOT come through here.
pub fn secret_hash(secret: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.finalize().to_vec()
}

/// A digest of a high-entropy secret, which can ONLY be produced by hashing.
///
/// Review round 1 raised that the store's `insert_auth_code` /
/// `insert_refresh_token` took `&[u8]` parameters merely NAMED `*_hash`: a
/// caller could pass the plaintext code and the repository would store it
/// verbatim, silently defeating the property that a schema dump yields nothing
/// presentable. Naming a parameter is a comment; this type is a compiler check.
///
/// The inner field is private and the only constructor is [`Self::of`], which
/// hashes. There is deliberately no `From<Vec<u8>>`, no `new(bytes)`, and no way
/// to build one from a value that was not put through [`secret_hash`] — so
/// "stored hashed" is now true by construction rather than by discipline. This
/// is the same shape as [`crate::tool::CallerContext`]'s entitlement
/// constructors, and for the same reason.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretHash(Vec<u8>);

impl SecretHash {
    /// Hash a plaintext secret into a storable digest. The only way to make one.
    pub fn of(secret: &str) -> Self {
        Self(secret_hash(secret))
    }

    /// The digest bytes, for binding into a query.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// `Debug` prints the digest, never anything derived from the plaintext — and
/// the plaintext is not recoverable from here in any case. Written by hand
/// rather than derived so the intent is explicit at the point someone might
/// otherwise add a plaintext field to this struct.
impl std::fmt::Debug for SecretHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretHash(<{} bytes>)", self.0.len())
    }
}

/// The PHC identifier every password and client-secret hash must carry.
const ARGON2ID_ID: &str = "argon2id";

/// A structurally-validated argon2id PHC string.
///
/// The counterpart to [`SecretHash`] for the attacker-chosen secrets
/// (passwords, client secrets), which must be stretched rather than merely
/// digested. This type does not hash — the argon2 parameters belong with the
/// verifier in RMCP-03/RMCP-08, which has to read them back out anyway — but it
/// does enforce that whatever reaches the store is genuinely PHC-shaped, so a
/// plaintext password can never be written into a `*_hash` column by a caller
/// that forgot to hash.
///
/// Round 2 of review caught that an earlier revision checked only the
/// `$argon2id$` PREFIX, which accepts `$argon2id$plaintext` — a value that is
/// effectively the plaintext with a decorative prefix, and exactly the mistake
/// the guard exists to stop. Validation is now structural: the full PHC layout
/// per the [PHC string format], with the version and all three cost parameters
/// present and numeric, and non-trivial salt and hash segments.
///
/// This deliberately does NOT verify that the digest is *correct* for any
/// password — that is not knowable here and is not the point. It rules out
/// "this is not a hash at all", which is the failure that loses a password
/// database.
///
/// [PHC string format]: https://github.com/P-H-C/phc-string-format/blob/master/phc-sf-spec.md
#[derive(Clone, PartialEq, Eq)]
pub struct Argon2idHash(String);

/// `Debug` redacts the PHC string entirely.
///
/// A password hash is not a presentable credential, but it IS offline-crackable
/// — a leaked `client_secret_hash` in a log is a cracking target with the salt
/// and cost parameters helpfully attached. Round 3 of review flagged the derived
/// `Debug` here, correctly. Written by hand rather than derived so that adding a
/// field cannot silently re-expose the value.
impl std::fmt::Debug for Argon2idHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Argon2idHash(<redacted>)")
    }
}

impl Argon2idHash {
    /// Minimum encoded length of the SALT segment, derived from argon2's own
    /// 8-byte minimum salt (8 bytes -> 11 unpadded base64 characters).
    const MIN_SALT_LEN: usize = 11;

    /// Minimum encoded length of the DIGEST segment. argon2 permits a 4-byte
    /// output, but no real password hash uses one; 16 bytes (22 unpadded base64
    /// characters) is comfortably below the 32-byte default every argon2
    /// implementation actually emits, so this rejects a decorative value
    /// without risking a genuine hash.
    const MIN_DIGEST_LEN: usize = 22;

    /// Characters permitted in the salt and digest segments.
    ///
    /// The PHC string format specifies STANDARD base64 without `=` padding, so
    /// `A-Za-z0-9+/` is the correct alphabet and is what argon2's reference
    /// implementation emits. Round 3 of review asserted that argon2 PHC uses
    /// the crypt(3) alphabet containing `.`; that is not what the PHC
    /// specification says (it is bcrypt's alphabet). The finding is not adopted
    /// as stated — but its underlying RISK is real and worth insuring against:
    /// if any implementation in this fleet's future did emit `.`, `-` or `_`,
    /// rejecting its output would break authentication in production. So those
    /// three characters are accepted too.
    ///
    /// Widening the alphabet costs this guard nothing. Its job is to rule out
    /// "this is not a hash at all" — a plaintext password, an empty string, a
    /// decorative prefix — and no plausible plaintext secret passes the
    /// structural field, version, cost-parameter and length checks regardless
    /// of which of those characters are permitted.
    fn is_segment_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'.' | b'-' | b'_')
    }

    /// Accept a PHC string, rejecting anything not structurally argon2id.
    ///
    /// Expected layout, six `$`-separated fields (the leading `$` produces an
    /// empty first field):
    ///
    /// ```text
    /// $argon2id$v=19$m=19456,t=2,p=1$<salt-b64>$<hash-b64>
    /// ```
    pub fn parse(phc: &str) -> Result<Self, ToolError> {
        let refuse = |why: &str| {
            // The message never echoes the input: if a caller really did pass a
            // plaintext password, repeating it into an error string (and from
            // there into a log) would turn this guard into the leak it prevents.
            ToolError::InvalidArgument(format!(
                "password/client-secret hashes must be argon2id PHC strings ({why}); refusing                  to store a value that is not one — a plaintext secret reaching this column is                  the failure this check exists to prevent"
            ))
        };

        let fields: Vec<&str> = phc.split('$').collect();
        if fields.len() != 6 || !fields[0].is_empty() {
            return Err(refuse("expected $argon2id$v=..$m=..,t=..,p=..$salt$hash"));
        }
        if fields[1] != ARGON2ID_ID {
            return Err(refuse("algorithm is not argon2id"));
        }

        // Version. Argon2 has exactly two published versions — 0x10 (16) and
        // the current 0x13 (19) — so an arbitrary number here is not a real
        // hash. Accepting any digits was the weaker check round 4 flagged.
        match fields[2].strip_prefix("v=") {
            Some("19") | Some("16") => {}
            _ => return Err(refuse("missing or unsupported argon2 version (expected v=19 or v=16)")),
        }

        // Cost parameters: exactly m, t and p, each numeric. Checked by NAME
        // rather than by position so a reordered-but-valid PHC string is still
        // accepted.
        let mut seen_m = false;
        let mut seen_t = false;
        let mut seen_p = false;
        for param in fields[3].split(',') {
            let (key, value) = match param.split_once('=') {
                Some(kv) => kv,
                None => return Err(refuse("malformed cost parameter")),
            };
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(refuse("non-numeric cost parameter"));
            }
            // A REPEATED parameter is rejected rather than last-write-wins:
            // `m=1,m=999999` is not a hash any implementation emits, and
            // silently accepting one of two conflicting cost values is how a
            // weak parameter hides behind a strong-looking one.
            let slot = match key {
                "m" => &mut seen_m,
                "t" => &mut seen_t,
                "p" => &mut seen_p,
                _ => return Err(refuse("unknown cost parameter")),
            };
            if *slot {
                return Err(refuse("duplicate cost parameter"));
            }
            *slot = true;
        }
        if !(seen_m && seen_t && seen_p) {
            return Err(refuse("missing one of the m/t/p cost parameters"));
        }

        // Salt and digest: long enough to be real, and drawn from the base64
        // alphabet (see `is_segment_char`).
        let segment_ok = |segment: &str, min: usize| {
            segment.len() >= min && segment.bytes().all(Self::is_segment_char)
        };
        if !segment_ok(fields[4], Self::MIN_SALT_LEN) {
            return Err(refuse("salt is too short or not base64"));
        }
        if !segment_ok(fields[5], Self::MIN_DIGEST_LEN) {
            return Err(refuse("digest is too short or not base64"));
        }

        Ok(Self(phc.to_string()))
    }

    /// The PHC string, for binding into a query.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored hash must never equal the plaintext it was derived from. This
    /// is the property a schema dump depends on, so it is asserted rather than
    /// assumed.
    #[test]
    fn secret_hash_is_not_the_plaintext() {
        let secret = "<REDACTED-SECRET>";
        let hashed = secret_hash(secret);
        assert_ne!(hashed.as_slice(), secret.as_bytes());
        assert_eq!(hashed.len(), 32, "SHA-256 digests are 32 bytes");
    }

    /// Lookup by hash requires determinism — a salted hash would silently break
    /// every `find_*_by_hash` in the store.
    #[test]
    fn secret_hash_is_deterministic() {
        assert_eq!(secret_hash("same-input"), secret_hash("same-input"));
        assert_ne!(secret_hash("one-input"), secret_hash("another-input"));
    }

    /// A blank materialized secret is a MISSING one. If this ever returned
    /// `Ok`, the pool would be opened against an empty URL and fail later with
    /// a confusing connection error instead of a clear config error here.
    #[test]
    fn blank_database_url_is_treated_as_absent() {
        // Exercises the same filter `from_env` applies, without mutating
        // process-global environment state that would race other tests.
        let blank: Option<String> = Some("   ".to_string())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        assert!(blank.is_none(), "a whitespace-only URL must read as absent");
    }

    /// The whole point of the newtype: a digest can only come from hashing, so
    /// "stored hashed" is a compiler-checked fact rather than a naming
    /// convention. If someone adds a `SecretHash::from_bytes`, this test still
    /// passes — but the review comment on the type says why they should not.
    #[test]
    fn secret_hash_newtype_only_holds_a_digest() {
        let plaintext = "a-refresh-token-value";
        let wrapped = SecretHash::of(plaintext);
        assert_ne!(wrapped.as_bytes(), plaintext.as_bytes());
        assert_eq!(wrapped.as_bytes(), secret_hash(plaintext).as_slice());
    }

    /// Debug must never become a side channel, even though the plaintext is not
    /// recoverable — a digest in a log is still more than a length.
    #[test]
    fn secret_hash_debug_shows_only_a_length() {
        let rendered = format!("{:?}", SecretHash::of("some-secret"));
        assert_eq!(rendered, "SecretHash(<32 bytes>)");
        assert!(!rendered.contains("some-secret"));
    }

    const VALID_PHC: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG";

    /// A genuine argon2id PHC string must still be accepted — the guard is
    /// worthless if it is so strict that the real hasher's output fails it.
    #[test]
    fn argon2id_hash_accepts_a_real_phc_string() {
        let parsed =
            Argon2idHash::parse(VALID_PHC).expect("a real argon2id PHC string must be accepted");
        assert_eq!(parsed.as_str(), VALID_PHC);
        // Parameter order is not significant in PHC.
        assert!(Argon2idHash::parse(
            "$argon2id$v=19$t=2,m=19456,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG"
        )
        .is_ok());
        // Both published argon2 versions are accepted.
        assert!(Argon2idHash::parse(
            "$argon2id$v=16$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG"
        )
        .is_ok());
        // The crypt-style `.` is accepted too, so an implementation that emits
        // it cannot break authentication (see `is_segment_char`).
        assert!(Argon2idHash::parse(
            "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzYWx.$RdescudvJCsgt3ubXbXdWRWJTmaaJOb."
        )
        .is_ok());
    }

    /// The specific catastrophic mistake this guard exists for: a caller that
    /// forgot to hash and passed the plaintext straight through. The
    /// `$argon2id$plaintext` case is the one round 2 of review found slipping
    /// past a prefix-only check — it is the whole reason validation is
    /// structural.
    #[test]
    fn argon2id_hash_rejects_anything_not_structurally_argon2id() {
        for bad in [
            "",
            "hunter2",
            "$argon2id$plaintext",
            "$argon2id$",
            "$2b$12$abcdefghijklmnopqrstuv",
            "$argon2i$v=19$m=1,t=1,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=$m=1,t=1,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=xx$m=1,t=1,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=1,t=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=a,t=1,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=1,t=1,p=1,q=9$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            // Round 4: a repeated cost parameter must not be last-write-wins.
            "$argon2id$v=19$m=1,m=99999,t=1,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            // Round 4: only argon2's two published versions are real.
            "$argon2id$v=99$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=0$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=1,t=1,p=1$short$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=1,t=1,p=1$c29tZXNhbHRzYWx0$sh",
            "$argon2id$v=19$m=1,t=1,p=1$c29tZXNhbHRzYWx0$has h$extra",
        ] {
            assert!(
                Argon2idHash::parse(bad).is_err(),
                "must refuse a non-argon2id value"
            );
        }
    }

    /// A password hash is offline-crackable, so it must never reach a log.
    #[test]
    fn argon2id_debug_is_redacted() {
        let parsed = Argon2idHash::parse(VALID_PHC).expect("valid");
        let rendered = format!("{parsed:?}");
        assert_eq!(rendered, "Argon2idHash(<redacted>)");
        assert!(!rendered.contains("RdescudvJCsgt3ub"));
        assert!(!rendered.contains("c29tZXNhbHRzYWx0"));
    }

    /// The refusal must not echo the rejected value: if a caller really did
    /// pass a plaintext password, repeating it into an error (and thence a log)
    /// would make this guard the leak it exists to prevent.
    #[test]
    fn argon2id_refusal_never_echoes_the_input() {
        let err = Argon2idHash::parse("correct-horse-battery-staple")
            .expect_err("plaintext must be refused");
        assert!(!err.to_string().contains("correct-horse-battery-staple"));
    }

    /// The config's own description must not be a channel for the URL.
    ///
    /// The fixture deliberately uses a DOTLESS host. The repo's own
    /// `no_pii_in_own_source_tree` self-check walks this file, and its email
    /// detector fires on a user part, an at-sign, and a dotted domain — which
    /// is exactly the shape of a realistic database DSN. So a natural-looking
    /// connection string in a test fixture (or even in a comment describing
    /// one) fails the PII gate rather than the assertion. A dotless host keeps
    /// the credential-in-URL shape this test is actually about.
    #[test]
    fn describe_never_contains_the_url() {
        let cfg = OauthConfig {
            database_url: "postgres://dbuser:not-a-real-password@db-host:5432/rmcp".to_string(),
        };
        let described = cfg.describe();
        assert!(!described.contains("not-a-real-password"));
        assert!(!described.contains("postgres://"));
        assert!(!described.contains("db-host"));
    }
}
