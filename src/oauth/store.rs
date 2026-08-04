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
use crate::oauth::groups::{
    normalize_description, validate_group, validate_patterns, GroupOwner, Pattern, STARTER_GROUPS,
};
use crate::oauth::model::{
    Account, AuthCode, Client, Consent, RefreshToken, ServerOwner, TokenFamily, ToolGroup,
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
const REQUIRED_TABLES: [&str; 10] = [
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
];

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
        found == REQUIRED_TABLES.len() as i64
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
            "SELECT id, name, password_hash, totp_secret_enc, disabled, created_at, updated_at \
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
    pub async fn insert_client(
        &self,
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
        sqlx::query_scalar::<_, Uuid>(
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
        .fetch_one(&self.pool)
        .await
        .map_err(unique_aware("a client with that client_id already exists"))
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

    /// Insert a tool group, VALIDATING it first (RMCP-06).
    ///
    /// This is the write-time gate the matcher depends on. Every pattern is
    /// parsed here — under `owner_kind`, which is what refuses a bare `*` from a
    /// delegated author — and the name is normalised, so no row can hold
    /// something [`crate::oauth::groups::Pattern::matches`] would have to cope
    /// with at dispatch time. Storing the CANONICAL rendering rather than the
    /// author's literal text means the round-trip is stable and two spellings of
    /// one pattern cannot both sit in the same row.
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
        owner_kind: GroupOwner,
    ) -> Result<Uuid, ToolError> {
        let _scope_write = ScopeWrite::begin();
        let group = validate_group(name, description, patterns, owner_kind)?;
        let rendered = group.rendered_patterns();
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO rmcp_tool_group (name, description, patterns, owner_account_id) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(&group.name)
        .bind(&group.description)
        .bind(rendered.as_slice())
        .bind(owner_account_id)
        .fetch_one(&self.pool)
        .await
        .map_err(unique_aware("a tool group with that name already exists"))
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

    /// Rewrite a group's description and patterns, after validating them and
    /// confirming `actor` owns the group.
    ///
    /// The ownership predicate is part of the UPDATE's `WHERE` rather than a
    /// preceding `SELECT`: one statement cannot race itself, so there is no
    /// window in which ownership could change between check and write — the
    /// same property [`Self::set_client_tool_groups`] buys with a row lock,
    /// obtained here for free because this is a single statement.
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
        owner_kind: GroupOwner,
    ) -> Result<(), ToolError> {
        // The name is not editable here (renaming would have to contend with the
        // fleet-wide UNIQUE constraint, which is RMCP-08's surface to own), so
        // only the two editable fields are validated.
        let description = normalize_description(description)?;
        let patterns: Vec<String> =
            validate_patterns(patterns, owner_kind)?.iter().map(Pattern::render).collect();
        let updated = sqlx::query(
            "UPDATE rmcp_tool_group SET description = $3, patterns = $4 \
             WHERE id = $1 AND owner_account_id = $2",
        )
        .bind(group_id)
        .bind(actor_account_id)
        .bind(&description)
        .bind(patterns.as_slice())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        if updated.rows_affected() == 0 {
            return Err(ToolError::NotFound("no such tool group for this account".into()));
        }
        Ok(())
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
    /// Returns the number of groups actually created.
    pub async fn seed_starter_groups(&self, owner_account_id: Uuid) -> Result<u64, ToolError> {
        let mut created = 0u64;
        for starter in STARTER_GROUPS {
            let patterns: Vec<String> = starter.patterns.iter().map(|p| (*p).to_string()).collect();
            // Validated on the way in like any other write, so a bad edit to the
            // seed list fails here rather than being the one path that bypasses
            // the matcher's contract.
            let group =
                validate_group(starter.name, starter.description, &patterns, GroupOwner::Operator)?;
            let rendered = group.rendered_patterns();
            let inserted = sqlx::query(
                "INSERT INTO rmcp_tool_group (name, description, patterns, owner_account_id) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (name) DO NOTHING",
            )
            .bind(&group.name)
            .bind(&group.description)
            .bind(rendered.as_slice())
            .bind(owner_account_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
            created += inserted.rows_affected();
        }
        Ok(created)
    }

    /// The groups a client draws on.
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
    /// Same note to RMCP-12 as on [`Self::client_tool_groups`]: if delegation
    /// introduces a legitimate operator override, THIS is the predicate to
    /// widen deliberately and with tests.
    pub async fn client_namespaces(&self, client_id: Uuid) -> Result<Vec<String>, ToolError> {
        sqlx::query_scalar::<_, String>(
            "SELECT s.namespace FROM rmcp_client_server s \
             JOIN rmcp_client c ON c.id = s.client_id AND NOT c.disabled \
             JOIN rmcp_server_owner o ON o.namespace = s.namespace \
                                     AND o.owner_account_id = c.owner_account_id \
             JOIN rmcp_account a ON a.id = o.owner_account_id AND NOT a.disabled \
             WHERE s.client_id = $1 ORDER BY s.namespace",
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
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
        let _scope_write = ScopeWrite::begin();
        let mut tx = self.pool.begin().await.map_err(db)?;

        // `FOR SHARE` locks the client row for the rest of the transaction, so
        // its ownership cannot be reassigned between this check and the write.
        // Without it the check is a TOCTOU: a concurrent transfer could land in
        // the gap and the write would proceed on stale authority.
        let owns_client = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM rmcp_client WHERE id = $1 AND owner_account_id = $2 FOR SHARE",
        )
        .bind(client_id)
        .bind(actor_account_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .is_some();
        if !owns_client {
            // Same answer for "no such client" and "not yours": distinguishing
            // them would confirm the existence of another account's client.
            return Err(ToolError::NotFound("no such client for this account".into()));
        }

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
        .bind(actor_account_id)
        .fetch_all(&mut *tx)
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
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        for group_id in group_ids {
            sqlx::query(
                "INSERT INTO rmcp_client_scope (client_id, tool_group_id) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(client_id)
            .bind(group_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        // Invalidates on the ERROR path too: a commit that failed to report is
        // not a commit that provably did not happen, and an unnecessary
        // invalidation costs one store read while a missed one leaves a revoked
        // permission live.
        tx.commit().await.map_err(db)
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

        // `FOR SHARE`, exactly as in `set_client_tool_groups`. Round 8 caught
        // that this copy had been left as an unlocked `SELECT EXISTS` while its
        // own doc comment claimed the same locking guarantee — a documented
        // promise the code did not keep, which is worse than an undocumented
        // gap because it stops the next reader looking.
        let owns_client = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM rmcp_client WHERE id = $1 AND owner_account_id = $2 FOR SHARE",
        )
        .bind(client_id)
        .bind(actor_account_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .is_some();
        if !owns_client {
            return Err(ToolError::NotFound("no such client for this account".into()));
        }

        // Locked with `FOR SHARE` for the same reason as the client and group
        // checks: `set_server_owner` could otherwise reassign a namespace
        // between the check and the insert, letting the PREVIOUS owner attach a
        // server they no longer own. Holding the lock to commit closes that.
        let owned_namespaces = sqlx::query_scalar::<_, String>(
            "SELECT namespace FROM rmcp_server_owner \
             WHERE namespace = ANY($1) AND owner_account_id = $2 FOR SHARE",
        )
        .bind(namespaces)
        .bind(actor_account_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?
        .len() as i64;
        let requested = namespaces.iter().collect::<std::collections::HashSet<_>>().len() as i64;
        if owned_namespaces != requested {
            return Err(ToolError::InvalidArgument(
                "one or more servers are not owned by this account".into(),
            ));
        }

        sqlx::query("DELETE FROM rmcp_client_server WHERE client_id = $1")
            .bind(client_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        for namespace in namespaces {
            sqlx::query(
                "INSERT INTO rmcp_client_server (client_id, namespace) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(client_id)
            .bind(namespace)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        // Invalidates on the ERROR path too: a commit that failed to report is
        // not a commit that provably did not happen, and an unnecessary
        // invalidation costs one store read while a missed one leaves a revoked
        // permission live.
        tx.commit().await.map_err(db)
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

    /// Assign (or reassign) ownership of a namespace. One owner per namespace,
    /// enforced by the primary key.
    pub async fn set_server_owner(
        &self,
        namespace: &str,
        owner_account_id: Uuid,
    ) -> Result<(), ToolError> {
        let _scope_write = ScopeWrite::begin();
        sqlx::query(
            "INSERT INTO rmcp_server_owner (namespace, owner_account_id) VALUES ($1, $2) \
             ON CONFLICT (namespace) DO UPDATE SET owner_account_id = EXCLUDED.owner_account_id, \
                                                   granted_at = now()",
        )
        .bind(namespace)
        .bind(owner_account_id)
        .execute(&self.pool)
        .await
        .map(|_| ())
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

    /// Remove a delegation.
    pub async fn clear_server_owner(&self, namespace: &str) -> Result<(), ToolError> {
        let _scope_write = ScopeWrite::begin();
        sqlx::query("DELETE FROM rmcp_server_owner WHERE namespace = $1")
            .bind(namespace)
            .execute(&self.pool)
            .await
            .map(|_| ())
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
            if !body.contains("let _scope_write = ScopeWrite::begin();") {
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
        assert_eq!(REQUIRED_TABLES.len(), 10);
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
        ] {
            assert!(REQUIRED_TABLES.contains(&table), "{table} missing from the readiness check");
        }
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
}
