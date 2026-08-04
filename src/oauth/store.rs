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
use crate::oauth::model::{
    Account, AuthCode, Client, Consent, RefreshToken, ServerOwner, ToolGroup,
};
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

/// Every table the S132 migration creates. [`OauthStore::schema_ready`] requires
/// all of them, so a partially applied migration reports NOT ready.
const REQUIRED_TABLES: [&str; 9] = [
    "rmcp_account",
    "rmcp_client",
    "rmcp_tool_group",
    "rmcp_client_scope",
    "rmcp_client_server",
    "rmcp_auth_code",
    "rmcp_refresh_token",
    "rmcp_consent",
    "rmcp_server_owner",
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
    // -----------------------------------------------------------------------

    /// Insert a tool group. An empty `patterns` slice is permitted and stores a
    /// group that matches nothing — a legitimate state (a group being built
    /// up), and one the matcher must handle rather than one to reject here.
    pub async fn insert_tool_group(
        &self,
        name: &str,
        description: &str,
        patterns: &[String],
        owner_account_id: Uuid,
    ) -> Result<Uuid, ToolError> {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO rmcp_tool_group (name, description, patterns, owner_account_id) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(name)
        .bind(description)
        .bind(patterns)
        .bind(owner_account_id)
        .fetch_one(&self.pool)
        .await
        .map_err(unique_aware("a tool group with that name already exists"))
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
    /// An unknown client, a disabled client, or a known client with no scope
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
             WHERE s.client_id = $1 ORDER BY g.name",
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// The federated namespaces a client may see. Empty for an unknown,
    /// disabled, or unscoped client — same join and same reasoning as
    /// [`Self::client_tool_groups`].
    pub async fn client_namespaces(&self, client_id: Uuid) -> Result<Vec<String>, ToolError> {
        sqlx::query_scalar::<_, String>(
            "SELECT s.namespace FROM rmcp_client_server s \
             JOIN rmcp_client c ON c.id = s.client_id AND NOT c.disabled \
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
    pub async fn revoke_consent(
        &self,
        account_id: Uuid,
        client_id: Uuid,
    ) -> Result<u64, ToolError> {
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

        sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = now() \
             WHERE account_id = $1 AND client_id = $2 AND revoked_at IS NULL",
        )
        .bind(account_id)
        .bind(client_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        tx.commit().await.map_err(db)?;
        Ok(consents)
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
        sqlx::query("DELETE FROM rmcp_server_owner WHERE namespace = $1")
            .bind(namespace)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(db)
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
        assert_eq!(REQUIRED_TABLES.len(), 9);
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
