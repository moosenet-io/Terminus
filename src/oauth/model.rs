//! Row types for the RMCP OAuth store.
//!
//! These mirror `migrations/S132-rmcp01-oauth-core.sql` one-for-one. Two
//! conventions run through the whole file and are worth stating once:
//!
//! 1. **No type here can hold a presentable credential**, and no type prints
//!    its sensitive material. `password_hash` and `client_secret_hash` are
//!    argon2id PHC strings; `code_hash` and `token_hash` are SHA-256 digests.
//!    None of those can be replayed as-is.
//!
//!    An earlier revision concluded from that that `Debug` was safe to derive.
//!    Round 3 of review pushed back, and was right to: "cannot be replayed" is
//!    not "harmless in a log". A client-secret PHC string is an offline
//!    cracking target with its own salt and cost parameters attached, and a
//!    code or token digest is live authentication material that confirms which
//!    token a log line concerns. So every type here that holds such a field
//!    implements `Debug` BY HAND with that field redacted, rather than deriving
//!    it — hand-written specifically so that adding a sensitive field later
//!    cannot silently re-expose it, which a derive would.
//!
//! 2. **Absence is denial, never a default.** No type implements `Default`, and
//!    no field is populated with a permissive fallback. An empty
//!    `ToolGroup::patterns` matches nothing; a `Client` with no scope rows
//!    reaches nothing. The natural refactor toward `unwrap_or_default()` is
//!    precisely the widening bug this shape is meant to prevent.

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;
use sqlx::Row;
use uuid::Uuid;

/// A human who can authenticate and consent.
///
/// Distinct from [`crate::mesh::Principal`]: an account MAPS to a principal
/// (RMCP-05), it does not replace one. That indirection is deliberate — it
/// keeps the OAuth door from becoming a second way to mint fleet identity.
///
/// No `Debug` derive: `totp_secret_enc` is encrypted rather than hashed (it has
/// to be recoverable to verify a code), so it is the one field in this module
/// that a careless `{:?}` could put in a log with the key sitting next to it.
#[derive(Clone)]
pub struct Account {
    pub id: Uuid,
    pub name: String,
    /// argon2id PHC string.
    pub password_hash: String,
    /// Encrypted TOTP seed, or `None` when this account has no second factor.
    pub totp_secret_enc: Option<Vec<u8>>,
    pub disabled: bool,
    /// Whether this account holds fleet-operator authority (RMCP-06).
    ///
    /// The ONLY source of truth for that question. It exists as a column rather
    /// than as an argument because the rule it backs — a delegated author may
    /// not write a bare `*` tool-group pattern — is an authorization rule, and
    /// an authorization rule whose input the caller supplies is advisory. The
    /// store reads it inside the same transaction as the write it authorizes;
    /// see [`crate::oauth::store::OauthStore::insert_tool_group`].
    ///
    /// Defaults to `false` in the schema, so an account nobody thought about is
    /// delegated rather than privileged.
    pub is_operator: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Account {
    /// This account's authority for group authoring, as the pure validator
    /// expects it.
    ///
    /// The conversion lives here so there is exactly one place that turns the
    /// stored state into a [`crate::oauth::groups::GroupOwner`], and no call
    /// site can invent the privileged variant from nothing.
    ///
    /// **Requires `is_operator` AND `!disabled`**, deliberately matching
    /// `(a.is_operator AND NOT a.disabled)` in
    /// [`crate::oauth::store::OauthStore::client_authorized_groups`] term for
    /// term. An earlier revision of this method read `is_operator` alone, which
    /// made the two disagree for a DISABLED operator — the SQL path revoked
    /// their wildcard, this one did not. Two expressions of one authorization
    /// rule that can disagree is the same drift the rest of this item keeps
    /// closing, and disabling an account is the most common revocation there is:
    /// it is what an operator reaches for when an account is compromised, so it
    /// must revoke everything that account's authority was propping up, not just
    /// its ability to log in.
    pub fn group_owner_kind(&self) -> crate::oauth::groups::GroupOwner {
        if self.is_operator && !self.disabled {
            crate::oauth::groups::GroupOwner::Operator
        } else {
            crate::oauth::groups::GroupOwner::Delegated
        }
    }
}

/// How a client came to exist. Not merely descriptive: a `Dcr` client is
/// created with no scope and must be scoped by an operator before it can reach
/// anything (RMCP-08).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationSource {
    /// Minted by an operator in the GUI or CLI.
    Operator,
    /// Self-registered through RFC 7591 dynamic client registration.
    Dcr,
}

impl RegistrationSource {
    /// The stored representation. Kept next to [`Self::parse`] so the two can
    /// never drift.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Dcr => "dcr",
        }
    }

    /// Parse the stored representation, fail-closed.
    ///
    /// An unrecognised value reads as [`Self::Dcr`] — the LESS privileged of
    /// the two — rather than as an error or as `Operator`. A corrupted or
    /// future-valued row should degrade a client toward "needs explicit
    /// scoping", never toward "operator-minted, trusted".
    pub fn parse(raw: &str) -> Self {
        match raw {
            "operator" => Self::Operator,
            _ => Self::Dcr,
        }
    }
}

/// One connector. `client_id` is the public identifier pasted into the client
/// application; `client_secret_hash` is `None` for a public client (which is
/// what Claude registers as under both DCR and CIMD).
#[derive(Clone)]
pub struct Client {
    pub id: Uuid,
    pub client_id: String,
    /// argon2id PHC string, or `None` for a public client.
    pub client_secret_hash: Option<String>,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub owner_account_id: Uuid,
    pub registration_source: String,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

impl Client {
    /// Typed view of [`Self::registration_source`].
    pub fn source(&self) -> RegistrationSource {
        RegistrationSource::parse(&self.registration_source)
    }

    /// Whether this client authenticates at the token endpoint.
    ///
    /// Derived from the presence of a secret hash rather than from
    /// `token_endpoint_auth_method`, so a row whose method column says
    /// `client_secret_post` but which holds no secret cannot be treated as
    /// confidential — the check that matters is whether there is anything to
    /// verify against.
    pub fn is_confidential(&self) -> bool {
        self.client_secret_hash.is_some()
    }
}

/// A named set of tool-name patterns (RMCP-06 owns the matcher).
///
/// An empty `patterns` matches NOTHING. This is the single most important
/// invariant in the type and is asserted by tests in both this module and the
/// store, because "empty means unrestricted" is the intuitive-but-catastrophic
/// reading.
#[derive(Clone, Debug)]
pub struct ToolGroup {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub patterns: Vec<String>,
    pub owner_account_id: Uuid,
    pub created_at: DateTime<Utc>,
}

impl ToolGroup {
    /// Whether this group can match anything at all.
    ///
    /// Exists so call sites read as `if group.is_empty() { deny }` rather than
    /// `if group.patterns.is_empty() { ... }`, which invites the wrong default.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// A single-use authorization code, bound to everything needed to detect a
/// replay or a substituted client, redirect, or resource.
#[derive(Clone)]
pub struct AuthCode {
    pub code_hash: Vec<u8>,
    pub client_id: Uuid,
    pub account_id: Uuid,
    pub redirect_uri: String,
    pub resource: String,
    pub code_challenge: String,
    pub scope: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
}

/// A refresh token in a rotation family.
#[derive(Clone)]
pub struct RefreshToken {
    pub token_hash: Vec<u8>,
    pub family_id: Uuid,
    pub client_id: Uuid,
    pub account_id: Uuid,
    pub resource: String,
    pub scope: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Set once this token has been exchanged for a successor. A presentation
    /// of a token with this set is a REUSE event, and RMCP-04 revokes the whole
    /// family on it.
    pub rotated_to: Option<Vec<u8>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl RefreshToken {
    /// Whether presenting this token is a reuse of an already-rotated one.
    pub fn is_rotated(&self) -> bool {
        self.rotated_to.is_some()
    }
}

/// One refresh-token FAMILY, aggregated — what RMCP-11 calls a "session".
///
/// A session is not a row anywhere: it is the set of `rmcp_refresh_token` rows
/// sharing a `family_id`, which is what a single authorization grows into as it
/// rotates. Naming the family rather than any individual token is deliberate and
/// runs through this whole item — an operator revoking access means "cut this
/// session off", not "invalidate one particular string", and the token hashes
/// are exactly the values that must never reach a listing or a log
/// (see the module rule above).
///
/// [`Self::revoked_at`] is the EARLIEST revocation across the family, and
/// [`Self::live`] is computed in SQL against the database clock. Both follow
/// [`crate::oauth::store::OauthStore::refresh_token_is_live`]'s family-wide
/// rule: any revoked row kills the whole family, including rows inserted
/// afterwards. A summary that reported per-row state would disagree with the
/// predicate that actually gates dispatch, and an operator would be reading a
/// different truth from the one being enforced.
#[derive(Clone, Debug)]
pub struct TokenFamily {
    pub family_id: Uuid,
    pub client_id: Uuid,
    pub account_id: Uuid,
    pub resource: String,
    pub scope: String,
    /// When the family began — the first token's issuance.
    pub issued_at: DateTime<Utc>,
    /// The most recent rotation's issuance.
    pub last_issued_at: DateTime<Utc>,
    /// The latest expiry in the family.
    pub expires_at: DateTime<Utc>,
    /// How many tokens the family has held, i.e. rotations + 1.
    pub token_count: i64,
    /// The earliest revocation in the family, if any.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Whether the family can still be refreshed, judged by the DATABASE clock
    /// at the moment of the query. Carried rather than derived in Rust so a
    /// process with a drifted clock cannot report a dead session as live.
    pub live: bool,
}

/// A recorded human approval of a client for a scope.
#[derive(Clone, Debug)]
pub struct Consent {
    pub id: Uuid,
    pub account_id: Uuid,
    pub client_id: Uuid,
    pub scope: String,
    pub granted_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A client as the ADMINISTRATION surface sees it (RMCP-08).
///
/// Deliberately a separate type from [`Client`] rather than three more fields
/// on it. [`Client`] is what the authorization and token paths read on every
/// request, and it is the type whose `client_secret_hash` those paths verify
/// against; widening it for the benefit of a listing tool would put an
/// administrative concern on the hot authentication path and drag every
/// `SELECT` and every test fixture along with it.
///
/// The difference that matters is the last field. This view carries
/// `confidential` — computed in SQL as `client_secret_hash IS NOT NULL` — and
/// carries **no hash at all**. A client secret is minted once, hashed, and then
/// unreachable: there is no field here that could carry it, so no listing, no
/// serialization and no tool response can leak one even by mistake.
#[derive(Clone, Debug)]
pub struct ClientAdmin {
    pub id: Uuid,
    pub client_id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub owner_account_id: Uuid,
    pub registration_source: String,
    pub disabled: bool,
    /// Whether a secret hash exists. Never the hash, and never the secret.
    pub confidential: bool,
    pub created_at: DateTime<Utc>,
    /// Optimistic-concurrency token. An update states the version it read; a
    /// stale value is a conflict, never a silent overwrite.
    pub version: i32,
}

impl ClientAdmin {
    /// Typed view of [`Self::registration_source`], with the same fail-closed
    /// parse [`Client::source`] uses — one reading of that column, not two.
    pub fn source(&self) -> RegistrationSource {
        RegistrationSource::parse(&self.registration_source)
    }
}

/// Which account administers a federated namespace (RMCP-12).
#[derive(Clone, Debug)]
pub struct ServerOwner {
    pub namespace: String,
    pub owner_account_id: Uuid,
    pub granted_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Redacted Debug
// ---------------------------------------------------------------------------
//
// Hand-written, not derived, for the three row types that carry hashed
// credential material (see rule 1 in the module docs). Each prints every
// non-sensitive field normally — the useful part for debugging — and replaces
// the sensitive one with a marker. `Account` has no `Debug` at all, because its
// encrypted TOTP seed is recoverable rather than hashed.

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("id", &self.id)
            .field("client_id", &self.client_id)
            // The PHC string is an offline cracking target; only its presence
            // (which is what `is_confidential` turns on) is ever interesting.
            .field("client_secret_hash", &self.client_secret_hash.as_ref().map(|_| "<redacted>"))
            .field("name", &self.name)
            .field("redirect_uris", &self.redirect_uris)
            .field("grant_types", &self.grant_types)
            .field("token_endpoint_auth_method", &self.token_endpoint_auth_method)
            .field("owner_account_id", &self.owner_account_id)
            .field("registration_source", &self.registration_source)
            .field("disabled", &self.disabled)
            .field("created_at", &self.created_at)
            .finish()
    }
}

impl std::fmt::Debug for AuthCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthCode")
            .field("code_hash", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("account_id", &self.account_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("resource", &self.resource)
            // The PKCE challenge is a digest of the verifier and is not secret,
            // but it identifies the exchange; there is no debugging value in it
            // that the code hash's presence does not already give.
            .field("code_challenge", &"<redacted>")
            .field("scope", &self.scope)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("consumed_at", &self.consumed_at)
            .finish()
    }
}

impl std::fmt::Debug for RefreshToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshToken")
            .field("token_hash", &"<redacted>")
            .field("family_id", &self.family_id)
            .field("client_id", &self.client_id)
            .field("account_id", &self.account_id)
            .field("resource", &self.resource)
            .field("scope", &self.scope)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            // Whether it was rotated is the load-bearing fact for reuse
            // detection; the successor's digest is not.
            .field("rotated_to", &self.rotated_to.as_ref().map(|_| "<redacted>"))
            .field("revoked_at", &self.revoked_at)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Row decoding
// ---------------------------------------------------------------------------
//
// Written by hand rather than derived: this workspace builds `sqlx` with
// `default-features = false` and without the `macros`/`derive` feature, so
// `#[derive(sqlx::FromRow)]` is not available (same constraint as
// `crate::scribe::graph::rules_store` and `crate::intake::discovery::storage`,
// which decode the same way). Each impl is keyed by COLUMN NAME, so it stays
// correct if a `SELECT`'s column order changes — only a rename can break it,
// and that breaks loudly at the first query rather than silently decoding the
// wrong field into the right-typed slot.
//
// S132/RMCP-SQLITE changed the row type from `PgRow` to `SqliteRow`. The
// decode-by-name discipline is unchanged and is what made that a one-line
// change for most types; the two that needed more are the ones holding a list.

/// Decode a `text[]`-replacement column: a JSON array of strings in a TEXT
/// column (see the type-mapping note in `migrations/S132-rmcp-sqlite-oauth.sql`).
///
/// ## Why this is BOUNDED here, on the read path
///
/// RMCP-06 caps a group at [`crate::oauth::groups::MAX_PATTERNS_PER_GROUP`]
/// patterns, and `validate_group` / `validate_patterns` enforce that at the
/// write gate. Under Postgres the write gate was the only way a row could come
/// into existence, so a write-time bound was a real bound.
///
/// It is not any more. The store is now a FILE, and an operator (or anything
/// that can write that file) can put a million-element array in this column
/// without passing through any Rust at all. A write-time check that a caller
/// can go around is exactly the shape of defect this item has spent its whole
/// review history removing — the same reasoning that makes every revocable
/// authority get re-derived on the read path applies to a bound that can be
/// bypassed on the write path.
///
/// So the limit is re-applied on DECODE, and exceeding it is a decode ERROR
/// rather than a truncation. Truncating would silently produce a DIFFERENT
/// permission set from the one stored and then resolve against it, which is the
/// "looks the same, is weaker" substitution this sprint keeps finding. An error
/// makes the row unreadable, and an unreadable group grants nothing.
///
/// A NULL is impossible (the column is `NOT NULL DEFAULT '[]'`) and malformed
/// JSON is refused by the column's own `json_valid` CHECK; both are still
/// handled here rather than assumed, because the CHECK constraint is only
/// enforced for writes SQLite performs, and this code should not be the thing
/// that trusts a file it has just finished arguing it cannot trust.
fn decode_string_list(row: &SqliteRow, column: &str) -> Result<Vec<String>, sqlx::Error> {
    let raw: String = row.try_get(column)?;
    let parsed: Vec<String> = serde_json::from_str(&raw).map_err(|e| sqlx::Error::ColumnDecode {
        index: column.to_string(),
        // The parse error can quote the offending input, which for these
        // columns is operator-authored pattern text rather than a credential —
        // but the standing rule in this module is that no stored value reaches
        // an error, so only the CATEGORY is reported. `serde_json`'s `Category`
        // is a four-variant enum (Io/Syntax/Data/Eof) carrying no payload from
        // the input, which is exactly why it is the thing reported and why
        // `{:?}` on it is safe — `{}` is not even available, since it does not
        // implement `Display`.
        source: format!("column `{column}` is not a JSON array of strings ({:?})", e.classify())
            .into(),
    })?;
    if parsed.len() > MAX_STORED_LIST_LEN {
        return Err(sqlx::Error::ColumnDecode {
            index: column.to_string(),
            source: format!(
                "column `{column}` holds {} entries, over the {MAX_STORED_LIST_LEN} bound; \
                 the row is refused rather than truncated",
                parsed.len()
            )
            .into(),
        });
    }
    Ok(parsed)
}

/// The read-path bound applied by [`decode_string_list`].
///
/// Set from [`crate::oauth::groups::MAX_PATTERNS_PER_GROUP`], which is the
/// largest of the three lists this decode serves (`rmcp_tool_group.patterns`;
/// `rmcp_client.redirect_uris` and `.grant_types` are far smaller in practice
/// and have no separate cap worth inventing). Deriving it rather than restating
/// a number means the write gate and the read gate cannot drift to two
/// different limits.
const MAX_STORED_LIST_LEN: usize = crate::oauth::groups::MAX_PATTERNS_PER_GROUP;

macro_rules! impl_from_row {
    ($ty:ident, $($field:ident),+ $(,)?) => {
        impl<'r> sqlx::FromRow<'r, SqliteRow> for $ty {
            fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
                Ok(Self { $($field: row.try_get(stringify!($field))?),+ })
            }
        }
    };
}

/// As [`impl_from_row`], but the named `$list` fields are decoded by
/// [`decode_string_list`] instead of by `try_get`.
///
/// A separate macro rather than a flag on the first one so that a list column
/// CANNOT be decoded the ordinary way by omission: `try_get::<Vec<String>>` on
/// a SQLite TEXT column does not compile, so leaving a list field out of the
/// `[…]` group is a build error rather than a silently wrong decode.
macro_rules! impl_from_row_with_lists {
    ($ty:ident, [$($list:ident),+ $(,)?], $($field:ident),+ $(,)?) => {
        impl<'r> sqlx::FromRow<'r, SqliteRow> for $ty {
            fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
                Ok(Self {
                    $($list: decode_string_list(row, stringify!($list))?,)+
                    $($field: row.try_get(stringify!($field))?),+
                })
            }
        }
    };
}

impl_from_row!(
    Account,
    id,
    name,
    password_hash,
    totp_secret_enc,
    disabled,
    is_operator,
    created_at,
    updated_at
);
impl_from_row_with_lists!(
    Client,
    [redirect_uris, grant_types],
    id,
    client_id,
    client_secret_hash,
    name,
    token_endpoint_auth_method,
    owner_account_id,
    registration_source,
    disabled,
    created_at
);
impl_from_row_with_lists!(
    ClientAdmin,
    [redirect_uris, grant_types],
    id,
    client_id,
    name,
    token_endpoint_auth_method,
    owner_account_id,
    registration_source,
    disabled,
    confidential,
    created_at,
    version
);
impl_from_row_with_lists!(
    ToolGroup,
    [patterns],
    id,
    name,
    description,
    owner_account_id,
    created_at
);
impl_from_row!(
    AuthCode,
    code_hash,
    client_id,
    account_id,
    redirect_uri,
    resource,
    code_challenge,
    scope,
    issued_at,
    expires_at,
    consumed_at
);
impl_from_row!(
    RefreshToken,
    token_hash,
    family_id,
    client_id,
    account_id,
    resource,
    scope,
    issued_at,
    expires_at,
    rotated_to,
    revoked_at
);
impl_from_row!(
    TokenFamily,
    family_id,
    client_id,
    account_id,
    resource,
    scope,
    issued_at,
    last_issued_at,
    expires_at,
    token_count,
    revoked_at,
    live
);
impl_from_row!(Consent, id, account_id, client_id, scope, granted_at, revoked_at);
impl_from_row!(ServerOwner, namespace, owner_account_id, granted_at);

#[cfg(test)]
mod tests {
    use super::*;

    /// An unknown stored value must degrade toward the LESS privileged source.
    /// Reading it as `Operator` would let a corrupted row masquerade as
    /// operator-minted and skip the "must be scoped first" rule.
    #[test]
    fn unknown_registration_source_reads_as_dcr() {
        assert_eq!(RegistrationSource::parse("operator"), RegistrationSource::Operator);
        assert_eq!(RegistrationSource::parse("dcr"), RegistrationSource::Dcr);
        assert_eq!(RegistrationSource::parse(""), RegistrationSource::Dcr);
        assert_eq!(RegistrationSource::parse("OPERATOR"), RegistrationSource::Dcr);
        assert_eq!(RegistrationSource::parse("something-new"), RegistrationSource::Dcr);
    }

    /// Round-trip, so `as_str` and `parse` cannot drift apart.
    #[test]
    fn registration_source_round_trips() {
        for source in [RegistrationSource::Operator, RegistrationSource::Dcr] {
            assert_eq!(RegistrationSource::parse(source.as_str()), source);
        }
    }

    fn account(is_operator: bool, disabled: bool) -> Account {
        Account {
            id: Uuid::nil(),
            name: "a".into(),
            password_hash: "<REDACTED-SECRET>".into(),
            totp_secret_enc: None,
            disabled,
            is_operator,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// Operator authority is carried by the stored state and nothing else. The
    /// `false` case is what a caller claiming to be an operator must not be able
    /// to talk its way past, and what an account created before the flag existed
    /// degrades to.
    #[test]
    fn group_authority_follows_the_stored_flag() {
        use crate::oauth::groups::GroupOwner;
        assert_eq!(account(true, false).group_owner_kind(), GroupOwner::Operator);
        assert_eq!(account(false, false).group_owner_kind(), GroupOwner::Delegated);
    }

    /// DISABLING an operator revokes their group authority, not merely their
    /// ability to log in.
    ///
    /// This is the revocation an operator actually reaches for when an account
    /// is compromised, and it must reach everything that account's authority was
    /// propping up — otherwise a disabled operator's stored `*` keeps expanding
    /// for every connector scoped to their groups.
    ///
    /// The assertion also pins this method to the SQL in
    /// `client_authorized_groups`, which computes
    /// `(a.is_operator AND NOT a.disabled)`. Two expressions of one
    /// authorization rule are only safe while they agree; an earlier revision
    /// read `is_operator` alone here and silently disagreed with the query for
    /// exactly this account.
    #[test]
    fn disabling_an_operator_revokes_group_authority() {
        use crate::oauth::groups::GroupOwner;
        assert_eq!(
            account(true, true).group_owner_kind(),
            GroupOwner::Delegated,
            "a disabled operator is not an operator for any purpose"
        );
        assert_eq!(account(false, true).group_owner_kind(), GroupOwner::Delegated);
    }

    fn group_with(patterns: Vec<String>) -> ToolGroup {
        ToolGroup {
            id: Uuid::nil(),
            name: "g".into(),
            description: String::new(),
            patterns,
            owner_account_id: Uuid::nil(),
            created_at: Utc::now(),
        }
    }

    /// The headline invariant: an empty group is empty, not unrestricted.
    #[test]
    fn empty_group_matches_nothing() {
        assert!(group_with(vec![]).is_empty());
        assert!(!group_with(vec!["weather_*".into()]).is_empty());
    }

    fn client_with_secret(hash: Option<&str>) -> Client {
        Client {
            id: Uuid::nil(),
            client_id: "cid".into(),
            client_secret_hash: hash.map(str::to_string),
            name: "c".into(),
            redirect_uris: vec![],
            grant_types: vec![],
            // Deliberately claims a confidential method while holding no secret.
            token_endpoint_auth_method: "<REDACTED-SECRET>".into(),
            owner_account_id: Uuid::nil(),
            registration_source: "operator".into(),
            disabled: false,
            created_at: Utc::now(),
        }
    }

    /// Confidentiality follows the stored secret, not the advertised method —
    /// otherwise a row with a confidential method and no secret would be
    /// "authenticated" by verifying against nothing.
    #[test]
    fn confidentiality_follows_the_stored_secret_not_the_method() {
        assert!(!client_with_secret(None).is_confidential());
        assert!(client_with_secret(Some("$argon2id$v=19$...")).is_confidential());
    }

    /// No hashed credential material may reach a log through `Debug`, while
    /// the fields that make a log line useful must still be there.
    #[test]
    fn debug_redacts_hashes_but_keeps_context() {
        let client = client_with_secret(Some("$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$ZGlnZXN0"));
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("argon2id"), "the PHC string must not appear: {rendered}");
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("cid"), "the client_id is not secret and aids debugging");

        let code = AuthCode {
            code_hash: b"\xde\xad\xbe\xef".to_vec(),
            client_id: Uuid::nil(),
            account_id: Uuid::nil(),
            redirect_uri: "https://example.test/cb".into(),
            resource: "https://example.test/mcp".into(),
            code_challenge: "a-pkce-challenge-value".into(),
            scope: "mcp".into(),
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            consumed_at: None,
        };
        let rendered = format!("{code:?}");
        assert!(!rendered.contains("dead"), "no digest bytes: {rendered}");
        assert!(!rendered.contains("a-pkce-challenge-value"));
        assert!(rendered.contains("https://example.test/mcp"), "the audience aids debugging");

        let token = RefreshToken {
            token_hash: vec![1, 2, 3],
            family_id: Uuid::nil(),
            client_id: Uuid::nil(),
            account_id: Uuid::nil(),
            resource: "https://example.test/mcp".into(),
            scope: "mcp".into(),
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            rotated_to: Some(vec![4, 5, 6]),
            revoked_at: None,
        };
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("[1, 2, 3]"), "no digest bytes: {rendered}");
        assert!(!rendered.contains("[4, 5, 6]"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn rotated_refresh_token_is_detectable() {
        let base = RefreshToken {
            token_hash: vec![1],
            family_id: Uuid::nil(),
            client_id: Uuid::nil(),
            account_id: Uuid::nil(),
            resource: "https://example.test/mcp".into(),
            scope: "mcp".into(),
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            rotated_to: None,
            revoked_at: None,
        };
        assert!(!base.is_rotated());
        assert!(RefreshToken { rotated_to: Some(vec![2]), ..base }.is_rotated());
    }
}
