//! Plane CE tool implementations (CHORD-06, hardened per the plane-helper port).
//!
//! Provides 28 Rust tools that wrap the Plane CE REST API via reqwest.
//! All configuration comes from environment variables — no hardcoded URLs or tokens.
//!
//! ## Configuration
//! - `PLANE_API_URL` — base URL of the Plane CE instance (required at call time)
//! - `PLANE_API_KEY` — default API key/token for authentication (required at call time)
//! - `PLANE_PAT_<NAME>` — additional named identities (e.g. `PLANE_PAT_CLAUDE`),
//!   see "Multi-identity" below
//! - `PLANE_IDENTITY_NAME` — human name for the default `PLANE_API_KEY` identity
//! - `PLANE_WORKSPACE` — workspace slug (default: "moosenet")
//! - `PLANE_RPM` / `PLANE_RATE_SHARE` — proactive pacing, default 60 RPM / share of 3
//!   (60/3 = 20 effective RPM = 3s minimum interval between requests, shared across
//!   every tool call via a single rate limiter)
//! - `PLANE_CACHE_TTL_SECS` — GET response cache TTL, default 5s
//!
//! ### Optional shared Redis backend
//! When `PLANE_REDIS_URL` (e.g. `redis://host:port/0`) is set, the GET cache AND
//! the rate limiter are coordinated through one shared Redis instance, so every
//! terminus process that talks to Plane shares a single cache and a single
//! coordinated rate budget instead of each keeping a private in-process copy.
//! This is robustly fail-open: every Redis op is short-timeout-bounded and
//! circuit-breaker-guarded, and on any Redis error/timeout/outage the call
//! transparently falls back to the in-process cache/limiter — Redis being down
//! never blocks or fails a Plane call. When `PLANE_REDIS_URL` is unset/empty the
//! behaviour is identical to a pure in-process cache + limiter (the default).
//! - `PLANE_REDIS_URL` — Redis endpoint; unset/empty disables the shared backend
//! - `PLANE_REDIS_PASSWORD` — optional AUTH password (kept out of the URL)
//! - `PLANE_REDIS_TIMEOUT_MS` — per-op Redis timeout, default 200ms
//!
//! When `PLANE_API_URL` is not set the tools register normally but return
//! `ToolError::NotConfigured` on every call.
//!
//! ## Multi-identity
//! This is a *replacement*, not a port, of the Python `plane_client.py`
//! `whoami()` design, which resolved identity by scanning other agents'
//! plaintext `.env` files for a matching token substring — a credential-sprawl
//! anti-pattern. Instead, named identities are configured explicitly via
//! `PLANE_PAT_<NAME>` secrets (injected into this process's environment at
//! start by the operator's secret manager, never read from another process's
//! files at call time). [`PlaneClient::for_identity`]
//! returns a clone of the client scoped to a named identity's token, sharing the
//! HTTP client, rate limiter, and GET cache. [`PlaneWhoami`] (`plane_whoami`)
//! reports the active identity, or resolves whether a named identity is configured.
//!
//! ### Acting as an identity
//! Every Plane CRUD tool accepts an OPTIONAL `identity` string argument. When
//! present, that call is authenticated as the matching `PLANE_PAT_<NAME>`
//! identity via [`PlaneClient::resolve_identity`] → [`PlaneClient::for_identity`];
//! when omitted, the call acts as the **active default** identity. The active
//! default is resolved once at construction ([`PlaneClient::from_env`]): if
//! `PLANE_IDENTITY_NAME` names a configured `PLANE_PAT_<NAME>` identity, the
//! default token IS that identity's token (so e.g. `PLANE_IDENTITY_NAME=lumina`
//! genuinely routes to `PLANE_PAT_LUMINA`); otherwise it falls back to the
//! unsuffixed `PLANE_API_KEY`, preserving backward compatibility for
//! deployments that configure only `PLANE_API_KEY`. `plane_whoami` can
//! additionally perform a real authenticated read (`verify: true`) to prove a
//! given identity's token is currently accepted (200) versus rejected (401/403).

pub mod types;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::IntoConnectionInfo;
use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use types::*;

/// True if `s` is a canonical 8-4-4-4-12 hyphenated UUID. // pii-test-fixture
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

// ─── Shared Redis backend (optional, fail-open) ──────────────────────────────
//
// When `PLANE_REDIS_URL` is set, the GET cache and the rate limiter below both
// coordinate through ONE shared Redis instance so that every terminus process
// talking to Plane (e.g. the standalone personal server and the proxy-embedded
// registry on the GPU host) shares a single cache and a single coordinated rate
// budget, instead of each keeping its own private in-process copy.
//
// Robustness is the whole point: every Redis operation is wrapped in a short
// timeout and guarded by a circuit breaker. On ANY error / timeout / breaker-open
// state the backend returns "not available" and the caller transparently falls
// back to its in-process cache / limiter for that one operation — a Redis outage
// NEVER blocks, slows (beyond one short timeout), or fails a Plane call. When
// Redis recovers the breaker half-opens, a probe succeeds, and coordination
// resumes with no restart.

/// Consecutive Redis-op failures that trip the circuit breaker open.
const REDIS_FAILURE_THRESHOLD: u32 = 3;
/// How long the breaker stays open before allowing a single half-open probe.
const REDIS_BREAKER_COOLDOWN: Duration = Duration::from_secs(5);
/// Default per-op Redis timeout (ms) — over/underridable via `PLANE_REDIS_TIMEOUT_MS`.
const REDIS_DEFAULT_TIMEOUT_MS: u64 = 200;

/// Atomic distributed rate-limiter reservation. Uses the Redis server's own
/// clock (`TIME`) so all instances agree on "now" regardless of client clock
/// skew, then reserves the next monotonically-increasing slot spaced by
/// `min_interval`. Returns how many milliseconds THIS caller must wait before
/// issuing its request. One coordinated budget, shared across every instance
/// and every identity — exactly the single-gate semantics the in-process
/// limiter has, lifted to the whole fleet.
const RATE_RESERVE_LUA: &str = r#"
local t = redis.call('TIME')
local now = (tonumber(t[1]) * 1000) + math.floor(tonumber(t[2]) / 1000)
local interval = tonumber(ARGV[1])
local buffer = tonumber(ARGV[2])
local last = tonumber(redis.call('GET', KEYS[1]) or '0')
local slot = now
if last + interval > now then slot = last + interval end
-- Key TTL must outlive the FULL reserved backlog (slot may be many intervals
-- ahead under a burst), else the key could expire before the last reserved slot
-- is reached and a later caller would see no prior slot and release too soon.
local ttl = (slot - now) + buffer
redis.call('SET', KEYS[1], slot, 'PX', ttl)
local wait = slot - now
if wait < 0 then wait = 0 end
return wait
"#;

/// A lightweight failure-tracking gate so a Redis outage doesn't mean paying a
/// timeout on every single call. Closed → attempts proceed. After
/// `REDIS_FAILURE_THRESHOLD` consecutive failures it opens for
/// `REDIS_BREAKER_COOLDOWN`, during which attempts are skipped outright (instant
/// fall-back to in-process). After the cooldown one half-open probe is allowed;
/// a success closes it, a failure re-opens it. Exactly one degradation warning
/// is logged per outage episode (reset on the next success), never one per call.
#[derive(Debug)]
struct CircuitBreaker {
    inner: StdMutex<BreakerInner>,
}

#[derive(Debug)]
struct BreakerInner {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    warned: bool,
}

impl CircuitBreaker {
    fn new() -> Self {
        Self { inner: StdMutex::new(BreakerInner { consecutive_failures: 0, open_until: None, warned: false }) }
    }

    /// True if a Redis op should be attempted now. When the cooldown has
    /// elapsed the breaker half-opens for exactly ONE probe: this caller
    /// *reserves* the probe by pushing `open_until` forward (so concurrent
    /// callers keep seeing it open and fall back instead of a thundering herd),
    /// and the probe's own `record_success`/`record_failure` then closes or
    /// re-opens the breaker.
    fn allow(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        match g.open_until {
            Some(t) if Instant::now() < t => false,
            Some(_) => {
                // Cooldown elapsed: reserve a single half-open probe for THIS
                // caller. Re-arm the window so any concurrent caller still sees
                // an open breaker until this probe records its result.
                g.open_until = Some(Instant::now() + REDIS_BREAKER_COOLDOWN);
                true
            }
            None => true,
        }
    }

    fn record_success(&self) {
        let mut g = self.inner.lock().unwrap();
        g.consecutive_failures = 0;
        g.open_until = None;
        g.warned = false;
    }

    /// Record a failure. Returns true exactly once per outage episode — when the
    /// breaker first trips open — so the caller emits a SINGLE degradation warning.
    fn record_failure(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        g.consecutive_failures = g.consecutive_failures.saturating_add(1);
        let tripped = g.consecutive_failures >= REDIS_FAILURE_THRESHOLD;
        if tripped {
            g.open_until = Some(Instant::now() + REDIS_BREAKER_COOLDOWN);
        }
        if tripped && !g.warned {
            g.warned = true;
            true
        } else {
            false
        }
    }

    /// Test hook: force the open cooldown to have already elapsed so half-open
    /// behaviour can be exercised deterministically without a real 5s wait.
    #[cfg(test)]
    fn test_expire_cooldown(&self) {
        let mut g = self.inner.lock().unwrap();
        if g.open_until.is_some() {
            g.open_until = Some(Instant::now() - Duration::from_millis(1));
        }
    }
}

/// Shared, optional Redis backend for the Plane GET cache + rate limiter.
/// Constructed once (via [`RedisBackend::from_env`]) and shared by `Arc` between
/// the `GetCache` and `RateLimiter` on a `PlaneClient`, so a single breaker
/// governs both and a single connection is multiplexed across all Plane traffic.
struct RedisBackend {
    /// Parsed client (holds the connection target + optional password). Opening
    /// it does not connect; the async connection is established lazily below.
    client: redis::Client,
    /// Lazily-initialised multiplexed connection manager (auto-reconnecting).
    /// Built on first use so construction stays synchronous and a Redis that is
    /// down at startup never blocks boot.
    conn: OnceCell<ConnectionManager>,
    /// Per-op timeout; a hung Redis can never stall a Plane call longer than this.
    op_timeout: Duration,
    breaker: CircuitBreaker,
    /// Key namespace prefix (e.g. `plane:`) so Plane keys never collide with any
    /// other tenant of the same Redis.
    key_prefix: String,
    /// Pre-built reservation script (SHA computed once, EVALSHA on the hot path).
    rate_script: redis::Script,
}

/// Hand-written `Debug`: never prints `client` (redis::Client's own `Debug`
/// includes the ConnectionInfo, which can carry the Redis password) or any
/// live connection. Only inert config is shown.
impl std::fmt::Debug for RedisBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisBackend")
            .field("op_timeout", &self.op_timeout)
            .field("key_prefix", &self.key_prefix)
            .finish_non_exhaustive()
    }
}

impl RedisBackend {
    /// Build from `PLANE_REDIS_URL` (+ optional `PLANE_REDIS_PASSWORD`,
    /// `PLANE_REDIS_TIMEOUT_MS`). Returns `None` when `PLANE_REDIS_URL` is
    /// unset/empty (→ pure in-process behaviour, identical to before this
    /// backend existed) or when the URL is unparseable (logged once, then the
    /// same in-process fallback). Never panics, never blocks on the network.
    fn from_env() -> Option<Arc<Self>> {
        let url = std::env::var("PLANE_REDIS_URL").ok().filter(|v| !v.trim().is_empty())?;
        let password = std::env::var("PLANE_REDIS_PASSWORD").ok().filter(|v| !v.is_empty());

        // Parse the URL, then layer the password from its own env var (kept out
        // of the URL so it never lands in a log line or process listing).
        let mut info = match url.as_str().into_connection_info() {
            Ok(i) => i,
            Err(e) => {
                warn!(
                    "PLANE_REDIS_URL is set but not a valid Redis URL ({:?}); Plane cache + rate limiter stay in-process",
                    e.kind()
                );
                return None;
            }
        };
        if let Some(pw) = password {
            info.redis.password = Some(pw);
        }
        let client = match redis::Client::open(info) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Failed to construct Plane Redis client ({:?}); cache + rate limiter stay in-process",
                    e.kind()
                );
                return None;
            }
        };

        let timeout_ms: u64 = std::env::var("PLANE_REDIS_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(REDIS_DEFAULT_TIMEOUT_MS)
            .max(1);

        Some(Arc::new(Self {
            client,
            conn: OnceCell::new(),
            op_timeout: Duration::from_millis(timeout_ms),
            breaker: CircuitBreaker::new(),
            key_prefix: "plane:".to_string(),
            rate_script: redis::Script::new(RATE_RESERVE_LUA),
        }))
    }

    /// Obtain a cloned connection manager, initialising it on first use.
    /// `ConnectionManager` is cheap to clone (internally `Arc`-shared) and
    /// reconnects on its own; a failed init is not cached, so a later call
    /// retries once Redis is reachable. Returns `None` on init failure.
    async fn conn(&self) -> Option<ConnectionManager> {
        let init = self
            .conn
            .get_or_try_init(|| ConnectionManager::new(self.client.clone()))
            .await;
        match init {
            Ok(m) => Some(m.clone()),
            Err(_) => None,
        }
    }

    /// Log a single degradation warning if this failure tripped the breaker.
    /// Never includes the error value/target — only that Redis is degraded — so
    /// no connection string or credential can leak into logs.
    fn note_failure(&self) {
        if self.breaker.record_failure() {
            warn!(
                "Plane Redis backend degraded; falling back to in-process cache + rate limiter until it recovers"
            );
        }
    }

    /// GET a namespaced cache key. `None` = not present OR Redis unavailable —
    /// the caller treats both identically (fall through to in-process).
    async fn cache_get(&self, key: &str) -> Option<String> {
        if !self.breaker.allow() {
            return None;
        }
        let fut = async {
            let mut conn = self
                .conn()
                .await
                .ok_or_else(|| redis::RedisError::from((redis::ErrorKind::IoError, "redis unavailable")))?;
            redis::cmd("GET").arg(key).query_async::<_, Option<String>>(&mut conn).await
        };
        match tokio::time::timeout(self.op_timeout, fut).await {
            Ok(Ok(v)) => {
                self.breaker.record_success();
                v
            }
            _ => {
                self.note_failure();
                None
            }
        }
    }

    /// SET a namespaced cache key with a millisecond TTL. Best-effort: a failure
    /// is swallowed (logged once via the breaker) — the in-process cache still
    /// holds the value, so a failed Redis write never affects correctness.
    async fn cache_set(&self, key: &str, body: &str, ttl: Duration) {
        if !self.breaker.allow() {
            return;
        }
        let ttl_ms = (ttl.as_millis() as u64).max(1);
        let fut = async {
            let mut conn = self
                .conn()
                .await
                .ok_or_else(|| redis::RedisError::from((redis::ErrorKind::IoError, "redis unavailable")))?;
            redis::cmd("SET")
                .arg(key)
                .arg(body)
                .arg("PX")
                .arg(ttl_ms)
                .query_async::<_, ()>(&mut conn)
                .await
        };
        match tokio::time::timeout(self.op_timeout, fut).await {
            Ok(Ok(())) => self.breaker.record_success(),
            _ => self.note_failure(),
        }
    }

    /// Atomically reserve the next slot in the shared rate budget. `Some(wait)` =
    /// sleep `wait` then proceed; `None` = Redis unavailable, caller should fall
    /// back to its in-process limiter.
    async fn rate_reserve(&self, min_interval: Duration) -> Option<Duration> {
        if !self.breaker.allow() {
            return None;
        }
        let interval_ms = min_interval.as_millis() as i64;
        // Buffer added ON TOP of the reserved backlog (slot - now) inside the
        // script, so an idle key still self-expires cleanly (a missing key =
        // "no prior slot") without ever expiring mid-backlog under a burst.
        let ttl_buffer_ms = (interval_ms.saturating_mul(4)).max(1000);
        let key = format!("{}ratelimit:global", self.key_prefix);
        let fut = async {
            let mut conn = self
                .conn()
                .await
                .ok_or_else(|| redis::RedisError::from((redis::ErrorKind::IoError, "redis unavailable")))?;
            self.rate_script
                .key(key)
                .arg(interval_ms)
                .arg(ttl_buffer_ms)
                .invoke_async::<_, i64>(&mut conn)
                .await
        };
        match tokio::time::timeout(self.op_timeout, fut).await {
            Ok(Ok(wait_ms)) => {
                self.breaker.record_success();
                Some(Duration::from_millis(wait_ms.max(0) as u64))
            }
            _ => {
                self.note_failure();
                None
            }
        }
    }
}

#[cfg(test)]
impl RedisBackend {
    /// A backend pointed at an address that refuses connections (port 1), used
    /// to exercise the fail-open path deterministically without a live Redis.
    /// Short op timeout so the tests stay fast.
    fn test_unreachable() -> Arc<Self> {
        let client = redis::Client::open("redis://localhost:1/0").expect("valid url");
        Arc::new(Self {
            client,
            conn: OnceCell::new(),
            op_timeout: Duration::from_millis(120),
            breaker: CircuitBreaker::new(),
            key_prefix: "plane:".to_string(),
            rate_script: redis::Script::new(RATE_RESERVE_LUA),
        })
    }
}

/// Namespace + hash a raw cache key (`token\0url`) into a Redis key. Hashing
/// keeps the raw token OUT of Redis keys entirely (no credential material in
/// key space, bounded key length) while preserving per-active-token isolation:
/// two identities requesting the same URL hash to different keys, so a shared
/// Redis cache can never serve one identity another's response. Uses a stable
/// SHA-1 digest (fixed algorithm, identical output across Rust versions/builds)
/// so processes on different builds sharing one Redis map the same token+URL to
/// the same key — a `std` `DefaultHasher` is explicitly NOT stable across
/// versions and would silently fragment the shared cache during rolling upgrades.
fn redis_cache_key(raw: &str) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(raw.as_bytes());
    format!("plane:cache:{}", h.digest())
}

// ─── Rate limiter (in-process, optionally Redis-coordinated) ─────────────────
//
// Replaces the Python client's `fcntl.flock`-guarded `/tmp/plane-helper.lock` +
// `/tmp/plane-helper.last` pacing. Every call — across every tool, every
// identity — passes through the same gate. When a shared `RedisBackend` is
// configured the gate is the fleet-wide reservation slot (so BOTH terminus
// instances pace against ONE coordinated budget); otherwise, and on any Redis
// failure, it is the local `min_interval`-since-last-call gate.

#[derive(Debug)]
struct RateLimiter {
    last: AsyncMutex<Option<Instant>>,
    min_interval: Duration,
    /// Shared distributed backend; `None` = purely in-process (the default and
    /// the fail-open fallback).
    redis: Option<Arc<RedisBackend>>,
}

impl RateLimiter {
    /// Build from `PLANE_RPM` / `PLANE_RATE_SHARE` (defaults: 60 / 3, i.e. a
    /// 3-second minimum interval), matching the Python client's env-var names.
    /// `redis` (when `Some`) makes the budget fleet-wide; otherwise pacing is
    /// per-process.
    fn from_env(redis: Option<Arc<RedisBackend>>) -> Self {
        let rpm: f64 = std::env::var("PLANE_RPM").ok().and_then(|v| v.parse().ok()).unwrap_or(60.0);
        let share: f64 = std::env::var("PLANE_RATE_SHARE").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
        let effective_rpm = if share > 0.0 { rpm / share } else { rpm };
        let min_interval = if effective_rpm > 0.0 {
            Duration::from_secs_f64(60.0 / effective_rpm)
        } else {
            Duration::ZERO
        };
        Self { last: AsyncMutex::new(None), min_interval, redis }
    }

    /// A purely in-process limiter (no Redis backend), used by tests. The
    /// production Redis-unconfigured path is `from_env` with `redis: None`.
    #[cfg(test)]
    fn local(min_interval: Duration) -> Self {
        Self { last: AsyncMutex::new(None), min_interval, redis: None }
    }

    /// Block until this call may proceed.
    ///
    /// Two gates apply IN ORDER, and the local lock is held across both so a
    /// process's Plane calls are strictly serialised:
    /// 1. The per-process floor (`min_interval` since this process's last ACTUAL
    ///    issue) is enforced FIRST. This is always applied, so pacing holds no
    ///    matter how Redis behaves.
    /// 2. Only THEN is the shared Redis slot reserved — at ~the actual send time,
    ///    best-effort. Reserving after the local wait (rather than before) means
    ///    the Redis key records the real send time, so other processes reserve
    ///    subsequent slots correctly even right after a Redis flush/recovery; a
    ///    Redis failure/timeout/outage just yields a zero wait (fail-open) and
    ///    never blocks or fails the call.
    ///
    /// Serialising a process's own Plane calls is exactly the intended pacing —
    /// the whole point is to not hammer Plane, so there is nothing to pipeline —
    /// and no Redis state (down, mid-flight death, restart/flush) can produce an
    /// intra-process burst.
    async fn acquire(&self) {
        // Pacing disabled → nothing to gate (and no Redis round-trip).
        if self.min_interval.is_zero() {
            return;
        }
        // Held across both waits → per-process serialisation.
        let mut last = self.last.lock().await;

        // 1. Per-process floor from the last ACTUAL issue time, enforced first so
        //    we never reserve a shared slot earlier than this process can send.
        if let Some(prev) = *last {
            let now = Instant::now();
            let floor = prev + self.min_interval;
            if floor > now {
                tokio::time::sleep(floor - now).await;
            }
        }

        // 2. Reserve the shared cross-process slot at ~the actual send time
        //    (best-effort). `None` (unconfigured OR any Redis failure) = zero wait.
        if let Some(backend) = &self.redis {
            if let Some(redis_wait) = backend.rate_reserve(self.min_interval).await {
                if !redis_wait.is_zero() {
                    tokio::time::sleep(redis_wait).await;
                }
            }
        }

        // Record the ACTUAL issue time (post-sleep) so the next caller is spaced
        // from when this request really went out, even if a wakeup overslept.
        *last = Some(Instant::now());
    }
}

// ─── GET cache (in-process, optionally Redis-backed + shared) ────────────────
//
// Replaces the Python client's shared `/tmp/plane-helper-cache.json` file. When
// a `RedisBackend` is configured the cache is shared across every terminus
// instance (one cache, keyed per active token + URL); on any Redis failure it
// transparently serves and populates the in-process map instead. The in-process
// map is ALWAYS written through on `set`, so it stays warm as an instant
// fail-open fallback whether or not Redis is currently reachable.

#[derive(Debug)]
struct GetCache {
    entries: AsyncMutex<HashMap<String, (Instant, String)>>,
    ttl: Duration,
    /// Shared distributed backend; `None` = purely in-process (the default and
    /// the fail-open fallback).
    redis: Option<Arc<RedisBackend>>,
}

impl GetCache {
    /// Build from `PLANE_CACHE_TTL_SECS` (default 5s, matching the Python client).
    /// `redis` (when `Some`) makes the cache shared across instances.
    fn from_env(redis: Option<Arc<RedisBackend>>) -> Self {
        let ttl_secs: u64 = std::env::var("PLANE_CACHE_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
        Self { entries: AsyncMutex::new(HashMap::new()), ttl: Duration::from_secs(ttl_secs), redis }
    }

    /// A purely in-process cache (no Redis backend), used by tests. The
    /// production Redis-unconfigured path is `from_env` with `redis: None`.
    #[cfg(test)]
    fn new(ttl: Duration) -> Self {
        Self { entries: AsyncMutex::new(HashMap::new()), ttl, redis: None }
    }

    async fn get(&self, key: &str) -> Option<String> {
        // Prefer the shared Redis cache; a hit there is authoritative. A miss OR
        // a Redis failure both fall through to the in-process map (fail-open).
        if let Some(backend) = &self.redis {
            if let Some(body) = backend.cache_get(&redis_cache_key(key)).await {
                // Warm the in-process fallback so a Redis outage within the TTL
                // still serves this entry instead of refetching from Plane —
                // important for instances that mostly consume shared cache hits.
                self.entries.lock().await.insert(key.to_string(), (Instant::now(), body.clone()));
                return Some(body);
            }
        }
        let entries = self.entries.lock().await;
        entries.get(key).and_then(|(ts, body)| {
            if ts.elapsed() < self.ttl { Some(body.clone()) } else { None }
        })
    }

    async fn set(&self, key: String, body: String) {
        // Write through to Redis (best-effort) AND always to the in-process map,
        // so the local fallback is warm the instant Redis becomes unreachable.
        if let Some(backend) = &self.redis {
            backend.cache_set(&redis_cache_key(&key), &body, self.ttl).await;
        }
        let mut entries = self.entries.lock().await;
        entries.insert(key, (Instant::now(), body));
    }
}

// ─── PlaneClient ─────────────────────────────────────────────────────────────

/// Env-var prefix that marks a per-agent named-identity token. A variable
/// `PLANE_PAT_<NAME>` registers the identity `<name>` (lowercased). This is the
/// single source of truth for the prefix — the `from_env` scan and the
/// `plane_list_identities` tool both derive from it, so the two can never drift.
/// The unsuffixed default token `PLANE_API_KEY` is deliberately NOT this prefix
/// and is handled on its own path.
const PLANE_IDENTITY_PREFIX: &str = "PLANE_PAT_";

/// Scan this process's own environment for `PLANE_PAT_<NAME>` named-identity
/// tokens, returning a `lowercased-name -> token` map. This is the ONLY place
/// the prefix is matched against the environment. Empty-valued vars are
/// skipped (a set-but-empty secret is treated as absent), and names are
/// lowercased so a later duplicate differing only by case collapses onto the
/// same entry — matching how [`PlaneClient::for_identity`] lowercases on
/// lookup. Never reads another process's files.
fn scan_named_identities() -> HashMap<String, String> {
    let mut identities: HashMap<String, String> = HashMap::new();
    for (k, v) in std::env::vars() {
        if let Some(name) = k.strip_prefix(PLANE_IDENTITY_PREFIX) {
            if !v.is_empty() {
                identities.insert(name.to_lowercase(), v);
            }
        }
    }
    identities
}

/// Shared HTTP client for the Plane CE API.
///
/// Constructed from environment variables. When `PLANE_API_URL` is absent,
/// `configured` is false and every tool returns `ToolError::NotConfigured`.
#[derive(Clone)]
pub struct PlaneClient {
    http: Client,
    base_url: Option<String>,
    /// Active token used for requests made directly through this client
    /// instance (the default identity, unless [`PlaneClient::for_identity`]
    /// produced this instance).
    api_key: Option<String>,
    /// Human name for the active token, if resolvable (see [`PlaneClient::from_env`]).
    identity_name: Option<String>,
    /// All configured named identities: lowercased name -> token. Populated
    /// from `PLANE_PAT_<NAME>` env vars only — never from another
    /// process's files.
    identities: Arc<HashMap<String, String>>,
    workspace: String,
    rate_limiter: Arc<RateLimiter>,
    cache: Arc<GetCache>,
}

/// Hand-written `Debug` impl: never prints `api_key` or `identities` (both
/// hold live credentials). Redacted as `Some(<redacted>)` / a bare count so
/// logs/panics/`{:?}` formatting can never leak a token.
impl std::fmt::Debug for PlaneClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("identity_name", &self.identity_name)
            .field("identities", &format!("<{} configured, redacted>", self.identities.len()))
            .field("workspace", &self.workspace)
            .finish()
    }
}

impl PlaneClient {
    /// Build a `PlaneClient` from environment variables.
    pub fn from_env() -> Self {
        let base_url = std::env::var("PLANE_API_URL").ok().map(|u| u.trim_end_matches('/').to_string());
        let default_api_key = std::env::var("PLANE_API_KEY").ok().filter(|v| !v.is_empty());
        let workspace = std::env::var("PLANE_WORKSPACE")
            .unwrap_or_else(|_| "moosenet".into());

        // Named identities: PLANE_PAT_<NAME> for any agent that needs its
        // own token (e.g. PLANE_PAT_CLAUDE, PLANE_PAT_HARMONY). Read once
        // at process start from this process's own environment (populated by
        // the operator's secret manager) — never from another process's files.
        let identities = scan_named_identities();

        // Resolve the active-default identity NAME (lowercased to match the
        // identities map / `for_identity` lookup): prefer an explicit
        // PLANE_IDENTITY_NAME, else the name of a PLANE_PAT_<NAME> whose value
        // happens to equal the unsuffixed default token.
        let identity_name = std::env::var("PLANE_IDENTITY_NAME")
            .ok()
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                default_api_key.as_ref().and_then(|tok| {
                    identities.iter().find(|(_, v)| *v == tok).map(|(k, _)| k.clone())
                })
            });

        // Resolve the active-default TOKEN so a named default genuinely ACTS as
        // that identity: when the active-default name matches a configured
        // PLANE_PAT_<NAME>, use THAT identity's token; otherwise fall back to the
        // unsuffixed PLANE_API_KEY. This makes `PLANE_IDENTITY_NAME=lumina`
        // route real calls through PLANE_PAT_LUMINA, while a deployment with only
        // PLANE_API_KEY (no named default) is unaffected — full backward compat.
        let api_key = identity_name
            .as_ref()
            .and_then(|name| identities.get(name).cloned())
            .or_else(|| default_api_key.clone());

        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");

        // Optional shared Redis backend (PLANE_REDIS_URL). One `Arc` is shared by
        // the cache and the limiter so a single circuit breaker governs both.
        // `None` (unset URL) → identical in-process behaviour as before.
        let redis_backend = RedisBackend::from_env();

        Self {
            http,
            base_url,
            api_key,
            identity_name,
            identities: Arc::new(identities),
            workspace,
            rate_limiter: Arc::new(RateLimiter::from_env(redis_backend.clone())),
            cache: Arc::new(GetCache::from_env(redis_backend)),
        }
    }

    /// Returns true if both PLANE_API_URL and PLANE_API_KEY are configured.
    pub fn configured(&self) -> bool {
        self.base_url.is_some() && self.api_key.is_some()
    }

    /// Test-only constructor for other in-crate modules that call this
    /// module's tools in-process (e.g. `scribe::mod::ScribeReportDiscrepancy`,
    /// SCRB-04) and need a `PlaneClient` pointed at a local mock server.
    /// Mirrors this module's own `tests::mock_client` exactly (zero-interval
    /// rate limiter so tests aren't paced, a short-lived GET cache). Only
    /// compiled for test builds -- never available to production code, and
    /// never reads real credentials.
    #[cfg(test)]
    pub(crate) fn test_client_with_base_url(base_url: String) -> Arc<Self> {
        Arc::new(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build reqwest client"),
            base_url: Some(base_url),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        })
    }

    /// Return a `ToolError::NotConfigured` with helpful message.
    fn not_configured(&self) -> ToolError {
        ToolError::NotConfigured(
            "PLANE_API_URL and PLANE_API_KEY must be set to use Plane tools".into(),
        )
    }

    /// Return a clone of this client scoped to a named identity's token
    /// (from `PLANE_PAT_<NAME>`) instead of the default. The HTTP client,
    /// rate limiter, and GET cache are shared (same `Arc`s) — only the active
    /// token and its resolved name differ, so identities never contend for
    /// separate rate budgets and never leak each other's tokens.
    pub fn for_identity(&self, name: &str) -> Result<Self, ToolError> {
        let key = name.trim().to_lowercase();
        let token = self.identities.get(&key).cloned().ok_or_else(|| {
            ToolError::InvalidArgument(format!(
                "No Plane identity named '{name}' is configured (expected {PLANE_IDENTITY_PREFIX}{})",
                key.to_uppercase()
            ))
        })?;
        Ok(Self {
            api_key: Some(token),
            identity_name: Some(key),
            ..self.clone()
        })
    }

    /// Resolve the effective client for a single tool invocation from its raw
    /// args. This is the ONE shared dispatch point every Plane CRUD tool uses
    /// to pick the token it authenticates with, so the selection rule lives in
    /// exactly one place rather than 28 call sites.
    ///
    /// - A non-empty `identity` string argument selects that named
    ///   `PLANE_PAT_<NAME>` identity (via [`PlaneClient::for_identity`]),
    ///   returning an owned, token-scoped clone.
    /// - Otherwise the call acts as this client's **active default** identity —
    ///   returned borrowed (no clone) — which was already resolved at
    ///   construction to the named-default token when `PLANE_IDENTITY_NAME`
    ///   matches a configured identity, else the unsuffixed `PLANE_API_KEY`.
    ///
    /// The resolved client is configuration-checked so callers get a single,
    /// consistent `ToolError::NotConfigured` when `PLANE_API_URL`/token are
    /// absent. The `identity` argument is consumed here for token selection
    /// only — it is never placed into a request body and never logged.
    fn resolve_identity<'a>(&'a self, args: &Value) -> Result<Cow<'a, Self>, ToolError> {
        let client = match args.get("identity").and_then(|v| v.as_str()) {
            Some(name) if !name.trim().is_empty() => Cow::Owned(self.for_identity(name)?),
            _ => Cow::Borrowed(self),
        };
        if !client.configured() {
            return Err(client.not_configured());
        }
        Ok(client)
    }

    /// The active identity's resolved name, if known.
    pub fn identity_name(&self) -> Option<&str> {
        self.identity_name.as_deref()
    }

    /// Names of all configured named identities (lowercased, sorted for stable
    /// output). These are exactly the names [`PlaneClient::for_identity`] can
    /// resolve. Never returns — and cannot be used to recover — token values.
    pub fn identity_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.identities.keys().cloned().collect();
        names.sort();
        names
    }

    /// Build a GET-cache key that is unique per active token, not just per
    /// URL — see the doc comment on `get_json_cached` for why. Uses the raw
    /// token as part of an in-memory-only key (never logged, never printed:
    /// this struct's `Debug` impl is hand-written to redact it).
    fn cache_key(&self, url: &str) -> String {
        format!("{}\u{0}{}", self.api_key.as_deref().unwrap_or(""), url)
    }

    /// Build the base URL for workspace-scoped endpoints.
    fn workspace_url(&self) -> String {
        format!(
            "{}/api/v1/workspaces/{}/",
            self.base_url.as_deref().unwrap_or(""),
            self.workspace
        )
    }

    /// Resolve a project identifier (e.g. "LM") or a UUID to a project UUID.
    ///
    /// Plane CE's project-scoped endpoints require the project UUID in the path;
    /// passing a human identifier like "LM" yields a 404 ("Page not found").
    /// UUIDs are returned unchanged (no network call); anything else is looked up
    /// against the workspace project list, matching on `identifier`
    /// (case-insensitive) or exact `id`.
    async fn resolve_project_id(&self, project_id: &str) -> Result<String, ToolError> {
        if is_uuid(project_id) {
            return Ok(project_id.to_string());
        }
        let url = format!("{}projects/", self.workspace_url());
        let body = self.get_json_cached(&url).await?;
        let list: ApiList<Project> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse projects: {e}")))?;
        list.into_items()
            .into_iter()
            .find(|p| p.identifier.eq_ignore_ascii_case(project_id) || p.id == project_id)
            .map(|p| p.id)
            .ok_or_else(|| {
                ToolError::NotFound(format!(
                    "No Plane project matches identifier or id '{project_id}'"
                ))
            })
    }

    /// Execute a GET request with rate-limit retry (max 3 attempts, 3 s delay).
    async fn get_with_retry(&self, url: &str) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .get(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
        })
        .await
    }

    /// GET `url` as raw JSON text, serving from the in-memory TTL cache when
    /// available. On a cache miss, performs the request (through the same
    /// rate-limited, retrying transport as every other call) and populates the
    /// cache with the response body on success. Callers deserialize the
    /// returned string with `serde_json::from_str`.
    ///
    /// The cache key includes the active token, not just the URL: Plane GET
    /// responses are not uniformly workspace-scoped (e.g. `plane_list_projects`
    /// only returns projects the calling token's user belongs to, and member
    /// listings can vary by role), so two [`PlaneClient::for_identity`] clones
    /// sharing this cache's `Arc` must never be served each other's cached
    /// response for the same URL.
    async fn get_json_cached(&self, url: &str) -> Result<String, ToolError> {
        let cache_key = self.cache_key(url);
        if let Some(body) = self.cache.get(&cache_key).await {
            debug!("Plane GET cache hit: {url}");
            return Ok(body);
        }
        let resp = self.get_with_retry(url).await?;
        let resp = Self::check_status(resp).await?;
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Http(format!("Failed to read response body: {e}")))?;
        self.cache.set(cache_key, body.clone()).await;
        Ok(body)
    }

    /// Execute a POST request with rate-limit retry.
    async fn post_with_retry(&self, url: &str, body: &Value) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .post(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Execute a PATCH request with rate-limit retry.
    async fn patch_with_retry(&self, url: &str, body: &Value) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .patch(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Execute a DELETE request with rate-limit retry.
    async fn delete_with_retry(&self, url: &str) -> Result<Response, ToolError> {
        self.request_with_retry(|| {
            let key = self.api_key.as_deref().unwrap_or("");
            self.http
                .delete(url)
                .header("X-API-Key", key)
                .header("Content-Type", "application/json")
        })
        .await
    }

    /// Core retry loop, ported from the Python client's semantics:
    /// - every attempt is paced by the shared [`RateLimiter`] first
    /// - 401/403 are never retried (auth failures are terminal)
    /// - 429 respects a `Retry-After` header, falling back to the backoff table
    /// - 5xx and network errors retry with the same backoff table
    /// - max 3 attempts total
    async fn request_with_retry<F>(&self, build: F) -> Result<Response, ToolError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        const MAX_ATTEMPTS: u8 = 3;
        const BACKOFF: [u64; 3] = [2, 5, 15];
        // A hostile or misconfigured server can send an arbitrarily large
        // `Retry-After`; without a ceiling that would hang a tool call far
        // beyond what "max 3 attempts" implies. Clamp to a sane upper bound.
        const MAX_RETRY_AFTER_SECS: u64 = 60;

        let mut attempts = 0u8;
        loop {
            attempts += 1;
            self.rate_limiter.acquire().await;

            let sent = build().send().await;
            let resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    if attempts >= MAX_ATTEMPTS {
                        return Err(ToolError::Http(format!(
                            "Request failed after {attempts} attempts: {e}"
                        )));
                    }
                    let delay = BACKOFF[(attempts - 1) as usize];
                    warn!("Plane network error ({e}), retrying in {delay}s (attempt {attempts}/{MAX_ATTEMPTS})");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    continue;
                }
            };

            let status = resp.status();

            // Auth failures are terminal — never retried.
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Ok(resp);
            }

            if status == StatusCode::TOO_MANY_REQUESTS {
                if attempts >= MAX_ATTEMPTS {
                    return Err(ToolError::Http(
                        "Plane rate limit exceeded — try again later".into(),
                    ));
                }
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(BACKOFF[(attempts - 1) as usize])
                    .min(MAX_RETRY_AFTER_SECS);
                warn!("Plane 429 received, retrying in {retry_after}s (attempt {attempts}/{MAX_ATTEMPTS})");
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                continue;
            }

            if status.is_server_error() {
                if attempts >= MAX_ATTEMPTS {
                    // Exhausted retries — return the response as-is so
                    // check_status() surfaces a proper Http error with body.
                    return Ok(resp);
                }
                let delay = BACKOFF[(attempts - 1) as usize];
                warn!("Plane server error {status}, retrying in {delay}s (attempt {attempts}/{MAX_ATTEMPTS})");
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }

            return Ok(resp);
        }
    }

    /// Map non-success HTTP status to a clean ToolError.
    async fn check_status(resp: Response) -> Result<Response, ToolError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        match status {
            StatusCode::NOT_FOUND => Err(ToolError::NotFound(format!("Resource not found: {body}"))),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(ToolError::Http(format!("Plane authentication failed: {status}")))
            }
            StatusCode::UNPROCESSABLE_ENTITY => {
                Err(ToolError::InvalidArgument(format!("Invalid request: {body}")))
            }
            _ => Err(ToolError::Http(format!("Plane returned {status}: {body}"))),
        }
    }
}

// ─── Helper macro for guard boilerplate ──────────────────────────────────────


macro_rules! require_arg {
    ($args:expr, $field:literal, $type:ident) => {
        $args
            .get($field)
            .and_then(|v| v.$type())
            .ok_or_else(|| ToolError::InvalidArgument(format!("missing required argument: {}", $field)))?
    };
}

// ─── Shared optional `identity` argument ─────────────────────────────────────
//
// Every Plane CRUD tool exposes the same optional `identity` argument, resolved
// centrally by `PlaneClient::resolve_identity`. These two helpers keep the
// schema fragment and its documentation in a single source of truth so all
// tools describe it identically and can never drift.

/// JSON-schema fragment for the optional `identity` argument.
fn identity_param_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional Plane identity to act as: a configured PLANE_PAT_<NAME> \
                        identity name (e.g. \"claude\", \"harmony\"). Omit to use the active \
                        default identity (PLANE_IDENTITY_NAME when it names a configured \
                        identity, otherwise the default PLANE_API_KEY). Call \
                        plane_list_identities to see the configured names."
    })
}

/// Add the shared optional `identity` property to a tool's parameter schema.
/// Idempotent and safe on any `{ "type": "object", "properties": { .. } }`
/// schema — inserts the `identity` property without disturbing the tool's own
/// arguments or its `required` list (identity is always optional).
fn with_identity_param(mut schema: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
        props.insert("identity".to_string(), identity_param_schema());
    }
    schema
}

// ─── 1. plane_list_projects ──────────────────────────────────────────────────

pub struct PlaneListProjects {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListProjects {
    fn name(&self) -> &str { "plane_list_projects" }
    fn description(&self) -> &str { "List all projects in the Plane workspace" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {},
            "required": []
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let url = format!("{}projects/", client.workspace_url());
        debug!("plane_list_projects GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Project> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse projects: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No projects found in workspace".into());
        }
        let mut out = format!("Found {} project(s):\n", items.len());
        for p in &items {
            out.push_str(&format!("  [{id}] {name} ({identifier})\n",
                id = p.id, name = p.name, identifier = p.identifier));
        }
        Ok(out)
    }
}

// ─── 2. plane_get_project ────────────────────────────────────────────────────

pub struct PlaneGetProject {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetProject {
    fn name(&self) -> &str { "plane_get_project" }
    fn description(&self) -> &str { "Get details for a specific Plane project by ID" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let url = format!("{}projects/{project_id}/", client.workspace_url());
        debug!("plane_get_project GET {url}");
        let body = client.get_json_cached(&url).await?;
        let p: Project = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse project: {e}")))?;
        Ok(format!(
            "Project: {name}\nID: {id}\nIdentifier: {identifier}\nDescription: {desc}",
            name = p.name,
            id = p.id,
            identifier = p.identifier,
            desc = p.description.as_deref().unwrap_or("(none)")
        ))
    }
}

// ─── 3. plane_list_work_items ────────────────────────────────────────────────

pub struct PlaneListWorkItems {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListWorkItems {
    fn name(&self) -> &str { "plane_list_work_items" }
    fn description(&self) -> &str { "List work items (issues) in a Plane project" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "limit": { "type": "integer", "description": "Max results to return (default 50)" }
            },
            "required": ["project_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let url = format!(
            "{}projects/{project_id}/issues/",
            client.workspace_url()
        );
        debug!("plane_list_work_items GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;
        let total = list.total_count();
        let items: Vec<Issue> = list.into_items().into_iter().take(limit).collect();
        if items.is_empty() {
            return Ok("No work items found".into());
        }
        let mut out = format!("Work items ({} shown of {}):\n", items.len(), total);
        for i in &items {
            let priority = i.priority.as_deref().unwrap_or("none");
            let seq = i.sequence_id.map(|s| format!("#{s}")).unwrap_or_default();
            out.push_str(&format!("  [{id}] {seq} {name} (priority: {priority})\n",
                id = i.id, seq = seq, name = i.name, priority = priority));
        }
        Ok(out)
    }
}

// ─── 4. plane_get_work_item ──────────────────────────────────────────────────

pub struct PlaneGetWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetWorkItem {
    fn name(&self) -> &str { "plane_get_work_item" }
    fn description(&self) -> &str { "Get details for a specific work item by ID" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" }
            },
            "required": ["project_id", "issue_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            client.workspace_url()
        );
        debug!("plane_get_work_item GET {url}");
        let body = client.get_json_cached(&url).await?;
        let i: Issue = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issue: {e}")))?;
        Ok(format!(
            "Issue: {name}\nID: {id}\nSequence: {seq}\nPriority: {priority}\nState: {state}\nDescription: {desc}",
            name = i.name,
            id = i.id,
            seq = i.sequence_id.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            priority = i.priority.as_deref().unwrap_or("none"),
            state = i.state.as_deref().unwrap_or("unknown"),
            desc = i.description.as_deref().unwrap_or("(none)")
        ))
    }
}

// ─── 5. plane_create_work_item ───────────────────────────────────────────────

pub struct PlaneCreateWorkItem {
    client: Arc<PlaneClient>,
}

impl PlaneCreateWorkItem {
    /// Construct directly for an in-process, in-crate caller (e.g.
    /// `scribe::mod::ScribeReportDiscrepancy`, SCRB-04) that calls this
    /// tool's `execute()` as a plain function call rather than a second HTTP
    /// hop through the MCP registry -- the "ONE sanctioned path" for Plane
    /// access still applies (this IS that path, called in-process, same
    /// crate), it just isn't going through `register()`'s registry lookup.
    /// `pub(crate)` (not `pub`, per cycle 1 review): only an in-crate caller
    /// is a legitimate use case; no external API surface should be able to
    /// construct these tools directly, bypassing `register()`'s catalog.
    pub(crate) fn new(client: Arc<PlaneClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RustTool for PlaneCreateWorkItem {
    fn name(&self) -> &str { "plane_create_work_item" }
    fn description(&self) -> &str { "Create a new work item (issue) in a Plane project" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "name": { "type": "string", "description": "Issue title" },
                "description_html": { "type": "string", "description": "Issue description (HTML)" },
                "state": { "type": "string", "description": "State UUID" },
                "priority": { "type": "string", "description": "Priority: urgent/high/medium/low/none" },
                "due_date": { "type": "string", "description": "Due date (YYYY-MM-DD)" },
                "parent": { "type": "string", "description": "Parent issue UUID (for sub-issues)" },
                "label_ids": { "type": "array", "items": { "type": "string" }, "description": "Label UUIDs to attach" }
            },
            "required": ["project_id", "name"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let name = require_arg!(args, "name", as_str);
        let mut body = json!({ "name": name });
        if let Some(v) = args.get("description_html").and_then(|v| v.as_str()) {
            body["description_html"] = json!(v);
        }
        if let Some(v) = args.get("state").and_then(|v| v.as_str()) {
            body["state"] = json!(v);
        }
        if let Some(v) = args.get("priority").and_then(|v| v.as_str()) {
            body["priority"] = json!(v);
        }
        if let Some(v) = args.get("due_date").and_then(|v| v.as_str()) {
            body["due_date"] = json!(v);
        }
        if let Some(v) = args.get("parent").and_then(|v| v.as_str()) {
            body["parent"] = json!(v);
        }
        if let Some(v) = args.get("label_ids").and_then(|v| v.as_array()) {
            body["label_ids"] = json!(v);
        }
        let url = format!(
            "{}projects/{project_id}/issues/",
            client.workspace_url()
        );
        debug!("plane_create_work_item POST {url}");
        let resp = client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created issue: {e}")))?;
        Ok(format!("Created issue: {name}\nID: {id}\nSequence: #{seq}",
            name = i.name, id = i.id,
            seq = i.sequence_id.unwrap_or(0)))
    }
}

// ─── 6. plane_update_work_item ───────────────────────────────────────────────

pub struct PlaneUpdateWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneUpdateWorkItem {
    fn name(&self) -> &str { "plane_update_work_item" }
    fn description(&self) -> &str { "Update fields on an existing Plane work item" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "name": { "type": "string", "description": "New title" },
                "description_html": { "type": "string", "description": "New description (HTML)" },
                "state": { "type": "string", "description": "New state UUID" },
                "priority": { "type": "string", "description": "New priority" },
                "due_date": { "type": "string", "description": "New due date (YYYY-MM-DD)" },
                "parent": { "type": "string", "description": "New parent issue UUID" },
                "label_ids": { "type": "array", "items": { "type": "string" }, "description": "New label UUIDs (replaces existing set)" }
            },
            "required": ["project_id", "issue_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let mut body = json!({});
        for field in &["name", "description_html", "state", "priority", "due_date", "parent"] {
            if let Some(v) = args.get(field).and_then(|v| v.as_str()) {
                body[*field] = json!(v);
            }
        }
        if let Some(v) = args.get("label_ids").and_then(|v| v.as_array()) {
            body["label_ids"] = json!(v);
        }
        if body.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            return Err(ToolError::InvalidArgument("No fields to update provided".into()));
        }
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            client.workspace_url()
        );
        debug!("plane_update_work_item PATCH {url}");
        let resp = client.patch_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse updated issue: {e}")))?;
        Ok(format!("Updated issue: {name} (ID: {id})", name = i.name, id = i.id))
    }
}

// ─── 7. plane_delete_work_item ───────────────────────────────────────────────

pub struct PlaneDeleteWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneDeleteWorkItem {
    fn name(&self) -> &str { "plane_delete_work_item" }
    fn description(&self) -> &str { "Delete a Plane work item permanently" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID to delete" }
            },
            "required": ["project_id", "issue_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            client.workspace_url()
        );
        debug!("plane_delete_work_item DELETE {url}");
        let resp = client.delete_with_retry(&url).await?;
        PlaneClient::check_status(resp).await?;
        Ok(format!("Deleted work item {issue_id}"))
    }
}

// ─── 8. plane_list_cycles ────────────────────────────────────────────────────

pub struct PlaneListCycles {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListCycles {
    fn name(&self) -> &str { "plane_list_cycles" }
    fn description(&self) -> &str { "List cycles (sprints) in a Plane project" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/cycles/",
            client.workspace_url()
        );
        debug!("plane_list_cycles GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Cycle> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse cycles: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No cycles found".into());
        }
        let mut out = format!("Found {} cycle(s):\n", items.len());
        for c in &items {
            let status = c.status.as_deref().unwrap_or("unknown");
            let start = c.start_date.as_deref().unwrap_or("-");
            let end = c.end_date.as_deref().unwrap_or("-");
            out.push_str(&format!("  [{id}] {name} ({status}) {start}..{end}\n",
                id = c.id, name = c.name, status = status, start = start, end = end));
        }
        Ok(out)
    }
}

// ─── 9. plane_get_cycle ──────────────────────────────────────────────────────

pub struct PlaneGetCycle {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetCycle {
    fn name(&self) -> &str { "plane_get_cycle" }
    fn description(&self) -> &str { "Get details for a specific Plane cycle" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "cycle_id": { "type": "string", "description": "Cycle UUID" }
            },
            "required": ["project_id", "cycle_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let cycle_id = require_arg!(args, "cycle_id", as_str);
        let url = format!(
            "{}projects/{project_id}/cycles/{cycle_id}/",
            client.workspace_url()
        );
        debug!("plane_get_cycle GET {url}");
        let body = client.get_json_cached(&url).await?;
        let c: Cycle = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse cycle: {e}")))?;
        Ok(format!(
            "Cycle: {name}\nID: {id}\nStatus: {status}\nDates: {start} to {end}",
            name = c.name, id = c.id,
            status = c.status.as_deref().unwrap_or("unknown"),
            start = c.start_date.as_deref().unwrap_or("-"),
            end = c.end_date.as_deref().unwrap_or("-")
        ))
    }
}

// ─── 10. plane_list_cycle_issues ─────────────────────────────────────────────

pub struct PlaneListCycleIssues {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListCycleIssues {
    fn name(&self) -> &str { "plane_list_cycle_issues" }
    fn description(&self) -> &str { "List issues in a specific Plane cycle" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "cycle_id": { "type": "string", "description": "Cycle UUID" }
            },
            "required": ["project_id", "cycle_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let cycle_id = require_arg!(args, "cycle_id", as_str);
        let url = format!(
            "{}projects/{project_id}/cycles/{cycle_id}/cycle-issues/",
            client.workspace_url()
        );
        debug!("plane_list_cycle_issues GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse cycle issues: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No issues in this cycle".into());
        }
        let mut out = format!("Cycle issues ({}):\n", items.len());
        for i in &items {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 11. plane_list_modules ──────────────────────────────────────────────────

pub struct PlaneListModules {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListModules {
    fn name(&self) -> &str { "plane_list_modules" }
    fn description(&self) -> &str { "List modules in a Plane project" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/modules/",
            client.workspace_url()
        );
        debug!("plane_list_modules GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Module> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse modules: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No modules found".into());
        }
        let mut out = format!("Found {} module(s):\n", items.len());
        for m in &items {
            let status = m.status.as_deref().unwrap_or("unknown");
            out.push_str(&format!("  [{id}] {name} ({status})\n",
                id = m.id, name = m.name, status = status));
        }
        Ok(out)
    }
}

// ─── 12. plane_get_module ────────────────────────────────────────────────────

pub struct PlaneGetModule {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetModule {
    fn name(&self) -> &str { "plane_get_module" }
    fn description(&self) -> &str { "Get details for a specific Plane module" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "module_id": { "type": "string", "description": "Module UUID" }
            },
            "required": ["project_id", "module_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let module_id = require_arg!(args, "module_id", as_str);
        let url = format!(
            "{}projects/{project_id}/modules/{module_id}/",
            client.workspace_url()
        );
        debug!("plane_get_module GET {url}");
        let body = client.get_json_cached(&url).await?;
        let m: Module = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse module: {e}")))?;
        Ok(format!(
            "Module: {name}\nID: {id}\nStatus: {status}\nDates: {start} to {end}",
            name = m.name, id = m.id,
            status = m.status.as_deref().unwrap_or("unknown"),
            start = m.start_date.as_deref().unwrap_or("-"),
            end = m.target_date.as_deref().unwrap_or("-")
        ))
    }
}

// ─── 13. plane_create_module ─────────────────────────────────────────────────

pub struct PlaneCreateModule {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCreateModule {
    fn name(&self) -> &str { "plane_create_module" }
    fn description(&self) -> &str { "Create a new module in a Plane project" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "name": { "type": "string", "description": "Module name" },
                "description": { "type": "string", "description": "Module description" },
                "status": { "type": "string", "description": "Module status" },
                "start_date": { "type": "string", "description": "Start date (YYYY-MM-DD)" },
                "target_date": { "type": "string", "description": "Target date (YYYY-MM-DD)" }
            },
            "required": ["project_id", "name"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let name = require_arg!(args, "name", as_str);
        let mut body = json!({ "name": name });
        for field in &["description", "status", "start_date", "target_date"] {
            if let Some(v) = args.get(field).and_then(|v| v.as_str()) {
                body[*field] = json!(v);
            }
        }
        let url = format!(
            "{}projects/{project_id}/modules/",
            client.workspace_url()
        );
        debug!("plane_create_module POST {url}");
        let resp = client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let m: Module = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created module: {e}")))?;
        Ok(format!("Created module: {name} (ID: {id})", name = m.name, id = m.id))
    }
}

// ─── 14. plane_list_module_issues ────────────────────────────────────────────

pub struct PlaneListModuleIssues {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListModuleIssues {
    fn name(&self) -> &str { "plane_list_module_issues" }
    fn description(&self) -> &str { "List issues in a specific Plane module" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "module_id": { "type": "string", "description": "Module UUID" }
            },
            "required": ["project_id", "module_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let module_id = require_arg!(args, "module_id", as_str);
        let url = format!(
            "{}projects/{project_id}/modules/{module_id}/module-issues/",
            client.workspace_url()
        );
        debug!("plane_list_module_issues GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse module issues: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No issues in this module".into());
        }
        let mut out = format!("Module issues ({}):\n", items.len());
        for i in &items {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 15. plane_list_states ───────────────────────────────────────────────────

pub struct PlaneListStates {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListStates {
    fn name(&self) -> &str { "plane_list_states" }
    fn description(&self) -> &str { "List workflow states in a Plane project" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/states/",
            client.workspace_url()
        );
        debug!("plane_list_states GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<State> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse states: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No states found".into());
        }
        let mut out = format!("States ({}):\n", items.len());
        for s in &items {
            out.push_str(&format!("  [{id}] {name} (group: {group}, color: {color})\n",
                id = s.id, name = s.name, group = s.group, color = s.color));
        }
        Ok(out)
    }
}

// ─── 16. plane_list_labels ───────────────────────────────────────────────────

pub struct PlaneListLabels {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListLabels {
    fn name(&self) -> &str { "plane_list_labels" }
    fn description(&self) -> &str { "List labels in a Plane project" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/labels/",
            client.workspace_url()
        );
        debug!("plane_list_labels GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Label> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse labels: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No labels found".into());
        }
        let mut out = format!("Labels ({}):\n", items.len());
        for l in &items {
            let color = l.color.as_deref().unwrap_or("-");
            out.push_str(&format!("  [{id}] {name} (color: {color})\n",
                id = l.id, name = l.name, color = color));
        }
        Ok(out)
    }
}

// ─── 17. plane_list_members ──────────────────────────────────────────────────

pub struct PlaneListMembers {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListMembers {
    fn name(&self) -> &str { "plane_list_members" }
    fn description(&self) -> &str { "List members of a Plane project" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" }
            },
            "required": ["project_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let url = format!(
            "{}projects/{project_id}/members/",
            client.workspace_url()
        );
        debug!("plane_list_members GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Member> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse members: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No members found".into());
        }
        let mut out = format!("Members ({}):\n", items.len());
        for m in &items {
            let name = m.member.as_ref()
                .and_then(|md| md.display_name.as_deref())
                .unwrap_or("unknown");
            out.push_str(&format!("  [{id}] {name} (role: {role})\n",
                id = m.id, name = name, role = m.role));
        }
        Ok(out)
    }
}

// ─── 18. plane_list_comments ─────────────────────────────────────────────────

pub struct PlaneListComments {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListComments {
    fn name(&self) -> &str { "plane_list_comments" }
    fn description(&self) -> &str { "List comments on a Plane work item" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" }
            },
            "required": ["project_id", "issue_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/comments/",
            client.workspace_url()
        );
        debug!("plane_list_comments GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Comment> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse comments: {e}")))?;
        let items = list.into_items();
        if items.is_empty() {
            return Ok("No comments on this issue".into());
        }
        let mut out = format!("Comments ({}):\n", items.len());
        for c in &items {
            let author = c.actor_detail.as_ref()
                .and_then(|a| a.display_name.as_deref())
                .unwrap_or("unknown");
            let text = c.comment_stripped.as_deref()
                .or(c.comment_html.as_deref())
                .unwrap_or("(empty)");
            out.push_str(&format!("  [{id}] {author}: {text}\n",
                id = c.id, author = author, text = text));
        }
        Ok(out)
    }
}

// ─── 19. plane_create_comment ────────────────────────────────────────────────

pub struct PlaneCreateComment {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCreateComment {
    fn name(&self) -> &str { "plane_create_comment" }
    fn description(&self) -> &str { "Add a comment to a Plane work item" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "comment": { "type": "string", "description": "Comment text" }
            },
            "required": ["project_id", "issue_id", "comment"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let comment_text = require_arg!(args, "comment", as_str);
        let body = json!({ "comment_html": format!("<p>{comment_text}</p>") });
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/comments/",
            client.workspace_url()
        );
        debug!("plane_create_comment POST {url}");
        let resp = client.post_with_retry(&url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let c: Comment = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse created comment: {e}")))?;
        Ok(format!("Comment added (ID: {id})", id = c.id))
    }
}

// ─── 20. plane_list_issues_by_state ──────────────────────────────────────────

pub struct PlaneListIssuesByState {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListIssuesByState {
    fn name(&self) -> &str { "plane_list_issues_by_state" }
    fn description(&self) -> &str { "List work items filtered by state group (backlog/unstarted/started/completed/cancelled)" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "state_group": {
                    "type": "string",
                    "description": "State group to filter by",
                    "enum": ["backlog", "unstarted", "started", "completed", "cancelled"]
                },
                "limit": { "type": "integer", "description": "Max results (default 50)" }
            },
            "required": ["project_id", "state_group"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let state_group = require_arg!(args, "state_group", as_str);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

        // Fetch all issues then filter client-side (state_group query param is broken in Plane CE)
        let url = format!(
            "{}projects/{project_id}/issues/",
            client.workspace_url()
        );
        debug!("plane_list_issues_by_state GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let filtered: Vec<Issue> = list.into_items()
            .into_iter()
            .filter(|i| {
                i.state_detail.as_ref()
                    .map(|sd| sd.group.to_lowercase() == state_group.to_lowercase())
                    .unwrap_or(false)
            })
            .take(limit)
            .collect();

        if filtered.is_empty() {
            return Ok(format!("No issues in state group '{state_group}'"));
        }
        let mut out = format!("Issues in '{}' ({}):\n", state_group, filtered.len());
        for i in &filtered {
            out.push_str(&format!("  [{id}] {name}\n", id = i.id, name = i.name));
        }
        Ok(out)
    }
}

// ─── 21. plane_get_issue_by_sequence ─────────────────────────────────────────

pub struct PlaneGetIssueBySequence {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetIssueBySequence {
    fn name(&self) -> &str { "plane_get_issue_by_sequence" }
    fn description(&self) -> &str { "Get a work item by its human-readable sequence number (e.g. LM-42)" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "sequence_id": { "type": "integer", "description": "Sequence number (numeric part of LM-42 etc.)" }
            },
            "required": ["project_id", "sequence_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let sequence_id = args.get("sequence_id").and_then(|v| v.as_u64())
            .ok_or_else(|| ToolError::InvalidArgument("missing required argument: sequence_id".into()))?;

        // Fetch all and filter by sequence_id
        let url = format!(
            "{}projects/{project_id}/issues/",
            client.workspace_url()
        );
        debug!("plane_get_issue_by_sequence GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let found = list.into_items()
            .into_iter()
            .find(|i| i.sequence_id == Some(sequence_id));

        match found {
            None => Err(ToolError::NotFound(format!("No issue with sequence_id #{sequence_id}"))),
            Some(i) => Ok(format!(
                "Issue #{seq}: {name}\nID: {id}\nPriority: {priority}\nState: {state}",
                seq = sequence_id,
                name = i.name,
                id = i.id,
                priority = i.priority.as_deref().unwrap_or("none"),
                state = i.state.as_deref().unwrap_or("unknown")
            )),
        }
    }
}

// ─── 22. plane_list_work_items_filtered ──────────────────────────────────────

pub struct PlaneListWorkItemsFiltered {
    client: Arc<PlaneClient>,
}

impl PlaneListWorkItemsFiltered {
    /// See `PlaneCreateWorkItem::new`'s doc comment -- same rationale, and
    /// same `pub(crate)` tightening (cycle 1 review).
    pub(crate) fn new(client: Arc<PlaneClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RustTool for PlaneListWorkItemsFiltered {
    fn name(&self) -> &str { "plane_list_work_items_filtered" }
    fn description(&self) -> &str { "List work items with optional priority and/or label filters" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "priority": { "type": "string", "description": "Filter by priority: urgent/high/medium/low/none" },
                "label_id": { "type": "string", "description": "Filter by label UUID" },
                "limit": { "type": "integer", "description": "Max results (default 50)" }
            },
            "required": ["project_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let priority_filter = args.get("priority").and_then(|v| v.as_str());
        let label_filter = args.get("label_id").and_then(|v| v.as_str());
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

        let url = format!(
            "{}projects/{project_id}/issues/",
            client.workspace_url()
        );
        debug!("plane_list_work_items_filtered GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Issue> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse issues: {e}")))?;

        let filtered: Vec<Issue> = list.into_items()
            .into_iter()
            .filter(|i| {
                let priority_ok = priority_filter.map(|p| {
                    i.priority.as_deref().unwrap_or("none").eq_ignore_ascii_case(p)
                }).unwrap_or(true);
                let label_ok = label_filter.map(|lf| {
                    i.label_ids.iter().any(|l| l == lf)
                }).unwrap_or(true);
                priority_ok && label_ok
            })
            .take(limit)
            .collect();

        if filtered.is_empty() {
            return Ok("No work items match the given filters".into());
        }
        let mut out = format!("Filtered work items ({}):\n", filtered.len());
        for i in &filtered {
            let priority = i.priority.as_deref().unwrap_or("none");
            out.push_str(&format!("  [{id}] {name} (priority: {priority})\n",
                id = i.id, name = i.name, priority = priority));
        }
        Ok(out)
    }
}

// ─── 23. plane_list_recent_activity ──────────────────────────────────────────

pub struct PlaneListRecentActivity {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListRecentActivity {
    fn name(&self) -> &str { "plane_list_recent_activity" }
    fn description(&self) -> &str { "List recent activity/audit events for a Plane work item" }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID" },
                "limit": { "type": "integer", "description": "Max results (default 20)" }
            },
            "required": ["project_id", "issue_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let url = format!(
            "{}projects/{project_id}/issues/{issue_id}/activities/",
            client.workspace_url()
        );
        debug!("plane_list_recent_activity GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<Activity> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse activities: {e}")))?;
        let items: Vec<Activity> = list.into_items().into_iter().take(limit).collect();
        if items.is_empty() {
            return Ok("No recent activity".into());
        }
        let mut out = format!("Recent activity ({}):\n", items.len());
        for a in &items {
            let actor = a.actor_detail.as_ref()
                .and_then(|ad| ad.display_name.as_deref())
                .unwrap_or("unknown");
            let verb = a.verb.as_deref().unwrap_or("updated");
            let field = a.field.as_deref().unwrap_or("");
            out.push_str(&format!("  {actor} {verb} {field}\n",
                actor = actor, verb = verb, field = field));
        }
        Ok(out)
    }
}

// ─── 24. plane_close_work_item ───────────────────────────────────────────────

pub struct PlaneCloseWorkItem {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneCloseWorkItem {
    fn name(&self) -> &str { "plane_close_work_item" }
    fn description(&self) -> &str {
        "Close a work item by moving it to the first available 'completed' state"
    }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "issue_id": { "type": "string", "description": "Issue UUID to close" }
            },
            "required": ["project_id", "issue_id"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let issue_id = require_arg!(args, "issue_id", as_str);

        // Fetch states to find the 'completed' group
        let states_url = format!(
            "{}projects/{project_id}/states/",
            client.workspace_url()
        );
        debug!("plane_close_work_item: fetching states from {states_url}");
        let body = client.get_json_cached(&states_url).await?;
        let list: ApiList<State> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse states: {e}")))?;

        let completed_state = list.into_items()
            .into_iter()
            .find(|s| s.group.to_lowercase() == "completed")
            .ok_or_else(|| ToolError::NotFound("No 'completed' state found in this project".into()))?;

        // PATCH the issue to use the completed state
        let body = json!({ "state": completed_state.id });
        let issue_url = format!(
            "{}projects/{project_id}/issues/{issue_id}/",
            client.workspace_url()
        );
        debug!("plane_close_work_item PATCH {issue_url}");
        let resp = client.patch_with_retry(&issue_url, &body).await?;
        let resp = PlaneClient::check_status(resp).await?;
        let i: Issue = resp.json().await
            .map_err(|e| ToolError::Http(format!("Failed to parse updated issue: {e}")))?;
        Ok(format!(
            "Closed work item: {name} (now in state '{state}')",
            name = i.name,
            state = completed_state.name
        ))
    }
}

// ─── 25. plane_get_state_by_name ─────────────────────────────────────────────

pub struct PlaneGetStateByName {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneGetStateByName {
    fn name(&self) -> &str { "plane_get_state_by_name" }
    fn description(&self) -> &str {
        "Resolve a Plane workflow state UUID by its human name (e.g. \"Backlog\", \"Done\"), case-insensitive"
    }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "name": { "type": "string", "description": "State name to match, case-insensitive (e.g. \"Backlog\", \"Done\")" }
            },
            "required": ["project_id", "name"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let project_id = client.resolve_project_id(project_id_arg).await?;
        let name = require_arg!(args, "name", as_str);
        let url = format!(
            "{}projects/{project_id}/states/",
            client.workspace_url()
        );
        debug!("plane_get_state_by_name GET {url}");
        let body = client.get_json_cached(&url).await?;
        let list: ApiList<State> = serde_json::from_str(&body)
            .map_err(|e| ToolError::Http(format!("Failed to parse states: {e}")))?;
        list.into_items()
            .into_iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .map(|s| format!("State '{}': {}", s.name, s.id))
            .ok_or_else(|| ToolError::NotFound(format!("No state named '{name}' in this project")))
    }
}

// ─── 26. plane_batch_create_work_items ───────────────────────────────────────

pub struct PlaneBatchCreateWorkItems {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneBatchCreateWorkItems {
    fn name(&self) -> &str { "plane_batch_create_work_items" }
    fn description(&self) -> &str {
        "Create multiple work items in a Plane project sequentially, returning each result"
    }
    fn parameters(&self) -> Value {
        with_identity_param(json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "Project UUID or identifier (e.g. \"LM\")" },
                "items": {
                    "type": "array",
                    "description": "Issues to create",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "description_html": { "type": "string" },
                            "priority": { "type": "string" },
                            "state": { "type": "string" }
                        },
                        "required": ["name"]
                    }
                }
            },
            "required": ["project_id", "items"]
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.client.resolve_identity(&args)?;
        let project_id_arg = require_arg!(args, "project_id", as_str);
        let items = args
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidArgument("missing required argument: items".into()))?;
        if items.is_empty() {
            return Err(ToolError::InvalidArgument("items must not be empty".into()));
        }
        let project_id = client.resolve_project_id(project_id_arg).await?;

        let url = format!(
            "{}projects/{project_id}/issues/",
            client.workspace_url()
        );
        let mut out = format!("Batch-created {} issue(s):\n", items.len());
        for (index, item) in items.iter().enumerate() {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidArgument(format!("items[{index}] missing required field: name"))
                })?;
            let mut body = json!({ "name": name });
            if let Some(v) = item.get("description_html").and_then(|v| v.as_str()) {
                body["description_html"] = json!(v);
            }
            if let Some(v) = item.get("priority").and_then(|v| v.as_str()) {
                body["priority"] = json!(v);
            }
            if let Some(v) = item.get("state").and_then(|v| v.as_str()) {
                body["state"] = json!(v);
            }
            debug!("plane_batch_create_work_items [{index}] POST {url}");
            let resp = client.post_with_retry(&url, &body).await?;
            let resp = PlaneClient::check_status(resp).await?;
            let created: Issue = resp
                .json()
                .await
                .map_err(|e| ToolError::Http(format!("Failed to parse created issue [{index}]: {e}")))?;
            out.push_str(&format!(
                "  {}/{}: [{}] {} (#{})\n",
                index + 1,
                items.len(),
                created.id,
                created.name,
                created.sequence_id.unwrap_or(0)
            ));
        }
        Ok(out)
    }
}

// ─── 27. plane_whoami ────────────────────────────────────────────────────────

pub struct PlaneWhoami {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneWhoami {
    fn name(&self) -> &str { "plane_whoami" }
    fn description(&self) -> &str {
        "Report which configured Plane identity is active, or check whether a named identity is configured. With verify=true, performs a real authenticated read as the selected identity (explicit `identity`, else the active default) to prove its token is currently accepted (200) or rejected (401/403 — likely expired). Never inspects other processes' files — identities come only from this process's own PLANE_PAT_<NAME> environment; never returns a token value."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "identity": { "type": "string", "description": "Optional identity name (e.g. \"claude\"). Omit to report/verify the active default identity." },
                "verify": { "type": "boolean", "description": "When true, make a real authenticated Plane read as the selected identity and report whether its token is currently valid (200) or rejected (401/403). Default false (config-only check, no network call)." }
            },
            "required": []
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let verify = args.get("verify").and_then(|v| v.as_bool()).unwrap_or(false);

        // verify=true: a real authenticated health check. Resolve the selected
        // identity (explicit `identity`, else the active default) and make one
        // lightweight read to prove the token is accepted vs rejected. The
        // response body is discarded — only the auth outcome is reported, never
        // a token value.
        if verify {
            let client = self.client.resolve_identity(&args)?;
            let name = client
                .identity_name()
                .map(|s| s.to_string())
                .or_else(|| {
                    args.get("identity")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_lowercase())
                })
                .unwrap_or_else(|| "default".to_string());
            // Bypass the GET cache so an expired token can't be masked by a
            // recent success within the cache TTL — always hit the network.
            let url = format!("{}projects/", client.workspace_url());
            let resp = client.get_with_retry(&url).await?;
            // Discriminate on the HTTP status itself (not on error-message text)
            // so only a genuine auth status reads as REJECTED; any other
            // non-success is surfaced as its real error.
            let status = resp.status();
            return if status.is_success() {
                Ok(format!(
                    "Plane identity '{name}': token VALID (authenticated read succeeded, 200)."
                ))
            } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                Ok(format!(
                    "Plane identity '{name}': token REJECTED (Plane returned {status} — the token \
                     is likely expired or revoked)."
                ))
            } else {
                // Non-auth failure (5xx, 422, …): surface the real ToolError.
                Err(PlaneClient::check_status(resp).await.unwrap_err())
            };
        }

        if let Some(identity) = args.get("identity").and_then(|v| v.as_str()) {
            let key = identity.trim().to_lowercase();
            let is_active_default = self.client.identity_name() == Some(key.as_str());
            if self.client.identities.contains_key(&key) || is_active_default {
                return Ok(format!("Identity '{identity}' is configured (token present)."));
            }
            return Err(ToolError::NotFound(format!(
                "No Plane identity named '{identity}' is configured (expected {PLANE_IDENTITY_PREFIX}{})",
                key.to_uppercase()
            )));
        }
        if !self.client.configured() {
            return Err(self.client.not_configured());
        }
        match self.client.identity_name() {
            Some(name) => Ok(format!("Active Plane identity: {name}")),
            None => Ok(
                "Active Plane identity: unknown (a default token is set but no PLANE_IDENTITY_NAME \
                 or matching PLANE_PAT_<NAME> is configured for it)"
                    .into(),
            ),
        }
    }
}

// ─── 28. plane_list_identities ───────────────────────────────────────────────

/// Lists the names of every configured `PLANE_PAT_<NAME>` identity so a caller
/// can see which identity it may act as before creating or assigning Plane work.
/// Names only — never token values, matching `plane_whoami`'s safety posture.
pub struct PlaneListIdentities {
    client: Arc<PlaneClient>,
}

#[async_trait]
impl RustTool for PlaneListIdentities {
    fn name(&self) -> &str { "plane_list_identities" }
    fn description(&self) -> &str {
        "List the names of all configured Plane identities (from PLANE_PAT_<NAME> environment vars) so you can see which identity to act as before creating or assigning Plane work. Returns names only, never token values, plus the active_default identity. Every Plane CRUD tool takes an optional `identity` argument set to one of these names to act AS that identity; omitting it uses the active default (PLANE_IDENTITY_NAME when it names a configured identity, otherwise the default PLANE_API_KEY). Use plane_whoami with verify=true to check whether a given identity's token is currently valid. Use the identity matching who should act on an item rather than always your own."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        // Derived from the client's already-scanned identities map (populated
        // once at start via `scan_named_identities`), so the list is exactly
        // what `for_identity()` can resolve — never a fresh, divergent env scan.
        let names = self.client.identity_names();
        let count = names.len();
        let active_default = self.client.identity_name().map(|s| s.to_string());
        let mut out = json!({
            "identities": names,
            "count": count,
            "active_default": active_default,
            "prefix": PLANE_IDENTITY_PREFIX,
        });
        if count == 0 {
            let note = if self.client.configured() {
                format!(
                    "No named identities configured; only the default PLANE_API_KEY identity is set. \
                     Provision named identities as {PLANE_IDENTITY_PREFIX}<NAME>."
                )
            } else {
                format!(
                    "No Plane identities configured. Provision named identities as \
                     {PLANE_IDENTITY_PREFIX}<NAME>."
                )
            };
            out["note"] = json!(note);
        }
        serde_json::to_string(&out)
            .map_err(|e| ToolError::Execution(format!("failed to serialize identity list: {e}")))
    }
}

// ─── Register all plane tools ─────────────────────────────────────────────────

/// Register all 28 Plane CE tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    let client = Arc::new(PlaneClient::from_env());

    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(PlaneListProjects { client: client.clone() }),
        Box::new(PlaneGetProject { client: client.clone() }),
        Box::new(PlaneListWorkItems { client: client.clone() }),
        Box::new(PlaneGetWorkItem { client: client.clone() }),
        Box::new(PlaneCreateWorkItem { client: client.clone() }),
        Box::new(PlaneUpdateWorkItem { client: client.clone() }),
        Box::new(PlaneDeleteWorkItem { client: client.clone() }),
        Box::new(PlaneListCycles { client: client.clone() }),
        Box::new(PlaneGetCycle { client: client.clone() }),
        Box::new(PlaneListCycleIssues { client: client.clone() }),
        Box::new(PlaneListModules { client: client.clone() }),
        Box::new(PlaneGetModule { client: client.clone() }),
        Box::new(PlaneCreateModule { client: client.clone() }),
        Box::new(PlaneListModuleIssues { client: client.clone() }),
        Box::new(PlaneListStates { client: client.clone() }),
        Box::new(PlaneListLabels { client: client.clone() }),
        Box::new(PlaneListMembers { client: client.clone() }),
        Box::new(PlaneListComments { client: client.clone() }),
        Box::new(PlaneCreateComment { client: client.clone() }),
        Box::new(PlaneListIssuesByState { client: client.clone() }),
        Box::new(PlaneGetIssueBySequence { client: client.clone() }),
        Box::new(PlaneListWorkItemsFiltered { client: client.clone() }),
        Box::new(PlaneListRecentActivity { client: client.clone() }),
        Box::new(PlaneCloseWorkItem { client: client.clone() }),
        Box::new(PlaneGetStateByName { client: client.clone() }),
        Box::new(PlaneBatchCreateWorkItems { client: client.clone() }),
        Box::new(PlaneWhoami { client: client.clone() }),
        Box::new(PlaneListIdentities { client: client.clone() }),
    ];

    for tool in tools {
        if let Err(e) = registry.register(tool) {
            tracing::warn!("Failed to register plane tool: {e}");
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    /// Build a PlaneClient pointing at the given mock server URL. Uses a
    /// zero-interval rate limiter so functional tests aren't slowed down by
    /// pacing — dedicated rate-limiting tests build their own `RateLimiter`
    /// with a real interval.
    fn mock_client(server: &MockServer) -> Arc<PlaneClient> {
        Arc::new(PlaneClient {
            http: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        })
    }

    /// Register a projects-list mock so `resolve_project_id` can map a non-UUID
    /// id/identifier back to itself. Matches on `id` (== `identifier` here).
    fn mock_projects(server: &MockServer, id: &str) {
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": id, "name": "Mock", "identifier": id, "network": 0}
            ]));
        });
    }

    // ── is_uuid helper ────────────────────────────────────────────────────────

    #[test]
    fn test_is_uuid_recognizes_canonical_uuid() {
        assert!(is_uuid("4ef3f3ec-e7ef-4af3-b258-881565e629f9")); // pii-test-fixture
        assert!(!is_uuid("LM"));
        assert!(!is_uuid("proj-abc"));
        assert!(!is_uuid("4ef3f3ec-e7ef-4af3-b258-881565e629f")); // 35 chars — pii-test-fixture
        assert!(!is_uuid("4ef3f3ecXe7ef-4af3-b258-881565e629f9")); // wrong separator — pii-test-fixture
    }

    // ── project identifier → UUID resolution ──────────────────────────────────

    #[tokio::test]
    async fn test_resolve_identifier_to_uuid_then_lists_issues() {
        let server = MockServer::start();
        // Resolution step: list projects, match identifier "LM".
        let projects_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "uuid-lm", "name": "Lumina Core", "identifier": "LM", "network": 0}
            ]));
        });
        // Issues fetched against the resolved UUID, not the identifier.
        let issues_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/uuid-lm/issues/");
            then.status(200).json_body(json!([
                {"id": "i1", "name": "Task", "project": "uuid-lm", "workspace": "testws", "sequence_id": 1}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListWorkItems { client };
        let result = tool.execute(json!({"project_id": "LM"})).await.unwrap();
        assert!(result.contains("Task"), "{result}");
        projects_mock.assert();
        issues_mock.assert();
    }

    // ── Not-configured guard ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_not_configured_when_env_absent() {
        // Client with no base_url
        let client = Arc::new(PlaneClient {
            http: Client::new(),
            base_url: None,
            api_key: None,
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "moosenet".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        });
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)),
            "Expected NotConfigured, got {err:?}");
    }

    // ── Auth header on all requests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_auth_header_sent_on_list_projects() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "test-api-key");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({})).await;
        mock.assert();
    }

    #[tokio::test]
    async fn test_auth_header_sent_on_create_work_item() {
        let server = MockServer::start();
        mock_projects(&server, "proj-1");
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/workspaces/testws/projects/proj-1/issues/")
                .header("x-api-key", "test-api-key");
            then.status(201).json_body(json!({
                "id": "issue-1",
                "name": "Test",
                "project": "proj-1",
                "workspace": "testws",
                "sequence_id": 1
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCreateWorkItem { client };
        let _ = tool.execute(json!({"project_id": "proj-1", "name": "Test"})).await;
        mock.assert();
    }

    // ── Correct HTTP methods and paths ────────────────────────────────────────

    #[tokio::test]
    async fn test_list_projects_get_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([
                {"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Alpha"), "Expected project name in output: {result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_get_project_by_id() {
        let server = MockServer::start();
        mock_projects(&server, "proj-abc");
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/proj-abc/");
            then.status(200).json_body(json!({
                "id": "proj-abc", "name": "My Project", "identifier": "MP", "network": 0
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({"project_id": "proj-abc"})).await.unwrap();
        assert!(result.contains("My Project"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_create_work_item_post_request() {
        let server = MockServer::start();
        mock_projects(&server, "proj-1");
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/workspaces/testws/projects/proj-1/issues/");
            then.status(201).json_body(json!({
                "id": "issue-99", "name": "Fix login bug",
                "project": "proj-1", "workspace": "testws", "sequence_id": 99
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCreateWorkItem { client };
        let result = tool.execute(json!({
            "project_id": "proj-1",
            "name": "Fix login bug",
            "priority": "high"
        })).await.unwrap();
        assert!(result.contains("Fix login bug"), "{result}");
        assert!(result.contains("99"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_update_work_item_patch_request() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(200).json_body(json!({
                "id": "i1", "name": "Updated name",
                "project": "p1", "workspace": "testws"
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneUpdateWorkItem { client };
        let result = tool.execute(json!({
            "project_id": "p1",
            "issue_id": "i1",
            "name": "Updated name"
        })).await.unwrap();
        assert!(result.contains("Updated name"), "{result}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_delete_work_item_delete_request() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::DELETE).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(204);
        });
        let client = mock_client(&server);
        let tool = PlaneDeleteWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await.unwrap();
        assert!(result.contains("i1"), "{result}");
        mock.assert();
    }

    // ── 429 retry logic ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_429_returns_rate_limit_error_after_3_attempts() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(429);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("rate limit") || err.contains("HTTP error"),
            "Expected rate limit error, got: {err}");
        assert!(mock.hits() >= 3, "Expected at least 3 retries, got {}", mock.hits());
    }

    // ── 404 → NotFound error ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_404_returns_not_found_error() {
        let server = MockServer::start();
        mock_projects(&server, "bad-id");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/bad-id/");
            then.status(404).body("Not found");
        });
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({"project_id": "bad-id"})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    // ── Missing required argument ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_missing_required_arg_returns_invalid_argument() {
        let server = MockServer::start();
        let client = mock_client(&server);
        let tool = PlaneGetProject { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_update_with_no_fields_returns_error() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let client = mock_client(&server);
        let tool = PlaneUpdateWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
    }

    // ── Empty response handled gracefully ─────────────────────────────────────

    #[tokio::test]
    async fn test_empty_project_list_returns_message() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("No projects"), "{result}");
    }

    // ── register() populates 28 tools ─────────────────────────────────────────

    #[test]
    fn test_register_all_plane_tools() {
        // Temporarily set env vars so client.configured() is true-ish
        // (not required for registration, only for execution)
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 28,
            "Expected 28 plane tools, got {}", registry.len());
    }

    #[test]
    fn test_all_plane_tool_names_unique() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        let names: Vec<String> = registry.list().iter().map(|t| t.name.clone()).collect();
        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(names.len(), deduped.len(),
            "Duplicate tool names found: {:?}", names);
    }

    #[test]
    fn test_all_plane_tools_have_descriptions() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        for info in registry.list() {
            assert!(!info.description.is_empty(),
                "Tool '{}' has empty description", info.name);
        }
    }

    #[test]
    fn test_all_plane_tools_have_valid_parameters_schema() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        for info in registry.list() {
            assert_eq!(info.parameters["type"], "object",
                "Tool '{}' parameters schema should have type: object", info.name);
        }
    }

    // ── Filter by state group (client-side) ───────────────────────────────────

    #[tokio::test]
    async fn test_list_issues_by_state_filters_correctly() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([
                {
                    "id": "i1", "name": "Open task",
                    "project": "p1", "workspace": "testws",
                    "state_detail": {"id": "s1", "name": "In Progress", "color": "#fff", "group": "started"}
                },
                {
                    "id": "i2", "name": "Done task",
                    "project": "p1", "workspace": "testws",
                    "state_detail": {"id": "s2", "name": "Done", "color": "#0f0", "group": "completed"}
                }
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneListIssuesByState { client };
        let result = tool.execute(json!({"project_id": "p1", "state_group": "started"})).await.unwrap();
        assert!(result.contains("Open task"), "{result}");
        assert!(!result.contains("Done task"), "{result}");
    }

    // ── Paginated response ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_paginated_response_parsed_correctly() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!({
                "count": 2,
                "next": null,
                "previous": null,
                "results": [
                    {"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0},
                    {"id": "p2", "name": "Beta", "identifier": "BT", "network": 0}
                ]
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Alpha"), "{result}");
        assert!(result.contains("Beta"), "{result}");
    }

    // ── close_work_item fetches states then patches ───────────────────────────

    #[tokio::test]
    async fn test_close_work_item_uses_completed_state() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let _states_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/states/");
            then.status(200).json_body(json!([
                {"id": "s-done", "name": "Done", "color": "#0f0", "group": "completed", "project": "p1"},
                {"id": "s-todo", "name": "Todo", "color": "#fff", "group": "unstarted", "project": "p1"}
            ]));
        });
        let _patch_mock = server.mock(|when, then| {
            when.method(httpmock::Method::PATCH).path("/api/v1/workspaces/testws/projects/p1/issues/i1/");
            then.status(200).json_body(json!({
                "id": "i1", "name": "My task",
                "project": "p1", "workspace": "testws",
                "state": "s-done"
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneCloseWorkItem { client };
        let result = tool.execute(json!({"project_id": "p1", "issue_id": "i1"})).await.unwrap();
        assert!(result.contains("Done") || result.contains("My task"), "{result}");
    }

    // ── get_issue_by_sequence finds correct issue ─────────────────────────────

    #[tokio::test]
    async fn test_get_issue_by_sequence_found() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([
                {"id": "i1", "name": "Task A", "sequence_id": 1, "project": "p1", "workspace": "testws"},
                {"id": "i42", "name": "Task B", "sequence_id": 42, "project": "p1", "workspace": "testws"}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneGetIssueBySequence { client };
        let result = tool.execute(json!({"project_id": "p1", "sequence_id": 42})).await.unwrap();
        assert!(result.contains("Task B"), "{result}");
        assert!(result.contains("42"), "{result}");
    }

    #[tokio::test]
    async fn test_get_issue_by_sequence_not_found() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(200).json_body(json!([]));
        });
        let client = mock_client(&server);
        let tool = PlaneGetIssueBySequence { client };
        let result = tool.execute(json!({"project_id": "p1", "sequence_id": 99})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    // ── New tools: state-by-name, batch create, whoami ───────────────────────

    #[tokio::test]
    async fn test_get_state_by_name_case_insensitive() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/p1/states/");
            then.status(200).json_body(json!([
                {"id": "s-done", "name": "Done", "color": "#0f0", "group": "completed", "project": "p1"}
            ]));
        });
        let client = mock_client(&server);
        let tool = PlaneGetStateByName { client };
        let result = tool.execute(json!({"project_id": "p1", "name": "done"})).await.unwrap();
        assert!(result.contains("s-done"), "{result}");
    }

    #[tokio::test]
    async fn test_batch_create_work_items_creates_each_and_reports_all() {
        let server = MockServer::start();
        mock_projects(&server, "p1");
        let post_mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/workspaces/testws/projects/p1/issues/");
            then.status(201).json_body(json!({
                "id": "generated", "name": "generated", "project": "p1", "workspace": "testws", "sequence_id": 1
            }));
        });
        let client = mock_client(&server);
        let tool = PlaneBatchCreateWorkItems { client };
        let result = tool.execute(json!({
            "project_id": "p1",
            "items": [{"name": "Task A"}, {"name": "Task B"}, {"name": "Task C"}]
        })).await.unwrap();
        assert!(result.contains("Batch-created 3"), "{result}");
        assert_eq!(post_mock.hits(), 3, "Expected one POST per item");
    }

    #[tokio::test]
    async fn test_batch_create_rejects_empty_items() {
        let server = MockServer::start();
        let client = mock_client(&server);
        let tool = PlaneBatchCreateWorkItems { client };
        let result = tool.execute(json!({"project_id": "p1", "items": []})).await;
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    // ── Rate limiting: proves actual pacing, not just that a sleep call exists ──

    #[tokio::test]
    async fn test_rate_limiter_enforces_minimum_interval_between_calls() {
        let interval = Duration::from_millis(250);
        let limiter = RateLimiter::local(interval);

        let start = Instant::now();
        for _ in 0..4 {
            limiter.acquire().await;
        }
        let elapsed = start.elapsed();

        // 4 calls through the gate = 3 enforced gaps of `interval`.
        let expected_min = interval * 3;
        assert!(
            elapsed >= expected_min,
            "Expected at least {expected_min:?} elapsed across 4 paced calls, got {elapsed:?}"
        );
        // Generous ceiling to catch a limiter that isn't pacing at all (e.g. sleeping way too long).
        assert!(
            elapsed < expected_min + Duration::from_millis(500),
            "Elapsed {elapsed:?} far exceeds expected pacing — limiter may be broken"
        );
    }

    #[tokio::test]
    async fn test_rate_limiter_paces_real_http_calls_through_client() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([]));
        });
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::from_millis(200))),
            cache: Arc::new(GetCache::new(Duration::from_millis(1))), // effectively disabled
        };

        let start = Instant::now();
        for _ in 0..3 {
            let url = format!("{}projects/", client.workspace_url());
            let _ = client.get_with_retry(&url).await.unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(400),
            "3 real HTTP calls through a 200ms-paced client should take >= 400ms, got {elapsed:?}"
        );
    }

    // ── GET caching: proves a second call within TTL skips the network ───────

    #[tokio::test]
    async fn test_get_json_cached_serves_second_call_from_cache_within_ttl() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([{"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0}]));
        });
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache::new(Duration::from_millis(300))),
        };
        let url = format!("{}projects/", client.workspace_url());

        let first = client.get_json_cached(&url).await.unwrap();
        let second = client.get_json_cached(&url).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(mock.hits(), 1, "Second call within TTL must be served from cache, not the network");
    }

    #[tokio::test]
    async fn test_get_json_cached_refetches_after_ttl_expiry() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([{"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0}]));
        });
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache::new(Duration::from_millis(100))),
        };
        let url = format!("{}projects/", client.workspace_url());

        let _ = client.get_json_cached(&url).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = client.get_json_cached(&url).await.unwrap();

        assert_eq!(mock.hits(), 2, "A GET after TTL expiry must hit the network again");
    }

    // ── Retry/backoff: real mocked failure modes ──────────────────────────────

    #[tokio::test]
    async fn test_429_respects_retry_after_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(429).header("Retry-After", "1");
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };

        let start = Instant::now();
        let result = tool.execute(json!({})).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert_eq!(mock.hits(), 3, "Expected exactly 3 attempts");
        // 2 waits of 1s (Retry-After) between the 3 attempts.
        assert!(elapsed >= Duration::from_millis(1900), "Expected >= ~2s from Retry-After pacing, got {elapsed:?}");
        assert!(elapsed < Duration::from_secs(8), "Retry-After should be used instead of the larger backoff table, got {elapsed:?}");
    }

    #[tokio::test]
    async fn test_5xx_retries_then_fails() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(503);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert_eq!(mock.hits(), 3, "Expected 3 attempts on repeated 5xx");
    }

    #[tokio::test]
    async fn test_network_error_retries_then_fails() {
        // Nothing is listening on this port — every attempt is a connection error.
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_millis(300)).build().unwrap(),
            base_url: Some("http://127.0.0.1:1".into()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        };
        let tool = PlaneListProjects { client: Arc::new(client) };
        let result = tool.execute(json!({})).await;
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::Http(_)), "{err:?}");
        assert!(err.to_string().contains("3 attempts"), "Expected retry-exhaustion message, got: {err}");
    }

    #[tokio::test]
    async fn test_401_does_not_retry() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(401);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result.unwrap_err(), ToolError::Http(_)));
        assert_eq!(mock.hits(), 1, "401 must never be retried");
    }

    #[tokio::test]
    async fn test_403_does_not_retry() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(403);
        });
        let client = mock_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({})).await;
        assert!(matches!(result.unwrap_err(), ToolError::Http(_)));
        assert_eq!(mock.hits(), 1, "403 must never be retried");
    }

    // ── Multi-identity: no cross-contamination, correct attribution ──────────

    /// Build a client with a default token plus two named identities, all
    /// sharing one mock server.
    fn multi_identity_client(server: &MockServer) -> Arc<PlaneClient> {
        let mut identities = HashMap::new();
        identities.insert("axon".to_string(), "token-axon".to_string());
        identities.insert("vigil".to_string(), "token-vigil".to_string());
        Arc::new(PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("token-default".into()),
            identity_name: Some("default".into()),
            identities: Arc::new(identities),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        })
    }

    #[tokio::test]
    async fn test_for_identity_uses_correct_token_per_identity_no_cross_contamination() {
        let server = MockServer::start();
        let axon_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(200).json_body(json!([]));
        });
        let vigil_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-vigil");
            then.status(200).json_body(json!([]));
        });

        let base = multi_identity_client(&server);
        let axon_client = base.for_identity("axon").unwrap();
        let vigil_client = base.for_identity("VIGIL").unwrap(); // case-insensitive lookup

        assert_eq!(axon_client.identity_name(), Some("axon"));
        assert_eq!(vigil_client.identity_name(), Some("vigil"));

        let axon_url = format!("{}projects/", axon_client.workspace_url());
        let _ = axon_client.get_with_retry(&axon_url).await.unwrap();
        let vigil_url = format!("{}projects/", vigil_client.workspace_url());
        let _ = vigil_client.get_with_retry(&vigil_url).await.unwrap();

        // Each identity's request must have hit ONLY the mock matching its own
        // token — proving no cross-identity token leakage.
        assert_eq!(axon_mock.hits(), 1);
        assert_eq!(vigil_mock.hits(), 1);
    }

    #[tokio::test]
    async fn test_for_identity_shared_cache_does_not_leak_across_identities() {
        // Two identities sharing the same GetCache Arc (as for_identity always
        // shares it) must never be served each other's cached response for the
        // same URL — Plane GET responses are not uniformly workspace-scoped
        // (e.g. project visibility varies by the calling token's membership),
        // so this exercises the exact path a URL-only cache key would leak.
        let server = MockServer::start();
        let axon_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(200).json_body(json!([
                {"id": "axon-only-project", "name": "Axon's Project", "identifier": "AX", "network": 0}
            ]));
        });
        let vigil_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-vigil");
            then.status(200).json_body(json!([
                {"id": "vigil-only-project", "name": "Vigil's Project", "identifier": "VG", "network": 0}
            ]));
        });

        let base = multi_identity_client(&server);
        let axon_client = base.for_identity("axon").unwrap();
        let vigil_client = base.for_identity("vigil").unwrap();
        assert!(Arc::ptr_eq(&axon_client.cache, &vigil_client.cache), "test setup must share one cache Arc");

        let url = format!("{}projects/", axon_client.workspace_url());

        // Axon populates the shared cache first.
        let axon_body = axon_client.get_json_cached(&url).await.unwrap();
        assert!(axon_body.contains("Axon's Project"));

        // Vigil requests the SAME url within the same TTL window. A URL-only
        // cache key would return Axon's cached body here without ever
        // reaching the network with Vigil's own token.
        let vigil_body = vigil_client.get_json_cached(&url).await.unwrap();
        assert!(vigil_body.contains("Vigil's Project"), "Vigil must get its own data, got: {vigil_body}");
        assert!(!vigil_body.contains("Axon's Project"), "Vigil must never see Axon's cached response");

        assert_eq!(axon_mock.hits(), 1, "Axon's own request should hit the network once");
        assert_eq!(vigil_mock.hits(), 1, "Vigil must make its own network request, not reuse Axon's cache entry");
    }

    #[tokio::test]
    async fn test_for_identity_unknown_name_returns_error() {
        let server = MockServer::start();
        let base = multi_identity_client(&server);
        let result = base.for_identity("nonexistent-agent");
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_plane_whoami_reports_active_default_identity() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("default"), "{result}");
    }

    #[tokio::test]
    async fn test_plane_whoami_checks_named_identity_configured() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let result = tool.execute(json!({"identity": "axon"})).await.unwrap();
        assert!(result.contains("configured"), "{result}");
    }

    #[tokio::test]
    async fn test_plane_whoami_unknown_identity_not_found() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let result = tool.execute(json!({"identity": "ghost"})).await;
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_resolves_identity_name_from_matching_token() {
        // Isolate from other tests / the real environment via serial_test,
        // since PlaneClient::from_env() reads process-wide env vars.
        std::env::set_var("PLANE_API_URL", "http://example.invalid");
        std::env::set_var("PLANE_API_KEY", "shared-token-value");
        std::env::set_var("PLANE_PAT_SEER", "shared-token-value");
        std::env::remove_var("PLANE_IDENTITY_NAME");

        let client = PlaneClient::from_env();
        assert_eq!(client.identity_name(), Some("seer"));

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var("PLANE_PAT_SEER");
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_registers_plane_pat_named_identity() {
        // Positive: a PLANE_PAT_<NAME> var IS recognized as a named identity.
        std::env::set_var("PLANE_API_URL", "http://example.invalid");
        std::env::set_var("PLANE_API_KEY", "default-token");
        std::env::set_var("PLANE_PAT_CLAUDE", "claude-token");
        std::env::remove_var("PLANE_IDENTITY_NAME");

        let client = PlaneClient::from_env();
        assert!(client.for_identity("claude").is_ok());
        assert!(
            client.identity_names().contains(&"claude".to_string()),
            "PLANE_PAT_CLAUDE must register the 'claude' identity, got {:?}",
            client.identity_names()
        );

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var("PLANE_PAT_CLAUDE");
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_ignores_retired_plane_api_key_prefix() {
        // Negative (breaking-rename proof): the OLD named-identity prefix must
        // NO LONGER be recognized — this proves the old behavior is gone, not
        // merely that the new behavior works. The retired var name is assembled
        // from parts on purpose: the literal retired prefix must not appear
        // verbatim anywhere in the source tree (the rename is complete).
        let retired_var = format!("{}{}", "PLANE_API_KEY", "_FOO");
        std::env::set_var("PLANE_API_URL", "http://example.invalid");
        std::env::set_var("PLANE_API_KEY", "default-token");
        std::env::set_var(&retired_var, "should-be-ignored");
        std::env::remove_var("PLANE_PAT_FOO");
        std::env::remove_var("PLANE_IDENTITY_NAME");

        let client = PlaneClient::from_env();
        assert!(
            matches!(client.for_identity("foo").unwrap_err(), ToolError::InvalidArgument(_)),
            "retired prefix must not resolve as a named identity"
        );
        assert!(
            !client.identity_names().contains(&"foo".to_string()),
            "retired prefix must not populate the identities map, got {:?}",
            client.identity_names()
        );

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var(&retired_var);
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_default_plane_api_key_is_not_a_named_identity() {
        // Edge case: the unsuffixed default PLANE_API_KEY (no trailing '_')
        // and unrelated PLANE_* vars must never be mis-scanned as identities.
        std::env::set_var("PLANE_API_URL", "http://example.invalid");
        std::env::set_var("PLANE_API_KEY", "default-token");
        std::env::remove_var("PLANE_IDENTITY_NAME");
        // Ensure no stray PAT vars from other tests remain.
        std::env::remove_var("PLANE_PAT_CLAUDE");
        std::env::remove_var("PLANE_PAT_SEER");

        let client = PlaneClient::from_env();
        // With only the default configured and no PLANE_PAT_* set, there must
        // be no named identity derived from PLANE_API_KEY / PLANE_API_URL.
        assert!(
            client.for_identity("api").is_err() && client.for_identity("url").is_err(),
            "default/URL vars must not leak in as named identities"
        );

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_API_KEY");
    }

    // ── plane_list_identities (PPAT-02) ───────────────────────────────────────

    #[tokio::test]
    async fn test_plane_list_identities_lists_sorted_names() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneListIdentities { client };
        let out = tool.execute(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["identities"], json!(["axon", "vigil"]), "names must be sorted/stable");
        assert_eq!(v["count"], 2);
        assert_eq!(v["active_default"], "default");
    }

    #[tokio::test]
    async fn test_plane_list_identities_never_leaks_token_values() {
        // The multi_identity_client's tokens are token-axon/token-vigil/
        // token-default — none may appear in the serialized output.
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneListIdentities { client };
        let out = tool.execute(json!({})).await.unwrap();
        assert!(!out.contains("token-axon"), "must not leak a token value: {out}");
        assert!(!out.contains("token-vigil"), "must not leak a token value: {out}");
        assert!(!out.contains("token-default"), "must not leak a token value: {out}");
    }

    #[tokio::test]
    async fn test_plane_list_identities_empty_returns_note_not_error() {
        // 0 named identities but a default token set: empty list + note, not error.
        let server = MockServer::start();
        let client = Arc::new(PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("token-default".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache::new(Duration::from_secs(5))),
        });
        let tool = PlaneListIdentities { client };
        let out = tool.execute(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["count"], 0);
        assert_eq!(v["identities"], json!([]));
        assert!(v.get("note").is_some(), "0-case must carry a note, got {out}");
    }

    // ── Acting AS an identity through CRUD tools (identity arg dispatch) ───────

    #[tokio::test]
    async fn test_crud_tool_acts_as_explicit_identity() {
        // A CRUD call with `identity` must authenticate as that PLANE_PAT_<NAME>
        // token, NOT the default — the core of the multi-identity requirement.
        let server = MockServer::start();
        let axon_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(200).json_body(json!([]));
        });
        let default_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-default");
            then.status(200).json_body(json!([]));
        });
        let client = multi_identity_client(&server);
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({"identity": "axon"})).await.unwrap();
        assert_eq!(axon_mock.hits(), 1, "explicit identity must act as PLANE_PAT_AXON");
        assert_eq!(default_mock.hits(), 0, "explicit identity must NOT use the default token");
    }

    #[tokio::test]
    async fn test_crud_tool_explicit_identity_is_case_insensitive() {
        let server = MockServer::start();
        let vigil_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-vigil");
            then.status(200).json_body(json!([]));
        });
        let client = multi_identity_client(&server);
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({"identity": "VIGIL"})).await.unwrap();
        assert_eq!(vigil_mock.hits(), 1, "identity lookup must be case-insensitive");
    }

    #[tokio::test]
    async fn test_crud_tool_default_identity_uses_default_token() {
        // Omitting `identity` must use the active default token — backward compat.
        let server = MockServer::start();
        let default_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-default");
            then.status(200).json_body(json!([]));
        });
        let client = multi_identity_client(&server);
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({})).await.unwrap();
        assert_eq!(default_mock.hits(), 1, "omitting identity must use the active default token");
    }

    #[tokio::test]
    async fn test_crud_tool_empty_identity_falls_back_to_default() {
        // A present-but-empty `identity` string must behave like omitting it.
        let server = MockServer::start();
        let default_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-default");
            then.status(200).json_body(json!([]));
        });
        let client = multi_identity_client(&server);
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({"identity": "  "})).await.unwrap();
        assert_eq!(default_mock.hits(), 1, "blank identity must fall back to the default");
    }

    #[tokio::test]
    async fn test_crud_tool_unknown_identity_returns_invalid_argument() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneListProjects { client };
        let result = tool.execute(json!({"identity": "ghost"})).await;
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_crud_write_tool_acts_as_explicit_identity() {
        // The identity dispatch also covers write tools (POST), not just reads.
        let server = MockServer::start();
        // resolve_project_id lists projects with the acting identity's token.
        let projects_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(200).json_body(json!([
                {"id": "proj-1", "name": "Mock", "identifier": "proj-1", "network": 0}
            ]));
        });
        let create_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/workspaces/testws/projects/proj-1/issues/")
                .header("x-api-key", "token-axon");
            then.status(201).json_body(json!({
                "id": "issue-1", "name": "T", "project": "proj-1",
                "workspace": "testws", "sequence_id": 1
            }));
        });
        let client = multi_identity_client(&server);
        let tool = PlaneCreateWorkItem { client };
        let _ = tool
            .execute(json!({"project_id": "proj-1", "name": "T", "identity": "axon"}))
            .await
            .unwrap();
        projects_mock.assert();
        create_mock.assert();
    }

    #[tokio::test]
    async fn test_crud_tools_expose_optional_identity_param() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        // Representative sample across list/create/update tools.
        let create = PlaneCreateWorkItem { client: client.clone() };
        let list = PlaneListProjects { client: client.clone() };
        let update = PlaneUpdateWorkItem { client: client.clone() };
        for schema in [create.parameters(), list.parameters(), update.parameters()] {
            assert_eq!(
                schema["properties"]["identity"]["type"], "string",
                "every CRUD tool must expose a string `identity` param: {schema}"
            );
            let required = schema["required"].as_array().unwrap();
            assert!(
                !required.iter().any(|r| r == "identity"),
                "`identity` must remain optional (never in `required`): {schema}"
            );
        }
    }

    // ── Default routing via PLANE_IDENTITY_NAME (from_env) ────────────────────

    #[tokio::test]
    #[serial]
    async fn test_from_env_default_identity_name_routes_to_named_token() {
        // 'default = lumina' must make the DEFAULT (no-identity-arg) path
        // genuinely authenticate with PLANE_PAT_LUMINA's token, not PLANE_API_KEY.
        let server = MockServer::start();
        let lumina_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "lumina-token");
            then.status(200).json_body(json!([]));
        });
        std::env::set_var("PLANE_API_URL", server.base_url());
        std::env::set_var("PLANE_WORKSPACE", "testws");
        std::env::set_var("PLANE_API_KEY", "default-token");
        std::env::set_var("PLANE_PAT_LUMINA", "lumina-token");
        std::env::set_var("PLANE_IDENTITY_NAME", "lumina");

        let client = Arc::new(PlaneClient::from_env());
        assert_eq!(client.identity_name(), Some("lumina"));
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({})).await.unwrap();
        lumina_mock.assert();

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_WORKSPACE");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var("PLANE_PAT_LUMINA");
        std::env::remove_var("PLANE_IDENTITY_NAME");
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_identity_name_without_matching_pat_falls_back_to_default() {
        // PLANE_IDENTITY_NAME set as a pure label with NO matching PLANE_PAT_
        // must still authenticate with the unsuffixed PLANE_API_KEY (back-compat).
        let server = MockServer::start();
        let default_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "default-token");
            then.status(200).json_body(json!([]));
        });
        std::env::set_var("PLANE_API_URL", server.base_url());
        std::env::set_var("PLANE_WORKSPACE", "testws");
        std::env::set_var("PLANE_API_KEY", "default-token");
        std::env::set_var("PLANE_IDENTITY_NAME", "nobody");
        std::env::remove_var("PLANE_PAT_NOBODY");
        std::env::remove_var("PLANE_PAT_LUMINA");
        std::env::remove_var("PLANE_PAT_CLAUDE");
        std::env::remove_var("PLANE_PAT_SEER");

        let client = Arc::new(PlaneClient::from_env());
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({})).await.unwrap();
        default_mock.assert();

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_WORKSPACE");
        std::env::remove_var("PLANE_API_KEY");
        std::env::remove_var("PLANE_IDENTITY_NAME");
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_backward_compat_only_default_key_acts_as_default() {
        // The classic single-token deployment (only PLANE_API_KEY, no named
        // default) must be completely unchanged.
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "only-default-token");
            then.status(200).json_body(json!([]));
        });
        std::env::set_var("PLANE_API_URL", server.base_url());
        std::env::set_var("PLANE_WORKSPACE", "testws");
        std::env::set_var("PLANE_API_KEY", "only-default-token");
        std::env::remove_var("PLANE_IDENTITY_NAME");
        std::env::remove_var("PLANE_PAT_LUMINA");
        std::env::remove_var("PLANE_PAT_CLAUDE");
        std::env::remove_var("PLANE_PAT_SEER");

        let client = Arc::new(PlaneClient::from_env());
        assert_eq!(client.identity_name(), None, "no named default without PLANE_IDENTITY_NAME");
        assert!(client.identity_names().is_empty(), "no named identities configured");
        let tool = PlaneListProjects { client };
        let _ = tool.execute(json!({})).await.unwrap();
        mock.assert();

        std::env::remove_var("PLANE_API_URL");
        std::env::remove_var("PLANE_WORKSPACE");
        std::env::remove_var("PLANE_API_KEY");
    }

    // ── plane_whoami verify=true: real per-identity health check ──────────────

    #[tokio::test]
    async fn test_plane_whoami_verify_reports_valid_for_accepted_token() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(200).json_body(json!([]));
        });
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let out = tool.execute(json!({"identity": "axon", "verify": true})).await.unwrap();
        assert!(out.contains("VALID"), "{out}");
        assert!(out.contains("axon"), "{out}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_plane_whoami_verify_reports_rejected_for_403() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-vigil");
            then.status(403);
        });
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let out = tool.execute(json!({"identity": "vigil", "verify": true})).await.unwrap();
        assert!(out.contains("REJECTED"), "an expired/revoked token must read as REJECTED: {out}");
    }

    #[tokio::test]
    async fn test_plane_whoami_verify_default_identity_no_arg() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-default");
            then.status(200).json_body(json!([]));
        });
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let out = tool.execute(json!({"verify": true})).await.unwrap();
        assert!(out.contains("VALID"), "{out}");
        assert!(out.contains("default"), "{out}");
        mock.assert();
    }

    #[tokio::test]
    async fn test_plane_whoami_verify_unknown_identity_returns_invalid_argument() {
        let server = MockServer::start();
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let result = tool.execute(json!({"identity": "ghost", "verify": true})).await;
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_plane_whoami_verify_non_auth_failure_is_error_not_rejected() {
        // A 5xx during verify must surface as a real error, never be mislabeled
        // as a REJECTED (expired-token) result — status-based discrimination.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(500);
        });
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let result = tool.execute(json!({"identity": "axon", "verify": true})).await;
        assert!(matches!(result.unwrap_err(), ToolError::Http(_)), "5xx must be an error, not REJECTED");
    }

    #[tokio::test]
    async fn test_plane_whoami_verify_never_leaks_token_value() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/workspaces/testws/projects/")
                .header("x-api-key", "token-axon");
            then.status(200).json_body(json!([]));
        });
        let client = multi_identity_client(&server);
        let tool = PlaneWhoami { client };
        let out = tool.execute(json!({"identity": "axon", "verify": true})).await.unwrap();
        assert!(!out.contains("token-axon"), "verify output must never leak a token value: {out}");
    }

    // ── S100: Redis-backed cache + rate limiter ──────────────────────────────

    // Namespaced key hashing: no credential/URL plaintext, per-token isolation.
    #[test]
    fn test_redis_cache_key_namespaced_and_hides_token_and_url() {
        // Raw key mirrors PlaneClient::cache_key: token + NUL + url.
        let raw = format!("{}\u{0}{}", "super-secret-token-value", "http://example.invalid/api/v1/projects/");
        let k = redis_cache_key(&raw);
        assert!(k.starts_with("plane:cache:"), "must be namespaced: {k}");
        assert!(!k.contains("super-secret-token-value"), "must not leak the token: {k}");
        assert!(!k.contains("example.invalid"), "must not leak the URL: {k}");
        assert!(!k.contains('/'), "hashed key must contain no URL path chars: {k}");
    }

    #[test]
    fn test_redis_cache_key_isolates_per_token() {
        let url = "http://example.invalid/api/v1/workspaces/testws/projects/";
        let a = redis_cache_key(&format!("{}\u{0}{}", "token-axon", url));
        let b = redis_cache_key(&format!("{}\u{0}{}", "token-vigil", url));
        assert_ne!(a, b, "same URL under two identities must hash to different keys");
        // Same token + url is stable (a real cache hit for the same identity).
        let a2 = redis_cache_key(&format!("{}\u{0}{}", "token-axon", url));
        assert_eq!(a, a2);
    }

    // Circuit breaker: opens after threshold, half-opens after cooldown, warns once.
    #[test]
    fn test_circuit_breaker_opens_after_threshold_then_half_opens() {
        let cb = CircuitBreaker::new();
        assert!(cb.allow(), "starts closed");
        // Below threshold: stays closed, no warning yet.
        for _ in 0..(REDIS_FAILURE_THRESHOLD - 1) {
            assert!(!cb.record_failure());
            assert!(cb.allow());
        }
        // The threshold-th failure trips it open and returns true exactly once.
        assert!(cb.record_failure(), "tripping failure warns once");
        assert!(!cb.allow(), "open breaker blocks attempts during cooldown");
        // A success closes it and re-arms the single-warning latch.
        cb.record_success();
        assert!(cb.allow(), "success closes the breaker");
    }

    #[test]
    fn test_circuit_breaker_half_open_allows_only_one_probe() {
        let cb = CircuitBreaker::new();
        for _ in 0..REDIS_FAILURE_THRESHOLD {
            cb.record_failure();
        }
        assert!(!cb.allow(), "open during cooldown");
        // Simulate the cooldown elapsing.
        cb.test_expire_cooldown();
        // Exactly ONE caller gets the half-open probe; concurrent callers that
        // race in before the probe records its result stay blocked (no herd).
        assert!(cb.allow(), "first caller after cooldown gets the single probe");
        assert!(!cb.allow(), "concurrent caller must NOT also probe");
        assert!(!cb.allow(), "still single-probe reserved");
        // A successful probe fully closes the breaker again.
        cb.record_success();
        assert!(cb.allow());
    }

    #[test]
    fn test_circuit_breaker_warns_once_per_episode() {
        let cb = CircuitBreaker::new();
        let mut warns = 0;
        // Many consecutive failures must warn exactly once (until a success).
        for _ in 0..(REDIS_FAILURE_THRESHOLD + 5) {
            if cb.record_failure() {
                warns += 1;
            }
        }
        assert_eq!(warns, 1, "exactly one degradation warning per outage episode");
    }

    // from_env: unset PLANE_REDIS_URL → no backend (pure in-process default).
    #[tokio::test]
    #[serial]
    async fn test_redis_backend_from_env_none_when_unset() {
        std::env::remove_var("PLANE_REDIS_URL");
        assert!(RedisBackend::from_env().is_none(), "no backend without PLANE_REDIS_URL");
        std::env::set_var("PLANE_REDIS_URL", "   ");
        assert!(RedisBackend::from_env().is_none(), "blank PLANE_REDIS_URL is treated as unset");
        std::env::remove_var("PLANE_REDIS_URL");
    }

    #[tokio::test]
    #[serial]
    async fn test_redis_backend_from_env_rejects_invalid_url() {
        std::env::set_var("PLANE_REDIS_URL", "not a redis url");
        assert!(RedisBackend::from_env().is_none(), "invalid URL falls back to in-process, no panic");
        std::env::remove_var("PLANE_REDIS_URL");
    }

    // Debug must never leak the client/ConnectionInfo (which can carry a password).
    #[test]
    fn test_redis_backend_debug_redacts_client() {
        let backend = RedisBackend::test_unreachable();
        let dbg = format!("{backend:?}");
        assert!(dbg.contains("RedisBackend"));
        assert!(!dbg.to_lowercase().contains("password"), "Debug must not mention a password field: {dbg}");
        assert!(!dbg.contains("localhost"), "Debug must not print the connection target: {dbg}");
    }

    // Fail-open: a configured-but-unreachable Redis must never block a cache op —
    // cache_get returns None fast, and GetCache serves/populates in-process.
    #[tokio::test]
    async fn test_get_cache_fails_open_when_redis_unreachable() {
        let cache = GetCache {
            entries: AsyncMutex::new(HashMap::new()),
            ttl: Duration::from_secs(5),
            redis: Some(RedisBackend::test_unreachable()),
        };
        // set() must not hang or panic even though Redis is down; it writes
        // through to the in-process map, which get() then serves.
        let start = Instant::now();
        cache.set("tok\u{0}http://example.invalid/x".to_string(), "cached-body".to_string()).await;
        let got = cache.get("tok\u{0}http://example.invalid/x").await;
        assert_eq!(got.as_deref(), Some("cached-body"), "in-process fallback must serve the value");
        assert!(start.elapsed() < Duration::from_secs(2), "fail-open must be fast, not blocking on Redis");
    }

    // Fail-open: an unreachable Redis rate reservation must fall through to the
    // in-process gate — acquire() still paces correctly, never blocks forever.
    #[tokio::test]
    async fn test_rate_limiter_fails_open_to_local_when_redis_unreachable() {
        let interval = Duration::from_millis(150);
        let limiter = RateLimiter {
            last: AsyncMutex::new(None),
            min_interval: interval,
            redis: Some(RedisBackend::test_unreachable()),
        };
        // Two paced calls: Redis is down, so each acquire pays at most one short
        // op timeout then uses the local gate — total is bounded and the local
        // min_interval spacing is still enforced on the second call.
        let start = Instant::now();
        limiter.acquire().await;
        limiter.acquire().await;
        let elapsed = start.elapsed();
        assert!(elapsed >= interval, "local pacing must still apply on fallback, got {elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "fail-open must not hang on a dead Redis, got {elapsed:?}");
    }

    // End-to-end fail-open through the real GET path: a client whose cache has a
    // dead Redis backend still fetches over the network and returns data.
    #[tokio::test]
    async fn test_get_json_cached_works_with_dead_redis_backend() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/workspaces/testws/projects/");
            then.status(200).json_body(json!([{"id": "p1", "name": "Alpha", "identifier": "AL", "network": 0}]));
        });
        let client = PlaneClient {
            http: Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base_url: Some(server.base_url()),
            api_key: Some("test-api-key".into()),
            identity_name: None,
            identities: Arc::new(HashMap::new()),
            workspace: "testws".into(),
            rate_limiter: Arc::new(RateLimiter::local(Duration::ZERO)),
            cache: Arc::new(GetCache {
                entries: AsyncMutex::new(HashMap::new()),
                ttl: Duration::from_millis(300),
                redis: Some(RedisBackend::test_unreachable()),
            }),
        };
        let url = format!("{}projects/", client.workspace_url());
        let first = client.get_json_cached(&url).await.unwrap();
        assert!(first.contains("Alpha"), "{first}");
        // Second call within TTL is served by the in-process fallback (Redis is
        // dead), so the network is not hit again.
        let second = client.get_json_cached(&url).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(mock.hits(), 1, "in-process fallback must cache within TTL despite dead Redis");
    }

    // Optional live-Redis distributed test: only runs when PLANE_TEST_REDIS_URL
    // points at a real Redis (ops defer provisioning per S100), else it is a
    // no-op. Proves the shared rate budget actually spaces calls across two
    // independent limiters sharing one Redis, and the shared cache round-trips.
    #[tokio::test]
    #[serial]
    async fn test_distributed_shared_budget_and_cache_live_redis() {
        let Some(url) = std::env::var("PLANE_TEST_REDIS_URL").ok().filter(|v| !v.is_empty()) else {
            eprintln!("skipping: PLANE_TEST_REDIS_URL not set (no live Redis in this env)");
            return;
        };
        std::env::set_var("PLANE_REDIS_URL", &url);
        let backend = RedisBackend::from_env().expect("live backend");
        std::env::remove_var("PLANE_REDIS_URL");

        // Two limiters (simulating two instances) sharing ONE backend/budget.
        let interval = Duration::from_millis(200);
        let a = RateLimiter { last: AsyncMutex::new(None), min_interval: interval, redis: Some(backend.clone()) };
        let b = RateLimiter { last: AsyncMutex::new(None), min_interval: interval, redis: Some(backend.clone()) };
        // Prime the shared slot, then two more reservations across both
        // instances must be spaced by the shared interval, not run back-to-back.
        a.acquire().await;
        let start = Instant::now();
        b.acquire().await;
        a.acquire().await;
        assert!(start.elapsed() >= interval, "shared budget must pace across instances");

        // Shared cache round-trip.
        let key = redis_cache_key("tok\u{0}http://example.invalid/shared");
        backend.cache_set(&key, "shared-value", Duration::from_secs(5)).await;
        assert_eq!(backend.cache_get(&key).await.as_deref(), Some("shared-value"));
    }
}
