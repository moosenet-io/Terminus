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
//! ## What is here so far
//! - **RMCP-01** — the persistence layer ([`model`], [`store`]) and the two
//!   credential-storage newtypes below. It landed with no network surface of
//!   its own, so the schema and its fail-closed contracts could be reviewed
//!   alone.
//! - **RMCP-02** — discovery ([`metadata`], [`router`]): the two `.well-known`
//!   documents and the `401` challenge that points at them. The only part of
//!   this module with NO store dependency — discovery must answer while the
//!   database is down, because "the connector is unreachable" and "the
//!   connector's database is down" are different operator problems that must
//!   not present identically.
//! - **RMCP-03** — the interactive half of the flow: [`authorize`] (the
//!   endpoint and its state machine), [`session`] (the signed, short-lived
//!   login cookie), [`password`] (argon2id verification) and [`templates`]
//!   (the server-rendered login and consent pages). Read [`authorize`]'s
//!   module docs before changing anything in it: the ORDER of its checks is a
//!   security property, not a style choice, because the first two failures
//!   must never produce a redirect.
//! - **RMCP-04** — the token endpoint ([`token`]) and access-token minting and
//!   verification ([`jwt`]). A standalone router, merged by [`mount`] (which
//!   rebuilds its route in order to rate-limit it, then delegates to
//!   [`token::TokenEndpoint::handle`] unchanged).
//!
//! - **RMCP-07** — the scoping resolver ([`scope`]): the intersection above,
//!   as one function ([`scope::decide`]) that backs BOTH the `tools/list`
//!   filter and the `tools/call` guard, plus [`scope::ScopeResolver`], which
//!   reads a client's scope rows, and the cache that keeps it off the hot path.
//!   It is wired into [`crate::gateway_framework`] and [`crate::mcp_server`],
//!   but it only ever engages for a request that resolved to a
//!   [`scope::ClientScope`] — which only an OAuth-authenticated caller can — so
//!   every existing mTLS and tailnet caller is unaffected byte-for-byte. See
//!   the second RISK note below for what is still missing on that path.
//!
//! ## Where the current state is written down
//!
//! **In the README, under *Exactly what is wired today* — not here.** That
//! section is the single account of what is mounted, what is enforced, and what
//! is still missing, and this module deliberately does not restate it.
//!
//! That is not a filing preference. This file previously carried its own status
//! prose, written a round at a time, and it drifted into saying the endpoints
//! were unreachable while `terminus_primary` was serving them — the same
//! contradiction the README had one round earlier, reproduced here by the same
//! mechanism. Two accounts of one state is how both happened. Anything that
//! becomes untrue should be edited in that README section; a note here would
//! become the third.
//!
//! The two ⚠ RISK sections below are the exception, and are disclosures rather
//! than a status account: they describe residual gaps in controls THIS module
//! ships, which is where a reader of this code needs them.
//!
//! ## What RMCP-09 adds ([`edge`])
//! The public door's NETWORK policy. [`edge`] is the separate internet-facing
//! listener and the per-path source-address policy that governs it — it decides
//! which requests are allowed to reach a handler, not what any handler does.
//! [`mount`]'s router is what it serves.
//! ## What RMCP-11 adds
//! The operational safety layer over the door, and the wiring that makes the
//! door exist: [`mount`] binds RMCP-03's, RMCP-04's and this item's routers into
//! the process router, served by the private listeners and by [`edge`] alike.
//! - [`limits`] — a per-endpoint, two-dimensional rate limiter with a bounded
//!   key space. Now the door's single budget table: the login POST, the token
//!   endpoint and revocation all draw on it, and RMCP-03's private login
//!   limiter was converged onto it (TERM #633) rather than left as a second
//!   definition that could drift.
//! - [`audit`] — the OAuth event vocabulary and a record type that accepts no
//!   free-form text at all, so there is nothing to redact and no redaction to
//!   trust.
//! - [`revoke`] — RFC 7009 revocation, session listing, the operator tools'
//!   implementation, and [`revoke::SessionStore::dispatch_state`], the
//!   per-family dispatch predicate — see the RISK note below for what is wired
//!   today and what waits on TERM #635.
//!
//! ## ⚠ RISK: revoking ONE session among several does not cut it off (TERM #635)
//!
//! Read this before treating per-session revocation as a complete control.
//!
//! As of RMCP-05 the dispatch path DOES re-derive session state on every call,
//! so the blanket warning this notice used to carry — "nothing checks, so a
//! revoked token works until it expires" — is no longer true. The guarantee an
//! operator can rely on is stated once, in RMCP-05's README section
//! (*Presenting the token*), and is deliberately not restated here: two
//! documents describing one guarantee is how they drift.
//!
//! What remains true, and is this module's to disclose because these are the
//! controls it ships:
//!
//! * The wired check asks whether ANY session is live for an `(account,
//!   client)` pair, because an access token carries no session claim and the
//!   server cannot tell which session presented it. Its rejection is named
//!   `AllSessionsRevoked` for exactly that reason.
//! * So revoking **every** session for a pair — or disabling the client,
//!   disabling the account, or revoking consent — denies the next dispatch.
//!   Revoking **one** session while another is live for the same pair does
//!   **not**: that access token keeps working until it expires. Its refresh
//!   token is already dead, so the session cannot be extended, but it is not an
//!   immediate cut-off.
//! * **TERM #635** (a session claim in the token) is the named blocker, and
//!   RMCP-05 carries a tripwire test asserting today's permissive outcome so it
//!   fails loudly when the fix lands.
//!
//! [`revoke::RevocationService::dispatch_state`] is the per-family
//! implementation that replaces the current check once #635 gives a token a
//! session to name. It is deliberately NOT called from the dispatch path today:
//! RMCP-05's check is the wired one, and a second live checker would be the
//! dual-writer hazard this subsystem has already been bitten by.
//!
//! ## What an authenticated connector reaches
//!
//! `terminus_primary` derives its scope source from the door itself, through
//! [`scope::scope_source_for_door`] — the door keeps its `OauthStore` handle
//! and the resolver shares it, so there is one pool and one answer to "is the
//! door up" (TERM #631, item 5).
//!
//! So a connector reaches exactly the intersection of its account's grant, its
//! tool groups and its namespaces. That means it still reaches NOTHING until an
//! operator has actually scoped it: a client with no group rows resolves to
//! [`scope::ClientScope::empty`], and so does a client on a process whose door
//! carries no store. Absence is the empty set at every level, never a default.
//!
//! Worth saying plainly, because the two cases present identically to an
//! operator who links a connector and sees no tools: an UNSCOPED client is
//! working as designed, and only a scoped client seeing nothing is a fault.
//! `handle_mcp` logs a warning for the second-order case (a connector arriving
//! at a process with no scope source at all).
//!
//! ### Which audit emission points are live, and which are deferred
//!
//! Review round 2 made the fair point that a structurally safe audit record
//! nothing emits is not an audit trail. So, explicitly:
//!
//! **Emitting today**, from code this item owns:
//! - every rate-limit refusal, from [`limits::OauthRateLimiter::check`] itself
//!   rather than from each caller — a record every handler must remember to
//!   write is one some handler will not write, and the missing one will be on
//!   whichever path is under attack;
//! - every revocation: applied, matched-nothing, and the loud
//!   verify-after-write failure ([`revoke::RevocationService::revoke`]);
//! - every RFC 7009 request outcome, including the unrecognised-token and
//!   foreign-client cases that deliberately answer `200`;
//! - refresh-token reuse detection ([`revoke::RevocationService::revoke_on_reuse`]);
//! - every dispatch denial ([`revoke::RevocationService::dispatch_state`]).
//!
//! **Deferred**: client registration, which has no handler to emit from —
//! RMCP-08 (dynamic client registration) is not merged. [`audit::OauthEvent`]
//! and [`audit::AuditDetail`] already carry the variants for it, so that item
//! adds a call site rather than a vocabulary.
//!
//! TERM #633 is **done**: RMCP-03's login POST used to carry its own
//! `InProcessRateLimiter` with same-sized account and source buckets — which
//! meant one address exhausting its own budget also exhausted the named
//! account's, a free lockout of any guessable account name. It now shares
//! [`limits::OauthRateLimiter`], so the login budget is defined once and
//! inherits the subject-over-address invariant. Its per-address numbers were
//! carried over unchanged: converging two definitions must not relax the
//! stricter one.
//!
//! ## What RMCP-05 adds
//! [`resource`] — bearer-token validation on `/mcp`, resolving a token to a
//! [`crate::mesh::Principal`]. The consuming half of the tokens [`token`]
//! mints, and it deliberately landed BEFORE the issuer was reachable: a
//! verifier written first cannot be quietly shaped around whatever the issuer
//! happened to emit.
//!
//! ## Credential storage — nothing here is presentable
//! No table in this schema stores a usable credential:
//! - Authorization codes and refresh tokens are high-entropy machine-generated
//!   values, stored as SHA-256 hashes ([`secret_hash`]). They need no salt or
//!   work factor precisely because they are full-entropy and short-lived;
//!   argon2 on a 256-bit random value buys nothing and costs latency on the
//!   token endpoint, which has a 10-second budget.
//! - Client secrets and account passwords are stored as argon2id PHC strings.
//!   [`password`] (RMCP-03) owns the hashing and verification; RMCP-08 owns
//!   provisioning.
//!
//! ## Secret access (S7/S8)
//! This crate has no separate `SecretManager::get()` API; the runtime secret
//! store is materialized into the process environment at startup, so an env
//! read here IS the vault read. See [`crate::pki`]'s module docs for the full
//! rationale and [`crate::pg::conn`] for the established precedent this
//! mirrors. The connection URL is read in exactly one place
//! ([`OauthConfig::from_env`]) and is never logged, returned, or embedded in an
//! error.

pub mod audit;
pub mod authorize;
/// RMCP-08: how a `client_id` comes into existence (operator minting and gated
/// DCR), and the client lifecycle behind both the tools and the RFC 7591
/// endpoint.
pub mod clients;
pub mod delegation;
pub mod edge;
pub mod groups;
pub mod jwt;
pub mod limits;
pub mod metadata;
pub mod model;
pub mod mount;
pub mod password;
/// RMCP-08: the RFC 7591 dynamic client registration endpoint. Off by default,
/// and never an unauthenticated write when on.
pub mod register;
pub mod revoke;
pub mod router;
/// RMCP-05: resource-server validation — the half that turns a bearer token
/// into a [`crate::mesh::Principal`] the existing gateway already authorizes.
pub mod resource;
pub mod scope;
pub mod session;
pub mod store;
pub mod templates;
pub mod token;

use crate::error::ToolError;

/// Env var naming the Postgres connection this module's own data plane uses.
///
/// This is the S9-pg "application service owns its own data plane" case: the
/// OAuth store is Terminus's own state, not ad-hoc fleet-database access, so it
/// holds a pool rather than routing through the `pg_*` tools. Fleet queries by
/// an agent still go through those tools.
pub const DATABASE_URL_ENV: &str = "RMCP_DATABASE_URL";

/// RMCP-12: which operator account the LOCAL tool surface acts as.
///
/// Only consulted when the fleet has more than one active operator account —
/// with exactly one there is nothing to choose, and the store resolves it
/// ([`crate::oauth::store::OauthStore::find_sole_operator_account`]). It names
/// an ACCOUNT, never a credential, and it is not an identity a request can
/// carry: a tool caller cannot set it, only the operator deploying the service
/// can.
pub const OPERATOR_ACCOUNT_ENV: &str = "RMCP_OPERATOR_ACCOUNT";

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

/// Generate a URL-safe, unpadded-base64 token carrying `bytes` bytes of
/// operating-system entropy.
///
/// The one place this module makes a random value — authorization codes,
/// refresh tokens and `jti`s all come from here, so there is a single answer to
/// "where does the entropy come from" rather than one per call site.
///
/// `getrandom` is used rather than a userspace PRNG deliberately: it is a thin
/// wrapper over the OS CSPRNG with no seeding, no reseeding-after-fork hazard,
/// and no fallible-initialisation state to get wrong. A failure is returned
/// rather than papered over with a weaker source — an entropy failure must
/// refuse to issue a credential, never quietly issue a guessable one.
pub fn random_token(bytes: usize) -> Result<String, ToolError> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| {
        ToolError::Execution(format!("cannot read operating-system entropy: {e}"))
    })?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
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

    /// Decode a segment as unpadded standard base64, or `None` when it uses
    /// characters outside that alphabet (see [`Self::is_segment_char`]) and so
    /// cannot be decoded that way.
    fn decode_standard_b64(segment: &str) -> Option<Vec<u8>> {
        use base64::engine::general_purpose::STANDARD_NO_PAD;
        use base64::Engine as _;
        STANDARD_NO_PAD.decode(segment).ok()
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

            // Numeric is not enough: argon2 requires t >= 1, p >= 1 and
            // m >= 8*p, so `m=0,t=0,p=0` parses as digits but is not a
            // configuration any implementation can have produced. Round 8
            // flagged it. Only the floors are enforced — an UPPER bound would
            // start rejecting hashes from a future, stronger configuration,
            // which is the failure mode this guard must never have.
            let numeric: u64 = value.parse().map_err(|_| refuse("cost parameter out of range"))?;
            let floor = match key {
                "m" => 8,
                _ => 1,
            };
            if numeric < floor {
                return Err(refuse("cost parameter below argon2's minimum"));
            }
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

        // Round 7 asked this parser to "establish that the value is an actual
        // argon2id hash". That is not achievable and is not what this guard
        // claims: whether a digest is the CORRECT output for some password is
        // unknowable without the password, and argon2 output is
        // indistinguishable from random bytes. What is achievable is raising
        // the bar from "looks base64-ish" to "decodes as base64 to a
        // cryptographically plausible length", which is done here for segments
        // written in the standard alphabet. A segment using the wider accepted
        // alphabet is length-checked only, deliberately: rejecting a genuine
        // hash would break authentication, and this guard's job is to catch a
        // plaintext password, not to adjudicate encodings.
        if let Some(salt) = Self::decode_standard_b64(fields[4]) {
            if salt.len() < 8 {
                return Err(refuse("salt decodes to fewer than argon2's 8 minimum bytes"));
            }
        }
        if let Some(digest) = Self::decode_standard_b64(fields[5]) {
            if digest.len() < 16 {
                return Err(refuse("digest decodes to an implausibly short length"));
            }
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

    /// A credential generator that repeated itself, or that returned a
    /// predictable value, would be the single worst bug this module could
    /// have — so the properties are asserted rather than assumed of the OS.
    #[test]
    fn random_tokens_are_long_and_never_repeat() {
        let token = random_token(32).expect("os entropy");
        // 32 bytes is 43 unpadded base64url characters, and the encoding must
        // stay URL-safe: these values travel in form bodies.
        assert_eq!(token.len(), 43);
        assert!(token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));

        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            assert!(seen.insert(random_token(32).expect("os entropy")), "a token repeated");
        }
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
            "$argon2i$v=19$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=xx$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=1,t=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=a,t=1,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=19456,t=2,p=1,q=9$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            // Round 8: numeric but impossible cost parameters.
            "$argon2id$v=19$m=0,t=0,p=0$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=19456,t=0,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=4,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            // Round 4: a repeated cost parameter must not be last-write-wins.
            "$argon2id$v=19$m=1,m=99999,t=1,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            // Round 4: only argon2's two published versions are real.
            "$argon2id$v=99$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=0$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=19456,t=2,p=1$short$RdescudvJCsgt3ubXbXdWRWJTmaaJObG",
            "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$sh",
            "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$has h$extra",
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
