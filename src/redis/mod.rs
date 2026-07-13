//! BLD-20 — the shared, pooled Redis backend for terminus-primary.
//!
//! There is exactly ONE Redis instance in the constellation (provisioned by
//! `deploy/redis/*`, owned by terminus-primary, bound loopback + mesh only,
//! `requirepass` from the vault). This module is the single in-process door to
//! it: a pooled client with **typed namespaces** so the several unrelated
//! consumers cannot collide on keys, and so the durable keyspaces
//! (`queue:*`, `prefix:*`) can live in a different logical DB — with a
//! different eviction policy (`noeviction`) — from the volatile ones
//! (`ratelimit:*`, `sccache:*`, evicted `allkeys-lru`). See `deploy/redis/redis.conf`.
//!
//! # Endpoint & secrets (S1/S7)
//! The endpoint is read from `REDIS_URL` (with `PLANE_REDIS_URL` accepted as a
//! backward-compatible fallback — the pre-BLD-20 name the prefix overlay used).
//! The password is read from `REDIS_PASSWORD` (fallback `PLANE_REDIS_PASSWORD`)
//! and layered onto the connection info OUT of the URL so it never lands in a
//! log line. **Both are materialized from the vault into the process
//! environment at boot** (see `crate::secrets_bootstrap`) — this module never
//! contains a literal endpoint, host, port, or password, and none belongs in
//! any tracked file.
//!
//! # Fail-safe posture
//! Construction is infallible-ish: [`RedisBackend::from_env`] returns `None`
//! when Redis is simply not configured (the whole feature degrades — see each
//! consumer's own fail-open/fail-closed rule). Once configured, every op is
//! timeout-bounded; a per-op failure surfaces as `Err` and the CALLER decides
//! how to degrade (the rate-limiter fails CLOSED, sccache/overlay fail OPEN).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ::redis::aio::ConnectionManager;
use ::redis::IntoConnectionInfo;
use tokio::sync::OnceCell;
use tracing::warn;

/// Default per-op Redis timeout (ms); overridable via `REDIS_TIMEOUT_MS`.
/// Kept short — every consumer treats a slow Redis as an unreachable one and
/// degrades rather than blocking a request or a build.
const DEFAULT_TIMEOUT_MS: u64 = 200;

/// Default logical DB holding the DURABLE keyspaces (`queue:*`, `prefix:*`).
/// Server-side this DB is configured `noeviction` (see `redis.conf`) so a
/// memory-pressure eviction can never drop a queued build or a prefix claim.
const DEFAULT_DB_DURABLE: i64 = 0;

/// Default logical DB holding the VOLATILE keyspaces (`ratelimit:*`,
/// `sccache:*`). Server-side this DB is `allkeys-lru`: a rate-limit counter or
/// a stale sccache index entry is safe to evict under pressure.
const DEFAULT_DB_VOLATILE: i64 = 1;

/// The typed namespaces sharing the one Redis. Each maps to (a) a key prefix so
/// consumers never collide, and (b) a logical DB (durable vs volatile) so the
/// server-side eviction policy protects the right keys.
///
/// A namespace is the ONLY sanctioned way to form a key: call [`Namespace::key`]
/// — never hand-concatenate a bare string — so the prefix + DB invariants hold
/// fleet-wide.
///
/// SCOPE / consumer ownership (intentional decomposition): BLD-20 PROVIDES this
/// shared pool + typed namespaces; it does NOT wire every consumer. The
/// `Sccache` and `Queue` namespaces are reserved here as the shared backend for
/// two SEPARATE downstream items — `Sccache` for the sccache shared compile
/// cache (BLD-05/BLD-03) and `Queue` for the durable compiler job queue /
/// scheduler (BLD-06). Those consumers land in THOSE items, not here; the only
/// consumers BLD-20 itself wires are the proxy rate-limiter/admission queue
/// (`Ratelimit`) and the Plane prefix overlay (`Prefix`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Namespace {
    /// sccache shared compile-cache index. Volatile. **Consumer: BLD-05/BLD-03**
    /// (the sccache backend) — reserved here, wired in that item, not BLD-20.
    Sccache,
    /// Compiler job-queue / scheduler state. Durable (DB `noeviction`).
    /// **Consumer: BLD-06** (the compiler queue) — reserved here, wired in that
    /// item, not BLD-20. This is the DURABLE job queue, distinct from the
    /// ephemeral proxy admission queue under `Ratelimit`.
    Queue,
    /// Plane prefix-registry overlay (durable cross-instance claims). Wired by BLD-20.
    Prefix,
    /// Proxy rate-limit counters + (ephemeral) request-admission queue metadata.
    /// Volatile. Wired by BLD-20.
    Ratelimit,
}

impl Namespace {
    /// The colon-terminated key prefix for this namespace (e.g. `"queue:"`).
    pub fn prefix(self) -> &'static str {
        match self {
            Namespace::Sccache => "sccache:",
            Namespace::Queue => "queue:",
            Namespace::Prefix => "prefix:",
            Namespace::Ratelimit => "ratelimit:",
        }
    }

    /// Whether this namespace is durable (must NOT be evicted). Drives which
    /// logical DB it lands in.
    pub fn is_durable(self) -> bool {
        matches!(self, Namespace::Queue | Namespace::Prefix)
    }

    /// Build a fully-qualified key: `"{prefix}{suffix}"`. The one sanctioned
    /// key constructor for this namespace.
    pub fn key(self, suffix: &str) -> String {
        format!("{}{}", self.prefix(), suffix)
    }
}

/// Resolve the Redis endpoint URL from the environment: `REDIS_URL`, else the
/// legacy `PLANE_REDIS_URL`. `None`/empty => Redis not configured. The value is
/// materialized from the vault at boot (never a literal — S1/S7).
pub fn resolve_url() -> Option<String> {
    env_nonempty("REDIS_URL").or_else(|| env_nonempty("PLANE_REDIS_URL"))
}

/// Resolve the Redis password from the environment: `REDIS_PASSWORD`, else the
/// legacy `PLANE_REDIS_PASSWORD`. Kept out of the URL so it never logs.
pub fn resolve_password() -> Option<String> {
    env_nonempty("REDIS_PASSWORD").or_else(|| env_nonempty("PLANE_REDIS_PASSWORD"))
}

/// Resolve the per-op timeout from `REDIS_TIMEOUT_MS` (fallback: the legacy
/// `PLANE_REDIS_TIMEOUT_MS`), clamped to `>= 1ms`.
pub fn resolve_timeout() -> Duration {
    let ms = env_nonempty("REDIS_TIMEOUT_MS")
        .or_else(|| env_nonempty("PLANE_REDIS_TIMEOUT_MS"))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .max(1);
    Duration::from_millis(ms)
}

/// Resolve a logical DB index override from env, else the given default.
fn resolve_db(key: &str, default: i64) -> i64 {
    env_nonempty(key)
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n >= 0)
        .unwrap_or(default)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// A pooled connection to ONE logical DB. `ConnectionManager` is redis-rs's
/// cloneable, auto-reconnecting multiplexed connection — i.e. the pool. Built
/// lazily on first use so an unreachable Redis never fails construction.
struct DbPool {
    client: ::redis::Client,
    conn: OnceCell<ConnectionManager>,
}

impl DbPool {
    /// Build the client for `url` with `password` layered on and the logical
    /// `db` selected. `None` if the URL is unparseable or the client cannot be
    /// constructed (logged once; caller degrades).
    fn new(url: &str, password: Option<&str>, db: i64) -> Option<Self> {
        let mut info = match url.into_connection_info() {
            Ok(i) => i,
            Err(e) => {
                warn!(
                    "REDIS_URL not a valid Redis URL ({:?}); Redis backend disabled",
                    e.kind()
                );
                return None;
            }
        };
        if let Some(pw) = password {
            info.redis.password = Some(pw.to_string());
        }
        info.redis.db = db;
        match ::redis::Client::open(info) {
            Ok(client) => Some(Self {
                client,
                conn: OnceCell::new(),
            }),
            Err(e) => {
                warn!("failed to build Redis client ({:?}); backend disabled", e.kind());
                None
            }
        }
    }

    /// A live pooled connection, or `None` if Redis is unreachable right now.
    async fn conn(&self) -> Option<ConnectionManager> {
        match self
            .conn
            .get_or_try_init(|| ConnectionManager::new(self.client.clone()))
            .await
        {
            Ok(m) => Some(m.clone()),
            Err(_) => None,
        }
    }
}

/// The shared Redis backend: one pooled connection per logical DB, addressed by
/// [`Namespace`]. Cheap to clone (`Arc`-internally); construct once at boot and
/// share `Arc<RedisBackend>` across consumers.
pub struct RedisBackend {
    pools: HashMap<i64, Arc<DbPool>>,
    db_durable: i64,
    db_volatile: i64,
    op_timeout: Duration,
}

impl std::fmt::Debug for RedisBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the client (its ConnectionInfo carries the password).
        f.debug_struct("RedisBackend")
            .field("db_durable", &self.db_durable)
            .field("db_volatile", &self.db_volatile)
            .field("op_timeout", &self.op_timeout)
            .finish()
    }
}

impl RedisBackend {
    /// Build from the environment. Returns `None` when Redis is not configured
    /// (`REDIS_URL`/`PLANE_REDIS_URL` unset) — the fully-degraded path each
    /// consumer already tolerates.
    pub fn from_env() -> Option<Arc<Self>> {
        let url = resolve_url()?;
        let password = resolve_password();
        let db_durable = resolve_db("REDIS_DB_DURABLE", DEFAULT_DB_DURABLE);
        let db_volatile = resolve_db("REDIS_DB_VOLATILE", DEFAULT_DB_VOLATILE);
        Self::build(&url, password.as_deref(), db_durable, db_volatile, resolve_timeout())
    }

    /// Shared constructor (also the test entry point). Builds one pool per
    /// distinct logical DB. `None` if no pool could be constructed.
    pub fn build(
        url: &str,
        password: Option<&str>,
        db_durable: i64,
        db_volatile: i64,
        op_timeout: Duration,
    ) -> Option<Arc<Self>> {
        let mut pools: HashMap<i64, Arc<DbPool>> = HashMap::new();
        for db in [db_durable, db_volatile] {
            if let std::collections::hash_map::Entry::Vacant(slot) = pools.entry(db) {
                let pool = DbPool::new(url, password, db)?;
                slot.insert(Arc::new(pool));
            }
        }
        Some(Arc::new(Self {
            pools,
            db_durable,
            db_volatile,
            op_timeout,
        }))
    }

    /// The logical DB index a namespace resolves to.
    pub fn db_for(&self, ns: Namespace) -> i64 {
        if ns.is_durable() {
            self.db_durable
        } else {
            self.db_volatile
        }
    }

    /// The configured per-op timeout.
    pub fn timeout(&self) -> Duration {
        self.op_timeout
    }

    fn pool_for(&self, ns: Namespace) -> Option<&Arc<DbPool>> {
        self.pools.get(&self.db_for(ns))
    }

    /// A live pooled connection into the DB backing `ns`, or `None` if Redis is
    /// unreachable. Callers wrap their op in [`RedisBackend::timeout`].
    pub async fn conn(&self, ns: Namespace) -> Option<ConnectionManager> {
        self.pool_for(ns)?.conn().await
    }

    /// A timeout-bounded round-trip for a namespaced consumer. `f` receives a
    /// pooled connection; **both the connection acquisition AND the op** are
    /// bounded by [`RedisBackend::timeout`] — a hung connect can never block
    /// past the deadline, so a Redis-unreachable condition always degrades
    /// within the op timeout (the fail-open/fail-closed edge cases all rely on
    /// this bound). `Err(())` means "unreachable / timed out / failed" — the
    /// caller degrades.
    pub async fn with_conn<T, F, Fut>(&self, ns: Namespace, f: F) -> Result<T, ()>
    where
        F: FnOnce(ConnectionManager) -> Fut,
        Fut: std::future::Future<Output = ::redis::RedisResult<T>>,
    {
        let op = async {
            // Connection resolution (which itself may hang against a dead Redis)
            // is INSIDE the timeout below.
            let conn = self.conn(ns).await.ok_or(())?;
            f(conn).await.map_err(|_| ())
        };
        match tokio::time::timeout(self.op_timeout, op).await {
            Ok(res) => res,
            Err(_) => Err(()),
        }
    }

    /// Liveness probe: `PING` the durable DB. `true` iff Redis answered within
    /// the op timeout. Used by health checks / `compiler_status`.
    pub async fn ping(&self) -> bool {
        self.with_conn(Namespace::Queue, |mut c| async move {
            ::redis::cmd("PING").query_async::<_, String>(&mut c).await
        })
        .await
        .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_prefixes_are_distinct_and_colon_terminated() {
        let all = [
            Namespace::Sccache,
            Namespace::Queue,
            Namespace::Prefix,
            Namespace::Ratelimit,
        ];
        for ns in all {
            assert!(ns.prefix().ends_with(':'), "{:?} prefix must end with ':'", ns);
        }
        // Pairwise distinct prefixes → no cross-namespace key collision.
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.prefix(), b.prefix(), "{:?} vs {:?} share a prefix", a, b);
            }
        }
    }

    #[test]
    fn key_is_prefix_plus_suffix() {
        assert_eq!(Namespace::Queue.key("build:42"), "queue:build:42");
        assert_eq!(Namespace::Ratelimit.key("dev-box:ledger"), "ratelimit:dev-box:ledger");
        assert_eq!(Namespace::Prefix.key("overlay:v1"), "prefix:overlay:v1");
        assert_eq!(Namespace::Sccache.key("stats"), "sccache:stats");
    }

    #[test]
    fn durable_and_volatile_split() {
        assert!(Namespace::Queue.is_durable());
        assert!(Namespace::Prefix.is_durable());
        assert!(!Namespace::Ratelimit.is_durable());
        assert!(!Namespace::Sccache.is_durable());
    }

    #[test]
    fn durable_and_volatile_land_in_different_dbs() {
        // Build against a syntactically valid URL (no connection is made until
        // first use, so this stays offline).
        let backend = RedisBackend::build(
            "redis://127.0.0.1:6379",
            Some("unused-in-offline-test"),
            0,
            1,
            Duration::from_millis(200),
        )
        .expect("client construction is offline and must succeed");
        assert_eq!(backend.db_for(Namespace::Queue), 0);
        assert_eq!(backend.db_for(Namespace::Prefix), 0);
        assert_eq!(backend.db_for(Namespace::Ratelimit), 1);
        assert_eq!(backend.db_for(Namespace::Sccache), 1);
        // Two distinct DBs → two pools.
        assert_eq!(backend.pools.len(), 2);
    }

    #[test]
    fn same_db_for_durable_and_volatile_collapses_to_one_pool() {
        let backend =
            RedisBackend::build("redis://127.0.0.1:6379", None, 0, 0, Duration::from_millis(200))
                .expect("offline construction");
        assert_eq!(backend.pools.len(), 1, "identical DB indices share one pool");
    }

    #[test]
    fn invalid_url_yields_none() {
        assert!(
            RedisBackend::build("not a url", None, 0, 1, Duration::from_millis(200)).is_none(),
            "an unparseable URL must degrade to None, not panic"
        );
    }

    #[test]
    fn debug_never_leaks_password() {
        let backend =
            RedisBackend::build("redis://127.0.0.1:6379", Some("s3cr3t"), 0, 1, Duration::from_millis(200))
                .expect("offline construction");
        let rendered = format!("{backend:?}");
        assert!(!rendered.contains("s3cr3t"), "Debug must not print the password");
    }
}
