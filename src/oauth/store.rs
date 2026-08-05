//! SQLite repository for the RMCP OAuth door.
//!
//! ## Three rules every method in this file obeys
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
//! written as `expires_at > unixepoch()` inside SQL rather than compared against
//! `Utc::unixepoch()` in Rust. A process whose clock has drifted must not be able to
//! honour an expired code or reject a live one. One clock, at the store.
//!
//! **Every authority is re-derived inside the transaction that acts on it.**
//! A write-time authorization check is point-in-time; anything REVOCABLE —
//! operator status, account enablement, namespace ownership, client ownership —
//! is read again in the same transaction as the write it authorizes, and again
//! on the read path when it decides what a token reaches. This is the single
//! rule the whole item's review history is made of, and the port below changes
//! the MECHANISM that makes it sound without changing the rule.
//!
//! All queries use sqlx parameter binding; there is no SQL string
//! interpolation anywhere in this module.
//!
//! ---------------------------------------------------------------------------
//! ## S132/RMCP-SQLITE — how the row locks survived the port to SQLite
//!
//! The Postgres version relied on `SELECT … FOR SHARE` and `SELECT … FOR
//! UPDATE` to hold a row for the rest of a transaction, so that an operator
//! demoted, disabled, or divested of a namespace between an authorization check
//! and the commit it authorized could not act on a stale result. SQLite has no
//! row-level locks at all. It has something coarser, and — for exactly this
//! property — stronger.
//!
//! **Every transaction in this module is opened `BEGIN IMMEDIATE`** (see
//! [`OauthStore::begin_immediate`]). In WAL mode that takes the database's
//! single WRITE lock at BEGIN and holds it until COMMIT or ROLLBACK, so:
//!
//! 1. **At most one write transaction exists at a time.** A second
//!    `BEGIN IMMEDIATE` blocks for `busy_timeout` and then fails; it does not
//!    proceed. Writers are therefore totally ordered, not merely conflict-
//!    ordered as they were per-row under Postgres.
//! 2. **A read taken inside a write transaction cannot be invalidated before
//!    that transaction commits**, because no other writer can commit in the
//!    interval. This is precisely the guarantee `FOR SHARE` was purchased for,
//!    obtained over the whole database rather than over one row.
//! 3. **Failure is fail-CLOSED.** Contention surfaces as `SQLITE_BUSY`, which
//!    aborts the write. The failure direction is "the write did not happen",
//!    never "the write happened on stale authority".
//!
//! The locks are therefore not removed; they are SUBSUMED. Each of the sites
//! that carried one is documented individually below with what it protected and
//! why the transaction now protects it, and the interleavings that are subtle
//! are proven by concurrency tests against a real database file in this
//! module's test module (`a_concurrent_demotion_cannot_interleave_*`), not
//! asserted.
//!
//! **The one thing this does NOT give, and it must not be claimed.** Postgres
//! made these guarantees across every replica sharing one database. A SQLite
//! file makes them across every connection to ONE FILE. They are equivalent
//! only while there is exactly one writer of that file — see
//! [`crate::oauth::SQLITE_PATH_ENV`] and the `rmcp_login_session_use` note in
//! the migration. This door is single-writer by construction.

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqlitePool, Transaction};
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
///
/// On SQLite the pool serves READERS. WAL mode lets any number of readers run
/// concurrently with the single writer, so a pool bigger than one is still
/// worth having; writers serialize on the write lock regardless of how many
/// connections exist, which is the point (see the module docs).
const DEFAULT_MAX_CONNECTIONS: u32 = 5;

/// Env var overriding [`DEFAULT_MAX_CONNECTIONS`].
const MAX_CONNECTIONS_ENV: &str = "RMCP_DB_MAX_CONNECTIONS";

/// How long a writer waits for the database write lock before giving up.
///
/// This is a correctness-adjacent knob, not tuning. Every authorization write
/// in this module happens inside one `BEGIN IMMEDIATE` transaction, so two
/// concurrent administrative actions CONTEND by design — that contention is the
/// mechanism that replaced the row locks. `busy_timeout` decides whether the
/// loser waits its turn or fails.
///
/// Five seconds is chosen to be comfortably longer than any transaction here
/// can take (each is a handful of indexed statements against a small database,
/// with no network and no user interaction inside it) and comfortably shorter
/// than the 10s discovery/token budget, so a genuinely wedged writer surfaces
/// as an error the caller can report rather than as a hung request.
///
/// **Zero would be wrong and is worth naming.** With no wait, ordinary
/// same-instant contention between two legitimate operator actions would fail
/// one of them for no reason. **Unbounded would also be wrong**: a stuck writer
/// would convert into every subsequent request hanging, and a door that hangs
/// is indistinguishable from one that is down. The failure direction on expiry
/// is a refused write, which is the safe one.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The statement every transaction in this module begins with.
///
/// `BEGIN IMMEDIATE`, never a bare `BEGIN`. A bare `BEGIN` is DEFERRED: it
/// takes no lock until the first statement, and it takes a READ lock if that
/// statement is a read — which is exactly the shape of every transaction here,
/// since they all read an authority before writing under it. A deferred
/// transaction that reads, then writes, can find its write refused with
/// `SQLITE_BUSY` after another writer committed in between, and — worse for
/// this module — it would have made its authorization decision against a
/// snapshot taken BEFORE that other writer's commit. Taking the write lock up
/// front is what makes the read-then-write sequence indivisible.
///
/// This is the whole replacement for `FOR SHARE` / `FOR UPDATE`. It is a
/// constant, and `every_transaction_in_this_module_is_immediate` fails the
/// build if any transaction in this file is opened another way.
const BEGIN_IMMEDIATE: &str = "BEGIN IMMEDIATE";

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
    pool: SqlitePool,
}

impl OauthStore {
    /// Open the pool against the configured database FILE.
    ///
    /// ## The four connection settings, and why none of them is tuning
    ///
    /// **`journal_mode(Wal)`.** WAL is what lets readers run concurrently with
    /// the single writer. Without it, every read would contend with every write
    /// on one global lock, and the `BEGIN IMMEDIATE` discipline that replaced
    /// this module's row locks would turn the dispatch path — which reads
    /// `dispatch_state` on EVERY request — into a queue behind any
    /// administrative write. WAL is also what makes the write lock a clean
    /// single-writer serialization rather than a reader/writer exclusion.
    ///
    /// **`foreign_keys(true)`.** SQLite defaults foreign-key enforcement OFF,
    /// and the setting is PER-CONNECTION. Several behaviours this door relies
    /// on are `ON DELETE CASCADE`: deleting a tool group REVOKES it from every
    /// client that drew on it, and deleting an account takes its clients,
    /// groups, codes, tokens and consents with it. Left off, every one of those
    /// cascades is silently inert and every reference unchecked — a
    /// same-looking, strictly weaker schema, which is the substitution this
    /// item's review history is made of. Enforced on every pooled connection,
    /// and proved against a live database by
    /// `foreign_keys_are_enforced_on_every_pooled_connection`.
    ///
    /// **`busy_timeout(BUSY_TIMEOUT)`.** See [`BUSY_TIMEOUT`]: this decides
    /// whether a writer that loses the race waits its turn or fails, and the
    /// failure direction is a refused write.
    ///
    /// **`synchronous(Full)`.** The default under WAL is `NORMAL`, which can
    /// lose the most recent committed transactions on an OS crash or power
    /// loss. For most data that trade is right. It is not right here: the
    /// transactions at risk are consent grants and — the direction that
    /// matters — REVOCATIONS. Losing a committed revocation resurrects access
    /// an operator has already been told was cut off, silently. `FULL` costs an
    /// fsync per commit on a database that commits a handful of times per
    /// login.
    ///
    /// **`create_if_missing` is deliberately NOT set.** An absent file is an
    /// unconfigured deployment, and the useful behaviour is to say so. Creating
    /// one would produce a door that starts cleanly with no accounts, no
    /// clients and no consents — which does not fail, it just silently
    /// authorizes nobody and forgets every connector, and would look to an
    /// operator exactly like the migration not having run.
    ///
    /// Connection failures are reported without the path. It is no longer a
    /// credential (S132/RMCP-SQLITE removed `RMCP_DATABASE_URL` entirely), but
    /// an error body is not a place to disclose filesystem layout.
    pub async fn connect(config: &OauthConfig) -> Result<Self, ToolError> {
        let options = SqliteConnectOptions::new()
            .filename(config.sqlite_path())
            .create_if_missing(false)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections())
            .connect_with(options)
            .await
            .map_err(|_| {
                ToolError::Database(
                    "cannot open the RMCP OAuth database file (check RMCP_SQLITE_PATH, that the \
                     file exists and is writable by this service, and that the S132 migration \
                     has been applied to it)"
                        .into(),
                )
            })?;
        Ok(Self { pool })
    }

    /// Build a store over an existing pool. Used by tests and by a caller that
    /// already owns a pool for this database.
    ///
    /// A caller building its own pool owes it the same settings
    /// [`Self::connect`] applies — `foreign_keys` in particular, which is
    /// per-connection and off by default. Tests in this crate go through
    /// [`Self::open_for_test`] rather than constructing options by hand, so
    /// there is one place those settings are written.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a real, empty, migrated database for a test.
    ///
    /// ## Why this exists at all, and why it is not `#[cfg(test)]`
    ///
    /// Under Postgres this crate stood up no database in tests: every fixture
    /// was a `PgPool::connect_lazy` handle that never opened a socket, and the
    /// store's guarantees were therefore asserted by SCANNING ITS OWN SOURCE
    /// for `FOR SHARE` and friends. Those scanners are still here and still
    /// earn their keep, but a text scan cannot tell you that a concurrent
    /// demotion actually loses the race.
    ///
    /// SQLite removes that constraint completely — a database is a temporary
    /// file — so the locking argument in the module docs is now something this
    /// module can PROVE rather than describe. That is the single biggest
    /// correctness gain of the port and it would be wasted behind a lazy pool.
    ///
    /// It is `pub(crate)` rather than `#[cfg(test)]` because the fixtures in
    /// `mount`, `register`, `scope` and `token` need it too, and a
    /// `#[cfg(test)]` item in this module is not visible to theirs.
    ///
    /// `path` names a file the caller owns (a `tempfile::TempDir` entry).
    /// Every setting matches [`Self::connect`] exactly — including
    /// `foreign_keys`, without which a test would pass against cascade
    /// behaviour production does not have — with the single difference that the
    /// file is created rather than required to exist.
    pub(crate) async fn open_for_test(path: &std::path::Path) -> Result<Self, ToolError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections())
            .connect_with(options)
            .await
            .map_err(db)?;
        let store = Self { pool };
        store.apply_migration().await?;
        Ok(store)
    }

    /// Apply `migrations/S132-rmcp-sqlite-oauth.sql` to this database.
    ///
    /// The migration file is `include_str!`'d rather than read from disk so a
    /// test cannot pass against a schema that has drifted from the one the
    /// deploy applies — there is exactly one schema text, and it is the file in
    /// the repository. This is NOT called from [`Self::connect`]: per the v4.6
    /// DEPLOY rule migrations are applied by the operator, sequenced with or
    /// before the image swap, and `schema_ready` is how the door reports a
    /// deployment where that did not happen.
    pub(crate) async fn apply_migration(&self) -> Result<(), ToolError> {
        // A single `execute` of a multi-statement string: sqlx's SQLite driver
        // runs them in sequence. Not wrapped in a transaction — the file is
        // `IF NOT EXISTS` throughout, so a re-run is a no-op and a partial run
        // is exactly what `schema_ready` exists to detect.
        use sqlx::Executor as _;
        self.pool
            .execute(include_str!("../../migrations/S132-rmcp-sqlite-oauth.sql"))
            .await
            .map(|_| ())
            .map_err(db)
    }

    /// Open a transaction holding the database WRITE lock for its whole life.
    ///
    /// **This is the mechanism that replaced every `FOR SHARE` and `FOR UPDATE`
    /// in this module.** See the module docs for the full argument; the short
    /// form is that holding the single write lock from BEGIN serializes writers
    /// completely, so a read taken inside this transaction cannot be
    /// invalidated by another writer before this transaction commits — which is
    /// what the row locks were bought for, obtained over the whole database
    /// instead of over one row.
    ///
    /// `begin_with` rather than `begin`: sqlx's default `BEGIN` is DEFERRED and
    /// would take only a read lock (see [`BEGIN_IMMEDIATE`]). sqlx additionally
    /// verifies that the custom statement really did open a transaction and
    /// returns `Error::BeginFailed` if not, so a typo here cannot degrade to
    /// autocommit — which would silently remove every guarantee above while
    /// every query still succeeded.
    async fn begin_immediate(&self) -> Result<Transaction<'_, Sqlite>, ToolError> {
        self.pool.begin_with(BEGIN_IMMEDIATE).await.map_err(db)
    }

    /// Whether the S132 schema is present.
    ///
    /// Migrations are not applied at startup (the v4.6 DEPLOY rule), so a
    /// deploy that ships this code without applying the migration is a real
    /// possibility. Reporting it as a clear "unconfigured" at boot beats every
    /// endpoint failing later with an opaque `relation does not exist`.
    pub async fn schema_ready(&self) -> bool {
        // Restricted to `type = 'table'`: `sqlite_master` also lists views,
        // indexes and triggers, so without this a VIEW named `rmcp_client`
        // would report the schema ready with no migrated table behind it. This
        // is the SQLite spelling of the Postgres version's `table_type =
        // 'BASE TABLE'`, and it is here for the same reason.
        //
        // Checks ALL eleven tables, not a sentinel one. Review round 1: probing
        // a single table reports "ready" for a partially applied migration —
        // precisely the state a half-finished deploy leaves behind, and the one
        // where a confident "ready" is most harmful.
        //
        // One query PER TABLE rather than the Postgres version's single
        // `name = ANY(?1)`. SQLite cannot bind an array, and the alternatives
        // are worse: an `IN (…)` list would have to be built with `format!`,
        // which forfeits the mechanically-checkable "no SQL interpolation in
        // this module" rule for a startup-only convenience (see
        // `CLIENT_ADMIN_BY_ID`'s note on why that rule is kept absolute). Eleven
        // indexed lookups once at boot cost nothing.
        for table in REQUIRED_TABLES {
            let found = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            )
            .bind(table)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
            if found != 1 {
                return false;
            }
        }

        // RMCP-06 and RMCP-08 add COLUMNS to existing tables, which the table
        // check above cannot see. Without this, a deploy whose migration was
        // interrupted between the `CREATE TABLE`s and these columns reports
        // "ready" and then fails every account lookup — i.e. the whole
        // authentication path — with an opaque "no such column". A schema check
        // that misses part of the file is exactly the confident-but-wrong
        // "ready" the check above exists to prevent.
        //
        // `pragma_table_info` is SQLite's table-valued equivalent of
        // `information_schema.columns`, and it accepts a bound argument, so this
        // stays interpolation-free too.
        for (table, column) in REQUIRED_COLUMNS {
            let present = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2",
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
             FROM rmcp_account WHERE name = ?1 AND NOT disabled",
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
             FROM rmcp_account WHERE id = ?1 AND NOT disabled",
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
            "SELECT EXISTS (SELECT 1 FROM rmcp_account WHERE id = ?1 AND NOT disabled)",
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
    /// The id is generated HERE rather than by a column default. Postgres had
    /// `gen_random_uuid()`; SQLite has no equivalent, and `randomblob(16)`
    /// would produce a value that is random but not a valid RFC 4122 v4 UUID —
    /// no version or variant bits — which every consumer of these ids
    /// (including `Uuid`'s own parser on the way back out) is entitled to
    /// assume. Generating in Rust keeps the ids exactly what they were.
    pub async fn insert_account(
        &self,
        name: &str,
        password_hash: &Argon2idHash,
        totp_secret_enc: Option<&[u8]>,
    ) -> Result<Uuid, ToolError> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO rmcp_account (id, name, password_hash, totp_secret_enc) \
             VALUES (?1, ?2, ?3, ?4)")
        .bind(id)
        .bind(name)
        .bind(password_hash.as_str())
        .bind(totp_secret_enc)
        .execute(&self.pool)
        .await
        .map_err(unique_aware("an account with that name already exists"))?;
        Ok(id)
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
             FROM rmcp_client WHERE client_id = ?1 AND NOT disabled",
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
        let mut tx = self.begin_immediate().await?;

        // `authorize_client_write` reads exactly this rule — operator, or the
        // owner themselves — so creation and modification are decided by ONE
        // function rather than two that could drift. The "client owner" it is
        // given is the owner the caller asked for, which is what makes
        // "creating a connector for somebody else" the operator-only case.
        let actor = Self::actor_authority(&mut tx, actor_account_id).await?;
        authorize_client_write(&actor, owner_account_id)?;

        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO rmcp_client (id, client_id, client_secret_hash, name, redirect_uris, \
                                      grant_types, token_endpoint_auth_method, \
                                      owner_account_id, registration_source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(id)
        .bind(client_id)
        .bind(client_secret_hash.map(Argon2idHash::as_str))
        .bind(name)
        .bind(json_list(redirect_uris)?)
        .bind(json_list(grant_types)?)
        .bind(token_endpoint_auth_method)
        .bind(owner_account_id)
        .bind(registration_source)
        .execute(&mut *tx)
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
             FROM rmcp_client WHERE owner_account_id = ?1 ORDER BY created_at",
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
        sqlx::query("UPDATE rmcp_client SET disabled = ?2 WHERE id = ?1")
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
    /// ## LOCK SITE 1 and 2 — what held this, and what holds it now
    ///
    /// Postgres locked the account row (`FOR SHARE`, via
    /// [`Self::locked_active_account`]) **and** every `rmcp_server_owner` row
    /// the account holds (`FOR SHARE`, below), so that neither operator-ness
    /// nor namespace ownership could move between this read and the write it
    /// authorizes.
    ///
    /// **What it protects.** This function derives BOTH halves of an actor's
    /// authority. If the account row could be demoted or disabled after this
    /// read, a write authorized by `is_operator` would land on an account that
    /// is no longer an operator. If a `rmcp_server_owner` row could be deleted
    /// after this read, `clear_server_owner` could land in the gap and
    /// `set_client_namespaces` would attach a namespace its owner no longer
    /// owns.
    ///
    /// **How SQLite preserves it.** Every caller of this function is inside a
    /// `BEGIN IMMEDIATE` transaction (see [`Self::begin_immediate`]), which
    /// holds the database's single write lock from BEGIN to COMMIT. A demotion,
    /// a disablement and a `DELETE FROM rmcp_server_owner` are all WRITES, and
    /// no other writer can commit while this transaction holds the lock — so
    /// none of them can occur between this read and this transaction's commit.
    /// That is strictly stronger than the row locks it replaces: `FOR SHARE`
    /// excluded writers of THOSE ROWS, `BEGIN IMMEDIATE` excludes every writer.
    ///
    /// Proven, not asserted, by
    /// `a_concurrent_demotion_cannot_interleave_with_an_authorized_write` and
    /// `a_concurrent_delegation_clear_cannot_interleave_with_a_namespace_write`.
    ///
    /// This function itself takes a `&mut Transaction`, so it is not reachable
    /// outside one; `every_transaction_in_this_module_is_immediate` covers the
    /// remaining half by proving no transaction here is opened any other way.
    ///
    /// A missing or DISABLED account yields [`ToolError::NotFound`] rather than
    /// a delegated authority: an account that cannot authenticate should not be
    /// authoring scoping records at all, so this refuses the write outright
    /// instead of quietly downgrading it to the less privileged path.
    ///
    /// ## RMCP-12: it now carries the OWNED NAMESPACES too
    ///
    /// Both halves of an actor's authority — is it an operator, and which
    /// servers does it own — are read in the SAME transaction, so neither can
    /// move between the check and the write it authorizes. That is the whole
    /// reason delegation's rules are pure functions over an [`ActorAuthority`]:
    /// the value can be derived where the write lock is held, and the rule can
    /// then be the same one the read path uses.
    async fn actor_authority(
        tx: &mut Transaction<'_, Sqlite>,
        account_id: Uuid,
    ) -> Result<ActorAuthority, ToolError> {
        let is_operator = Self::locked_active_account(tx, account_id).await?;
        let owned = sqlx::query_scalar::<_, String>(
            "SELECT namespace FROM rmcp_server_owner WHERE owner_account_id = ?1 \
             ORDER BY namespace",
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
    /// **The name still says `locked`, and it is still accurate.** Postgres
    /// held this row with `FOR SHARE`; SQLite holds the whole database with the
    /// write lock this transaction took at `BEGIN IMMEDIATE`. Either way the
    /// point is the same and it is the whole point: the answer is true at
    /// COMMIT, not merely true when it was read. Every authorization that
    /// depends on account state goes through here, so there is one place where
    /// "is this account allowed, right now, and will it still be when this
    /// write lands" is answered.
    ///
    /// Taking `&mut Transaction` is what enforces that: the type makes it
    /// impossible to call this on the pool in autocommit, which would answer
    /// the same question with no guarantee attached to the answer. That is the
    /// exact shape of a check that "looks the same and is weaker", so it is
    /// prevented by the signature rather than by a comment.
    ///
    /// A missing or disabled account is [`ToolError::NotFound`] with one shared
    /// message — not two, and not a downgrade to a less privileged authority.
    async fn locked_active_account(
        tx: &mut Transaction<'_, Sqlite>,
        account_id: Uuid,
    ) -> Result<bool, ToolError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT is_operator FROM rmcp_account WHERE id = ?1 AND NOT disabled",
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
        tx: &mut Transaction<'_, Sqlite>,
        actor: &ActorAuthority,
        client_owner: Uuid,
    ) -> Result<ActorAuthority, ToolError> {
        if client_owner == actor.account_id() {
            return Ok(actor.clone());
        }
        Self::actor_authority(tx, client_owner).await
    }

    /// The client's owner, held for the rest of the transaction.
    ///
    /// ## LOCK SITE 3
    ///
    /// **What it protects.** Ownership must not be reassigned between this read
    /// and the write it authorizes. Without that, an administrative edit could
    /// be authorized against the owner a client HAD and then land on a client
    /// somebody else now owns — a TOCTOU letting a write proceed on stale
    /// authority. `redirect_uris` is reachable through that path, and rewriting
    /// one redirects where a linked account's authorization code is delivered,
    /// so it is the most attacker-valuable field in the item.
    ///
    /// **How SQLite preserves it.** `UPDATE rmcp_client SET owner_account_id
    /// = …` is a write; this function is only reachable from inside a
    /// `BEGIN IMMEDIATE` transaction (enforced by the `&mut Transaction`
    /// parameter), which holds the write lock until commit; therefore no
    /// reassignment can commit in the window. Same argument as sites 1–2, and
    /// again strictly wider than the single row `FOR SHARE` held.
    ///
    /// `None` when there is no such client; callers must answer that exactly as
    /// they answer "not yours", so this is not an existence oracle.
    async fn locked_client_owner(
        tx: &mut Transaction<'_, Sqlite>,
        client_id: Uuid,
    ) -> Result<Option<Uuid>, ToolError> {
        sqlx::query_scalar::<_, Uuid>("SELECT owner_account_id FROM rmcp_client WHERE id = ?1")
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
        let mut tx = self.begin_immediate().await?;
        let authority = Self::actor_authority(&mut tx, owner_account_id).await?;
        let group = validate_group(name, description, patterns, &authority.authoring())?;
        let rendered = group.rendered_patterns();
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO rmcp_tool_group (id, name, description, patterns, owner_account_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(&group.name)
        .bind(&group.description)
        .bind(json_list(&rendered)?)
        .bind(owner_account_id)
        .execute(&mut *tx)
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
             FROM rmcp_tool_group WHERE owner_account_id = ?1 ORDER BY name",
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
        let mut tx = self.begin_immediate().await?;
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
            "UPDATE rmcp_tool_group SET description = ?3, patterns = ?4 \
             WHERE id = ?1 AND owner_account_id = ?2",
        )
        .bind(group_id)
        .bind(actor_account_id)
        .bind(&description)
        .bind(json_list(&patterns)?)
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
        let deleted = sqlx::query("DELETE FROM rmcp_tool_group WHERE id = ?1 AND owner_account_id = ?2")
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
        let mut tx = self.begin_immediate().await?;
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
            // which holds the database write lock for its whole life.
            let group = validate_group(
                starter.name,
                starter.description,
                &patterns,
                &crate::oauth::delegation::Authoring::Operator,
            )?;
            let rendered = group.rendered_patterns();
            let inserted = sqlx::query(
                "INSERT INTO rmcp_tool_group (id, name, description, patterns, owner_account_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (name) DO NOTHING",
            )
            .bind(Uuid::new_v4())
            .bind(&group.name)
            .bind(&group.description)
            .bind(json_list(&rendered)?)
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
             WHERE s.client_id = ?1 ORDER BY g.name",
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
             WHERE s.client_id = ?1 ORDER BY g.name",
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
             WHERE s.client_id = ?1 \
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
        let mut tx = self.begin_immediate().await?;
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
    /// `locked_client_owner` + [`authorize_client_write`], all read inside the
    /// caller's `BEGIN IMMEDIATE` transaction. This split moved WHERE the
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
        tx: &mut Transaction<'_, Sqlite>,
        _scope_write: &ScopeWrite,
        actor_account_id: Uuid,
        client_id: Uuid,
        group_ids: &[Uuid],
    ) -> Result<(), ToolError> {
        // The actor's authority and the client's owner, both read inside this
        // write-locked transaction. Without that the checks are a TOCTOU: a
        // concurrent transfer or demotion could land in the gap and the write
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

        // ## LOCK SITE 4
        //
        // **What it protects.** Every requested group must belong to the owner,
        // and that ownership must not be reassigned between this check and the
        // `rmcp_client_scope` insert below. Postgres held each matching row
        // with `FOR SHARE`. Without it, a group could be transferred in the gap
        // and this client would end up scoped to a group its owner does not
        // own — which `client_authorized_groups` would then stop resolving,
        // meaning the write silently produced nothing. Worse in the other
        // direction: a group could be transferred TO the owner mid-check.
        //
        // **How SQLite preserves it.** Ownership is reassigned only by an
        // UPDATE, and this transaction holds the write lock, so no reassignment
        // can commit before the insert lands.
        //
        // **The query shape changed, and the reason is the "no interpolation"
        // rule.** Postgres matched the whole set with `id = ANY(?1)`; SQLite
        // cannot bind an array, and an `IN (…)` list would have to be built
        // with `format!`. Rather than forfeit a mechanically-checkable rule for
        // one query, the ids are probed one at a time. That is sound here in a
        // way it would not be under Postgres: N separate statements inside ONE
        // write-locked transaction cannot interleave with anything, so the set
        // of answers is as consistent as a single statement's would be. The
        // loop is bounded by `check_group_budget` (MAX_GROUPS_PER_CLIENT),
        // which ran before the transaction was opened.
        //
        // Counted over DISTINCT ids so a duplicate in the input cannot inflate
        // the match — the same property the Postgres version got from comparing
        // against the distinct input count.
        let distinct: std::collections::HashSet<&Uuid> = group_ids.iter().collect();
        let mut owned_groups = 0i64;
        for group_id in &distinct {
            let found = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM rmcp_tool_group WHERE id = ?1 AND owner_account_id = ?2",
            )
            .bind(*group_id)
            .bind(owner_authority.account_id())
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
            if found.is_some() {
                owned_groups += 1;
            }
        }
        let requested = distinct.len() as i64;
        if owned_groups != requested {
            return Err(ToolError::InvalidArgument(
                "one or more tool groups do not belong to this account".into(),
            ));
        }

        // Delete-then-insert rather than a diff: a partially applied scope
        // change is a permission state nobody chose, and under concurrent edits
        // a diff can interleave into exactly that. Wholesale replacement makes
        // the outcome always one of the two intended states.
        sqlx::query("DELETE FROM rmcp_client_scope WHERE client_id = ?1")
            .bind(client_id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        for group_id in group_ids {
            sqlx::query(
                "INSERT INTO rmcp_client_scope (client_id, tool_group_id) VALUES (?1, ?2) \
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
        let mut tx = self.begin_immediate().await?;
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
        tx: &mut Transaction<'_, Sqlite>,
        _scope_write: &ScopeWrite,
        actor_account_id: Uuid,
        client_id: Uuid,
        namespaces: &[String],
    ) -> Result<(), ToolError> {
        // Inside the caller's write-locked transaction throughout, exactly as
        // in `set_client_tool_groups`. Round 8 caught that this copy had been
        // left as an UNLOCKED `SELECT EXISTS` while its own doc comment claimed
        // the same locking guarantee — a documented promise the code did not
        // keep, which is worse than an undocumented gap because it stops the
        // next reader looking.
        //
        // That finding is worth re-reading in the SQLite context, because the
        // port could have reintroduced it in a subtler form: an unlocked read
        // here would now mean a read taken on a pool connection in autocommit
        // rather than through `tx`, which looks almost identical at the call
        // site and carries no guarantee at all. `actor_authority` and
        // `locked_client_owner` both take `&mut Transaction`, so that mistake
        // does not compile.
        //
        // `actor_authority` re-reads the account row AND every
        // `rmcp_server_owner` row that account holds, which is what closes the
        // other half of the race: `clear_server_owner` could otherwise land
        // between the ownership read and the insert, letting a former owner
        // attach a server they no longer own. Under SQLite it cannot, because
        // that DELETE is a write and this transaction holds the write lock.
        let actor = Self::actor_authority(tx, actor_account_id).await?;
        let Some(client_owner) = Self::locked_client_owner(tx, client_id).await? else {
            return Err(ToolError::NotFound("no such client for this account".into()));
        };
        authorize_client_write(&actor, client_owner)?;

        // RMCP-12: ONE function decides this, here and on every other write
        // path. It is given an authority derived inside this transaction, under
        // its write lock — never one a caller supplied, and never one read
        // before the transaction began.
        //
        // It is the CLIENT OWNER's authority, because that is whose ownership
        // `client_namespaces` re-joins on at resolution time. Checking the
        // actor's instead would let an operator write rows for a delegated
        // user's client that then resolve to nothing.
        let owner_authority = Self::client_owner_authority(tx, &actor, client_owner).await?;
        authorize_namespace_scoping(&owner_authority, namespaces)?;

        sqlx::query("DELETE FROM rmcp_client_server WHERE client_id = ?1")
            .bind(client_id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        for namespace in namespaces {
            sqlx::query(
                "INSERT INTO rmcp_client_server (client_id, namespace) VALUES (?1, ?2) \
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
    // `authorize_operator_action`), derived INSIDE the writing transaction,
    // which holds the database write lock. There is deliberately no second
    // mechanism and no
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
         FROM rmcp_client WHERE id = ?1";

    /// The `::uuid` / `::boolean` / `::text[]` casts the Postgres versions
    /// carried are gone, and their absence is not a simplification to gloss
    /// over. Postgres needed them because it type-checks a parameter used only
    /// in a NULL test and cannot infer what `?1` is. SQLite is dynamically
    /// typed: `?1 IS NULL` and `COALESCE(?3, disabled)` need no annotation, and
    /// there is no cast that could be wrong. The NULL-tolerant SEMANTICS —
    /// `NULL` means "every client" for the filter and "leave this column alone"
    /// for the update — are unchanged, which is the part that matters.
    const CLIENT_ADMIN_BY_OWNER: &'static str = "SELECT \
         id, client_id, name, redirect_uris, grant_types, token_endpoint_auth_method, \
         owner_account_id, registration_source, disabled, \
         (client_secret_hash IS NOT NULL) AS confidential, created_at, version \
         FROM rmcp_client WHERE (?1 IS NULL OR owner_account_id = ?1) \
         ORDER BY created_at";

    const CLIENT_ADMIN_UPDATE: &'static str = "UPDATE rmcp_client SET \
         disabled = COALESCE(?3, disabled), \
         redirect_uris = COALESCE(?4, redirect_uris), \
         version = version + 1 \
         WHERE id = ?1 AND version = ?2 \
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
    /// inside THIS write-locked transaction, and [`authorize_client_write`]
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
        let mut tx = self.begin_immediate().await?;

        // AUTHORIZE FIRST, in this transaction, before anything is written.
        let actor = Self::actor_authority(&mut tx, actor_account_id).await?;
        let Some(client_owner) = Self::locked_client_owner(&mut tx, client_id).await? else {
            return Err(ToolError::NotFound("no such client for this account".into()));
        };
        authorize_client_write(&actor, client_owner)?;

        // `None` must reach SQL as a NULL so `COALESCE` leaves the column
        // alone. `Option<String>` binds that way; mapping through `json_list`
        // only when there is a value is what keeps "not supplied" and "supplied
        // as the empty list" distinct. Collapsing them would mean an edit that
        // touched only `disabled` silently cleared every redirect URI — which
        // does not fail, it just breaks the connector at the next login.
        let redirect_uris_json = redirect_uris.map(json_list).transpose()?;
        let updated = sqlx::query_as::<_, ClientAdmin>(Self::CLIENT_ADMIN_UPDATE)
        .bind(client_id)
        .bind(expected_version)
        .bind(disabled)
        .bind(redirect_uris_json)
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
        let mut tx = self.begin_immediate().await?;

        let actor = Self::actor_authority(&mut tx, actor_account_id).await?;
        let Some(client_owner) = Self::locked_client_owner(&mut tx, client_id).await? else {
            return Err(ToolError::NotFound("no such client for this account".into()));
        };
        authorize_client_write(&actor, client_owner)?;

        // The version is bumped so a concurrent editor holding a pre-revocation
        // read is refused rather than re-enabling the client by saving a stale
        // form.
        //
        // `disabled = 1`, not `true`: SQLite has no boolean literal keyword and
        // parses a bare `true` as an identifier in some builds. The column is
        // INTEGER 0/1 and `NOT disabled` reads it exactly as before.
        sqlx::query("UPDATE rmcp_client SET disabled = 1, version = version + 1 WHERE id = ?1")
            .bind(client_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;

        let tokens = sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = unixepoch() \
             WHERE client_id = ?1 AND revoked_at IS NULL",
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
        let mut tx = self.begin_immediate().await?;
        let actor = Self::actor_authority(&mut tx, issued_by).await?;
        authorize_operator_action(&actor, OPERATOR_ONLY_REGISTRATION_TOKEN)?;
        // `unixepoch() + ?5` replaces `now() + make_interval(secs => …)`. Both
        // evaluate the DATABASE clock and add a TTL in seconds; with an INTEGER
        // epoch column the addition is exact integer arithmetic rather than an
        // interval against a `double precision`, so the rounding the old form
        // could introduce is gone. `ttl_seconds` is bound as the `i64` it
        // already is instead of being widened to `f64`.
        sqlx::query(
            "INSERT INTO rmcp_registration_token \
                 (token_hash, issued_by, label, uses_remaining, expires_at) \
             VALUES (?1, ?2, ?3, ?4, unixepoch() + ?5)",
        )
        .bind(token_hash.as_bytes())
        .bind(issued_by)
        .bind(label)
        .bind(uses)
        .bind(ttl_seconds)
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
    /// The token row is read and its issuer authorized BEFORE the decrement. A
    /// token presented while its issuer is unauthorized is not consumed — it
    /// was never spendable, and burning a use would let anyone holding a copy
    /// exhaust a legitimate token by presenting it during a demotion.
    ///
    /// ## LOCK SITE 5 — the one that needed the most care
    ///
    /// **What it protects.** Bounded use. Two concurrent redemptions of a
    /// single-use token must not both succeed. Postgres locked the row `FOR
    /// UPDATE` — not `FOR SHARE`, because the row is about to be decremented —
    /// and relied on READ COMMITTED re-evaluating the `WHERE` after the lock
    /// was granted, so the loser saw `uses_remaining = 0` and got `None`.
    ///
    /// **How SQLite preserves it, and why the reasoning is DIFFERENT here.**
    /// SQLite has neither row locks nor Postgres's re-evaluation behaviour, so
    /// "the loser re-reads and sees zero" is NOT the mechanism any more, and
    /// carrying that sentence over would have been exactly the kind of
    /// same-looking, unsupported claim this item keeps finding. The mechanism
    /// is instead that the loser never runs concurrently at all: both
    /// redemptions call [`Self::begin_immediate`], which takes the database
    /// write lock, so the second `BEGIN IMMEDIATE` blocks until the first has
    /// COMMITTED. It then performs its `SELECT` against the committed state and
    /// sees `uses_remaining = 0`, failing the `uses_remaining > 0` predicate
    /// and returning `None`.
    ///
    /// The outcome is identical and the ordering is total rather than
    /// per-row. `a_concurrent_redemption_cannot_double_spend_a_single_use_token`
    /// proves it against a real database.
    ///
    /// This is also why the read and the decrement must stay inside ONE
    /// transaction. Split across two, the write lock would be released in
    /// between and both redeemers could read an unspent token.
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
        // `begin_immediate`, and the IMMEDIATE is load-bearing rather than
        // uniform: this transaction READS a state it is about to decrement, so
        // a DEFERRED begin would take only a read lock here and could find
        // another redeemer had committed before its own UPDATE. Taking the
        // write lock up front is what makes the check and the spend one
        // indivisible act. See LOCK SITE 5 above.
        let mut tx = self.begin_immediate().await?;

        let Some(issued_by) = sqlx::query_scalar::<_, Uuid>(
            "SELECT issued_by FROM rmcp_registration_token \
             WHERE token_hash = ?1 AND uses_remaining > 0 AND expires_at > unixepoch() \
               AND revoked_at IS NULL",
        )
        .bind(token_hash.as_bytes())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        else {
            return Ok(None);
        };

        // RE-DERIVE the issuer's authority, here, in this write-locked
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
             WHERE token_hash = ?1",
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
        let mut tx = self.begin_immediate().await?;
        let actor = Self::actor_authority(&mut tx, actor_account_id).await?;
        authorize_operator_action(&actor, OPERATOR_ONLY_REGISTRATION_TOKEN)?;
        let revoked = sqlx::query(
            "UPDATE rmcp_registration_token SET revoked_at = unixepoch() \
             WHERE revoked_at IS NULL AND uses_remaining > 0 AND expires_at > unixepoch()",
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
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch() + ?8)",
        )
        .bind(code_hash.as_bytes())
        .bind(client_id)
        .bind(account_id)
        .bind(redirect_uri)
        .bind(resource)
        .bind(code_challenge)
        .bind(scope)
        .bind(ttl_seconds)
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
            "UPDATE rmcp_auth_code SET consumed_at = unixepoch() \
             WHERE code_hash = ?1 AND consumed_at IS NULL AND expires_at > unixepoch() \
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
    /// `expires_at > unixepoch()`.
    pub async fn purge_expired_auth_codes(&self) -> Result<u64, ToolError> {
        sqlx::query("DELETE FROM rmcp_auth_code WHERE expires_at < unixepoch()")
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
             VALUES (?1, unixepoch() + ?2) \
             ON CONFLICT (jti_hash) DO NOTHING",
        )
        .bind(jti_hash.as_bytes())
        .bind(ttl_seconds)
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
        sqlx::query("DELETE FROM rmcp_login_session_use WHERE expires_at < unixepoch()")
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
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch() + ?7)",
        )
        .bind(token_hash.as_bytes())
        .bind(family_id)
        .bind(client_id)
        .bind(account_id)
        .bind(resource)
        .bind(scope)
        .bind(ttl_seconds)
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
             FROM rmcp_refresh_token WHERE token_hash = ?1",
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
             WHERE t.token_hash = ?1 AND t.rotated_to IS NULL AND t.revoked_at IS NULL \
               AND t.expires_at > unixepoch() \
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
        let mut tx = self.begin_immediate().await?;

        // SQLite does not accept a table ALIAS in `UPDATE`, so the `t.` prefix
        // is dropped and the correlated subquery refers to the target table by
        // its full name. Same predicate, same family-wide revocation rule: ANY
        // revoked row in the family kills the whole family, including rows
        // inserted after the revocation ran (round 7).
        let rotated = sqlx::query(
            "UPDATE rmcp_refresh_token SET rotated_to = ?2 \
             WHERE token_hash = ?1 AND rotated_to IS NULL AND revoked_at IS NULL \
               AND expires_at > unixepoch() \
               AND NOT EXISTS (SELECT 1 FROM rmcp_refresh_token r \
                               WHERE r.family_id = rmcp_refresh_token.family_id \
                                 AND r.revoked_at IS NOT NULL)",
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
             SELECT ?2, family_id, client_id, account_id, resource, scope, \
                    unixepoch() + ?3 \
             FROM rmcp_refresh_token WHERE token_hash = ?1",
        )
        .bind(token_hash.as_bytes())
        .bind(successor_hash.as_bytes())
        .bind(ttl_seconds)
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
                 SELECT 1 FROM rmcp_refresh_token WHERE account_id = ?1 AND client_id = ?2 \
             ) OR EXISTS ( \
                 SELECT 1 FROM rmcp_refresh_token \
                 WHERE account_id = ?1 AND client_id = ?2 \
                   AND revoked_at IS NULL AND expires_at > unixepoch() \
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
            "UPDATE rmcp_refresh_token SET revoked_at = unixepoch() \
             WHERE family_id = ?1 AND revoked_at IS NULL",
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
            "UPDATE rmcp_refresh_token SET revoked_at = unixepoch() \
             WHERE client_id = ?1 AND revoked_at IS NULL",
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
            "INSERT INTO rmcp_consent (id, account_id, client_id, scope) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(Uuid::new_v4())
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
             WHERE account_id = ?1 AND client_id = ?2 AND scope = ?3 AND revoked_at IS NULL",
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
             WHERE account_id = ?1 AND client_id = ?2 AND revoked_at IS NULL)",
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
        let mut tx = self.begin_immediate().await?;
        let consents = sqlx::query(
            "UPDATE rmcp_consent SET revoked_at = unixepoch() \
             WHERE account_id = ?1 AND client_id = ?2 AND revoked_at IS NULL",
        )
        .bind(account_id)
        .bind(client_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        let tokens = sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = unixepoch() \
             WHERE account_id = ?1 AND client_id = ?2 AND revoked_at IS NULL",
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
        let mut tx = self.begin_immediate().await?;
        let consents = sqlx::query(
            "UPDATE rmcp_consent SET revoked_at = unixepoch() \
             WHERE account_id = ?1 AND revoked_at IS NULL",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        let tokens = sqlx::query(
            "UPDATE rmcp_refresh_token SET revoked_at = unixepoch() \
             WHERE account_id = ?1 AND revoked_at IS NULL",
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
                    COALESCE(min(t.revoked_at) IS NULL AND max(t.expires_at) > unixepoch(), false) AS live \
             FROM rmcp_refresh_token t \
             WHERE (?1 IS NULL OR t.account_id = ?1) \
               AND (?2 IS NULL OR t.client_id = ?2) \
               AND (?3 IS NULL OR t.family_id = ?3) \
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
            "SELECT family_id FROM rmcp_refresh_token WHERE token_hash = ?1",
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
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM rmcp_account WHERE name = ?1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    /// Resolve a public `client_id` to its internal id, including a disabled
    /// client — same reasoning as [`Self::resolve_account_id`].
    pub async fn resolve_client_id(&self, client_id: &str) -> Result<Option<Uuid>, ToolError> {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM rmcp_client WHERE client_id = ?1")
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
    /// revision accepted `Option<Uuid>` and wrote `?3 IS NULL OR (…)`,
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
               EXISTS (SELECT 1 FROM rmcp_client WHERE id = ?2 AND NOT disabled)  AS client_ok, \
               EXISTS (SELECT 1 FROM rmcp_account WHERE id = ?1 AND NOT disabled) AS account_ok, \
               EXISTS (SELECT 1 FROM rmcp_consent \
                       WHERE account_id = ?1 AND client_id = ?2 AND revoked_at IS NULL) AS consent_ok, \
                   EXISTS (SELECT 1 FROM rmcp_refresh_token \
                           WHERE family_id = ?3 AND account_id = ?1 AND client_id = ?2) \
               AND NOT EXISTS (SELECT 1 FROM rmcp_refresh_token \
                               WHERE family_id = ?3 AND revoked_at IS NOT NULL) AS family_ok",
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
        let mut tx = self.begin_immediate().await?;

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

        // ## LOCK SITE 6
        //
        // **What it protects.** This read decides `DelegationChange.reassigned`
        // — whether this grant TOOK a namespace from a previous owner — and it
        // must describe the same state the upsert immediately below writes
        // over. Postgres held the row `FOR UPDATE` so a concurrent
        // `set_server_owner` or `clear_server_owner` for the same namespace
        // could not land in between, which would make the reported outcome
        // describe a delegation that no longer existed.
        //
        // **How SQLite preserves it.** Both of those are writes and this
        // transaction holds the database write lock, so neither can commit
        // between this SELECT and this transaction's COMMIT. The upsert, the
        // narrowing, and this read are one indivisible unit.
        let previous = sqlx::query_scalar::<_, Uuid>(
            "SELECT owner_account_id FROM rmcp_server_owner WHERE namespace = ?1",
        )
        .bind(namespace)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "INSERT INTO rmcp_server_owner (namespace, owner_account_id) VALUES (?1, ?2) \
             ON CONFLICT (namespace) DO UPDATE SET owner_account_id = EXCLUDED.owner_account_id, \
                                                   granted_at = unixepoch()",
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
        tx: &mut Transaction<'_, Sqlite>,
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
        // Postgres's `DELETE … USING` has no SQLite equivalent, so the join
        // moves into a subquery selecting the CLIENT IDS that lose the row.
        // The predicate is transcribed term for term — operator owners keep
        // their rows (their reach is by default, not by delegation), and a row
        // survives only if its client's owner still owns this namespace.
        //
        // The one substitution to check carefully is `o.namespace = s.namespace`
        // becoming `o.namespace = ?1`: the outer `WHERE namespace = ?1` already
        // pins `s.namespace` to that value, so the two are the same condition,
        // not a widening. This matters because a WIDER delete here would remove
        // reach the read path would still have honoured — and keeping this
        // predicate identical to `client_namespaces`'s is the whole reason this
        // cleanup is provably safe.
        sqlx::query(
            "DELETE FROM rmcp_client_server \
             WHERE namespace = ?1 \
               AND client_id IN ( \
                   SELECT c.id FROM rmcp_client c \
                   JOIN rmcp_account a ON a.id = c.owner_account_id \
                   WHERE NOT a.is_operator \
                     AND NOT EXISTS (SELECT 1 FROM rmcp_server_owner o \
                                     WHERE o.namespace = ?1 \
                                       AND o.owner_account_id = c.owner_account_id))",
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
            "SELECT namespace FROM rmcp_server_owner WHERE owner_account_id = ?1 \
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
             WHERE namespace = ?1",
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
        let mut tx = self.begin_immediate().await?;

        // Re-verified under lock, exactly as in `set_server_owner` and for the
        // same reason. There is no "but this one only narrows" exemption: a
        // revocation is an administrative action on someone else's access, and
        // an account that has just been disabled must not be able to complete
        // one on a proof it minted a moment earlier.
        let live_actor = Self::actor_authority(&mut tx, revocation.actor()).await?;
        reverify_delegation_change(revocation.actor(), &live_actor)?;

        sqlx::query("DELETE FROM rmcp_server_owner WHERE namespace = ?1")
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
            "SELECT is_operator FROM rmcp_account WHERE id = ?1 AND NOT disabled",
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
        sqlx::query_scalar::<_, String>("SELECT name FROM rmcp_account WHERE id = ?1")
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

/// Render a `text[]`-replacement value as the JSON array its column holds.
///
/// The single place a list is encoded, mirroring [`crate::oauth::model`]'s
/// single place a list is decoded. Serialising a `&[String]` cannot actually
/// fail, but the fallible signature is kept rather than `expect`-ing: a panic
/// on the write path of an authorization record is a worse outcome than a
/// refused write, and the refusal costs one `?`.
fn json_list(values: &[String]) -> Result<String, ToolError> {
    serde_json::to_string(values).map_err(|_| {
        // Deliberately carries nothing of the input. These are operator-authored
        // tool patterns and redirect URIs rather than credentials, but this
        // module's standing rule is that no stored value reaches an error.
        ToolError::Database("RMCP OAuth store could not encode a stored list".into())
    })
}

/// Map a sqlx error to a [`ToolError`] without leaking storage details.
///
/// sqlx's `Display` can include the database FILE PATH for open and I/O errors
/// (it included the URL's host and user before S132/RMCP-SQLITE), so the
/// message is a fixed string plus the database's own error code where one
/// exists — enough to diagnose, not enough to disclose.
fn db(e: sqlx::Error) -> ToolError {
    match e.as_database_error().and_then(|d| d.code()) {
        Some(code) => {
            ToolError::Database(format!("RMCP OAuth store query failed (SQLite code {code})"))
        }
        None => ToolError::Database("RMCP OAuth store query failed".into()),
    }
}

/// SQLite's extended result codes for a violated uniqueness constraint.
///
/// Two, not one, and both are needed. SQLite reports `SQLITE_CONSTRAINT_UNIQUE`
/// (2067) for a UNIQUE index and `SQLITE_CONSTRAINT_PRIMARYKEY` (1555) for a
/// PRIMARY KEY, where Postgres reported SQLSTATE 23505 for both. Every conflict
/// this function is asked about is reachable through either: `rmcp_account.name`
/// and `rmcp_client.client_id` are UNIQUE columns, while `rmcp_consent`'s
/// idempotence rests on a partial UNIQUE index and the id columns are PRIMARY
/// KEYs.
///
/// Matching only 2067 would be the quiet half-fix: the common case would keep
/// working and a primary-key collision would surface as an opaque
/// `ToolError::Database` instead of the "already exists" a caller can act on.
const SQLITE_CONSTRAINT_UNIQUE: &str = "2067";
const SQLITE_CONSTRAINT_PRIMARYKEY: &str = "1555";

/// Map a unique-constraint violation to a [`ToolError::Conflict`] with a
/// caller-supplied message, and everything else through [`db`].
///
/// Matched by CODE, never by message text — the same discipline as the TERM-608
/// fit_score fallback, where classifying on message text broke as soon as the
/// wording changed. SQLite's constraint messages ("UNIQUE constraint failed:
/// rmcp_account.name") are especially tempting to match on and especially
/// unstable, so the rule is worth restating here rather than assumed to have
/// survived the port.
fn unique_aware(conflict_message: &'static str) -> impl Fn(sqlx::Error) -> ToolError {
    move |e: sqlx::Error| {
        let code = e.as_database_error().and_then(|d| d.code());
        match code.as_deref() {
            Some(SQLITE_CONSTRAINT_UNIQUE) | Some(SQLITE_CONSTRAINT_PRIMARYKEY) => {
                ToolError::Conflict(conflict_message.into())
            }
            _ => db(e),
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
    sqlx::query("UPDATE rmcp_server_owner SET owner_account_id = ?2 WHERE namespace = ?1")
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

    /// The error mapper must not become a channel for storage details. Before
    /// S132/RMCP-SQLITE that meant the connection string's host and user; it
    /// now means the database FILE PATH, which sqlx includes in open and I/O
    /// errors. The path is no longer a credential, but an error body is not a
    /// place to publish filesystem layout — and asserting on `/` is the version
    /// that cannot pass by accident, since every configurable path has one.
    #[test]
    fn db_error_never_carries_storage_details() {
        let err = db(sqlx::Error::PoolTimedOut);
        let text = err.to_string();
        assert!(text.contains("RMCP OAuth store query failed"));
        assert!(!text.contains('/'), "no path fragment: {text}");
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
            // …and it must be an IMMEDIATE transaction, so the authority read
            // is held for the rest of it. A helper reading outside one is a
            // point-in-time proof again, which is the shape this whole sprint
            // kept reopening.
            //
            // S132/RMCP-SQLITE made this assertion STRONGER, not merely
            // renamed. Under Postgres any transaction plus `FOR SHARE` sufficed
            // and this line only had to see a `begin()`. Under SQLite a bare
            // `pool.begin()` is DEFERRED: it would take a READ lock, another
            // writer could commit a demotion before this transaction's own
            // write, and the authorization decision would have been made
            // against the pre-demotion snapshot. That failure is invisible —
            // every statement still succeeds — so it is pinned here by
            // requiring the exact constructor that takes the write lock up
            // front. `every_transaction_in_this_module_is_immediate` extends the
            // same requirement to every other transaction in the file.
            assert!(
                body.contains("self.begin_immediate()"),
                "{function} must open its own BEGIN IMMEDIATE transaction (via \
                 `self.begin_immediate()`), so the authority it reads cannot be invalidated by \
                 another writer before this write commits. A deferred `pool.begin()` takes only \
                 a read lock and reintroduces exactly that TOCTOU."
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
            // "Under lock" is in this test's NAME, so what supplies the lock has
            // to be pinned or the name outlives the property. Postgres supplied
            // it with `FOR SHARE` on the rows `actor_authority` read; SQLite
            // supplies it with the write lock `BEGIN IMMEDIATE` takes. A
            // deferred `pool.begin()` would leave the re-verification reading a
            // snapshot another writer can still commit over — which is a
            // re-verification that verifies nothing, the exact failure the
            // round-2 finding was about.
            assert!(
                body.contains("self.begin_immediate()"),
                "fn {function} must open a BEGIN IMMEDIATE transaction; a deferred one takes \
                 only a read lock, so the re-verification could be invalidated before commit"
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

    // =======================================================================
    // S132/RMCP-SQLITE — the port's own invariants
    //
    // The first two are source guards, in the same tradition as the ones above
    // and for the same reason: they pin properties that hold across the WHOLE
    // module, where a per-call-site test would leave the next call site
    // uncovered. The rest are LIVE tests against a real database file — the
    // capability the port bought, and the reason the locking argument in the
    // module docs is proven rather than asserted.
    // =======================================================================

    /// **Every transaction in this module is `BEGIN IMMEDIATE`.**
    ///
    /// This is the single load-bearing invariant of the SQLite port. The row
    /// locks (`FOR SHARE`, `FOR UPDATE`) that made every authorization here
    /// TOCTOU-free were replaced by one mechanism: the database write lock,
    /// taken at BEGIN and held to COMMIT. That substitution is sound only while
    /// EVERY transaction takes it.
    ///
    /// A single `pool.begin()` would be a silent, total regression of the
    /// property, and it is worth being precise about why it is silent: a
    /// deferred transaction that reads an authority and then writes under it
    /// executes every statement successfully. Nothing errors. It simply made
    /// its decision against a snapshot another writer could commit over first —
    /// which is the original TOCTOU, restored, looking exactly like working
    /// code. That is this sprint's defect class in its purest form, so it is
    /// pinned by a machine rather than by review attention.
    ///
    /// Mutation-verify: change any `self.begin_immediate()` in this file to
    /// `self.pool.begin()` and this goes red naming the line.
    #[test]
    fn every_transaction_in_this_module_is_immediate() {
        let file = include_str!("store.rs");
        let production = file.split("\n#[cfg(test)]").next().expect("production half");

        // The ONE sanctioned constructor's own body is where `begin_with`
        // legitimately appears; everywhere else must go through it.
        let offenders: Vec<(usize, &str)> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//") && !line.trim_start().starts_with("///"))
            .filter(|(_, line)| line.contains(".begin()") || line.contains("pool.begin("))
            .map(|(i, line)| (i + 1, line.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "a transaction is opened without BEGIN IMMEDIATE, which silently reintroduces the \\
             read-then-write TOCTOU the row locks used to close — every statement still \\
             succeeds, so nothing else will catch this: {offenders:?}"
        );

        // Non-vacuity, in both directions. The scan must be looking at a file
        // that really does open transactions (otherwise an empty result proves
        // nothing), and the sanctioned constructor must really carry the
        // IMMEDIATE statement (otherwise every call site is compliant with a
        // helper that does the wrong thing).
        assert!(
            production.matches("self.begin_immediate()").count() >= 8,
            "the scan found almost no transactions; it is matching nothing and would pass \\
             whatever it was given"
        );
        assert_eq!(BEGIN_IMMEDIATE, "BEGIN IMMEDIATE");
        assert!(
            production.contains("begin_with(BEGIN_IMMEDIATE)"),
            "begin_immediate must open the transaction with the IMMEDIATE statement; without \\
             that every call site above is compliant with a helper that takes only a read lock"
        );
    }

    /// **No timestamp is ever BOUND from Rust.**
    ///
    /// `expires_at`, `issued_at`, `revoked_at` and friends are INTEGER
    /// unix-epoch columns written exclusively by SQLite's `unixepoch()`. Two
    /// separate rules depend on nothing in Rust ever writing one:
    ///
    /// 1. **The database's clock is the only clock.** A bound `Utc::now()`
    ///    would put the PROCESS clock into an expiry column, and a process
    ///    whose clock has drifted could then mint a code that outlives its TTL
    ///    or one that is born expired.
    /// 2. **One storage representation.** `sqlx` DECODES a `DateTime<Utc>` from
    ///    an INTEGER natively, but ENCODES one as RFC3339 TEXT. A bind would
    ///    therefore store TEXT into a column every comparison treats as an
    ///    integer, and SQLite — being dynamically typed — would accept it
    ///    silently and then compare it as a string against `unixepoch()`'s
    ///    number. In SQLite's type ordering integers sort BEFORE text, so such
    ///    a row would compare as later than every real timestamp: an expired
    ///    credential that never expires. Nothing would error.
    ///
    /// Mutation-verify: add `.bind(chrono::Utc::now())` to any query here and
    /// this goes red.
    #[test]
    fn no_timestamp_is_bound_from_rust() {
        let file = include_str!("store.rs");
        let production = file.split("\n#[cfg(test)]").next().expect("production half");
        let offenders: Vec<(usize, &str)> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//"))
            .filter(|(_, line)| {
                let l = line.trim();
                l.starts_with(".bind(")
                    && (l.contains("Utc::now") || l.contains("DateTime") || l.contains("Local::now"))
            })
            .map(|(i, line)| (i + 1, line.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "a timestamp is bound from Rust: that writes the PROCESS clock into a column the \\
             database clock owns, and stores TEXT where every comparison expects an INTEGER \\
             (which SQLite sorts AFTER every real timestamp, making the row never expire): \\
             {offenders:?}"
        );

        // Non-vacuity: the module must actually be writing timestamps, via the
        // database clock. If this count ever hits zero the guard above is
        // trivially satisfied and means nothing.
        assert!(
            production.matches("unixepoch()").count() >= 15,
            "the module no longer writes timestamps with the database clock"
        );
    }

    // -----------------------------------------------------------------------
    // Live database tests.
    //
    // Under Postgres this crate stood up no database at all, so the locking
    // guarantees could only be asserted by scanning source text for `FOR
    // SHARE`. SQLite makes a database a temporary file, so the substitution
    // argued in the module docs — that `BEGIN IMMEDIATE` subsumes every row
    // lock — is PROVEN below against real concurrent writers rather than
    // described.
    // -----------------------------------------------------------------------

    /// A fresh, migrated database in a temporary directory.
    ///
    /// Returns the `TempDir` alongside the store because dropping it deletes
    /// the file; a test that let it drop would run against a deleted database
    /// and, under SQLite's `unlink`-while-open semantics, still appear to work
    /// for a while. Keeping the guard alive in the caller is what stops that.
    async fn temp_store() -> (tempfile::TempDir, OauthStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = OauthStore::open_for_test(&dir.path().join("rmcp-test.db"))
            .await
            .expect("open and migrate");
        (dir, store)
    }

    /// Insert an account directly, returning its id. Used to build fixtures
    /// without going through the authorization paths under test.
    async fn seed_account(store: &OauthStore, name: &str, is_operator: bool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO rmcp_account (id, name, password_hash, is_operator) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(id)
        .bind(name)
        .bind("$argon2id$v=19$m=1,t=1,p=1$c29tZXNhbHQ$fixture")
        .bind(is_operator)
        .execute(&store.pool)
        .await
        .expect("seed account");
        id
    }

    /// The migration in the tree must actually apply to a real SQLite database,
    /// and `schema_ready` must then agree that it did.
    ///
    /// This is the test the Postgres version could not have: `schema_ready` was
    /// previously covered only by asserting its CONSTANT LISTS matched the
    /// migration text, which cannot catch a migration that does not parse, a
    /// table the readiness check spells differently from the DDL, or a column
    /// added to the wrong table.
    #[tokio::test]
    async fn the_migration_applies_and_the_schema_reports_ready() {
        let (_dir, store) = temp_store().await;
        assert!(store.schema_ready().await, "the shipped migration must satisfy schema_ready");
    }

    /// `schema_ready` must report NOT ready for a partially applied migration.
    ///
    /// The property round 1 asked for, now demonstrable: dropping ONE table
    /// from a fully migrated database is exactly the state an interrupted
    /// migration leaves, and a readiness check that still says "ready" for it
    /// is the confident-but-wrong answer the whole check exists to prevent.
    #[tokio::test]
    async fn a_partially_applied_migration_reports_not_ready() {
        let (_dir, store) = temp_store().await;
        assert!(store.schema_ready().await);
        sqlx::query("DROP TABLE rmcp_registration_token")
            .execute(&store.pool)
            .await
            .expect("drop");
        assert!(
            !store.schema_ready().await,
            "a missing table must report NOT ready, not merely fail later"
        );
    }

    /// The COLUMN half of the same rule, which the table check cannot see.
    #[tokio::test]
    async fn a_missing_operator_column_reports_not_ready() {
        let (_dir, store) = temp_store().await;
        sqlx::query("ALTER TABLE rmcp_account DROP COLUMN is_operator")
            .execute(&store.pool)
            .await
            .expect("drop column");
        assert!(
            !store.schema_ready().await,
            "`is_operator` is the only source of truth for who may write a bare `*`; a deploy \\
             missing it must report NOT ready rather than treating every account as delegated"
        );
    }

    /// **Foreign keys are enforced on every pooled connection.**
    ///
    /// SQLite defaults `PRAGMA foreign_keys` to OFF and applies it
    /// PER-CONNECTION, so this is not a schema property that can be assumed
    /// from the DDL — it is a property of how the pool was opened, and getting
    /// it wrong leaves every `ON DELETE CASCADE` in this schema silently inert
    /// while every query still succeeds.
    ///
    /// Asserted twice over, because the two halves fail differently: a rejected
    /// bad reference proves enforcement is on, and an observed CASCADE proves
    /// the behaviour the store's docs actually rely on ("deleting a group
    /// REVOKES it from every client that drew on it").
    ///
    /// Mutation-verify: remove `.foreign_keys(true)` from `open_for_test` and
    /// both halves go red.
    #[tokio::test]
    async fn foreign_keys_are_enforced_on_every_pooled_connection() {
        let (_dir, store) = temp_store().await;
        let owner = seed_account(&store, "fk-owner", true).await;

        // Half one: a dangling reference is refused.
        let orphan = sqlx::query(
            "INSERT INTO rmcp_tool_group (id, name, description, patterns, owner_account_id) \
             VALUES (?1, ?2, '', '[]', ?3)",
        )
        .bind(Uuid::new_v4())
        .bind("orphan-group")
        .bind(Uuid::new_v4()) // an account that does not exist
        .execute(&store.pool)
        .await;
        assert!(
            orphan.is_err(),
            "a tool group referencing a non-existent account was accepted, so foreign keys are \\
             OFF and every ON DELETE CASCADE in this schema is inert"
        );

        // Half two: the cascade the store's documented behaviour rests on.
        let group = store
            .insert_tool_group("cascade-group", "", &["weather_*".to_string()], owner)
            .await
            .expect("insert group");
        let client = store
            .insert_client(
                owner,
                "cascade-client",
                None,
                "cascade",
                &["https://example.invalid/cb".to_string()],
                &["authorization_code".to_string()],
                "none",
                owner,
                "operator",
            )
            .await
            .expect("insert client");
        store.set_client_tool_groups(owner, client, &[group]).await.expect("scope");
        assert_eq!(store.client_tool_groups(client).await.expect("read").len(), 1);

        store.delete_tool_group(owner, group).await.expect("delete group");
        assert!(
            store.client_tool_groups(client).await.expect("read").is_empty(),
            "deleting a group must REVOKE it from every client that drew on it; without the \\
             cascade the scope row survives and the client keeps a group nobody owns"
        );
    }

    /// **A concurrent demotion cannot interleave with the write it would have
    /// forbidden.**
    ///
    /// This is LOCK SITES 1–3, proven. Under Postgres the account row was held
    /// `FOR SHARE` for the life of the authorizing transaction; under SQLite
    /// the whole database write lock is. The interleaving that must be
    /// impossible is:
    ///
    /// 1. an operator's transaction reads `is_operator = true`;
    /// 2. a concurrent writer demotes that account and commits;
    /// 3. the first transaction commits a write it was only entitled to make
    ///    while it was an operator.
    ///
    /// The test forces the ordering by holding an open `BEGIN IMMEDIATE`
    /// transaction that has already read the authority, and attempting the
    /// demotion from a second connection with a busy timeout short enough to
    /// fail fast. The demotion must NOT succeed while that transaction is open.
    ///
    /// Mutation-verify: change `begin_immediate` to `self.pool.begin()` and the
    /// demotion lands mid-transaction, turning this red.
    #[tokio::test]
    async fn a_concurrent_demotion_cannot_interleave_with_an_authorized_write() {
        let (_dir, store) = temp_store().await;
        let operator = seed_account(&store, "race-operator", true).await;

        // Transaction A: opened IMMEDIATE, authority read, not yet committed —
        // exactly the window every authorized write in this module sits in.
        let mut tx = store.begin_immediate().await.expect("begin");
        let is_operator = OauthStore::locked_active_account(&mut tx, operator)
            .await
            .expect("read authority");
        assert!(is_operator, "fixture must start as an operator");

        // Transaction B, on a different pooled connection: the demotion.
        let demote = sqlx::query("UPDATE rmcp_account SET is_operator = 0 WHERE id = ?1")
            .bind(operator)
            .execute(&store.pool)
            .await;
        assert!(
            demote.is_err(),
            "a demotion committed while an authorizing transaction held the write lock and had \\
             already read that authority — the authorized write would then land on an account \\
             that is no longer an operator, which is precisely the TOCTOU `FOR SHARE` closed"
        );

        // And once A finishes, the demotion is free to proceed. Serialised, not
        // forbidden: the mechanism must ORDER writers, not deadlock them.
        tx.commit().await.expect("commit");
        sqlx::query("UPDATE rmcp_account SET is_operator = 0 WHERE id = ?1")
            .bind(operator)
            .execute(&store.pool)
            .await
            .expect("the demotion must succeed once the transaction has committed");
    }

    /// **A concurrent delegation clear cannot interleave with a namespace
    /// scoping write.**
    ///
    /// The other half of LOCK SITES 1–2: `actor_authority` read the
    /// `rmcp_server_owner` rows under `FOR SHARE` so `clear_server_owner` could
    /// not land between that read and the insert it authorizes, which would let
    /// a former owner attach a server they no longer own.
    #[tokio::test]
    async fn a_concurrent_delegation_clear_cannot_interleave_with_a_namespace_write() {
        let (_dir, store) = temp_store().await;
        let owner = seed_account(&store, "race-delegate", false).await;
        sqlx::query("INSERT INTO rmcp_server_owner (namespace, owner_account_id) VALUES (?1, ?2)")
            .bind("peerhub")
            .bind(owner)
            .execute(&store.pool)
            .await
            .expect("seed delegation");

        let mut tx = store.begin_immediate().await.expect("begin");
        let authority = OauthStore::actor_authority(&mut tx, owner).await.expect("authority");
        assert!(authority.owned().contains("peerhub"), "fixture must start with the delegation");

        let cleared = sqlx::query("DELETE FROM rmcp_server_owner WHERE namespace = ?1")
            .bind("peerhub")
            .execute(&store.pool)
            .await;
        assert!(
            cleared.is_err(),
            "a delegation was revoked while a transaction that had already read it was still \\
             open; the scoping write would then attach a namespace its owner no longer holds"
        );
        tx.commit().await.expect("commit");
    }

    /// **Two concurrent redemptions cannot double-spend a single-use
    /// registration token.**
    ///
    /// LOCK SITE 5, and the one whose REASONING changed rather than merely its
    /// spelling. Postgres relied on `FOR UPDATE` plus READ COMMITTED
    /// re-evaluating the `WHERE` after the lock was granted. SQLite has
    /// neither; the guarantee instead comes from the second redeemer's
    /// `BEGIN IMMEDIATE` not starting until the first has committed, after
    /// which it reads `uses_remaining = 0` and returns `None`.
    ///
    /// Because the argument is different, asserting it is not optional — this
    /// is exactly the site where carrying the old sentence over would have
    /// produced a claim with nothing behind it.
    #[tokio::test]
    async fn a_concurrent_redemption_cannot_double_spend_a_single_use_token() {
        let (_dir, store) = temp_store().await;
        let operator = seed_account(&store, "mint-operator", true).await;
        let token = SecretHash::of("a-single-use-registration-token");
        store
            .insert_registration_token(&token, operator, "one-off", 1, 600)
            .await
            .expect("mint");

        // Both redemptions issued concurrently against the same pool. Whatever
        // the scheduler does, the write lock orders them.
        let (a, b) = tokio::join!(
            store.claim_registration_token(&token),
            store.claim_registration_token(&token)
        );
        let spent = [a.expect("no error"), b.expect("no error")];
        let successes = spent.iter().filter(|r| r.is_some()).count();
        assert_eq!(
            successes, 1,
            "a single-use token was redeemed {successes} times; bounded use is what makes an \\
             initial access token an invitation rather than a standing one"
        );

        // And it stays spent.
        assert!(
            store.claim_registration_token(&token).await.expect("no error").is_none(),
            "a spent token must stay spent"
        );
    }

    /// **A demoted issuer's registration token stops working at REDEMPTION.**
    ///
    /// Round 3's finding, re-proven end to end now that a database is
    /// available: the token's own state (unspent, unexpired, unrevoked) is
    /// unchanged, and it must still be refused because the authority behind it
    /// is gone. A bearer credential is a read path.
    ///
    /// The refusal must also NOT consume a use — a token that was never
    /// spendable must not be burnable by anyone holding a copy.
    #[tokio::test]
    async fn a_demoted_issuers_registration_token_is_refused_and_not_consumed() {
        let (_dir, store) = temp_store().await;
        let operator = seed_account(&store, "demoted-issuer", true).await;
        let token = SecretHash::of("a-token-whose-issuer-gets-demoted");
        store.insert_registration_token(&token, operator, "before", 1, 600).await.expect("mint");

        sqlx::query("UPDATE rmcp_account SET is_operator = 0 WHERE id = ?1")
            .bind(operator)
            .execute(&store.pool)
            .await
            .expect("demote");

        assert!(
            store.claim_registration_token(&token).await.expect("no error").is_none(),
            "a token minted by an account that is no longer an operator must be refused"
        );

        // Not consumed: re-promote and it works again. This is what proves the
        // refusal was a refusal rather than a silent spend.
        sqlx::query("UPDATE rmcp_account SET is_operator = 1 WHERE id = ?1")
            .bind(operator)
            .execute(&store.pool)
            .await
            .expect("re-promote");
        assert!(
            store.claim_registration_token(&token).await.expect("no error").is_some(),
            "the refused presentation consumed a use, so anyone holding a copy could exhaust a \\
             legitimate token by presenting it during a demotion"
        );
    }

    /// **The login single-use claim admits exactly one caller.**
    ///
    /// RMCP-03's property. It is atomic across every connection to ONE FILE,
    /// which is what this test covers — and it is worth restating what it does
    /// NOT cover, because the port narrowed the scope of the guarantee: under
    /// Postgres this held across every replica sharing a database. It now holds
    /// across every writer of one file, so the door must be run single-writer.
    /// See the `rmcp_login_session_use` note in the migration.
    #[tokio::test]
    async fn a_login_session_can_only_be_claimed_once() {
        let (_dir, store) = temp_store().await;
        let jti = SecretHash::of("a-login-session-jti");
        let (a, b) =
            tokio::join!(store.claim_login_session(&jti, 300), store.claim_login_session(&jti, 300));
        let claims = [a.expect("no error"), b.expect("no error")];
        assert_eq!(
            claims.iter().filter(|c| **c).count(),
            1,
            "one authentication must yield at most ONE authorization code"
        );
    }

    /// **Expiry is judged against the database clock, in integer seconds.**
    ///
    /// The type-mapping decision, tested rather than assumed. A row written
    /// with an expiry in the past must not be honoured, and one in the future
    /// must be — which together catch the failure mode a TEXT column would have
    /// introduced (a string compared against `unixepoch()`'s number, which
    /// SQLite orders AFTER every integer, making every such row eternally
    /// live).
    #[tokio::test]
    async fn an_expired_row_is_never_honoured_and_a_live_one_is() {
        let (_dir, store) = temp_store().await;
        let operator = seed_account(&store, "clock-operator", true).await;

        let expired = SecretHash::of("an-already-expired-token");
        store.insert_registration_token(&expired, operator, "expired", 1, -60).await.expect("mint");
        assert!(
            store.claim_registration_token(&expired).await.expect("no error").is_none(),
            "an expired token was honoured; the expiry comparison is not working against the \\
             database clock"
        );

        let live = SecretHash::of("a-still-live-token");
        store.insert_registration_token(&live, operator, "live", 1, 600).await.expect("mint");
        assert!(
            store.claim_registration_token(&live).await.expect("no error").is_some(),
            "a live token was refused, so the guard above may be passing for the wrong reason"
        );
    }

    /// **A stored pattern list round-trips through the JSON column unchanged,
    /// and an oversized one is REFUSED rather than truncated.**
    ///
    /// The `text[]` decision. Truncating would silently resolve a DIFFERENT
    /// permission set from the one stored — the "looks the same, is weaker"
    /// substitution this item keeps finding — so the read path refuses the row,
    /// and a group that will not decode grants nothing.
    ///
    /// The oversized row is written by BYPASSING the store, which is the point:
    /// the file is now hand-editable, so the write gate is no longer the only
    /// way a row can come into existence.
    #[tokio::test]
    async fn a_pattern_list_round_trips_and_an_oversized_one_is_refused() {
        let (_dir, store) = temp_store().await;
        let owner = seed_account(&store, "list-owner", true).await;

        let patterns = vec!["weather_*".to_string(), "peerhub::*".to_string()];
        let group = store
            .insert_tool_group("round-trip", "a group", &patterns, owner)
            .await
            .expect("insert");
        let read = store.list_tool_groups(owner).await.expect("list");
        let stored = read.iter().find(|g| g.id == group).expect("the group");
        assert_eq!(stored.patterns, patterns, "the list must survive the JSON round trip intact");

        // Now forge an oversized row directly in the file, as a hand-edit would.
        let oversized: Vec<String> = (0..(crate::oauth::groups::MAX_PATTERNS_PER_GROUP + 1))
            .map(|i| format!("tool{i}_*"))
            .collect();
        sqlx::query("UPDATE rmcp_tool_group SET patterns = ?2 WHERE id = ?1")
            .bind(group)
            .bind(serde_json::to_string(&oversized).expect("encode"))
            .execute(&store.pool)
            .await
            .expect("forge");
        assert!(
            store.list_tool_groups(owner).await.is_err(),
            "an oversized pattern list decoded successfully; the RMCP-06 bound was a write-time \\
             check only, and the store is now a file that can be written without passing it"
        );
    }
}
