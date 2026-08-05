//! Postgres repository for the RMCP OAuth door.
//!
//! ## Two rules every method in this file obeys
//!
//! **Absence is `None`, and `None` means deny.** No method returns a permissive
//! default, and none takes an `unwrap_or` shortcut toward one. A caller that
//! cannot find a client, a consent, or a scope row must treat that as "no
//! access", which is only sound if this layer never invents a fallback. That is
//! why, for example, [`OauthStore::client_tool_groups`] returns an empty `Vec`
//! for an unknown client rather than erroring or returning every group — the
//! empty set is the correct, safe answer, and it flows straight into RMCP-07's
//! intersection.
//!
//! **The database's clock is the only clock.** Every expiry comparison is
//! written as `expires_at > now()` inside SQL rather than compared against
//! `Utc::now()` in Rust. Terminus runs on several hosts, and a process whose
//! clock has drifted must not be able to honour an expired code or reject a
//! live one. One clock, at the store.
//!
//! All queries use sqlx parameter binding; there is no SQL string
//! interpolation anywhere in this module.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::delegation::{
    authorize_client_write, authorize_namespace_scoping, authorize_operator_action,
    reverify_delegation_change, ActorAuthority, DelegationChange, DelegationGrant,
    DelegationRevocation, DelegationStore,
};
use crate::oauth::groups::{
    normalize_description, validate_group, validate_patterns, AuthorizedGroup, Pattern,
    MAX_GROUPS_PER_CLIENT, STARTER_GROUPS,
};
use crate::oauth::model::{
    Account, AuthCode, Client, ClientAdmin, Consent, RefreshToken, ServerOwner, TokenFamily,
    ToolGroup,
};
use crate::oauth::revoke::{DispatchState, SessionStore};
use crate::oauth::scope::ScopeWrite;
use crate::oauth::{Argon2idHash, OauthConfig, SecretHash};

/// Maximum pooled connections. The OAuth endpoints are latency-sensitive
/// (Anthropic allows 10s for discovery/token, 30s for refresh) but very
/// low-volume compared to the tool-dispatch path, so a small pool is right —
/// this door should never be able to starve the rest of the process of
/// database connections.
const DEFAULT_MAX_CONNECTIONS: u32 = 5;

/// Env var overriding [`DEFAULT_MAX_CONNECTIONS`].
const MAX_CONNECTIONS_ENV: &str = "RMCP_DB_MAX_CONNECTIONS";

/// Resolve the pool size, falling back to the default on absent or unparseable
/// input. A bad value degrades to the safe default rather than failing the
/// door: an operator typo in a tuning knob should not take authentication
/// offline, which is the opposite of the fail-closed rule applied to
/// PERMISSIONS — this value grants nothing.
fn max_connections() -> u32 {
    std::env::var(MAX_CONNECTIONS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_CONNECTIONS)
}

/// Every table the S132 migrations create. [`OauthStore::schema_ready`] requires
/// all of them, so a partially applied migration reports NOT ready.
///
/// Spans BOTH S132 migration files — `S132-rmcp01-oauth-core.sql` (the first
/// nine) and `S132-rmcp03-login-session.sql` (the last). Deliberately one list
/// rather than one per item: "the schema this code needs" is a single question,
/// and a per-item split would let a deploy that applied only the older file
/// report ready while the login path's single-use guard had no table behind it.
const REQUIRED_TABLES: [&str; 11] = [
    "rmcp_account",
    "rmcp_client",
    "rmcp_tool_group",
    "rmcp_client_scope",
    "rmcp_client_server",
    "rmcp_auth_code",
    "rmcp_refresh_token",
    "rmcp_consent",
    "rmcp_server_owner",
    "rmcp_login_session_use",
    // RMCP-08. Listed even though dynamic client registration is OFF by
    // default: the table also backs the operator's minting of initial access
    // tokens, and — more to the point — a readiness check that passes on a
    // partially applied migration is the confident-but-wrong "ready" this
    // whole check exists to prevent. A deployment that has not applied the
    // RMCP-08 migration is not ready, whatever it has DCR set to.
    "rmcp_registration_token",
];

/// Columns added to an EXISTING table by a later S132 migration, which the
/// table-level readiness check cannot detect on its own.
///
/// `rmcp_account.is_operator` (RMCP-06) is load-bearing for authorization, not
/// merely for a feature: it is the only source of truth for whether an author
/// may write a bare `*` pattern. A deploy missing it must report NOT ready.
const REQUIRED_COLUMNS: [(&str, &str); 2] = [
    ("rmcp_account", "is_operator"),
    // RMCP-08. `rmcp_client.version` is the optimistic-concurrency token every
    // administrative write states and re-checks. Without the column the
    // administration tools fail with an opaque "column does not exist" on the
    // first edit; with this line the deployment says so at startup instead.
    ("rmcp_client", "version"),
];

/// The refusal an unauthorized initial-access-token action carries (RMCP-08).
///
/// A `&'static str`, so it names the action for the operator reading the error
/// and can carry nothing a caller submitted. It never reaches the audit record,
/// which stays the closed `ScopingRefusal` vocabulary.
const OPERATOR_ONLY_REGISTRATION_TOKEN: &str =
    "only an operator account may mint or revoke registration tokens";

/// Repository over the `rmcp_*` tables.
#[derive(Clone)]
pub struct OauthStore {
    pool: PgPool,
}

impl OauthStore {
    /// Open the pool.
    ///
    /// Connection failures are reported without the URL — the error text from
    /// sqlx can contain the host and user, so it is deliberately not
    /// interpolated into the message.
    pub async fn connect(config: &OauthConfig) -> Result<Self, ToolError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections())
            .connect(config.database_url())
            .await
            .map_err(|_| {
                ToolError::Database(
                    "cannot connect to the RMCP OAuth database (check RMCP_DATABASE_URL and \
                     that the S132 migration has been applied)"
                        .into(),
                )
            })?;
        Ok(Self { pool })
    }

    /// Build a store over an existing pool. Used by tests and by a caller that
    /// already owns a pool for this database.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Whether the S132 schema is present.
    ///
    /// Migrations are not applied at startup (the v4.6 DEPLOY rule), so a
    /// deploy that ships this code without applying the migration is a real
    /// possibility. Reporting it as a clear "unconfigured" at boot beats every
    /// endpoint failing later with an opaque `relation does not exist`.
    pub async fn schema_ready(&self) -> bool {
        // Restricted to BASE TABLE: `information_schema.tables` also lists
        // views, so without this a view named `rmcp_client` would report the
        // schema ready with no migrated table behind it.
        //
        // Checks ALL nine tables, not a sentinel one. Review round 1: probing a
        // single table reports "ready" for a partially applied migration —
        // precisely the state a half-finished deploy leaves behind, and the one
        // where a confident "ready" is most harmful. Counting the full set costs
        // one query at startup and makes a partial apply loud.
        let found = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_name = ANY($1) \
               AND table_type = 'BASE TABLE'",
        )
        .bind(REQUIRED_TABLES.iter().map(|t| t.to_string()).collect::<Vec<_>>())
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);
        if found != REQUIRED_TABLES.len() as i64 {
            return false;
        }

        // RMCP-06 adds a COLUMN to an existing table, which the table check
        // above cannot see. Without this, a deploy that applied the RMCP-01
        // migration but not the RMCP-06 one reports "ready" and then fails every
        // account lookup — i.e. the whole authentication path — with an opaque
        // "column does not exist". A schema check that misses the second
        // migration is exactly the confident-but-wrong "ready" the check above
        // exists to prevent.
        for (table, column) in REQUIRED_COLUMNS {
            let present = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
            )
            .bind(table)
            .bind(column)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
            if present != 1 {
                return false;
            }
        }
        true
    }

    // -----------------------------------------------------------------------
    // Accounts
    // -----------------------------------------------------------------------

    /// Look up an account by name. `None` for unknown OR disabled.
    ///
    /// Collapsing "no such account" and "account disabled" into one answer is
    /// intentional: every caller treats both as "cannot authenticate", and
    /// keeping them distinct here would invite a caller to branch on it and
    /// hand an attacker an account-existence oracle.
    pub async fn find_active_account_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Account>, ToolError> {
        sqlx::query_as::<_, Account>(
            "SELECT id, name, password_hash, totp_secret_enc, disabled, is_operator, \
                    created_at, updated_at \
             FROM rmcp_account WHERE name = $1 AND NOT disabled",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Look up an account by its row id. `None` for unknown OR disabled — same
    /// collapsing, and the same anti-oracle reasoning, as
    /// [`Self::find_active_account_by_name`].
    ///
    /// RMCP-05 needs this because an access token carries the account UUID in
    /// `sub` while the operator-authored principal map is keyed on the account
    /// NAME. Resolving one to the other per request is what keeps the
    /// authorization key coming from the database rather than from the
    /// credential: a renamed or disabled account cannot be outlived by a token
    /// still carrying its old identity.
    pub async fn find_active_account_by_id(
        &self,
        account_id: Uuid,
    ) -> Result<Option<Account>, ToolError> {
        sqlx::query_as::<_, Account>(
            "SELECT id, name, password_hash, totp_secret_enc, disabled, created_at, updated_at \
             FROM rmcp_account WHERE id = $1 AND NOT disabled",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Whether an account exists and is not disabled.
    ///
    /// Exists for the token endpoint (RMCP-04), which holds an account UUID
    /// taken from an authorization code or a refresh-token row rather than a
    /// name, and must re-check the account on every exchange: a grant is
    /// created before the account is disabled, and it must not outlive the
    /// human. Returns `false` — never an error and never a default — for an
    /// unknown id, so the caller's `if !active { deny }` is correct for both
    /// "gone" and "turned off", exactly as
    /// [`Self::find_active_account_by_name`] collapses the same two cases.
    pub async fn account_is_active(&self, account_id: Uuid) -> Result<bool, ToolError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM rmcp_account WHERE id = $1 AND NOT disabled)",
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)
    }

    /// Insert an account.
    ///
    /// Takes an [`Argon2idHash`], not a `&str`. This layer never hashes — the
    /// argon2 parameters belong with the verifier in RMCP-03 — but requiring
    /// the verified type means a caller that forgot to hash cannot reach this
    /// column with a plaintext password. Review round 1: a `&str` parameter
    /// named `password_hash` documented the requirement without enforcing it.
    pub async fn insert_account(
        &self,
        name: &str,
        password_hash: &Argon2idHash,
        totp_secret_enc: Option<&[u8]>,
    ) -> Result<Uuid, ToolError> {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO rmcp_account (name, password_hash, totp_secret_enc) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(name)
        .bind(password_hash.as_str())
        .bind(totp_secret_enc)
        .fetch_one(&self.pool)
        .await
        .map_err(unique_aware("an account with that name already exists"))
    }

    // -----------------------------------------------------------------------
    // Clients
    // -----------------------------------------------------------------------

    /// Look up a client by its public `client_id`. `None` for unknown OR
    /// disabled — same reasoning as accounts: a disabled client must behave
    /// exactly like one that never existed.
    pub async fn find_active_client(&self, client_id: &str) -> Result<Option<Client>, ToolError> {
        sqlx::query_as::<_, Client>(
            "SELECT id, client_id, client_secret_hash, name, redirect_uris, grant_types, \
                    token_endpoint_auth_method, owner_account_id, registration_source, \
                    disabled, created_at \
             FROM rmcp_client WHERE client_id = $1 AND NOT disabled",
        )
        .bind(client_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Insert a client. `client_secret_hash` is `None` for a public client
    /// (which is what Claude registers as), and an [`Argon2idHash`] otherwise —
    /// same enforcement-by-type as [`Self::insert_account`].
    #[allow(clippy::too_many_arguments)]
    ///
    /// `actor_account_id` is the account CREATING the client, and it is
    /// authorized inside this transaction: an operator may create a connector
    /// owned by anyone, and anyone else only one owned by themselves (RMCP-08
    /// review round 2).
    ///
    /// Without that check, naming another account as `owner` would be enough to
    /// mint a connector in their name and then scope it to THEIR groups and
    /// namespaces — because the scoping write authorizes against the client's
    /// owner. It is the same defect class as the unauthorized edit, reached
    /// through creation rather than through modification.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_client(
        &self,
        actor_account_id: Uuid,
        client_id: &str,
        client_secret_hash: Option<&Argon2idHash>,
        name: &str,
        redirect_uris: &[String],
        grant_types: &[String],
        token_endpoint_auth_method: &str,
        owner_account_id: Uuid,
        registration_source: &str,
    ) -> Result<Uuid, ToolError> {
        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;

        // `authorize_client_write` reads exactly this rule — operator, or the
        // owner themselves — so creation and modification are decided by ONE
        // function rather than two that could drift. The "client owner" it is
        // given is the owner the caller asked for, which is what makes
        // "creating a connector for somebody else" the operator-only case.
        let actor = Self::actor_authority(&mut tx, actor_account_id).await?;
        authorize_client_write(&actor, owner_account_id)?;

        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO rmcp_client (client_id, client_secret_hash, name, redirect_uris, \
                                      grant_types, token_endpoint_auth_method, \
                                      owner_account_id, registration_source) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
        )
        .bind(client_id)
        .bind(client_secret_hash.map(Argon2idHash::as_str))
        .bind(name)
        .bind(redirect_uris)
        .bind(grant_types)
        .bind(token_endpoint_auth_method)
        .bind(owner_account_id)
        .bind(registration_source)
        .fetch_one(&mut *tx)
        .await
        .map_err(unique_aware("a client with that client_id already exists"))?;

        tx.commit().await.map_err(db)?;
        Ok(id)
    }

    /// List the clients an account owns.
    pub async fn list_clients_for_owner(
        &self,
        owner_account_id: Uuid,
    ) -> Result<Vec<Client>, ToolError> {
        sqlx::query_as::<_, Client>(
            "SELECT id, client_id, client_secret_hash, name, redirect_uris, grant_types, \
                    token_endpoint_auth_method, owner_account_id, registration_source, \
                    disabled, created_at \
             FROM rmcp_client WHERE owner_account_id = $1 ORDER BY created_at",
        )
        .bind(owner_account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Enable or disable a client. Disabling is the revocation control: a
    /// disabled client stops resolving in [`Self::find_active_client`], so its
    /// live tokens stop being honoured at the next dispatch rather than at
    /// their next expiry.
    pub async fn set_client_disabled(
        &self,
        client_id: Uuid,
        disabled: bool,
    ) -> Result<(), ToolError> {
        let _scope_write = ScopeWrite::begin();
        sqlx::query("UPDATE rmcp_client SET disabled = $2 WHERE id = $1")
            .bind(client_id)
            .bind(disabled)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(db)
    }

    // -----------------------------------------------------------------------
    // Tool groups and client scoping
    //
    // INVARIANT: every method touching a table in
    // `crate::oauth::scope::SCOPE_AFFECTING_TABLES` opens with
    //
    //     let _scope_write = ScopeWrite::begin();
    //
    // which invalidates RMCP-07's resolution cache on BOTH sides of the write —
    // before it, so a committed revocation is never served from a cache hit
    // while the write is still in progress, and again on drop, for a resolve
    // that read the old rows and is about to cache them. See `ScopeWrite`.
    //
    // The binding must be NAMED. `let _ = ScopeWrite::begin()` drops the guard
    // immediately, putting both bumps before the write and losing the trailing
    // one entirely.
    //
    // This is ENFORCED, not merely stated, and CRATE-WIDE:
    // `no_in_crate_write_bypasses_scope_invalidation` walks every `.rs` file
    // under `src/` and fails — naming the file and the function — if a mutation
    // of one of those tables appears anywhere without the guard. The rule is
    // not special to this file: a future admin endpoint or ops tool elsewhere
    // may legitimately write these tables, it simply cannot do so without
    // invalidating. `ScopeWrite` is `pub(crate)` precisely so it can comply.
    // -----------------------------------------------------------------------

    /// Resolve an account's group-authoring authority FROM THE DATABASE, inside
    /// the caller's transaction.
    ///
    /// ## Why this is not a parameter
    /// The rule it feeds — a delegated author may not write a bare `*` pattern
    /// — is an authorization rule. The first cut of RMCP-06 let the caller pass
    /// its own `owner_kind`, which made the rule advisory: a delegated caller
    /// passing `Operator` stored a `*`, and [`crate::oauth::groups::Pattern::parse_stored`]
    /// then honours it for the life of the row, by design. Review (gpt56)
    /// rejected that, and was right to — it is the same defect RMCP-01 argued
    /// through five rounds, where a caller-minted marker token was thrown out
    /// and the fix that landed was to check authority in SQL inside the write's
    /// own transaction. This is that fix, for this rule.
    ///
    /// `FOR SHARE` locks the account row for the rest of the transaction, so
    /// operator-ness cannot be granted or revoked in the window between this
    /// read and the write it authorizes.
    ///
    /// A missing or DISABLED account yields [`ToolError::NotFound`] rather than
    /// a delegated authority: an account that cannot authenticate should not be
    /// authoring scoping records at all, so this refuses the write outright
    /// instead of quietly downgrading it to the less privileged path.
    ///
    /// ## RMCP-12: it now carries the OWNED NAMESPACES too
    ///
    /// Both halves of an actor's authority — is it an operator, and which
    /// servers does it own — are read in the SAME transaction, under `FOR
    /// SHARE`, so neither can move between the check and the write it
    /// authorizes. That is the whole reason delegation's rules are pure
    /// functions over an [`ActorAuthority`]: the value can be derived where the
    /// locks are, and the rule can then be the same one the read path uses.
    ///
    /// The namespace rows are locked as well as the account row. Without that,
    /// `clear_server_owner` could land between this read and the insert, and
    /// the write would proceed on a delegation that no longer exists.
    async fn actor_authority(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        account_id: Uuid,
    ) -> Result<ActorAuthority, ToolError> {
        let is_operator = Self::locked_active_account(tx, account_id).await?;
        let owned = sqlx::query_scalar::<_, String>(
            "SELECT namespace FROM rmcp_server_owner WHERE owner_account_id = $1 \
             ORDER BY namespace FOR SHARE",
        )
        .bind(account_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(db)?;
        Ok(ActorAuthority::from_live_state(account_id, is_operator, owned))
    }

    /// Read an account's operator flag, requiring it to be ACTIVE, and hold the
    /// row for the rest of the transaction.
    ///
    /// `FOR SHARE` is the whole point: it is what makes the answer true at
    /// COMMIT and not merely true when it was read. Every authorization that
    /// depends on account state goes through here, so there is one place where
    /// "is this account allowed, right now, and will it still be when this
    /// write lands" is answered.
    ///
    /// A missing or disabled account is [`ToolError::NotFound`] with one shared
    /// message — not two, and not a downgrade to a less privileged authority.
    async fn locked_active_account(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        account_id: Uuid,
    ) -> Result<bool, ToolError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT is_operator FROM rmcp_account WHERE id = $1 AND NOT disabled FOR SHARE",
        )
        .bind(account_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(|| ToolError::NotFound("no such active account".into()))
    }

    /// The authority of the account that OWNS a client, derived in the same
    /// transaction — reusing the actor's own when they are the same account.
    ///
    /// Why the client's owner and not the actor: what a scoping row resolves to
    /// is decided by the CLIENT OWNER's holdings
    /// ([`Self::client_namespaces`], [`Self::client_authorized_groups`] — both
    /// join on `c.owner_account_id`). An operator administering a delegated
    /// user's client must therefore attach that user's servers and groups, not
    /// their own; the alternative is a write that succeeds and then resolves to
    /// nothing, which is the most confusing possible outcome for the operator
    /// trying to help.
    async fn client_owner_authority(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        actor: &ActorAuthority,
        client_owner: Uuid,
    ) -> Result<ActorAuthority, ToolError> {
        if client_owner == actor.account_id() {
            return Ok(actor.clone());
        }
        Self::actor_authority(tx, client_owner).await
    }

    /// The client's owner, locked for the rest of the transaction.
    ///
    /// `FOR SHARE` so ownership cannot be reassigned between this read and the
    /// write it authorizes — a TOCTOU that would let a write proceed on stale
    /// authority. `None` when there is no such client; callers must answer that
    /// exactly as they answer "not yours", so this is not an existence oracle.
    async fn locked_client_owner(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        client_id: Uuid,
    ) -> Result<Option<Uuid>, ToolError> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT owner_account_id FROM rmcp_client WHERE id = $1 FOR SHARE",
        )
        .bind(client_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)
    }

    /// Insert a tool group, VALIDATING it first (RMCP-06).
    ///
    /// This is the write-time gate the matcher depends on. Every pattern is
    /// parsed here and the name is normalised, so no row can hold something
    /// [`crate::oauth::groups::Pattern::matches`] would have to cope with at
    /// dispatch time. Storing the CANONICAL rendering rather than the author's
    /// literal text means the round-trip is stable and two spellings of one
    /// pattern cannot both sit in the same row.
    ///
    /// The authority that decides whether a bare `*` is acceptable is read from
    /// `owner_account_id`'s own row, in this transaction — see
    /// [`Self::actor_authority`]. There is deliberately no parameter for it:
    /// the caller states WHO is writing, never WHAT they are allowed to write.
    ///
    /// An empty `patterns` slice is permitted and stores a group that matches
    /// nothing — a legitimate state (a group being built up), and one the
    /// matcher already handles, rather than one to reject here.
    pub async fn insert_tool_group(
        &self,
        name: &str,
        description: &str,
        patterns: &[String],
        owner_account_id: Uuid,
    ) -> Result<Uuid, ToolError> {
        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;
        let authority = Self::actor_authority(&mut tx, owner_account_id).await?;
        let group = validate_group(name, description, patterns, &authority.authoring())?;
        let rendered = group.rendered_patterns();
        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO rmcp_tool_group (name, description, patterns, owner_account_id) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(&group.name)
        .bind(&group.description)
        .bind(rendered.as_slice())
        .bind(owner_account_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(unique_aware("a tool group with that name already exists"))?;
        tx.commit().await.map_err(db)?;
        Ok(id)
    }

    /// Every group owned by one account, name-ordered.
    ///
    /// Scoped to the owner rather than listing globally: the group NAME column
    /// is unique fleet-wide, so an unscoped list would let any account enumerate
    /// every other account's groups. RMCP-12 layers the operator's cross-account
    /// view on top of this; the default view is your own.
    pub async fn list_tool_groups(&self, owner_account_id: Uuid) -> Result<Vec<ToolGroup>, ToolError> {
        sqlx::query_as::<_, ToolGroup>(
            "SELECT id, name, description, patterns, owner_account_id, created_at \
             FROM rmcp_tool_group WHERE owner_account_id = $1 ORDER BY name",
        )
        .bind(owner_account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Rewrite a group's description and patterns, after validating them under
    /// the actor's DERIVED authority and confirming the actor owns the group.
    ///
    /// Two separate checks, and they are not redundant. Owning the group decides
    /// whether this row may be touched at all; being an operator decides whether
    /// a bare `*` may be written into it. An edit path that checked only the
    /// first would be the obvious way to launder a wildcard into a group that
    /// was created without one — create it clean as a delegated user, then edit
    /// it. Both run inside one transaction, so neither can be raced.
    ///
    /// Patterns are replaced wholesale, never merged. A partially applied
    /// permission change is a state nobody chose.
    ///
    /// Returns [`ToolError::NotFound`] for both "no such group" and "not
    /// yours", deliberately without distinguishing them — the distinction would
    /// confirm the existence of another account's group.
    pub async fn update_tool_group(
        &self,
        actor_account_id: Uuid,
        group_id: Uuid,
        description: &str,
        patterns: &[String],
    ) -> Result<(), ToolError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let authority = Self::actor_authority(&mut tx, actor_account_id).await?;
        // The name is not editable here (renaming would have to contend with the
        // fleet-wide UNIQUE constraint, which is RMCP-08's surface to own), so
        // only the two editable fields are validated.
        let description = normalize_description(description)?;
        let patterns: Vec<String> =
            validate_patterns(patterns, &authority.authoring())?.iter().map(Pattern::render).collect();
        // Editing a group's patterns changes what every client scoped to it
        // resolves to, so the resolver's cache must see this write. Held to the
        // end of the function, past the commit.
        let _scope_write = ScopeWrite::begin();
        let updated = sqlx::query(
            "UPDATE rmcp_tool_group SET description = $3, patterns = $4 \
             WHERE id = $1 AND owner_account_id = $2",
        )
        .bind(group_id)
        .bind(actor_account_id)
        .bind(&description)
        .bind(patterns.as_slice())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        if updated.rows_affected() == 0 {
            return Err(ToolError::NotFound("no such tool group for this account".into()));
        }
        tx.commit().await.map_err(db)
    }

    /// Delete a group the actor owns.
    ///
    /// `rmcp_client_scope` cascades, so deleting a group REVOKES it from every
    /// client that drew on it. That direction is safe by construction — a
    /// deletion can only ever narrow what a connector reaches, which is the one
    /// direction of change this schema never has to guard.
    pub async fn delete_tool_group(
        &self,
        actor_account_id: Uuid,
        group_id: Uuid,
    ) -> Result<(), ToolError> {
        // Deleting cascades `rmcp_client_scope`, so it REVOKES the group from
        // every client that drew on it — the direction where a stale cache
        // entry is authority that still works.
        let _scope_write = ScopeWrite::begin();
        let deleted = sqlx::query("DELETE FROM rmcp_tool_group WHERE id = $1 AND owner_account_id = $2")
            .bind(group_id)
            .bind(actor_account_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        if deleted.rows_affected() == 0 {
            return Err(ToolError::NotFound("no such tool group for this account".into()));
        }
        Ok(())
    }

    /// Seed [`STARTER_GROUPS`] for an operator account, idempotently.
    ///
    /// Exists so a fresh install has usable scoping the moment the first
    /// connector is registered. The alternative — an operator facing several
    /// hundred tool names — is how a wildcard gets reached for, which is the
    /// outcome this whole item is built to avoid.
    ///
    /// `ON CONFLICT DO NOTHING` on the unique name makes re-running a no-op, so
    /// this is safe to call on every startup and, crucially, will not overwrite
    /// an operator's edits to a seeded group. Seeded rows are ordinary rows:
    /// editable, deletable, and not re-created if deleted on purpose... which is
    /// exactly why this is not called automatically from anywhere yet. RMCP-08
    /// owns when it runs.
    ///
    /// The target account must actually BE an operator — verified against its
    /// row, not asserted by the caller. Seeding is an operator action, and
    /// letting it run for a delegated account would hand that account a set of
    /// broad prefix groups it never authored.
    ///
    /// Returns the number of groups actually created.
    pub async fn seed_starter_groups(&self, owner_account_id: Uuid) -> Result<u64, ToolError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        if !Self::actor_authority(&mut tx, owner_account_id).await?.is_operator() {
            return Err(ToolError::InvalidArgument(
                "starter groups may only be seeded onto an operator account".into(),
            ));
        }
        // After the authority check, so a refused seed does not flush the cache.
        let _scope_write = ScopeWrite::begin();
        let mut created = 0u64;
        for starter in STARTER_GROUPS {
            let patterns: Vec<String> = starter.patterns.iter().map(|p| (*p).to_string()).collect();
            // Validated on the way in like any other write, so a bad edit to the
            // seed list fails here rather than being the one path that bypasses
            // the matcher's contract.
            //
            // `Authoring::Operator` is sound HERE and only here: the account was
            // verified to be an active operator above, in this transaction,
            // under `FOR SHARE`.
            let group = validate_group(
                starter.name,
                starter.description,
                &patterns,
                &crate::oauth::delegation::Authoring::Operator,
            )?;
            let rendered = group.rendered_patterns();
            let inserted = sqlx::query(
                "INSERT INTO rmcp_tool_group (name, description, patterns, owner_account_id) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (name) DO NOTHING",
            )
            .bind(&group.name)
            .bind(&group.description)
            .bind(rendered.as_slice())
            .bind(owner_account_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            created += inserted.rows_affected();
        }
        tx.commit().await.map_err(db)?;
        Ok(created)
    }

    /// The groups a client draws on, as stored rows.
    ///
    /// For DISPLAY. Resolving a tool set from these is a bug: the rows carry a
    /// stored `*` with no indication of whether its owner is still an operator,
    /// and honouring one from a demoted owner is precisely the revocation gap
    /// round 2 of review found. Use [`Self::client_authorized_groups`], which
    /// reads the authority in the same query.
    ///
    /// Joins `rmcp_client` and requires the client to be ENABLED **and to share
    /// the group's owner account**.
    ///
    /// The owner match is a READ-path re-check of what the write path already
    /// enforces, and round 9 was right to ask for it: the write check is
    /// point-in-time, so a scope row written legitimately and then followed by
    /// an ownership TRANSFER would leave a group owned by one account still
    /// attached to another account's client. Re-checking on read means such a
    /// stale grant silently stops resolving rather than silently continuing to
    /// work. Narrowing is always the safe direction to be wrong in.
    ///
    /// NOTE for RMCP-12: this makes cross-owner attachment unreadable, full
    /// stop. If the delegation model introduces a legitimate operator override
    /// (an operator-owned group attached to a delegated user's client), THIS
    /// predicate is what must be widened, deliberately and with its own tests —
    /// not quietly relaxed because a case stopped working. The caller
    /// normally arrives via [`Self::find_active_client`], which already filters
    /// disabled clients — but this method takes a raw internal id, so a caller
    /// that obtained one another way would otherwise read scope for a client an
    /// operator had just switched off. Review round 1 (free) flagged exactly
    /// that path; defence in depth costs one join.
    ///
    /// TERM #637 (part B, finding 3) added the OWNER-ACCOUNT re-check
    /// (`JOIN rmcp_account … AND NOT a.disabled`). Disabling an account is what
    /// an operator reaches for when it is compromised, and without this join a
    /// disabled owner's groups kept authorizing: the caller behind a token need
    /// NOT be the client's owner (consent is `(account_id, client_id)` and is
    /// independent of `rmcp_client.owner_account_id`), so RMCP-05's
    /// active-account check on the CALLER does not cover the OWNER. The group's
    /// authority outlived the account it derived from — the same read-path
    /// re-derivation rule this method already applied to ownership, with one
    /// input missed.
    ///
    /// An unknown client, a disabled client, a client whose owner is disabled,
    /// or a known client with no scope
    /// rows all yield an EMPTY vector — which RMCP-07 intersects to the empty set. This is the
    /// fail-closed default the whole scoping model rests on, and the reason
    /// this method does not signal "unknown client" differently: there is no
    /// caller for whom the distinction should change the outcome.
    pub async fn client_tool_groups(&self, client_id: Uuid) -> Result<Vec<ToolGroup>, ToolError> {
        sqlx::query_as::<_, ToolGroup>(
            "SELECT g.id, g.name, g.description, g.patterns, g.owner_account_id, g.created_at \
             FROM rmcp_tool_group g \
             JOIN rmcp_client_scope s ON s.tool_group_id = g.id \
             JOIN rmcp_client c ON c.id = s.client_id AND NOT c.disabled \
                                AND c.owner_account_id = g.owner_account_id \
             JOIN rmcp_account a ON a.id = g.owner_account_id AND NOT a.disabled \
             WHERE s.client_id = $1 ORDER BY g.name",
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// The groups a client draws on, each paired with its owner's CURRENT
    /// authority — the input [`crate::oauth::groups::resolve_groups`] needs.
    ///
    /// This is the resolution entry point RMCP-07 should call.
    /// [`Self::client_tool_groups`] returns the rows alone and is for display;
    /// resolving from it would drop the authority and honour a stale `*`.
    ///
    /// ## Why the authority is read HERE, in this query
    /// A bare `*` is only legitimate from an operator, and operator-ness is
    /// revocable. Reading it in the same statement as the group rows means there
    /// is no window in which an account could be demoted between the two reads,
    /// and no way for a caller to supply an authority of its own choosing.
    ///
    /// ## One gate, composed — not two half-checks
    /// Owner state is decided in exactly ONE place: the `rmcp_account` join,
    /// which carries `AND NOT a.disabled` verbatim as in
    /// [`Self::client_tool_groups`] and [`Self::client_namespaces`] (TERM #637B).
    /// A disabled owner's groups therefore do not appear here AT ALL, so the
    /// projected `owner_is_operator` does not restate the disabled check — by
    /// the time a row exists, its owner is known enabled.
    ///
    /// An earlier revision had this backwards and it is worth recording why,
    /// because a clean rebase produced it silently: the join omitted
    /// `NOT a.disabled` and the projection carried
    /// `(a.is_operator AND NOT a.disabled)` instead. That reads as equivalent
    /// and is not. It gated only the WILDCARD on the owner being enabled, so a
    /// disabled owner's `*` collapsed while every one of their ordinary
    /// patterns — `weather_*`, `peerhub::*` — kept resolving. The two hunks live
    /// in different functions, so there was no textual conflict to notice; the
    /// enforcing query was simply weaker than the display query beside it, on
    /// exactly the hole 637B had just closed.
    ///
    /// If the join predicate is ever changed, the projection must be revisited
    /// with it. They are one rule written in two clauses, not two rules.
    ///
    /// It also carries [`Self::client_tool_groups`]'s read-path OWNER re-check
    /// (`c.owner_account_id = g.owner_account_id`, added on main by round 9).
    /// That check belongs here more than it belongs there: this is the query a
    /// tool set is actually resolved from, so a group left attached across an
    /// ownership TRANSFER must stop resolving here, not merely stop being
    /// displayed. Losing it in a rebase would have left the enforcing path
    /// weaker than the display path.
    ///
    /// The join to `rmcp_account` is INNER, so a group whose owning account has
    /// gone yields no row at all rather than a group with no authority. That is
    /// the fail-closed direction and it costs nothing: `owner_account_id`
    /// cascades on delete, so this only fires in states that should not exist.
    pub async fn client_authorized_groups(
        &self,
        client_id: Uuid,
    ) -> Result<Vec<AuthorizedGroup>, ToolError> {
        sqlx::query_as::<_, AuthorizedGroup>(
            "SELECT g.id, g.name, g.description, g.patterns, g.owner_account_id, g.created_at, \
                    a.is_operator AS owner_is_operator \
             FROM rmcp_tool_group g \
             JOIN rmcp_client_scope s ON s.tool_group_id = g.id \
             JOIN rmcp_client c ON c.id = s.client_id AND NOT c.disabled \
                                AND c.owner_account_id = g.owner_account_id \
             JOIN rmcp_account a ON a.id = g.owner_account_id AND NOT a.disabled \
             WHERE s.client_id = $1 ORDER BY g.name",
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// The federated namespaces a client may see.
    ///
    /// Empty for an unknown, disabled, or unscoped client — and, like
    /// [`Self::client_tool_groups`], this RE-CHECKS ownership on the read path
    /// rather than trusting the write-time check. Round 10 caught that the
    /// previous revision had added that re-check for tool groups but left the
    /// symmetric gap here, which is the more dangerous of the two: namespaces
    /// are reassignable and REVOCABLE (`set_server_owner` / `clear_server_owner`),
    /// so a `rmcp_client_server` row outlives the ownership that justified it.
    /// Without the join, clearing a delegation would leave the former owner's
    /// connector still reaching that whole federated server.
    ///
    /// Joining `rmcp_server_owner` means an UNOWNED namespace also resolves to
    /// nothing, consistent with the write path's refusal to attach one: "nobody
    /// has claimed this server" must never read as "everyone may reach it".
    ///
    /// And joining `rmcp_account` means a DISABLED owner's delegation stops
    /// resolving too — TERM #637 part B, the symmetric half of the same gap
    /// closed in [`Self::client_tool_groups`]. A federated server delegated to
    /// an account that has since been disabled must not remain reachable
    /// through that account's connectors.
    ///
    /// ## RMCP-12 widened this predicate, deliberately and with tests
    ///
    /// This is the operator override the note above invited, and it is the ONE
    /// widening in this item. `rmcp_server_owner` records DELEGATIONS; the
    /// operator owns every namespace by default and has no row. Without the
    /// override, an operator's own connector could not reach a federated server
    /// at all unless the operator first delegated it to themselves — and the
    /// symptom would be a scoping row that saves and then resolves to nothing.
    ///
    /// What it does NOT do is widen for anyone else. Read the predicate as two
    /// disjoint cases:
    ///
    /// - **The client owner is an active operator** — an explicit
    ///   `rmcp_client_server` row is honoured. Still explicit reach: no row, no
    ///   namespace. And still revocable — demote or disable the account and the
    ///   `a` join drops every row on the next call.
    /// - **Anyone else** — `o.owner_account_id = c.owner_account_id` must hold,
    ///   exactly as before. An unowned namespace (`o` NULL through the LEFT
    ///   JOIN) fails it, so "nobody has claimed this server" still never reads
    ///   as "everyone may reach it".
    ///
    /// **Carrying TERM #637B's hardening across the rewrite.** The previous
    /// version joined `rmcp_account` on `o.owner_account_id`, which is what
    /// made a DISABLED delegated owner's namespaces stop resolving. This
    /// version joins on `c.owner_account_id` instead — and that is not a
    /// weakening, because in the delegated branch the predicate
    /// `o.owner_account_id = c.owner_account_id` forces those two columns to be
    /// the SAME account, so the same row is checked and `NOT a.disabled` still
    /// applies to it. Moving the join is what lets the operator branch see the
    /// client owner's `is_operator` at all. If that equality predicate is ever
    /// relaxed, this join has to be revisited with it: they are one rule in two
    /// clauses, exactly as `client_authorized_groups` documents for its own
    /// pair.
    pub async fn client_namespaces(&self, client_id: Uuid) -> Result<Vec<String>, ToolError> {
        sqlx::query_scalar::<_, String>(
            "SELECT s.namespace FROM rmcp_client_server s \
             JOIN rmcp_client c ON c.id = s.client_id AND NOT c.disabled \
             JOIN rmcp_account a ON a.id = c.owner_account_id AND NOT a.disabled \
             LEFT JOIN rmcp_server_owner o ON o.namespace = s.namespace \
             WHERE s.client_id = $1 \
               AND (a.is_operator OR o.owner_account_id = c.owner_account_id) \
             ORDER BY s.namespace",
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// The group-count bound, checked before any transaction is opened.
    ///
    /// Bound the client's total resolution cost at the point an operator is
    /// present to read the error. Every group contributes up to
    /// MAX_PATTERNS_PER_GROUP patterns, and resolution walks the concatenated
    /// list once per catalog tool, so the group count is what turns a bounded
    /// per-group cap into an unbounded aggregate. Refused rather than
    /// truncated: silently dropping groups would store a scope different from
    /// the one the operator just asked for.
    ///
    /// Split out so RMCP-08's atomic administrative edit applies the same bound
    /// before IT opens a transaction, rather than carrying a second copy of the
    /// number.
    fn check_group_budget(group_ids: &[Uuid]) -> Result<(), ToolError> {
        let distinct = group_ids.iter().collect::<std::collections::HashSet<_>>().len();
        if distinct > MAX_GROUPS_PER_CLIENT {
            return Err(ToolError::InvalidArgument(format!(
                "a client may be scoped to at most {MAX_GROUPS_PER_CLIENT} tool groups \
                 (requested {distinct}); combine patterns into fewer groups"
            )));
        }
        Ok(())
    }

    /// Replace a client's group assignments wholesale, in one transaction,
    /// after verifying that `actor` owns both the client and every group.
    ///
    /// ## Why the check is HERE, after four rounds of argument about it
    /// Earlier revisions left this method unchecked and documented the
    /// obligation, then narrowed its visibility, then demanded a marker token.
    /// Review round 5 correctly observed that a data-free token proves nothing:
    /// any in-crate caller can mint one and claim an audit it never did.
    ///
    /// The check is expressible right here, so it is done right here. Ownership
    /// is already in this schema — `rmcp_client.owner_account_id` and
    /// `rmcp_tool_group.owner_account_id` — and both are read inside the SAME
    /// transaction as the write, so there is no window in which ownership could
    /// change between the check and the mutation.
    ///
    /// RMCP-12 still owns the DELEGATION model on top of this (operator
    /// override, namespace delegation, the UI's read scoping). What it inherits
    /// is a repository that already refuses a cross-account write, rather than
    /// one that trusts it was called correctly.
    ///
    /// Returns [`ToolError::NotFound`] when the client is not the actor's, and
    /// [`ToolError::InvalidArgument`] when any group is not — deliberately
    /// without naming which, so this is not an enumeration oracle for another
    /// account's group ids.
    pub async fn set_client_tool_groups(
        &self,
        actor_account_id: Uuid,
        client_id: Uuid,
        group_ids: &[Uuid],
    ) -> Result<(), ToolError> {
        Self::check_group_budget(group_ids)?;

        // Established AFTER the cheap argument check: `ScopeWrite::begin` bumps
        // the epoch immediately, so entering it only to reject the request would
        // invalidate every cached resolution for a write that never happened.
        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;
        Self::set_client_tool_groups_in_tx(
            &mut tx,
            &_scope_write,
            actor_account_id,
            client_id,
            group_ids,
        )
        .await?;
        // Invalidates on the ERROR path too: a commit that failed to report is
        // not a commit that provably did not happen, and an unnecessary
        // invalidation costs one store read while a missed one leaves a revoked
        // permission live.
        tx.commit().await.map_err(db)
    }

    /// The group-scoping write itself, INSIDE a caller's transaction.
    ///
    /// ## Why this is split out
    /// RMCP-08's administrative edit changes a client's enabled state, its
    /// redirect URIs, its groups and its namespaces in one operator action.
    /// Applied as separate transactions, a failure partway leaves the client
    /// ENABLED with its old scope — a half-applied authorization change, and the
    /// one kind least worth leaving behind. Sharing this body is what lets the
    /// whole edit commit or not at all, without a second copy of the
    /// authorization rules that decide it.
    ///
    /// Every rule below is RMCP-12's, unchanged: `actor_authority` +
    /// `locked_client_owner` + [`authorize_client_write`], all read under `FOR
    /// SHARE` in the caller's transaction. This split moved WHERE the
    /// transaction is opened, never what it checks.
    ///
    /// ## The `_scope_write` parameter
    /// A WITNESS, not a value: a `&ScopeWrite` cannot be produced without a live
    /// guard, so this function is uncallable unless the caller is holding one
    /// and will therefore invalidate the resolution cache on both sides of the
    /// write. That is a compiler check where the crate-wide scanner
    /// (`no_in_crate_write_bypasses_scope_invalidation`) can only make a
    /// textual one, and the scanner recognises it for exactly that reason.
    async fn set_client_tool_groups_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _scope_write: &ScopeWrite,
        actor_account_id: Uuid,
        client_id: Uuid,
        group_ids: &[Uuid],
    ) -> Result<(), ToolError> {
        // The actor's authority and the client's owner, both read under `FOR
        // SHARE` in this transaction. Without the locks the checks are a TOCTOU:
        // a concurrent transfer or demotion could land in the gap and the write
        // would proceed on stale authority.
        //
        // Same answer for "no such client" and "not yours" (see
        // `delegation::authorize_client_write`): distinguishing them would
        // confirm the existence of another account's client.
        let actor = Self::actor_authority(tx, actor_account_id).await?;
        let Some(client_owner) = Self::locked_client_owner(tx, client_id).await? else {
            return Err(ToolError::NotFound("no such client for this account".into()));
        };
        authorize_client_write(&actor, client_owner)?;
        // What this scope RESOLVES to is decided by the client owner's
        // holdings, so that is whose authority the group check runs against.
        let owner_authority = Self::client_owner_authority(tx, &actor, client_owner).await?;

        // Every requested group must belong to the actor, and each matching row
        // is LOCKED for the rest of the transaction (`FOR SHARE`) so ownership
        // cannot be reassigned underneath the write. An aggregate cannot be
        // combined with a row lock, so the ids are selected and counted here
        // rather than counted in SQL. Comparing against the DISTINCT input
        // count means a duplicate in the input cannot inflate the match.
        let owned_groups = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM rmcp_tool_group WHERE id = ANY($1) AND owner_account_id = $2 \
             FOR SHARE",
        )
        .bind(group_ids)
        .bind(owner_authority.account_id())
        .fetch_all(&mut **tx)
        .await
        .map_err(db)?
        .len() as i64;
        let requested = group_ids.iter().collect::<std::collections::HashSet<_>>().len() as i64;
        if owned_groups != requested {
            return Err(ToolError::InvalidArgument(
                "one or more tool groups do not belong to this account".into(),
            ));
        }

        // Delete-then-insert rather than a diff: a partially applied scope
        // change is a permission state nobody chose, and under concurrent edits
        // a diff can interleave into exactly that. Wholesale replacement makes
        // the outcome always one of the two intended states.
        sqlx::query("DELETE FROM rmcp_client_scope WHERE client_id = $1")
            .bind(client_id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        for group_id in group_ids {
            sqlx::query(
                "INSERT INTO rmcp_client_scope (client_id, tool_group_id) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(client_id)
            .bind(group_id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        }
        Ok(())
    }

    /// Replace a client's namespace assignments wholesale, after verifying that
    /// `actor` owns the client and every namespace.
    ///
    /// Namespace ownership comes from `rmcp_server_owner`. An UNOWNED namespace
    /// is refused rather than allowed: "nobody has claimed this server" must
    /// mean "no delegated owner may attach it", never "it is free for anyone".
    /// Same transaction, same reasoning, and the same deliberately unspecific
    /// error as [`Self::set_client_tool_groups`].
    pub async fn set_client_namespaces(
        &self,
        actor_account_id: Uuid,
        client_id: Uuid,
        namespaces: &[String],
    ) -> Result<(), ToolError> {
        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;
        Self::set_client_namespaces_in_tx(
            &mut tx,
            &_scope_write,
            actor_account_id,
            client_id,
            namespaces,
        )
        .await?;
        // Invalidates on the ERROR path too: a commit that failed to report is
        // not a commit that provably did not happen, and an unnecessary
        // invalidation costs one store read while a missed one leaves a revoked
        // permission live.
        tx.commit().await.map_err(db)
    }

    /// The namespace-scoping write itself, INSIDE a caller's transaction. Split
    /// out for the same reason as [`Self::set_client_tool_groups_in_tx`], and
    /// carrying the same `_scope_write` witness — see that function's doc. Every
    /// authorization rule below is RMCP-12's, unchanged.
    async fn set_client_namespaces_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _scope_write: &ScopeWrite,
        actor_account_id: Uuid,
        client_id: Uuid,
        namespaces: &[String],
    ) -> Result<(), ToolError> {
        // `FOR SHARE` throughout, exactly as in `set_client_tool_groups`. Round
        // 8 caught that this copy had been left as an unlocked `SELECT EXISTS`
        // while its own doc comment claimed the same locking guarantee — a
        // documented promise the code did not keep, which is worse than an
        // undocumented gap because it stops the next reader looking.
        //
        // `actor_authority` locks the account row AND every `rmcp_server_owner`
        // row that account holds, which is what closes the other half of that
        // race: `clear_server_owner` could otherwise land between the ownership
        // read and the insert, letting a former owner attach a server they no
        // longer own.
        let actor = Self::actor_authority(tx, actor_account_id).await?;
        let Some(client_owner) = Self::locked_client_owner(tx, client_id).await? else {
            return Err(ToolError::NotFound("no such client for this account".into()));
        };
        authorize_client_write(&actor, client_owner)?;

        // RMCP-12: ONE function decides this, here and on every other write
        // path. It is given an authority derived inside this transaction, under
        // those locks — never one a caller supplied, and never one read before
        // the transaction began.
        //
        // It is the CLIENT OWNER's authority, because that is whose ownership
        // `client_namespaces` re-joins on at resolution time. Checking the
        // actor's instead would let an operator write rows for a delegated
        // user's client that then resolve to nothing.
        let owner_authority = Self::client_owner_authority(tx, &actor, client_owner).await?;
        authorize_namespace_scoping(&owner_authority, namespaces)?;

        sqlx::query("DELETE FROM rmcp_client_server WHERE client_id = $1")
            .bind(client_id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        for namespace in namespaces {
            sqlx::query(
                "INSERT INTO rmcp_client_server (client_id, namespace) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(client_id)
            .bind(namespace)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // RMCP-08 — client administration and initial access tokens
    //
    // Every write below authorizes through RMCP-12's machinery
    // (`actor_authority` + `locked_client_owner` + `authorize_client_write`, or
    // `authorize_operator_action`), derived INSIDE the writing transaction
    // under `FOR SHARE`. There is deliberately no second mechanism and no
    // authority a caller can carry in: round 2 of RMCP-08's review found these
    // writes constrained only by `id` and `version`, with the tool layer
    // passing the TARGET ROW's own owner as the actor — which asks the object
    // being modified who may modify it, and can only answer yes.
    // -----------------------------------------------------------------------

    /// The three administrative client queries, written out in full.
    ///
    /// ## Why the column list is repeated instead of shared
    ///
    /// It was a `const` spliced in with `format!()`. The interpolated value was
    /// a local constant, so it was not exploitable — and review round 3
    /// (`gpt56`) asked for it changed anyway, for a reason worth recording: the
    /// value of "no SQL string interpolation anywhere in this module" is that it
    /// is MECHANICALLY CHECKABLE. A scanner cannot tell "interpolating a
    /// private constant" from "interpolating something a caller reached", so
    /// every benign exception costs the rule its ability to be enforced by
    /// anything except someone noticing. This sprint has already found several
    /// guards that could not fail for the case they existed to catch; an
    /// advisory version of this one would be another.
    ///
    /// The cost of writing it out is drift between three literals. That is
    /// bounded and it is tested: `the_admin_queries_never_select_the_secret_hash`
    /// asserts every one of them projects `(client_secret_hash IS NOT NULL) AS
    /// confidential` and that none selects the hash itself — which is the only
    /// property the shared constant was actually protecting.
    const CLIENT_ADMIN_BY_ID: &'static str = "SELECT \
         id, client_id, name, redirect_uris, grant_types, token_endpoint_auth_method, \
         owner_account_id, registration_source, disabled, \
         (client_secret_hash IS NOT NULL) AS confidential, created_at, version \
         FROM rmcp_client WHERE id = $1";

    const CLIENT_ADMIN_BY_OWNER: &'static str = "SELECT \
         id, client_id, name, redirect_uris, grant_types, token_endpoint_auth_method, \
         owner_account_id, registration_source, disabled, \
         (client_secret_hash IS NOT NULL) AS confidential, created_at, version \
         FROM rmcp_client WHERE ($1::uuid IS NULL OR owner_account_id = $1) \
         ORDER BY created_at";

    const CLIENT_ADMIN_UPDATE: &'static str = "UPDATE rmcp_client SET \
         disabled = COALESCE($3::boolean, disabled), \
         redirect_uris = COALESCE($4::text[], redirect_uris), \
         version = version + 1 \
         WHERE id = $1 AND version = $2 \
         RETURNING \
         id, client_id, name, redirect_uris, grant_types, token_endpoint_auth_method, \
         owner_account_id, registration_source, disabled, \
         (client_secret_hash IS NOT NULL) AS confidential, created_at, version";

    /// One client, administratively. Includes DISABLED clients, unlike
    /// [`Self::find_active_client`]: an operator managing a revoked connector
    /// has to be able to see it, and this view authenticates nothing.
    pub async fn find_client_admin(&self, id: Uuid) -> Result<Option<ClientAdmin>, ToolError> {
        sqlx::query_as::<_, ClientAdmin>(Self::CLIENT_ADMIN_BY_ID)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Every client, administratively, or just one owner's.
    ///
    /// `None` means "every client". It is a READ, so a broad default is safe
    /// here in a way it never is for a write — the same split
    /// [`crate::tools::rmcp_session`] makes between its listing and its
    /// revocation.
    pub async fn list_clients_admin(
        &self,
        owner_account_id: Option<Uuid>,
    ) -> Result<Vec<ClientAdmin>, ToolError> {
        sqlx::query_as::<_, ClientAdmin>(Self::CLIENT_ADMIN_BY_OWNER)
        .bind(owner_account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Apply an administrative edit — AUTHORIZED, and the WHOLE edit, in ONE
    /// transaction.
    ///
    /// ## The two defects this shape exists for
    ///
    /// **Round 1: it was three transactions.** The client fields, the tool
    /// groups and the namespaces were written separately, so a failure partway
    /// left the client with its new enabled state and redirect URIs and its OLD
    /// scope — a half-applied authorization change that looks, from either
    /// side, like a deliberate configuration.
    ///
    /// **Round 2: it was not authorized at all.** The field `UPDATE` below
    /// constrains `id` and `version` and nothing else, so without the check it
    /// is reachable by anyone holding both. An ownership check DID run, but on
    /// the scope path only — so an edit touching just `enabled` or
    /// `redirect_uris` routed around it. `redirect_uris` is where an
    /// authorization code is delivered: rewriting one redirects where a linked
    /// account's credentials land, which makes it the most attacker-valuable
    /// field in the item.
    ///
    /// ## How it is authorized
    ///
    /// RMCP-12's machinery, unchanged and not duplicated: [`Self::actor_authority`]
    /// and [`Self::locked_client_owner`] read the actor and the client's owner
    /// under `FOR SHARE` in THIS transaction, and [`authorize_client_write`]
    /// decides. There is no proof value to carry in — the authority is derived
    /// where it is used, so an actor demoted or disabled between an earlier
    /// read and this commit cannot act on a stale snapshot.
    ///
    /// Returns `Ok(None)` when no row matched the version — either the client
    /// is gone or it has moved on. The caller re-reads either way.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_client_admin_edit(
        &self,
        actor_account_id: Uuid,
        client_id: Uuid,
        expected_version: i32,
        disabled: Option<bool>,
        redirect_uris: Option<&[String]>,
        tool_group_ids: Option<&[Uuid]>,
        namespaces: Option<&[String]>,
    ) -> Result<Option<ClientAdmin>, ToolError> {
        // The cheap argument bound first, before the epoch is bumped or a
        // transaction is opened — same ordering as the standalone writer.
        if let Some(group_ids) = tool_group_ids {
            Self::check_group_budget(group_ids)?;
        }

        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;

        // AUTHORIZE FIRST, in this transaction, before anything is written.
        let actor = Self::actor_authority(&mut tx, actor_account_id).await?;
        let Some(client_owner) = Self::locked_client_owner(&mut tx, client_id).await? else {
            return Err(ToolError::NotFound("no such client for this account".into()));
        };
        authorize_client_write(&actor, client_owner)?;

        let updated = sqlx::query_as::<_, ClientAdmin>(Self::CLIENT_ADMIN_UPDATE)
        .bind(client_id)
        .bind(expected_version)
        .bind(disabled)
        .bind(redirect_uris)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;

        let Some(updated) = updated else {
            tx.rollback().await.map_err(db)?;
            return Ok(None);
        };

        // The scoping writes re-derive the same authority inside this same
        // transaction. That is not redundant: it is what keeps ONE rule for
        // "may this actor scope this client", evaluated by the same function
        // whether the caller came through here or through the standalone
        // writers.
        if let Some(group_ids) = tool_group_ids {
            Self::set_client_tool_groups_in_tx(
                &mut tx,
                &_scope_write,
                actor_account_id,
                client_id,
                group_ids,
            )
            .await?;
        }
        if let Some(namespaces) = namespaces {
            Self::set_client_namespaces_in_tx(
                &mut tx,
                &_scope_write,
                actor_account_id,
                client_id,
                namespaces,
            )
            .await?;
        }

        // Any `?` above returns before this line, dropping `tx` and rolling the
        // WHOLE edit back — including the field update that had already
        // succeeded. That is the property this method exists for.
        tx.commit().await.map_err(db)?;
        Ok(Some(updated))
    }

    /// Revoke a connector: disable it and kill its live refresh tokens, in ONE
    /// authorized transaction.
    ///
    /// Round 2 (`gpt56`) found the revoke path taking only a client id — no
    /// actor, no authority, nothing. Revocation only ever NARROWS access, which
    /// is why it is not approval-gated; but "only narrows" is not "anyone may",
    /// because disabling somebody else's connector is a denial of service
    /// against their linked account.
    ///
    /// Both halves commit together. Disabling without killing the refresh
    /// tokens would let a later re-enable silently resurrect a session that had
    /// been cut off; killing tokens without disabling would let the client
    /// re-authorize immediately.
    ///
    /// Returns the number of refresh tokens newly revoked. Idempotent.
    pub async fn revoke_client(
        &self,
        actor_account_id: Uuid,
        client_id: Uuid,
    ) -> Result<u64, ToolError> {
        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;

        let actor = Self::actor_authority(&mut tx, actor_account_id).await?;
        let Some(client_owner) = Self::locked_client_owner(&mut tx, client_id).await? else {
            return Err(ToolError::NotFound("no such client for this account".into()));
        };
        authorize_client_write(&actor, client_owner)?;

        // The version is bumped so a concurrent editor holding a pre-revocation
        // read is refused rather than re-enabling the client by saving a stale
        // form.
        sqlx::query("UPDATE rmcp_client SET disabled = true, version = version + 1 WHERE id = $1")
            .bind(client_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;

        let tokens = sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = now() \
             WHERE client_id = $1 AND revoked_at IS NULL",
        )
        .bind(client_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        tx.commit().await.map_err(db)?;
        Ok(tokens)
    }

    /// Store an initial access token's digest. OPERATOR-only.
    ///
    /// Takes a [`SecretHash`], which can only be produced by hashing — the same
    /// enforcement-by-type as authorization codes and refresh tokens.
    ///
    /// The operator check is [`authorize_operator_action`], against an authority
    /// derived in this transaction. Round 2 (`gpt56`) found no check at all
    /// here, and an initial access token is precisely the thing that makes
    /// gated DCR reachable — so an unauthorized mint hands out the ability to
    /// create clients.
    pub async fn insert_registration_token(
        &self,
        token_hash: &SecretHash,
        issued_by: Uuid,
        label: &str,
        uses: i32,
        ttl_seconds: i64,
    ) -> Result<(), ToolError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let actor = Self::actor_authority(&mut tx, issued_by).await?;
        authorize_operator_action(&actor, OPERATOR_ONLY_REGISTRATION_TOKEN)?;
        sqlx::query(
            "INSERT INTO rmcp_registration_token \
                 (token_hash, issued_by, label, uses_remaining, expires_at) \
             VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5::double precision))",
        )
        .bind(token_hash.as_bytes())
        .bind(issued_by)
        .bind(label)
        .bind(uses)
        .bind(ttl_seconds as f64)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)
    }

    /// Spend one use of an initial access token — atomically, and only if the
    /// ISSUING ACCOUNT still holds the authority that minted it.
    ///
    /// ## The defect this closes (review round 3)
    ///
    /// The previous version checked the TOKEN's own state — unspent, unexpired,
    /// unrevoked — and nothing about the account behind it. So a token minted
    /// by an operator who was later demoted or disabled went on registering
    /// clients until it expired.
    ///
    /// That is this sprint's defect class for the seventh time, and it is worth
    /// naming as a sequence rather than an incident: a write-time check trusted
    /// on the read path (rounds 1–2), the same hole reached through creation
    /// rather than modification (round 2's fifth instance), and now a BEARER
    /// CREDENTIAL whose backing authority was never re-checked at redemption.
    /// Every direct write path in this item derives authority inside its own
    /// transaction; a carried credential walked around all of them.
    ///
    /// **A token is a read path.** Any authority that can be REVOKED must be
    /// re-derived when it is used, not trusted from when it was issued.
    ///
    /// ## Why re-derivation rather than revoking tokens on demotion
    ///
    /// Cascading a revocation at demotion time would also stop these tokens,
    /// and it is worth doing eventually — but it is only sound if nothing can
    /// be minted in the window between the demotion and the cascade, and it
    /// fixes nothing for a token minted by an account that is disabled rather
    /// than demoted, or for a cascade that fails halfway. Re-derivation is
    /// correct regardless of ordering and regardless of what else ran, which is
    /// why it is the control rather than the optimisation.
    ///
    /// ## Ordering, and why the use is not spent first
    ///
    /// The token row is locked `FOR UPDATE` and its issuer authorized BEFORE
    /// the decrement. A token presented while its issuer is unauthorized is not
    /// consumed — it was never spendable, and burning a use would let anyone
    /// holding a copy exhaust a legitimate token by presenting it during a
    /// demotion.
    ///
    /// Single-use atomicity is preserved by that same lock: two concurrent
    /// redemptions serialize on it, and PostgreSQL re-evaluates the `WHERE`
    /// after the lock is granted under READ COMMITTED, so the loser sees
    /// `uses_remaining = 0` and gets `None`.
    ///
    /// ## One answer for every failure
    ///
    /// Unknown, expired, revoked, exhausted, AND issued-by-an-account-that-no-
    /// longer-qualifies all return `None`. A caller that could tell them apart
    /// would have an oracle reporting which of its guesses was once a real
    /// token — and, worse, which operator had just been demoted.
    pub async fn claim_registration_token(
        &self,
        token_hash: &SecretHash,
    ) -> Result<Option<Uuid>, ToolError> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        // Lock the token row. `FOR UPDATE`, not `FOR SHARE`: this row is about
        // to be decremented, and two redeemers must not both pass the check.
        let Some(issued_by) = sqlx::query_scalar::<_, Uuid>(
            "SELECT issued_by FROM rmcp_registration_token \
             WHERE token_hash = $1 AND uses_remaining > 0 AND expires_at > now() \
               AND revoked_at IS NULL \
             FOR UPDATE",
        )
        .bind(token_hash.as_bytes())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        else {
            return Ok(None);
        };

        // RE-DERIVE the issuer's authority, here, under `FOR SHARE`, in this
        // transaction — the same discipline every direct write path in this
        // item uses. `actor_authority` errors for a missing or disabled
        // account, and the operator check is the same
        // `authorize_operator_action` the mint used, so the predicate is
        // evaluated twice against two different reads rather than being two
        // rules that could drift.
        let still_authorized = match Self::actor_authority(&mut tx, issued_by).await {
            Ok(actor) => {
                authorize_operator_action(&actor, OPERATOR_ONLY_REGISTRATION_TOKEN).is_ok()
            }
            // A disabled or deleted issuer. Not an error to the caller — the
            // same `None` as every other unusable token.
            Err(_) => false,
        };
        if !still_authorized {
            // Nothing is consumed, and the transaction is rolled back
            // explicitly so the intent is visible where it matters.
            tx.rollback().await.map_err(db)?;
            return Ok(None);
        }

        sqlx::query(
            "UPDATE rmcp_registration_token SET uses_remaining = uses_remaining - 1 \
             WHERE token_hash = $1",
        )
        .bind(token_hash.as_bytes())
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        tx.commit().await.map_err(db)?;
        Ok(Some(issued_by))
    }

    /// Revoke every outstanding initial access token. OPERATOR-only, verified
    /// in this transaction.
    ///
    /// The blunt control on purpose. A minted token is never readable again —
    /// only its digest is stored — so there is no way to name one, and an
    /// operator reaching for this has decided that no outstanding invitation
    /// should remain valid. Returns how many were still live.
    pub async fn revoke_all_registration_tokens(
        &self,
        actor_account_id: Uuid,
    ) -> Result<u64, ToolError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let actor = Self::actor_authority(&mut tx, actor_account_id).await?;
        authorize_operator_action(&actor, OPERATOR_ONLY_REGISTRATION_TOKEN)?;
        let revoked = sqlx::query(
            "UPDATE rmcp_registration_token SET revoked_at = now() \
             WHERE revoked_at IS NULL AND uses_remaining > 0 AND expires_at > now()",
        )
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();
        tx.commit().await.map_err(db)?;
        Ok(revoked)
    }

    // -----------------------------------------------------------------------
    // Authorization codes
    // -----------------------------------------------------------------------

    /// Store a code. `ttl_seconds` is applied against the DATABASE clock.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_auth_code(
        &self,
        code_hash: &SecretHash,
        client_id: Uuid,
        account_id: Uuid,
        redirect_uri: &str,
        resource: &str,
        code_challenge: &str,
        scope: &str,
        ttl_seconds: i64,
    ) -> Result<(), ToolError> {
        sqlx::query(
            "INSERT INTO rmcp_auth_code (code_hash, client_id, account_id, redirect_uri, \
                                         resource, code_challenge, scope, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(secs => $8::double precision))",
        )
        .bind(code_hash.as_bytes())
        .bind(client_id)
        .bind(account_id)
        .bind(redirect_uri)
        .bind(resource)
        .bind(code_challenge)
        .bind(scope)
        .bind(ttl_seconds as f64)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(db)
    }

    /// Atomically consume a code, returning it only if this call is the one
    /// that consumed it.
    ///
    /// This is the single-use guarantee, and it is why the check and the
    /// consumption are one statement. A read-then-write would let two
    /// concurrent redemptions both observe an unconsumed code and both succeed
    /// — a code replay that yields two valid token sets. The `WHERE consumed_at
    /// IS NULL` predicate is evaluated under the row lock the UPDATE takes, so
    /// exactly one caller can match; the loser gets `None` and must fail the
    /// exchange. Expiry is folded into the same predicate against the database
    /// clock, so an expired code can never be consumed either.
    pub async fn consume_auth_code(
        &self,
        code_hash: &SecretHash,
    ) -> Result<Option<AuthCode>, ToolError> {
        sqlx::query_as::<_, AuthCode>(
            "UPDATE rmcp_auth_code SET consumed_at = now() \
             WHERE code_hash = $1 AND consumed_at IS NULL AND expires_at > now() \
             RETURNING code_hash, client_id, account_id, redirect_uri, resource, \
                       code_challenge, scope, issued_at, expires_at, consumed_at",
        )
        .bind(code_hash.as_bytes())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Delete codes that are past expiry. Housekeeping only — correctness never
    /// depends on this having run, because every read already filters on
    /// `expires_at > now()`.
    pub async fn purge_expired_auth_codes(&self) -> Result<u64, ToolError> {
        sqlx::query("DELETE FROM rmcp_auth_code WHERE expires_at < now()")
            .execute(&self.pool)
            .await
            .map(|r| r.rows_affected())
            .map_err(db)
    }

    // -----------------------------------------------------------------------
    // Login sessions (RMCP-03)
    // -----------------------------------------------------------------------

    /// Claim a login session as spent, cluster-wide. Returns `true` only for
    /// the caller that claimed it; every later presentation returns `false`.
    ///
    /// This is what makes "one authentication yields at most one authorization
    /// code" true in a multi-replica deployment. RMCP-03's first revision kept
    /// the spent identifiers in a process-local map, which review round 1
    /// rejected: the same signed session cookie arriving at two replicas is
    /// unspent at both, so each issues a code. The property was right and the
    /// place was wrong.
    ///
    /// The claim is a single `INSERT … ON CONFLICT DO NOTHING`. The PRIMARY KEY
    /// on `jti_hash` is the arbiter, so exactly one caller anywhere in the
    /// cluster sees `rows_affected() == 1` — no lock, and no read-then-write
    /// window in which two callers both observe an unclaimed session. It is the
    /// same reasoning as [`Self::consume_auth_code`]'s conditional UPDATE:
    /// the check and the claim have to be one statement.
    ///
    /// Takes a [`SecretHash`], not the raw `jti`: the identifier lives inside a
    /// live session cookie, and this schema's standing rule is that no table
    /// holds anything presentable. `ttl_seconds` is applied against the
    /// DATABASE clock, like every other expiry here.
    ///
    /// A database error propagates rather than degrading to `true`. The caller
    /// must treat a failure as "cannot issue a code" — a guard that opens when
    /// its backing store is unreachable is not a guard.
    pub async fn claim_login_session(
        &self,
        jti_hash: &SecretHash,
        ttl_seconds: i64,
    ) -> Result<bool, ToolError> {
        let claimed = sqlx::query(
            "INSERT INTO rmcp_login_session_use (jti_hash, expires_at) \
             VALUES ($1, now() + make_interval(secs => $2::double precision)) \
             ON CONFLICT (jti_hash) DO NOTHING",
        )
        .bind(jti_hash.as_bytes())
        .bind(ttl_seconds as f64)
        .execute(&self.pool)
        .await
        .map_err(db)?
        .rows_affected();
        Ok(claimed == 1)
    }

    /// Delete login-session claims that are past their retention window.
    ///
    /// Housekeeping only. Correctness never depends on this having run: a row
    /// still present always denies, and a session whose row has been purged
    /// expired as a token long before.
    pub async fn purge_expired_login_sessions(&self) -> Result<u64, ToolError> {
        sqlx::query("DELETE FROM rmcp_login_session_use WHERE expires_at < now()")
            .execute(&self.pool)
            .await
            .map(|r| r.rows_affected())
            .map_err(db)
    }

    // -----------------------------------------------------------------------
    // Refresh tokens
    // -----------------------------------------------------------------------

    /// Store a refresh token in a family.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_refresh_token(
        &self,
        token_hash: &SecretHash,
        family_id: Uuid,
        client_id: Uuid,
        account_id: Uuid,
        resource: &str,
        scope: &str,
        ttl_seconds: i64,
    ) -> Result<(), ToolError> {
        sqlx::query(
            "INSERT INTO rmcp_refresh_token (token_hash, family_id, client_id, account_id, \
                                             resource, scope, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7::double precision))",
        )
        .bind(token_hash.as_bytes())
        .bind(family_id)
        .bind(client_id)
        .bind(account_id)
        .bind(resource)
        .bind(scope)
        .bind(ttl_seconds as f64)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(db)
    }

    /// Fetch a refresh token by hash REGARDLESS of its rotation, revocation, or
    /// expiry state.
    ///
    /// Deliberately unfiltered, unlike every other lookup in this file. Reuse
    /// detection needs to see the rotated and revoked rows: if this filtered
    /// them out, presenting a stolen already-rotated token would look
    /// indistinguishable from presenting a token that never existed, and the
    /// theft signal — the thing that lets RMCP-04 revoke the family — would be
    /// lost. The caller is responsible for checking [`RefreshToken::is_rotated`],
    /// `revoked_at`, and expiry; [`Self::refresh_token_is_live`] is the helper
    /// for the ordinary path.
    pub async fn find_refresh_token(
        &self,
        token_hash: &SecretHash,
    ) -> Result<Option<RefreshToken>, ToolError> {
        sqlx::query_as::<_, RefreshToken>(
            "SELECT token_hash, family_id, client_id, account_id, resource, scope, \
                    issued_at, expires_at, rotated_to, revoked_at \
             FROM rmcp_refresh_token WHERE token_hash = $1",
        )
        .bind(token_hash.as_bytes())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Whether a token row is usable right now, judged against the database
    /// clock rather than the process clock.
    ///
    /// **A revoked FAMILY kills every member, including rows inserted later.**
    /// That is not merely a convenience: round 7 of review found a real race
    /// without it. A concurrent `revoke_refresh_family` can take its snapshot
    /// before a rotation inserts the successor, block on the predecessor's row
    /// lock, and then revoke only the rows it could see — leaving the successor
    /// live in a family that was supposed to be dead. Deciding liveness from
    /// "does ANY row in this family carry a revocation" makes that ordering
    /// irrelevant, with no lock and no possibility of a row being missed
    /// because it did not exist yet. Family revocation is one-way and
    /// permanent, which is exactly the intended meaning of revoking a family
    /// after a detected token theft.
    pub async fn refresh_token_is_live(&self, token_hash: &SecretHash) -> Result<bool, ToolError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM rmcp_refresh_token t \
             WHERE t.token_hash = $1 AND t.rotated_to IS NULL AND t.revoked_at IS NULL \
               AND t.expires_at > now() \
               AND NOT EXISTS (SELECT 1 FROM rmcp_refresh_token r \
                               WHERE r.family_id = t.family_id AND r.revoked_at IS NOT NULL))",
        )
        .bind(token_hash.as_bytes())
        .fetch_one(&self.pool)
        .await
        .map_err(db)
    }

    /// Atomically rotate a live refresh token into a freshly-issued successor,
    /// with the successor's binding COPIED FROM the row being rotated.
    ///
    /// Returns `true` when this call performed the rotation, `false` when the
    /// presented token was not live (already rotated, revoked, expired, or
    /// unknown) — in which case the caller must treat it as a REUSE and revoke
    /// the family.
    ///
    /// Two review rounds shaped this signature, and both changes matter:
    ///
    /// - **Round 1:** retiring the old token and inserting the successor were
    ///   separate calls, so a failure between them left the user with a
    ///   rotated-away token and no successor — locked out, with the family
    ///   looking healthy. Both writes are now one transaction: the outcome is
    ///   always either "old token retired and successor usable" or "nothing
    ///   changed".
    ///
    /// - **Round 2:** the successor's `family_id`, `client_id`, `account_id`,
    ///   `resource` and `scope` were caller-supplied. That let a caller rotate a
    ///   live token into a successor bound to a DIFFERENT client, account, or
    ///   scope — laundering a narrow token into a broad one through what looks
    ///   like an ordinary refresh. They are now selected from the rotated row
    ///   itself (`INSERT … SELECT`), so a refresh can only ever reproduce the
    ///   binding it already had. The parameters are gone rather than validated,
    ///   because a check can be skipped by a new call site and a missing
    ///   parameter cannot.
    ///
    /// - **Round 7:** a concurrent family revocation could snapshot before the
    ///   successor existed and therefore revoke only the predecessor, leaving
    ///   the successor live in a dead family. Both this UPDATE and
    ///   [`Self::refresh_token_is_live`] now treat ANY revoked row in a family
    ///   as revoking the whole family, so a row that did not exist when the
    ///   revocation ran is still dead. No lock is needed and no row can be
    ///   missed for not existing yet.
    ///
    /// The conditional UPDATE still decides the sole winner under concurrency:
    /// two simultaneous refreshes both see a live row, but only one UPDATE
    /// matches `rotated_to IS NULL`, and the loser returns before the INSERT.
    pub async fn rotate_refresh_token(
        &self,
        token_hash: &SecretHash,
        successor_hash: &SecretHash,
        ttl_seconds: i64,
    ) -> Result<bool, ToolError> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        let rotated = sqlx::query(
            "UPDATE rmcp_refresh_token t SET rotated_to = $2 \
             WHERE t.token_hash = $1 AND t.rotated_to IS NULL AND t.revoked_at IS NULL \
               AND t.expires_at > now() \
               AND NOT EXISTS (SELECT 1 FROM rmcp_refresh_token r \
                               WHERE r.family_id = t.family_id AND r.revoked_at IS NOT NULL)",
        )
        .bind(token_hash.as_bytes())
        .bind(successor_hash.as_bytes())
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        if rotated != 1 {
            // Not live. Roll back rather than commit a no-op so nothing about
            // this attempt is persisted, and let the caller handle the reuse.
            tx.rollback().await.map_err(db)?;
            return Ok(false);
        }

        // The successor inherits its binding from the predecessor row. Only the
        // token hash and the expiry are new.
        sqlx::query(
            "INSERT INTO rmcp_refresh_token (token_hash, family_id, client_id, account_id, \
                                             resource, scope, expires_at) \
             SELECT $2, family_id, client_id, account_id, resource, scope, \
                    now() + make_interval(secs => $3::double precision) \
             FROM rmcp_refresh_token WHERE token_hash = $1",
        )
        .bind(token_hash.as_bytes())
        .bind(successor_hash.as_bytes())
        .bind(ttl_seconds as f64)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        tx.commit().await.map_err(db)?;
        Ok(true)
    }

    /// Revoke every token in a family. The response to a detected reuse: the
    /// legitimate holder and the thief cannot be told apart, so both are cut
    /// off and the human re-authorizes.
    /// Whether the (account, client) pair has ANY live session, for the
    /// per-request check on the RESOURCE-SERVER dispatch path (RMCP-05).
    ///
    /// ## Read the name literally — this is NOT per-session
    /// It answers a question about the PAIR, not about the session the
    /// presented token belongs to, and the difference is a real gap rather than
    /// a rounding error:
    ///
    /// - Revoking EVERY session for the pair (what consent revocation and
    ///   client disablement do) ⇒ this returns `false` ⇒ the next dispatch is
    ///   denied. That is the case this check enforces.
    /// - Revoking ONE session while another is still active ⇒ this returns
    ///   `true`, and **an access token minted for the revoked session is still
    ///   accepted** until it expires.
    ///
    /// An earlier revision of this comment called that "coarser but never
    /// wider", on the grounds that it can only deny what a family-precise check
    /// would also deny. That reasoning is wrong and worth naming so it is not
    /// reintroduced: denying LESS is permitting MORE, which on the security
    /// axis is WIDER, not narrower. This check is the permissive direction, and
    /// the gap above is its cost.
    ///
    /// ## Why it is not per-session today
    /// Not a design preference — there is nothing to key on. An access token
    /// carries `iss`, `sub`, `aud`, `client_id`, `scope`, `jti`, `exp`, `iat`
    /// and `nbf`, and none of them identifies a session: `sub` is the account,
    /// `client_id` is the client, and the `jti` is generated at mint time and
    /// persisted nowhere — this schema has no access-token table at all.
    /// Relating `iat` to a refresh row would be a guess, because families
    /// overlap in time and rotation moves the rows.
    ///
    /// **TERM #635 is the blocker.** Closing it means putting the family id in
    /// the access token; the family is already in scope at both mint sites in
    /// `crate::oauth::token`, so it is a claim away. Until then, per-session
    /// revocation at dispatch is not expressible and must not be claimed.
    ///
    /// The rule, exactly: no refresh rows for the pair ⇒ live (a client that
    /// never asked for `offline_access` has no sessions, and denying it would
    /// break every non-offline connector); at least one row unrevoked and
    /// unexpired ⇒ live; rows exist and all are revoked or expired ⇒ not live.
    pub async fn any_session_is_live(
        &self,
        account_id: Uuid,
        client_id: Uuid,
    ) -> Result<bool, ToolError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT NOT EXISTS ( \
                 SELECT 1 FROM rmcp_refresh_token WHERE account_id = $1 AND client_id = $2 \
             ) OR EXISTS ( \
                 SELECT 1 FROM rmcp_refresh_token \
                 WHERE account_id = $1 AND client_id = $2 \
                   AND revoked_at IS NULL AND expires_at > now() \
             )",
        )
        .bind(account_id)
        .bind(client_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)
    }

    pub async fn revoke_refresh_family(&self, family_id: Uuid) -> Result<u64, ToolError> {
        sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = now() \
             WHERE family_id = $1 AND revoked_at IS NULL",
        )
        .bind(family_id)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected())
        .map_err(db)
    }

    /// Revoke every live refresh token issued to a client.
    pub async fn revoke_client_refresh_tokens(&self, client_id: Uuid) -> Result<u64, ToolError> {
        sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = now() \
             WHERE client_id = $1 AND revoked_at IS NULL",
        )
        .bind(client_id)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected())
        .map_err(db)
    }

    // -----------------------------------------------------------------------
    // Consents
    // -----------------------------------------------------------------------

    /// Record a consent, or return the existing live one.
    ///
    /// Idempotent via the partial unique index on live rows, so a double-submit
    /// of the consent form cannot produce two approvals to revoke separately.
    pub async fn record_consent(
        &self,
        account_id: Uuid,
        client_id: Uuid,
        scope: &str,
    ) -> Result<Uuid, ToolError> {
        if let Some(existing) = self.find_live_consent(account_id, client_id, scope).await? {
            return Ok(existing.id);
        }
        let inserted = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO rmcp_consent (account_id, client_id, scope) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(account_id)
        .bind(client_id)
        .bind(scope)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;

        match inserted {
            Some(id) => Ok(id),
            // The ON CONFLICT path: another request inserted the same live
            // consent between our check and our insert. Re-read rather than
            // erroring — the caller's intent ("this consent exists") is
            // satisfied either way, and surfacing a conflict here would turn a
            // harmless double-submit of the consent form into a failed login.
            None => self
                .find_live_consent(account_id, client_id, scope)
                .await?
                .map(|c| c.id)
                .ok_or_else(|| {
                    // Neither inserted nor findable: the row was revoked
                    // between the conflict and the re-read. Report it rather
                    // than looping, so a pathological race is visible instead
                    // of spinning.
                    ToolError::Conflict(
                        "consent changed concurrently during recording; retry the request".into(),
                    )
                }),
        }
    }

    /// Find a live (unrevoked) consent.
    pub async fn find_live_consent(
        &self,
        account_id: Uuid,
        client_id: Uuid,
        scope: &str,
    ) -> Result<Option<Consent>, ToolError> {
        sqlx::query_as::<_, Consent>(
            "SELECT id, account_id, client_id, scope, granted_at, revoked_at \
             FROM rmcp_consent \
             WHERE account_id = $1 AND client_id = $2 AND scope = $3 AND revoked_at IS NULL",
        )
        .bind(account_id)
        .bind(client_id)
        .bind(scope)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Whether this account still consents to this client at all, for any
    /// scope. Checked on the dispatch path so revoking consent takes effect at
    /// the next call rather than at the next token expiry.
    pub async fn has_live_consent(
        &self,
        account_id: Uuid,
        client_id: Uuid,
    ) -> Result<bool, ToolError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM rmcp_consent \
             WHERE account_id = $1 AND client_id = $2 AND revoked_at IS NULL)",
        )
        .bind(account_id)
        .bind(client_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)
    }

    /// Revoke a consent and, in the same transaction, every refresh-token
    /// family issued to that client for that account.
    ///
    /// One transaction because the two halves must not be separable: a revoked
    /// consent whose refresh tokens still work is not a revocation, and an
    /// operator who saw "revoked" would reasonably believe it was.
    ///
    /// Returns the number of consents revoked. RMCP-11 needed the token count
    /// too, so the implementation moved to
    /// [`Self::revoke_consent_and_tokens`] and this signature was left exactly
    /// as it was rather than widened — there is one transaction, one set of SQL,
    /// and no second way to revoke a consent that could drift from this one.
    pub async fn revoke_consent(
        &self,
        account_id: Uuid,
        client_id: Uuid,
    ) -> Result<u64, ToolError> {
        self.revoke_consent_and_tokens(account_id, client_id).await.map(|(consents, _)| consents)
    }

    /// [`Self::revoke_consent`], also reporting how many token rows it killed.
    pub async fn revoke_consent_and_tokens(
        &self,
        account_id: Uuid,
        client_id: Uuid,
    ) -> Result<(u64, u64), ToolError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let consents = sqlx::query(
            "UPDATE rmcp_consent SET revoked_at = now() \
             WHERE account_id = $1 AND client_id = $2 AND revoked_at IS NULL",
        )
        .bind(account_id)
        .bind(client_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        let tokens = sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = now() \
             WHERE account_id = $1 AND client_id = $2 AND revoked_at IS NULL",
        )
        .bind(account_id)
        .bind(client_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        tx.commit().await.map_err(db)?;
        Ok((consents, tokens))
    }

    /// Revoke EVERY consent and EVERY refresh token an account holds, across
    /// all of its clients, in one transaction. Returns `(consents, tokens)`.
    ///
    /// The "somebody has my laptop" control. One transaction for the same
    /// reason [`Self::revoke_consent_and_tokens`] is one: a partial revocation
    /// here — some clients cut off, others still live — is a state nobody chose
    /// and one an operator would have no way to notice.
    pub async fn revoke_account_sessions(&self, account_id: Uuid) -> Result<(u64, u64), ToolError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let consents = sqlx::query(
            "UPDATE rmcp_consent SET revoked_at = now() \
             WHERE account_id = $1 AND revoked_at IS NULL",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        let tokens = sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = now() \
             WHERE account_id = $1 AND revoked_at IS NULL",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        tx.commit().await.map_err(db)?;
        Ok((consents, tokens))
    }

    // -----------------------------------------------------------------------
    // Sessions (RMCP-11)
    // -----------------------------------------------------------------------

    /// Refresh-token families, aggregated into sessions, filtered by any
    /// combination of account, client and family.
    ///
    /// Every filter is `NULL`-tolerant in SQL (`$n IS NULL OR col = $n`) so one
    /// statement serves every selector. The aggregate follows
    /// [`Self::refresh_token_is_live`]'s family-wide rule exactly:
    /// `min(revoked_at)` means ANY revoked row dates the family's death, and
    /// `live` is computed with the DATABASE clock so a process with a drifted
    /// clock cannot show a dead session as live. Deriving liveness in Rust from
    /// the returned timestamps would reintroduce the multi-clock problem
    /// RMCP-01's module docs rule out.
    ///
    /// `resource` and `scope` are safe to group by because
    /// [`Self::rotate_refresh_token`] copies them from the predecessor row —
    /// a family cannot contain two different bindings by construction.
    pub async fn list_token_families(
        &self,
        account_id: Option<Uuid>,
        client_id: Option<Uuid>,
        family_id: Option<Uuid>,
    ) -> Result<Vec<TokenFamily>, ToolError> {
        sqlx::query_as::<_, TokenFamily>(
            "SELECT t.family_id, t.client_id, t.account_id, t.resource, t.scope, \
                    min(t.issued_at)   AS issued_at, \
                    max(t.issued_at)   AS last_issued_at, \
                    max(t.expires_at)  AS expires_at, \
                    count(*)           AS token_count, \
                    min(t.revoked_at)  AS revoked_at, \
                    COALESCE(min(t.revoked_at) IS NULL AND max(t.expires_at) > now(), false) AS live \
             FROM rmcp_refresh_token t \
             WHERE ($1::uuid IS NULL OR t.account_id = $1) \
               AND ($2::uuid IS NULL OR t.client_id = $2) \
               AND ($3::uuid IS NULL OR t.family_id = $3) \
             GROUP BY t.family_id, t.client_id, t.account_id, t.resource, t.scope \
             ORDER BY min(t.issued_at) DESC",
        )
        .bind(account_id)
        .bind(client_id)
        .bind(family_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// The family a presented refresh token belongs to, whatever its state.
    ///
    /// Unfiltered for the same reason [`Self::find_refresh_token`] is: RFC 7009
    /// revocation of an already-dead token must still answer `200`, and reuse
    /// detection needs to see rotated and revoked rows. A filtered lookup would
    /// make a stolen token indistinguishable from a token that never existed.
    pub async fn family_of_refresh_token(
        &self,
        token_hash: &SecretHash,
    ) -> Result<Option<TokenFamily>, ToolError> {
        let family_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT family_id FROM rmcp_refresh_token WHERE token_hash = $1",
        )
        .bind(token_hash.as_bytes())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        match family_id {
            Some(id) => Ok(self.list_token_families(None, None, Some(id)).await?.into_iter().next()),
            None => Ok(None),
        }
    }

    /// Resolve an account NAME to its id, including a disabled account.
    ///
    /// Deliberately distinct from [`Self::find_active_account_by_name`], which
    /// hides disabled accounts so the authentication path cannot become an
    /// existence oracle. That rule protects authentication; applying it here
    /// would mean an operator could not revoke the sessions of an account they
    /// had just disabled — exactly when they most want to. This method is
    /// reachable only from the revocation path, which can only ever narrow
    /// access, so it never widens anything.
    pub async fn resolve_account_id(&self, name: &str) -> Result<Option<Uuid>, ToolError> {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM rmcp_account WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    /// Resolve a public `client_id` to its internal id, including a disabled
    /// client — same reasoning as [`Self::resolve_account_id`].
    pub async fn resolve_client_id(&self, client_id: &str) -> Result<Option<Uuid>, ToolError> {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM rmcp_client WHERE client_id = $1")
            .bind(client_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    /// Whether a request carrying this account, client and (optional) session
    /// may dispatch RIGHT NOW.
    ///
    /// One statement, four independent predicates, evaluated against live rows
    /// on every call. This is what makes revocation effective at the next
    /// dispatch rather than at the next token expiry: a validated JWT signature
    /// proves only that this server minted the token at some point in the past,
    /// and nothing about whether the authorization behind it still stands.
    ///
    /// The session predicate requires the family to be BOUND to this account and
    /// client, not merely to exist and be unrevoked. A token quoting a family id
    /// that belongs to some other binding is refused rather than accepted on the
    /// strength of the family being healthy — the same anti-substitution
    /// discipline [`Self::rotate_refresh_token`] applies to a successor's
    /// binding.
    ///
    /// `family_id` is mandatory and the SQL has NO null-tolerant arm. An earlier
    /// revision accepted `Option<Uuid>` and wrote `$3::uuid IS NULL OR (…)`,
    /// which made a request that named no session automatically session-valid —
    /// so revoking a family left an access token arriving without one still
    /// dispatching. Deciding absence is
    /// [`crate::oauth::revoke::RevocationService::dispatch_state`]'s job, and it
    /// denies; by the time this query runs there is no absent case left, so
    /// there is no null branch for a later edit to make permissive again.
    pub async fn dispatch_state(
        &self,
        account_id: Uuid,
        client_id: Uuid,
        family_id: Uuid,
    ) -> Result<crate::oauth::revoke::DispatchState, ToolError> {

        // Decoded by COLUMN NAME off a raw row rather than into a tuple: this
        // workspace builds sqlx without the derive/macros features (see
        // `crate::oauth::model`'s row-decoding note), and a positional tuple
        // decode would silently swap two `bool` columns if the SELECT were ever
        // reordered — which here would mean reporting the wrong denial reason,
        // or worse, the wrong decision.
        use sqlx::Row as _;
        let row = sqlx::query(
            "SELECT \
               EXISTS (SELECT 1 FROM rmcp_client WHERE id = $2 AND NOT disabled)  AS client_ok, \
               EXISTS (SELECT 1 FROM rmcp_account WHERE id = $1 AND NOT disabled) AS account_ok, \
               EXISTS (SELECT 1 FROM rmcp_consent \
                       WHERE account_id = $1 AND client_id = $2 AND revoked_at IS NULL) AS consent_ok, \
                   EXISTS (SELECT 1 FROM rmcp_refresh_token \
                           WHERE family_id = $3 AND account_id = $1 AND client_id = $2) \
               AND NOT EXISTS (SELECT 1 FROM rmcp_refresh_token \
                               WHERE family_id = $3 AND revoked_at IS NOT NULL) AS family_ok",
        )
        .bind(account_id)
        .bind(client_id)
        .bind(family_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;

        // A decode failure denies. `unwrap_or(false)` here is the fail-CLOSED
        // direction on every one of these columns — an unreadable answer to
        // "may this dispatch?" is not permission.
        let ok = |name: &str| row.try_get::<bool, _>(name).unwrap_or(false);

        // Ordered from the coarsest lever to the finest, so an operator reading
        // the audit trail sees the outermost reason a request was refused.
        Ok(if !ok("client_ok") {
            DispatchState::ClientDisabled
        } else if !ok("account_ok") {
            DispatchState::AccountDisabled
        } else if !ok("consent_ok") {
            DispatchState::ConsentRevoked
        } else if !ok("family_ok") {
            DispatchState::SessionRevoked
        } else {
            DispatchState::Allowed
        })
    }

    // -----------------------------------------------------------------------
    // Server ownership (RMCP-12)
    // -----------------------------------------------------------------------

    /// Assign (or reassign) ownership of a namespace, narrowing any clients the
    /// PREVIOUS owner had scoped to it. One owner per namespace, enforced by the
    /// primary key.
    ///
    /// **PRIVATE to this module, and it takes the authorization rather than the
    /// arguments.** Both halves matter, and round 1 of review is why:
    ///
    /// - Private, so no other module in the crate can reach it. `ScopeResolver`
    ///   used to call the `pub` version with no actor at all, which made
    ///   `DelegationService` the polite path rather than the only one. Rust's
    ///   module privacy is what turns "nobody should call this directly" from a
    ///   comment into a compile error.
    /// - It takes a [`DelegationGrant`], whose only constructor runs the
    ///   operator check, so even in-module the arguments cannot arrive
    ///   unauthorized. The rule itself still lives in exactly one place
    ///   (`delegation::authorize_delegation_change`); this method does not
    ///   restate it, it DEMANDS it.
    ///
    /// What this method owes is atomicity, which is the thing only it can
    /// provide.
    ///
    /// The narrowing is in the SAME transaction as the reassignment. The read
    /// path already refuses those rows the instant ownership moves — that is the
    /// enforcement — so this is the state catching up with the decision rather
    /// than the decision itself. Doing it here rather than lazily means an
    /// operator inspecting the former owner's client sees what it can actually
    /// reach.
    async fn set_server_owner(
        &self,
        grant: &DelegationGrant,
    ) -> Result<DelegationChange, ToolError> {
        let namespace = grant.namespace();
        let owner_account_id = grant.grantee();
        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;

        // RE-VERIFY, under lock, inside the writing transaction (round 2).
        // The proof establishes that the check ran; these two reads establish
        // that it still HOLDS at commit. Without them an operator demoted or
        // disabled between minting the proof and this statement could still
        // complete the grant — and that window is exactly the moment an
        // operator is racing to cut off a compromised account.
        let live_actor = Self::actor_authority(&mut tx, grant.actor()).await?;
        reverify_delegation_change(grant.actor(), &live_actor)?;
        // The GRANTEE's active status is equally point-in-time: the service
        // checked it before the transaction opened. Locked here so a delegation
        // cannot land on an account that was disabled in the meantime — the
        // read path would refuse it anyway (`client_namespaces` joins the
        // owner's account), so this stops the row existing rather than stopping
        // it working.
        Self::locked_active_account(&mut tx, owner_account_id).await?;

        let previous = sqlx::query_scalar::<_, Uuid>(
            "SELECT owner_account_id FROM rmcp_server_owner WHERE namespace = $1 FOR UPDATE",
        )
        .bind(namespace)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "INSERT INTO rmcp_server_owner (namespace, owner_account_id) VALUES ($1, $2) \
             ON CONFLICT (namespace) DO UPDATE SET owner_account_id = EXCLUDED.owner_account_id, \
                                                   granted_at = now()",
        )
        .bind(namespace)
        .bind(owner_account_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        // Only the clients whose owner no longer owns this namespace lose the
        // row — never the new owner's, and never an operator's, whose reach is
        // by default rather than by delegation.
        let rows_narrowed = Self::narrow_clients_losing(&mut tx, namespace).await?;
        tx.commit().await.map_err(db)?;
        Ok(DelegationChange {
            reassigned: previous.is_some_and(|prior| prior != owner_account_id),
            rows_narrowed,
        })
    }

    /// Delete every `rmcp_client_server` row for `namespace` that its client's
    /// owner can no longer justify.
    ///
    /// The predicate is the WRITE-side mirror of [`Self::client_namespaces`]'s
    /// read predicate, and deliberately so: a row this deletes is a row that
    /// query had already stopped returning. Keeping them identical is what makes
    /// the cleanup provably unable to remove reach that the read path would
    /// still have honoured.
    async fn narrow_clients_losing(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        namespace: &str,
    ) -> Result<u64, ToolError> {
        // Both callers already hold a guard, so this one is redundant for
        // correctness — and it is here anyway, because the crate-wide scan
        // (`no_in_crate_write_bypasses_scope_invalidation`) reasons per
        // FUNCTION, and it is right to: a helper that mutates a scope-affecting
        // table must invalidate on its own account, or the next caller added to
        // it inherits an obligation nothing checks. The epoch is a COUNT, so
        // nesting is exactly what it is built for.
        let _scope_write = ScopeWrite::begin();
        sqlx::query(
            "DELETE FROM rmcp_client_server s \
             USING rmcp_client c, rmcp_account a \
             WHERE s.namespace = $1 AND c.id = s.client_id AND a.id = c.owner_account_id \
               AND NOT a.is_operator \
               AND NOT EXISTS (SELECT 1 FROM rmcp_server_owner o \
                               WHERE o.namespace = s.namespace \
                                 AND o.owner_account_id = c.owner_account_id)",
        )
        .bind(namespace)
        .execute(&mut **tx)
        .await
        .map(|done| done.rows_affected())
        .map_err(db)
    }

    /// The namespaces an account owns. Empty for an account that owns none —
    /// which RMCP-12 reads as "may scope a client to nothing", never as "may
    /// scope a client to everything".
    pub async fn namespaces_owned_by(&self, owner_account_id: Uuid) -> Result<Vec<String>, ToolError> {
        sqlx::query_scalar::<_, String>(
            "SELECT namespace FROM rmcp_server_owner WHERE owner_account_id = $1 \
             ORDER BY namespace",
        )
        .bind(owner_account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Who owns a namespace, if anyone. An unowned namespace resolves to
    /// `None`, which every caller must read as "no delegated owner may touch
    /// it" rather than "anyone may".
    pub async fn find_server_owner(
        &self,
        namespace: &str,
    ) -> Result<Option<ServerOwner>, ToolError> {
        sqlx::query_as::<_, ServerOwner>(
            "SELECT namespace, owner_account_id, granted_at FROM rmcp_server_owner \
             WHERE namespace = $1",
        )
        .bind(namespace)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Remove a delegation, narrowing every client that drew on it.
    ///
    /// Private, and takes the authorization — see [`Self::set_server_owner`].
    ///
    /// Idempotent: clearing an absent delegation reports zero narrowed rows
    /// rather than failing, because "this namespace is not delegated" is the
    /// state the caller asked for.
    ///
    /// As with [`Self::set_server_owner`], the ENFORCEMENT is the read path —
    /// `client_namespaces` stops returning those namespaces the moment the row
    /// is gone, on the very next call, with no TTL in between. If this
    /// transaction's cleanup half failed, the rows it left behind would already
    /// authorize nothing.
    async fn clear_server_owner(
        &self,
        revocation: &DelegationRevocation,
    ) -> Result<DelegationChange, ToolError> {
        let namespace = revocation.namespace();
        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;

        // Re-verified under lock, exactly as in `set_server_owner` and for the
        // same reason. There is no "but this one only narrows" exemption: a
        // revocation is an administrative action on someone else's access, and
        // an account that has just been disabled must not be able to complete
        // one on a proof it minted a moment earlier.
        let live_actor = Self::actor_authority(&mut tx, revocation.actor()).await?;
        reverify_delegation_change(revocation.actor(), &live_actor)?;

        sqlx::query("DELETE FROM rmcp_server_owner WHERE namespace = $1")
            .bind(namespace)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let rows_narrowed = Self::narrow_clients_losing(&mut tx, namespace).await?;
        tx.commit().await.map_err(db)?;
        Ok(DelegationChange { reassigned: false, rows_narrowed })
    }

    /// Every delegation, namespace-ordered. Unfiltered — the CALLER's view is
    /// narrowed by [`crate::oauth::delegation::DelegationService::list`], which
    /// owns the rule that a delegated owner sees only their own row.
    pub async fn list_server_owners(&self) -> Result<Vec<ServerOwner>, ToolError> {
        sqlx::query_as::<_, ServerOwner>(
            "SELECT namespace, owner_account_id, granted_at FROM rmcp_server_owner \
             ORDER BY namespace",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// `Some(is_operator)` for an ACTIVE account, `None` for one that is missing
    /// or disabled — collapsed deliberately, because neither may act and
    /// distinguishing them is an account-existence oracle.
    pub async fn account_authority(&self, account_id: Uuid) -> Result<Option<bool>, ToolError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT is_operator FROM rmcp_account WHERE id = $1 AND NOT disabled",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// The single active operator account, when there is exactly one.
    ///
    /// This is how the local tool surface establishes WHO is acting without
    /// taking an identity from its arguments — the same doctrine as
    /// [`crate::tool::CallerContext`], where an identity a caller can type is no
    /// identity at all.
    ///
    /// Three outcomes, and the two failures are different on purpose: `None`
    /// means no active operator exists (nothing to act as), and
    /// [`ToolError::Conflict`] means SEVERAL do, so the caller must say which
    /// via [`crate::oauth::OPERATOR_ACCOUNT_ENV`]. Guessing between operators
    /// would attribute an audited administrative action to the wrong human.
    pub async fn find_sole_operator_account(&self) -> Result<Option<Uuid>, ToolError> {
        let operators = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM rmcp_account WHERE is_operator AND NOT disabled ORDER BY id LIMIT 2",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        match operators.len() {
            0 => Ok(None),
            1 => Ok(Some(operators[0])),
            _ => Err(ToolError::Conflict(
                "this fleet has more than one operator account; name the acting one in the \
                 environment so the action is attributed to a person"
                    .into(),
            )),
        }
    }
}

/// RMCP-12: the delegation seam, satisfied by the real repository.
///
/// A thin forwarding impl, exactly like [`SessionStore`] above and for the same
/// reason: the RULES live in [`crate::oauth::delegation`] where they are
/// testable without a database, and anything implemented here instead would be
/// out of reach of those tests.
#[async_trait::async_trait]
impl DelegationStore for OauthStore {
    async fn account_authority(&self, account_id: Uuid) -> Result<Option<bool>, ToolError> {
        OauthStore::account_authority(self, account_id).await
    }

    async fn namespaces_owned_by(&self, account_id: Uuid) -> Result<Vec<String>, ToolError> {
        OauthStore::namespaces_owned_by(self, account_id).await
    }

    async fn grant_namespace(
        &self,
        grant: &DelegationGrant,
    ) -> Result<DelegationChange, ToolError> {
        self.set_server_owner(grant).await
    }

    async fn revoke_namespace(
        &self,
        revocation: &DelegationRevocation,
    ) -> Result<DelegationChange, ToolError> {
        self.clear_server_owner(revocation).await
    }

    async fn list_server_owners(&self) -> Result<Vec<ServerOwner>, ToolError> {
        OauthStore::list_server_owners(self).await
    }

    async fn account_id_by_name(&self, name: &str) -> Result<Option<Uuid>, ToolError> {
        self.resolve_account_id(name).await
    }

    async fn account_name(&self, account_id: Uuid) -> Result<Option<String>, ToolError> {
        sqlx::query_scalar::<_, String>("SELECT name FROM rmcp_account WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }
}

/// RMCP-11: the revocation/session seam, satisfied by the real repository.
///
/// A thin forwarding impl on purpose. The trait exists so
/// [`crate::oauth::revoke::RevocationService`]'s logic — idempotence, the
/// verify-after-write step, the RFC 7009 client-ownership rule — is testable
/// without a database; putting any behaviour in this impl rather than in the
/// service would put it back out of reach of those tests.
#[async_trait::async_trait]
impl SessionStore for OauthStore {
    async fn resolve_account(&self, name: &str) -> Result<Option<Uuid>, ToolError> {
        self.resolve_account_id(name).await
    }

    async fn resolve_client(&self, client_id: &str) -> Result<Option<Uuid>, ToolError> {
        self.resolve_client_id(client_id).await
    }

    async fn list_families(
        &self,
        account_id: Option<Uuid>,
        client_id: Option<Uuid>,
        family_id: Option<Uuid>,
    ) -> Result<Vec<TokenFamily>, ToolError> {
        self.list_token_families(account_id, client_id, family_id).await
    }

    async fn family_of_refresh_token(
        &self,
        token_hash: &SecretHash,
    ) -> Result<Option<TokenFamily>, ToolError> {
        OauthStore::family_of_refresh_token(self, token_hash).await
    }

    async fn revoke_family(&self, family_id: Uuid) -> Result<u64, ToolError> {
        self.revoke_refresh_family(family_id).await
    }

    async fn revoke_client_tokens(&self, client_id: Uuid) -> Result<u64, ToolError> {
        self.revoke_client_refresh_tokens(client_id).await
    }

    async fn revoke_consent_and_tokens(
        &self,
        account_id: Uuid,
        client_id: Uuid,
    ) -> Result<(u64, u64), ToolError> {
        OauthStore::revoke_consent_and_tokens(self, account_id, client_id).await
    }

    async fn revoke_account_everything(&self, account_id: Uuid) -> Result<(u64, u64), ToolError> {
        self.revoke_account_sessions(account_id).await
    }

    async fn dispatch_state(
        &self,
        account_id: Uuid,
        client_id: Uuid,
        family_id: Uuid,
    ) -> Result<DispatchState, ToolError> {
        OauthStore::dispatch_state(self, account_id, client_id, family_id).await
    }
}

/// Map a sqlx error to a [`ToolError`] without leaking connection details.
///
/// sqlx's `Display` can include the database URL's host and user for connection
/// errors, so the message is a fixed string plus the database's own error code
/// where one exists — enough to diagnose, not enough to disclose.
fn db(e: sqlx::Error) -> ToolError {
    match e.as_database_error().and_then(|d| d.code()) {
        Some(code) => ToolError::Database(format!("RMCP OAuth store query failed (SQLSTATE {code})")),
        None => ToolError::Database("RMCP OAuth store query failed".into()),
    }
}

/// Map a unique-constraint violation to a [`ToolError::Conflict`] with a
/// caller-supplied message, and everything else through [`db`].
///
/// SQLSTATE 23505 is matched by CODE, never by message text — the same
/// discipline as the TERM-608 fit_score fallback, where classifying on message
/// text broke as soon as the wording changed.
fn unique_aware(conflict_message: &'static str) -> impl Fn(sqlx::Error) -> ToolError {
    move |e: sqlx::Error| {
        if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") {
            ToolError::Conflict(conflict_message.into())
        } else {
            db(e)
        }
    }
}

#[cfg(test)]
mod tests {

    /// One function that mutates a scope-affecting table without invalidating.
    #[derive(Debug, PartialEq, Eq)]
    struct UnguardedWrite {
        file: String,
        function: String,
        detail: String,
    }

    /// The detector, as a PURE function over one file's source.
    ///
    /// Pure and separately callable for two reasons. It lets the real guard run
    /// over the WHOLE crate rather than one file (round 6's finding), and it
    /// lets the detector itself be tested against synthetic source containing a
    /// deliberate violation — without which "the guard would catch a write in
    /// another module" would be an untested claim about a scanner that has
    /// never once seen a positive case.
    fn scan_for_unguarded_scope_writes(file: &str, source: &str) -> Vec<UnguardedWrite> {
        // Production code only: a `#[cfg(test)]` module quotes these very SQL
        // shapes as fixtures.
        let production = source
            .split_once("\n#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(source);

        // Split into functions at ANY indentation — a free function at column 0,
        // an inherent method at 4, a nested one deeper. The store's methods sit
        // at 4, but a violation in another module will not.
        let mut current = "<file scope>".to_string();
        let mut bodies: Vec<(String, String)> = vec![(current.clone(), String::new())];
        for line in production.lines() {
            let trimmed = line.trim_start();
            let is_fn_decl = trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("async fn ")
                || trimmed.starts_with("pub async fn ")
                || trimmed.starts_with("pub(crate) fn ")
                || trimmed.starts_with("pub(crate) async fn ")
                || trimmed.starts_with("pub(super) fn ")
                || trimmed.starts_with("pub(super) async fn ");
            if is_fn_decl {
                current = trimmed
                    .rsplit_once("fn ")
                    .map(|(_, rest)| rest.split(['(', '<', ' ']).next().unwrap_or(rest).to_string())
                    .unwrap_or_else(|| trimmed.to_string());
                bodies.push((current.clone(), String::new()));
            }
            // COMMENTS ARE NOT CODE. The store's section comment quotes the
            // guard verbatim as an example, and without this a method that lost
            // its real guard could still be "covered" by prose sitting in its
            // span. A guard satisfied by a comment is the exact failure mode
            // this whole review thread has been about.
            if trimmed.starts_with("//") {
                continue;
            }
            let last = bodies.last_mut().expect("seeded above");
            last.1.push_str(line);
            last.1.push('\n');
        }

        let verbs = ["INSERT INTO", "UPDATE", "DELETE FROM"];
        let mut offenders = Vec::new();
        for (name, body) in &bodies {
            // Collapse whitespace so the `\` continuations inside multi-line SQL
            // literals do not hide a `<verb> <table>` pair.
            let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut mutated: Vec<&str> = Vec::new();
            for table in crate::oauth::scope::SCOPE_AFFECTING_TABLES {
                for verb in verbs {
                    if flat.contains(&format!("{verb} {table}")) {
                        mutated.push(table);
                    }
                }
            }
            if mutated.is_empty() {
                continue;
            }
            mutated.sort_unstable();
            mutated.dedup();
            // A function satisfies the rule either by OPENING a guard, or by
            // taking one as a `&ScopeWrite` WITNESS parameter — which is a
            // stronger form of the same guarantee, and a compiler-checked one:
            // a reference cannot exist without a live guard somewhere up the
            // call stack, so such a function is uncallable outside an
            // invalidating scope. RMCP-08 needed this to make an administrative
            // edit atomic (the field write and both scoping writes share one
            // transaction), which means the SQL had to move into helpers that
            // do not own the guard.
            //
            // Textual, like the rest of this scanner, but it recognises a
            // property the compiler is already enforcing rather than widening
            // what counts as compliance.
            let holds_guard = body.contains("let _scope_write = ScopeWrite::begin();");
            let takes_witness = body.contains("_scope_write: &ScopeWrite");
            if !holds_guard && !takes_witness {
                offenders.push(UnguardedWrite {
                    file: file.to_string(),
                    function: name.clone(),
                    detail: format!("mutates {mutated:?} without holding the ScopeWrite guard"),
                });
            } else if body.contains("let _ = ScopeWrite::begin()") {
                // The guard's whole value is being HELD across the write. Bound
                // to `_` it drops immediately, so both bumps land before the
                // write and the trailing invalidation is silently lost.
                offenders.push(UnguardedWrite {
                    file: file.to_string(),
                    function: name.clone(),
                    detail: "binds ScopeWrite to `_`, which drops it immediately and loses the \
                             post-write invalidation — bind it to a NAME"
                        .to_string(),
                });
            }
        }
        offenders
    }

    /// Every `.rs` file under `src/`.
    fn crate_source_files() -> Vec<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        files.sort();
        files
    }

    /// **Enforces**, across the WHOLE CRATE, that every mutation of a
    /// scope-affecting table invalidates RMCP-07's resolution cache.
    ///
    /// Round 6 of review found this scanning only `store.rs` via
    /// `include_str!`. That made the enforced invariant "scope-affecting writes
    /// invalidate, PROVIDED they live in one file" — materially weaker than
    /// what the README claims, and blind to exactly the thing it exists to
    /// notice: a future admin endpoint, migration helper or ops tool updating
    /// `rmcp_server_owner` from another module would bypass the chokepoint
    /// AND the detector in one step.
    ///
    /// The rule is uniform rather than store-specific: ANY function anywhere
    /// that mutates one of these tables must hold the guard. `ScopeWrite` is
    /// `pub(crate)`, so a legitimate future writer elsewhere can comply — it
    /// just cannot do so silently.
    ///
    /// Scope boundary, stated so the widened scan is not mistaken for more than
    /// it is: this proves NO IN-CRATE WRITE can bypass invalidation. It says
    /// nothing about an out-of-process edit (an operator changing the tables by
    /// hand), which no in-crate mechanism can observe and which the short cache
    /// TTL remains the backstop for.
    #[test]
    fn no_in_crate_write_bypasses_scope_invalidation() {
        let files = crate_source_files();
        assert!(
            files.len() > 50,
            "the crate walk found only {} file(s); it is not scanning the tree",
            files.len()
        );

        let mut offenders: Vec<UnguardedWrite> = Vec::new();
        let mut guarded_writes = 0usize;
        let mut scanned = 0usize;
        for path in &files {
            let Ok(source) = std::fs::read_to_string(path) else { continue };
            scanned += 1;
            let label = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            offenders.extend(scan_for_unguarded_scope_writes(&label, &source));
            guarded_writes += source
                .split_once("\n#[cfg(test)]")
                .map(|(before, _)| before)
                .unwrap_or(&source)
                .lines()
                .filter(|l| {
                    !l.trim_start().starts_with("//")
                        && l.contains("let _scope_write = ScopeWrite::begin();")
                })
                .count();
        }

        let report: Vec<String> = offenders
            .iter()
            .map(|o| format!("  {}: fn {} — {}", o.file, o.function, o.detail))
            .collect();
        assert!(
            offenders.is_empty(),
            "RMCP-07: {} function(s) across the crate mutate a scope-affecting table without \
             invalidating the resolution cache. A cached connector scope would keep permitting \
             the OLD answer until the TTL expired — narrowing a group's patterns or a client's \
             scope is a REVOCATION and must take effect on the next call. Open the function \
             with `let _scope_write = ScopeWrite::begin();`:\n{}",
            offenders.len(),
            report.join("\n")
        );

        // Non-vacuity, both halves: the walk read real files, and it still sees
        // the known writes. A refactor that broke the function splitting, or a
        // path change that silently scanned nothing, fails here rather than
        // passing green.
        assert!(scanned > 50, "only {scanned} file(s) were read");
        assert!(
            guarded_writes >= 7,
            "expected the known scope-affecting writes to be found; saw {guarded_writes}"
        );
    }

    /// The detector must actually FIRE — and name the file.
    ///
    /// Without this, "a write in another module would be caught" is a claim
    /// about a scanner that has only ever been run against clean source. It is
    /// also the mutation target: narrow the real guard back to `store.rs` and
    /// the violation planted in another module here stops being reported.
    #[test]
    fn the_detector_catches_an_unguarded_write_in_any_module() {
        // A plausible future violation: an admin endpoint reassigning a
        // federated server's owner, in a module that is not the store.
        let elsewhere = r#"
pub async fn reassign_owner(pool: &PgPool, ns: &str, owner: Uuid) -> Result<(), ToolError> {
    sqlx::query("UPDATE rmcp_server_owner SET owner_account_id = $2 WHERE namespace = $1")
        .bind(ns)
        .bind(owner)
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(db)
}
"#;
        let found = scan_for_unguarded_scope_writes("src/admin/servers.rs", elsewhere);
        assert_eq!(found.len(), 1, "the violation must be reported: {found:?}");
        assert_eq!(found[0].file, "src/admin/servers.rs", "the message must name the FILE");
        assert_eq!(found[0].function, "reassign_owner", "and the function");
        assert!(found[0].detail.contains("rmcp_server_owner"));

        // The same write, guarded, is clean.
        let guarded = elsewhere.replace(
            "    sqlx::query(",
            "    let _scope_write = ScopeWrite::begin();\n    sqlx::query(",
        );
        assert!(
            scan_for_unguarded_scope_writes("src/admin/servers.rs", &guarded).is_empty(),
            "a guarded write elsewhere in the crate is legitimate"
        );

        // A guard bound to `_` drops immediately and is still an offence.
        let dropped_early = elsewhere.replace(
            "    sqlx::query(",
            "    let _ = ScopeWrite::begin();\n    let _scope_write = ScopeWrite::begin();\n    sqlx::query(",
        );
        assert_eq!(
            scan_for_unguarded_scope_writes("src/admin/servers.rs", &dropped_early).len(),
            1,
            "binding the guard to `_` must still be reported"
        );

        // The WITNESS form is clean: a function taking `&ScopeWrite` cannot be
        // called without a live guard, which is a compiler check where this
        // scanner can only make a textual one. RMCP-08 needed it so an
        // administrative edit could apply its field write and both scoping
        // writes in ONE transaction.
        let witnessed = elsewhere.replace(
            "pub async fn reassign_owner(pool: &PgPool, ns: &str, owner: Uuid)",
            "pub async fn reassign_owner(pool: &PgPool, _scope_write: &ScopeWrite, ns: &str, owner: Uuid)",
        );
        assert!(
            scan_for_unguarded_scope_writes("src/admin/servers.rs", &witnessed).is_empty(),
            "a write inside a function holding a guard WITNESS is legitimate"
        );

        // …and the witness must be the real thing. A parameter that merely
        // resembles it does not count, or the exemption would be a hole
        // anybody could open by naming a variable suggestively.
        for near_miss in [
            "_scope_write: &str",
            "scope_write: &ScopeWrite2",
            "_scope_writer: &ScopeWrite",
        ] {
            let faked = elsewhere.replace(
                "pool: &PgPool, ns: &str",
                &format!("pool: &PgPool, {near_miss}, ns: &str"),
            );
            assert_eq!(
                scan_for_unguarded_scope_writes("src/admin/servers.rs", &faked).len(),
                1,
                "{near_miss} must not satisfy the guard"
            );
        }

        // A table NOT in the scope-affecting set is none of this guard's
        // business — the rule is keyed on the table, and staying narrow is what
        // keeps it from flushing the cache on every token issuance.
        let unrelated = elsewhere.replace("rmcp_server_owner", "rmcp_refresh_token");
        assert!(scan_for_unguarded_scope_writes("src/oauth/tokens.rs", &unrelated).is_empty());
    }
    use super::*;

    /// The error mapper must not become a channel for the connection string.
    /// A connection failure is where sqlx is most likely to include host and
    /// user details, so that is the case asserted.
    #[test]
    fn db_error_never_carries_connection_details() {
        let err = db(sqlx::Error::PoolTimedOut);
        let text = err.to_string();
        assert!(text.contains("RMCP OAuth store query failed"));
        assert!(!text.contains("postgres://"));
        assert!(!text.contains('@'), "no host/user fragment: {text}");
    }

    /// A non-unique error must not be misreported as a conflict — a caller that
    /// sees `Conflict` will tell the user "that name is taken", which would be
    /// a confusing lie for, say, a connection timeout.
    #[test]
    fn unique_aware_passes_non_unique_errors_through() {
        let mapper = unique_aware("taken");
        let mapped = mapper(sqlx::Error::PoolTimedOut);
        assert!(
            matches!(mapped, ToolError::Database(_)),
            "a pool timeout is not a uniqueness conflict: {mapped:?}"
        );
    }

    /// The pool size degrades to a safe default rather than failing the door.
    /// A tuning knob grants no permission, so the fail-closed rule that governs
    /// PERMISSIONS does not apply to it — an operator typo here should not take
    /// authentication offline.
    #[test]
    fn max_connections_falls_back_on_bad_input() {
        // Exercises the same parse/filter chain as `max_connections`, without
        // mutating process-global environment state that would race other tests.
        let resolve = |raw: Option<&str>| -> u32 {
            raw.and_then(|v| v.trim().parse::<u32>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_MAX_CONNECTIONS)
        };
        assert_eq!(resolve(None), DEFAULT_MAX_CONNECTIONS);
        assert_eq!(resolve(Some("not-a-number")), DEFAULT_MAX_CONNECTIONS);
        assert_eq!(resolve(Some("0")), DEFAULT_MAX_CONNECTIONS, "zero would deadlock the pool");
        assert_eq!(resolve(Some(" 12 ")), 12);
    }

    /// `schema_ready` must require the WHOLE migration. Asserting the table
    /// list here is what makes a future table addition fail loudly in review
    /// rather than silently weaken the readiness check.
    #[test]
    fn schema_readiness_covers_every_migrated_table() {
        assert_eq!(REQUIRED_TABLES.len(), 11);
        for table in [
            "rmcp_account",
            "rmcp_client",
            "rmcp_tool_group",
            "rmcp_client_scope",
            "rmcp_client_server",
            "rmcp_auth_code",
            "rmcp_refresh_token",
            "rmcp_consent",
            "rmcp_server_owner",
            // RMCP-03's durable single-use login-session marker. Listed here so
            // a deploy that applies only the core migration reports NOT ready
            // rather than running the login path with no guard behind it.
            "rmcp_login_session_use",
            // RMCP-08's initial access tokens. Same reasoning: a deploy that
            // applied the earlier migrations and not this one must report NOT
            // ready, rather than failing the first registration attempt with an
            // opaque `relation does not exist`.
            "rmcp_registration_token",
        ] {
            assert!(REQUIRED_TABLES.contains(&table), "{table} missing from the readiness check");
        }

        // The COLUMN half. A migration that adds a column to an existing table
        // is invisible to the table check above, which is the whole reason
        // `REQUIRED_COLUMNS` exists — and a second entry is exactly where the
        // first one's lesson gets forgotten.
        assert_eq!(REQUIRED_COLUMNS.len(), 2);
        for column in [("rmcp_account", "is_operator"), ("rmcp_client", "version")] {
            assert!(
                REQUIRED_COLUMNS.contains(&column),
                "{column:?} missing from the readiness check"
            );
        }

        // Every table and column named above must actually be created by a
        // migration in the tree. Without this the readiness check could name a
        // table nothing ships, which fails a deployment that is in fact correct
        // — the opposite error, and one a green test suite would never show.
        let migrations: String = std::fs::read_dir(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"),
        )
        .expect("migrations directory")
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "sql"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .collect();
        for table in REQUIRED_TABLES {
            assert!(
                migrations.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
                "{table} is required at startup but no migration creates it"
            );
        }
        for (table, column) in REQUIRED_COLUMNS {
            assert!(
                migrations.contains(column),
                "{table}.{column} is required at startup but no migration adds it"
            );
        }
    }

    /// The three administrative queries must never select the secret hash, and
    /// must all project the same `confidential` boolean.
    ///
    /// This is what the shared `format!()`ed column constant was actually
    /// protecting, restated as a test now that the SQL is written out three
    /// times (review round 3 — see `CLIENT_ADMIN_BY_ID`'s doc for why the
    /// interpolation had to go even though it was not exploitable).
    ///
    /// The mutation target: change any one of the three to select
    /// `client_secret_hash` directly and this goes red.
    #[test]
    fn the_admin_queries_never_select_the_secret_hash() {
        let queries = [
            ("CLIENT_ADMIN_BY_ID", OauthStore::CLIENT_ADMIN_BY_ID),
            ("CLIENT_ADMIN_BY_OWNER", OauthStore::CLIENT_ADMIN_BY_OWNER),
            ("CLIENT_ADMIN_UPDATE", OauthStore::CLIENT_ADMIN_UPDATE),
        ];
        for (name, sql) in queries {
            assert!(
                sql.contains("(client_secret_hash IS NOT NULL) AS confidential"),
                "{name} must project the derived boolean"
            );
            // The hash itself must appear ONLY inside that projection. Any other
            // occurrence would be selecting a credential digest into a type that
            // has no field for it — or, worse, into one that later gains a field.
            assert_eq!(
                sql.matches("client_secret_hash").count(),
                1,
                "{name} references the secret hash outside the derived projection"
            );
        }

        // And the three agree on the column list, which is the drift the shared
        // constant used to prevent structurally.
        let columns = |sql: &str| {
            sql.split_whitespace()
                .filter(|t| t.ends_with(',') || *t == "version")
                .map(|t| t.trim_end_matches(',').to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            columns(OauthStore::CLIENT_ADMIN_BY_ID),
            columns(OauthStore::CLIENT_ADMIN_BY_OWNER),
            "the two read queries have drifted apart"
        );
    }

    /// **No SQL string in this module is built by interpolation.**
    ///
    /// The rule round 3 restored. Its value is that it can be checked by a
    /// machine: a scanner cannot distinguish a benign interpolation of a
    /// private constant from one that reached a caller's value, so a single
    /// exception turns the rule into a matter of human attention. There is now
    /// no exception, and this is what keeps it that way.
    #[test]
    fn no_sql_in_this_module_is_built_by_interpolation() {
        let file = include_str!("store.rs");
        let production = file.split("\n#[cfg(test)]").next().expect("production half");
        let offenders: Vec<(usize, &str)> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//"))
            // Deliberately narrow: a `sqlx::query*` call whose argument is a
            // `format!`. `format!` on its own is fine and common here — it
            // builds error MESSAGES, which are not sent to the database — so
            // flagging it would make this guard noisy and it would be loosened.
            // Both halves must appear on the query's own line, which is where
            // the removed interpolation lived.
            .filter(|(_, line)| line.contains("sqlx::query") && line.contains("format!"))
            .map(|(i, line)| (i + 1, line.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "SQL (or something adjacent to it) is being built by interpolation, which makes the \
             no-interpolation rule uncheckable by anything but a careful reader: {offenders:?}"
        );
    }

    /// **Every administrative write is AUTHORIZED, and authorized first.**
    ///
    /// Round 2 (`gpt56`) found `apply_client_admin_edit` writing the client's
    /// fields constrained only by `id` and `version` — no owner, no actor —
    /// while an ownership check ran on the SCOPE path only. An edit touching
    /// just `enabled` or `redirect_uris` routed around it entirely, and
    /// `redirect_uris` decides where an authorization code is delivered.
    /// `revoke_client`, the token mint and the token revoke-all had no check at
    /// all.
    ///
    /// A source-text guard, like `no_in_crate_write_bypasses_scope_invalidation`
    /// and for the same reason: the property is a SQL predicate plus an
    /// ordering, so it is not reachable from a unit test without a database —
    /// but it is exactly the kind of check that gets dropped in a refactor
    /// without anything going red.
    ///
    /// It asserts two things per entry point: that the authorization is
    /// present, and that it comes BEFORE the mutation. Presence alone would
    /// pass for a check bolted on after the write, which authorizes nothing.
    ///
    /// The mutation target: delete any one authorization call, or move it below
    /// its statement, and this goes red naming the function.
    #[test]
    fn every_administrative_write_is_authorized_before_it_mutates() {
        let file = include_str!("store.rs");
        let production = file.split("\n#[cfg(test)]").next().expect("production half");

        // (function, the authorization it must perform, the mutation it guards)
        //
        // `claim_registration_token` is in this list even though it is a READ
        // path, and that is the round-3 lesson: a bearer credential is a read
        // path, and the authority behind it must be re-derived when it is
        // spent. It was the seventh instance of this sprint's defect class.
        let entry_points = [
            (
                "claim_registration_token",
                "actor_authority(&mut tx, issued_by)",
                "SET uses_remaining = uses_remaining - 1",
            ),
            // The mutation marker is the CONSTANT this executes, not the SQL
            // text: round 3 moved that text into `CLIENT_ADMIN_UPDATE` to get
            // `format!()` out of the query, and this guard correctly went red
            // until the marker followed it. What the constant itself contains
            // is pinned by `the_admin_queries_never_select_the_secret_hash`.
            ("apply_client_admin_edit", "authorize_client_write(&actor", "Self::CLIENT_ADMIN_UPDATE"),
            ("revoke_client", "authorize_client_write(&actor", "UPDATE rmcp_client SET disabled"),
            (
                "insert_registration_token",
                "authorize_operator_action(&actor",
                "INSERT INTO rmcp_registration_token",
            ),
            (
                "revoke_all_registration_tokens",
                "authorize_operator_action(&actor",
                "UPDATE rmcp_registration_token SET",
            ),
        ];

        for (function, authorization, mutation) in entry_points {
            let marker = format!("pub async fn {function}(");
            let start = production
                .find(&marker)
                .unwrap_or_else(|| panic!("{function} has been renamed or removed"));
            let rest = &production[start..];
            let end = rest[1..]
                .find("\n    pub async fn ")
                .or_else(|| rest[1..].find("\n    async fn "))
                .map(|i| i + 1)
                .unwrap_or(rest.len());
            let body: String = rest[..end]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");

            let authorized_at = body.find(authorization).unwrap_or_else(|| {
                panic!(
                    "{function} mutates a client or a registration token WITHOUT calling \
                     {authorization}. Anyone able to reach it could then act on an object they \
                     do not own — for `redirect_uris`, that is control over where an \
                     authorization code is delivered."
                )
            });
            let mutates_at = body.find(mutation).unwrap_or_else(|| {
                panic!("{function} no longer contains the mutation {mutation:?} this guard pins")
            });
            assert!(
                authorized_at < mutates_at,
                "{function} authorizes AFTER it mutates, which authorizes nothing"
            );
            // And the authority it authorizes against must be derived in THIS
            // transaction, not passed in. A caller-supplied authority is the
            // stale-snapshot shape RMCP-12 already had to close once.
            assert!(
                body.contains("Self::actor_authority(&mut tx"),
                "{function} must derive the actor's authority inside its own transaction"
            );
            // …and it must be a TRANSACTION, so the authority is locked for the
            // rest of it. A helper reading outside one is a point-in-time proof
            // again, which is the shape this whole sprint kept reopening.
            assert!(
                body.contains("self.pool.begin()"),
                "{function} must open its own transaction so the authority read is locked"
            );
        }
    }

    /// Every query that feeds an authorization decision from an OWNER account
    /// must exclude a disabled owner, in the join.
    ///
    /// This is a source-text guard, which is unusual and deliberate. The
    /// property is a SQL predicate, so it is not reachable from a unit test
    /// without a database — but it regressed silently once already: TERM #637B
    /// added `AND NOT a.disabled` to two queries while a third, added on a
    /// branch, kept the check only in a projected column. The two hunks were in
    /// different functions, so the rebase that combined them produced no
    /// conflict and no failing test, and the query that actually feeds
    /// resolution ended up weaker than the one beside it.
    ///
    /// Asserting the text is a blunt instrument that would have caught exactly
    /// that. It cannot prove the predicate is correct; it can prove nobody
    /// deleted it.
    #[test]
    fn every_owner_scoped_query_excludes_a_disabled_owner() {
        // Scan the PRODUCTION half only, and drop comments — the same two rules
        // the `ScopeWrite` detector applies, both load-bearing here.
        //
        // Comments, because the doc above deliberately quotes the WRONG spelling
        // as the anti-pattern, and a guard its own explanation can break is a
        // guard people delete.
        //
        // The test module, for a sharper reason: the markers below are string
        // literals in THIS file. Scanning the whole file would find them in the
        // test's own array and pass even if every query had been deleted —
        // false coverage of exactly the kind this guard exists to prevent.
        let file = include_str!("store.rs");
        let production = file.split("#[cfg(test)]").next().expect("file has a production half");
        let src: String = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // Proof the scan is looking at something: the queries themselves.
        assert!(src.contains("FROM rmcp_tool_group g"), "scan lost the production text");

        for (function, marker) in [
            ("client_tool_groups / client_authorized_groups",
             "JOIN rmcp_account a ON a.id = g.owner_account_id AND NOT a.disabled"),
            // RMCP-12 moved this join from `o.owner_account_id` to
            // `c.owner_account_id`, and the guard was updated with it rather
            // than around it. The two are the SAME account in the delegated
            // branch, because that branch requires
            // `o.owner_account_id = c.owner_account_id` — which is asserted
            // separately below, since without it this join would no longer
            // cover the delegated owner at all and this marker alone would pass
            // a genuinely weakened query.
            ("client_namespaces",
             "JOIN rmcp_account a ON a.id = c.owner_account_id AND NOT a.disabled"),
        ] {
            assert!(
                src.contains(marker),
                "{function} must exclude a disabled owner in its join: missing {marker:?}"
            );
        }

        // The other half of that one rule. Delete it and a namespace delegated
        // to ANY account would resolve for ANY other account's client.
        assert!(
            src.contains("AND (a.is_operator OR o.owner_account_id = c.owner_account_id)"),
            "client_namespaces must bind the delegation to the CLIENT's own owner, with the \
             operator override as the only alternative branch"
        );
        // BOTH group queries, not just one — the regression was exactly that the
        // display query had it and the resolution query did not.
        assert_eq!(
            src.matches("JOIN rmcp_account a ON a.id = g.owner_account_id AND NOT a.disabled").count(),
            2,
            "both client_tool_groups and client_authorized_groups must carry the join"
        );

        // Assembled from parts so this needle does not appear verbatim in the
        // file it is scanning.
        let split_projection =
            format!("({} AND NOT a.disabled) AS owner_is_operator", "a.is_operator");
        assert!(
            !src.contains(&split_projection),
            "owner state belongs in the join, not split across the projection"
        );
        assert!(src.contains("a.is_operator AS owner_is_operator"));
    }

    /// **Enforces, across the WHOLE CRATE, that the raw delegation mutators are
    /// reachable only through the authorized path.**
    ///
    /// Round 1 of review found that `set_server_owner`/`clear_server_owner` were
    /// `pub`, unauthenticated, and called directly by `ScopeResolver` with no
    /// actor — so `DelegationService` was the polite way to mutate a delegation,
    /// not the only way. Two things now stop that, and this test covers the half
    /// the compiler does not:
    ///
    /// 1. **The compiler.** Both methods are private to this module, so no other
    ///    file CAN call them, and both demand a `DelegationGrant` /
    ///    `DelegationRevocation` whose only constructor runs the operator check.
    /// 2. **This scan.** Privacy stops other MODULES; it does not stop a future
    ///    method added inside `store.rs` from calling them with a proof minted
    ///    for something else, and it does not stop the `DelegationStore` trait
    ///    forwarders being pointed somewhere new. So the call sites are pinned
    ///    by name.
    ///
    /// Mutation-verify by adding a call to `self.set_server_owner(` in any
    /// function other than `grant_namespace`: this goes red naming that
    /// function. Delete the `let expected` filter and the non-vacuity assertion
    /// below goes red instead — the guard cannot be silently emptied.
    #[test]
    fn the_raw_delegation_mutators_have_exactly_the_callers_we_intend() {
        // The authorized forwarders, and nothing else. `grant_namespace` and
        // `revoke_namespace` are the `DelegationStore` impl methods, which can
        // only be reached with a proof value.
        let expected: &[(&str, &str)] =
            &[("set_server_owner", "grant_namespace"), ("clear_server_owner", "revoke_namespace")];

        let files = crate_source_files();
        assert!(files.len() > 50, "the crate walk is not scanning the tree");

        let mut found: Vec<(String, String, String)> = Vec::new();
        for path in &files {
            let Ok(source) = std::fs::read_to_string(path) else { continue };
            let label = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            // Production half only: the test module below names these methods in
            // string literals, and scanning it would find the guard's own
            // vocabulary and call it a caller.
            let production = source.split("\n#[cfg(test)]").next().unwrap_or(&source);
            let mut current_fn = String::from("(top level)");
            for line in production.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                if let Some(rest) = trimmed
                    .strip_prefix("pub async fn ")
                    .or_else(|| trimmed.strip_prefix("async fn "))
                    .or_else(|| trimmed.strip_prefix("pub fn "))
                    .or_else(|| trimmed.strip_prefix("fn "))
                {
                    current_fn = rest
                        .split(|c: char| c == '(' || c == '<')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    continue;
                }
                for (method, _) in expected {
                    if trimmed.contains(&format!(".{method}(")) {
                        found.push((label.clone(), current_fn.clone(), (*method).to_string()));
                    }
                }
            }
        }

        let unexpected: Vec<&(String, String, String)> = found
            .iter()
            .filter(|(_, caller, method)| {
                !expected.iter().any(|(m, allowed)| m == method && allowed == caller)
            })
            .collect();
        assert!(
            unexpected.is_empty(),
            "RMCP-12: the raw delegation mutators are reachable from somewhere that has not \
             proved an operator authorized the change. Route it through \
             `delegation::DelegationService`, which is what mints the proof value these \
             methods demand:\n{}",
            unexpected
                .iter()
                .map(|(file, caller, method)| format!("  {file}: fn {caller} calls {method}"))
                .collect::<Vec<_>>()
                .join("\n")
        );

        // Non-vacuity: the scan must actually have SEEN both authorized call
        // sites. A rename, a refactor, or a broken function-splitter fails here
        // rather than passing green having matched nothing.
        for (method, caller) in expected {
            assert!(
                found.iter().any(|(_, f, m)| f == caller && m == method),
                "the scan did not find the known {method} call site in fn {caller}; it is \
                 matching nothing and would pass whatever it was given"
            );
        }
    }

    /// **Both delegation mutators must RE-VERIFY the actor inside their own
    /// transaction** (round 2).
    ///
    /// The proof value they take establishes that the operator check ran; it
    /// cannot establish that it still holds at commit, because an account can be
    /// demoted or disabled in between. The re-read is the thing that closes
    /// that, and it is precisely the kind of code a later reader deletes as
    /// redundant — it looks like a second copy of a check that already happened.
    ///
    /// A text guard for the same reason the disabled-owner guard is one: the
    /// property is "this SQL ran under a lock in this transaction", which no
    /// unit test can observe without a database. `delegation`'s own tests prove
    /// the RULE refuses a stale proof; this proves the mutators actually ask it.
    ///
    /// Mutation-verify: delete either `reverify_delegation_change` call and this
    /// goes red naming that function.
    #[test]
    fn both_delegation_mutators_re_verify_the_actor_under_lock() {
        let file = include_str!("store.rs");
        let production = file.split("#[cfg(test)]").next().expect("file has a production half");

        for function in ["set_server_owner", "clear_server_owner"] {
            let start = production
                .find(&format!("async fn {function}("))
                .unwrap_or_else(|| panic!("fn {function} not found; has it been renamed?"));
            // The body runs to the next `\n    async fn ` / `\n    pub ` at the
            // same indentation, which is enough to bound one method.
            let rest = &production[start..];
            let end = rest[1..]
                .find("\n    /// ")
                .map(|i| i + 1)
                .unwrap_or(rest.len());
            let body: String = rest[..end]
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");

            assert!(
                body.contains("Self::actor_authority(&mut tx,"),
                "fn {function} must re-read the actor's authority INSIDE its transaction — a \
                 proof minted before a demotion must not authorize a write after it"
            );
            assert!(
                body.contains("reverify_delegation_change("),
                "fn {function} must re-verify the proof against that live authority; the proof \
                 alone only establishes that the check RAN, never that it still holds"
            );
        }

        // The grantee's active status is re-read under lock too, and only the
        // grant path has a grantee.
        let grant_start = production.find("async fn set_server_owner(").unwrap();
        let grant_body = &production[grant_start..(grant_start + 4000).min(production.len())];
        assert!(
            grant_body.contains("Self::locked_active_account(&mut tx, owner_account_id)"),
            "set_server_owner must re-check the GRANTEE under lock; its active status was \
             established before the transaction opened and can change under it"
        );
    }

    /// The operator flag is an AUTHORIZATION input, so a deploy that is missing
    /// it must report NOT ready rather than run with every account silently
    /// unable to be an operator. Asserted here because the table-level check
    /// cannot see a column, which is how this migration could otherwise ship
    /// unapplied and unnoticed.
    #[test]
    fn schema_readiness_covers_the_operator_flag_column() {
        assert!(
            REQUIRED_COLUMNS.contains(&("rmcp_account", "is_operator")),
            "the RMCP-06 operator flag must be part of the readiness check"
        );
    }

    // NOTE on what is deliberately NOT unit-tested here.
    //
    // An earlier revision carried a test that built an empty `Vec` and asserted
    // it was empty, as a "proof" of the fail-closed scope contract. Review round
    // 1 correctly called that vacuous: it exercised no query and no repository
    // behaviour, and a test that cannot fail is worse than no test because it
    // reads as coverage. It has been removed rather than reworded.
    //
    // The real invariants at this layer — that an unknown, disabled, or
    // unscoped client yields no groups and no namespaces — live in SQL
    // predicates and can only be verified against a database. They are covered
    // by RMCP-07's property test (`effective ⊆ account grant`, over generated
    // inputs) and by RMCP-14's end-to-end test, both of which run against a real
    // schema. Adding a DB-backed integration harness is not this item's scope.
    //
    // The same applies to `actor_authority`: that a delegated account cannot
    // write a bare `*` is now enforced by a SQL read of `rmcp_account.is_operator`
    // inside the write's transaction, which is only meaningfully testable against
    // a database. What IS unit-testable — that the flag, and nothing else,
    // decides the authority, and that the pure validator refuses `*` for a
    // delegated author — is covered in `model` and `groups` respectively. A test
    // here that constructed a `GroupOwner` by hand and asserted the validator
    // agreed would be testing the thing that was never in doubt.
}
