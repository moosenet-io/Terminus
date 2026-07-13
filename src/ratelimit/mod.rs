//! BLD-20 — Redis-backed rate-limiter + fair request queue for the proxy
//! surfaces (Chord + terminus-primary).
//!
//! This is the durable, cross-instance replacement for the in-process
//! [`crate::gateway_framework::rate_limit::InProcessRateLimiter`]. It
//! implements the SAME [`RateLimiter`] trait — the drop-in seam that module
//! was written against — so wiring it in is an `Arc<dyn RateLimiter>`
//! construction change at the gateway's `main()`, not a rewrite of any call
//! site.
//!
//! # Why Redis / why Lua
//! A token bucket read-modify-written from several gateway workers (or ever
//! several gateway instances) races: two workers both read `tokens = 1` and
//! both allow, oversubscribing the budget. The check-and-consume is therefore
//! done in a single **atomic Lua script** ([`TOKEN_BUCKET_LUA`]) evaluated
//! server-side — the refill + compare + decrement happen without interleaving,
//! so N concurrent over-limit requests are throttled correctly and limits hold
//! across a gateway restart (the bucket lives in Redis, not process memory).
//!
//! # Fail-safe posture (EDGE CASES)
//! For a PROXY, an unreachable limiter must fail **CLOSED** — if we cannot
//! prove a request is within budget we deny it, protecting the backends from an
//! un-throttled flood. (This is the opposite of sccache, which fails OPEN so a
//! cache outage never blocks a build.) The request queue persists its intake to
//! Redis; a caller that also has an intake-DB fallback keeps working when Redis
//! is down.
//!
//! # Secrets / infra (S1/S7)
//! No endpoint or password here — the shared [`RedisBackend`] owns endpoint
//! resolution (`REDIS_URL` from the vault). This module only forms keys via the
//! `ratelimit:` [`Namespace`].

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::gateway_framework::rate_limit::{RateLimitDecision, RateLimiter};
use crate::redis::{Namespace, RedisBackend};

/// Atomic token-bucket check-and-consume.
///
/// `KEYS[1]` = the bucket key (a Redis hash `{tokens, ts}`).
/// `ARGV[1]` = capacity (max tokens / burst).
/// `ARGV[2]` = refill rate (tokens per second).
/// `ARGV[3]` = now (ms since epoch, from the caller's clock — one authority).
/// `ARGV[4]` = requested tokens (normally 1).
///
/// Returns `{allowed, tokens_remaining_millitokens}` where `allowed` is 1 or 0.
/// The whole refill→compare→decrement is atomic (single-threaded Lua), so
/// concurrent callers cannot both consume the last token. A TTL is set so idle
/// buckets are reclaimed by the volatile DB's LRU without manual cleanup.
pub const TOKEN_BUCKET_LUA: &str = r#"
local key = KEYS[1]
local capacity = tonumber(ARGV[1])
local refill = tonumber(ARGV[2])
local now_ms = tonumber(ARGV[3])
local requested = tonumber(ARGV[4])

local data = redis.call('HMGET', key, 'tokens', 'ts')
local tokens = tonumber(data[1])
local ts = tonumber(data[2])
if tokens == nil then
  tokens = capacity
  ts = now_ms
end

local elapsed = math.max(0, now_ms - ts) / 1000.0
tokens = math.min(capacity, tokens + elapsed * refill)

local allowed = 0
if tokens >= requested then
  tokens = tokens - requested
  allowed = 1
end

redis.call('HMSET', key, 'tokens', tokens, 'ts', now_ms)
-- Reclaim an idle bucket: hold it long enough to refill from empty to full.
local ttl = 60
if refill > 0 then
  ttl = math.ceil(capacity / refill) + 1
end
redis.call('EXPIRE', key, ttl)

return {allowed, math.floor(tokens * 1000)}
"#;

/// Milliseconds since the Unix epoch (the single clock authority passed into
/// the Lua script, so all callers agree regardless of server clock skew).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A Redis-backed token-bucket rate limiter. One instance covers every
/// `(identity, action)` key; each key gets an independent bucket under the
/// `ratelimit:` namespace.
pub struct RedisRateLimiter {
    backend: Arc<RedisBackend>,
    capacity: f64,
    refill_per_sec: f64,
}

impl RedisRateLimiter {
    /// Build with an explicit burst capacity and refill rate (tokens/sec).
    pub fn new(backend: Arc<RedisBackend>, capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            backend,
            capacity: capacity.max(1) as f64,
            refill_per_sec: refill_per_sec.max(0.001),
        }
    }

    /// Build from the shared config knobs (same env the in-process limiter
    /// reads), so switching backends changes only which constructor `main()`
    /// calls.
    pub fn from_env(backend: Arc<RedisBackend>) -> Self {
        Self::new(
            backend,
            crate::config::gateway_rate_limit_burst(),
            crate::config::gateway_rate_limit_refill_per_sec(),
        )
    }

    /// The Redis key for a rate-limit bucket: `ratelimit:{key}`.
    fn bucket_key(key: &str) -> String {
        Namespace::Ratelimit.key(key)
    }
}

#[async_trait]
impl RateLimiter for RedisRateLimiter {
    async fn check(&self, key: &str) -> RateLimitDecision {
        let bucket = Self::bucket_key(key);
        let capacity = self.capacity;
        let refill = self.refill_per_sec;
        let now = now_ms();
        // Build the script fresh (sha1 of a tiny script is negligible next to a
        // network round-trip) so we depend on no `Clone` of `redis::Script`.
        let script = redis::Script::new(TOKEN_BUCKET_LUA);

        let outcome: Result<Vec<i64>, ()> = self
            .backend
            .with_conn(Namespace::Ratelimit, |mut conn| async move {
                script
                    .key(bucket)
                    .arg(capacity)
                    .arg(refill)
                    .arg(now)
                    .arg(1)
                    .invoke_async::<_, Vec<i64>>(&mut conn)
                    .await
            })
            .await;

        match outcome {
            Ok(v) if v.first().copied() == Some(1) => RateLimitDecision::Allowed,
            Ok(_) => RateLimitDecision::Limited,
            // FAIL CLOSED for the proxy: an unreachable limiter denies, so a
            // Redis outage cannot become an un-throttled flood at the backends.
            Err(()) => RateLimitDecision::Limited,
        }
    }
}

/// A fail-CLOSED sentinel limiter: every `check` returns `Limited`. Used when
/// the proxy is CONFIGURED for Redis (`REDIS_URL` set) but the backend could not
/// be constructed (e.g. an unparseable URL) — a misconfiguration must not
/// silently downgrade to the in-process limiter (which would drop the
/// cross-instance + fail-closed guarantees). Denying every request makes the
/// misconfiguration loud and safe rather than invisibly permissive.
pub struct AlwaysLimited;

#[async_trait]
impl RateLimiter for AlwaysLimited {
    async fn check(&self, _key: &str) -> RateLimitDecision {
        RateLimitDecision::Limited
    }
}

/// A fair (FIFO) request queue backed by a Redis list under the `ratelimit:`
/// namespace. Used to admit over-limit proxy requests in order rather than
/// dropping them. Enqueue/dequeue are single atomic list ops (`RPUSH`/`LPOP`),
/// so ordering holds under concurrency.
pub struct RequestQueue {
    backend: Arc<RedisBackend>,
    /// The queue name (becomes `ratelimit:queue:{name}`).
    name: String,
    /// Per-INSTANCE random salt (see [`instance_salt`]). Combined with a
    /// Redis-atomic `INCR` sequence, it makes every admission ticket globally
    /// unique across ALL gateway instances — so ownership-by-value
    /// (`LINDEX head == my ticket`) and `LREM` can never touch another
    /// instance's entry.
    instance_salt: String,
}

/// A per-PROCESS random salt, generated ONCE at first use (uuid v4 — random, NOT
/// derived from hostname/IP, so no S1 infra literal). Every `RequestQueue` in
/// this process shares it. Two distinct gateway processes get distinct salts, so
/// even a Redis reset (which would restart the `INCR` counter) cannot make two
/// live instances collide on a ticket.
fn instance_salt() -> &'static str {
    static SALT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SALT.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
}

impl RequestQueue {
    pub fn new(backend: Arc<RedisBackend>, name: impl Into<String>) -> Self {
        Self {
            backend,
            name: name.into(),
            instance_salt: instance_salt().to_string(),
        }
    }

    fn list_key(&self) -> String {
        Namespace::Ratelimit.key(&format!("queue:{}", self.name))
    }

    /// The Redis key holding this queue's atomic ticket sequence counter.
    fn seq_key(&self) -> String {
        Namespace::Ratelimit.key(&format!("queue:{}:seq", self.name))
    }

    /// Append `item` to the tail (FIFO). `Err(())` if Redis is unreachable — the
    /// caller falls back to its own intake path.
    pub async fn enqueue(&self, item: &str) -> Result<(), ()> {
        let key = self.list_key();
        let item = item.to_string();
        self.backend
            .with_conn(Namespace::Ratelimit, |mut conn| async move {
                redis::cmd("RPUSH")
                    .arg(&key)
                    .arg(&item)
                    .query_async::<_, i64>(&mut conn)
                    .await
            })
            .await
            .map(|_| ())
    }

    /// Pop the head (FIFO). `Ok(None)` = empty; `Err(())` = unreachable.
    pub async fn dequeue(&self) -> Result<Option<String>, ()> {
        let key = self.list_key();
        self.backend
            .with_conn(Namespace::Ratelimit, |mut conn| async move {
                redis::cmd("LPOP")
                    .arg(&key)
                    .query_async::<_, Option<String>>(&mut conn)
                    .await
            })
            .await
    }

    /// Current queue depth. `Err(())` if unreachable.
    pub async fn depth(&self) -> Result<u64, ()> {
        let key = self.list_key();
        self.backend
            .with_conn(Namespace::Ratelimit, |mut conn| async move {
                redis::cmd("LLEN").arg(&key).query_async::<_, u64>(&mut conn).await
            })
            .await
    }

    /// Bounded FIFO admission for an over-limit proxy request. Atomically
    /// allocates a GLOBALLY-UNIQUE ticket and enqueues it (only if the queue has
    /// room), then waits — in FIFO order — until this ticket is at the head AND
    /// `acquire` (a re-check of the rate limiter) succeeds, or until `max_wait`
    /// elapses. Returns [`Admission::Admitted`] on success,
    /// [`Admission::QueueFull`] if the queue is at `max_depth`,
    /// [`Admission::TimedOut`] if the wait elapsed (the ticket is removed), or
    /// [`Admission::Unavailable`] (fail CLOSED) on any Redis error. The ticket is
    /// generated internally (per-instance salt + Redis-atomic `INCR`), never by
    /// the caller — so it is unique across gateway instances. See
    /// [`run_admission`] for the backend-agnostic loop this delegates to.
    pub async fn admit<F, Fut>(
        &self,
        max_depth: i64,
        max_wait: Duration,
        poll: Duration,
        acquire: F,
    ) -> Admission
    where
        F: Fn() -> Fut,
        Fut: Future<Output = bool>,
    {
        run_admission(self, max_depth, max_wait, poll, acquire).await
    }
}

/// Atomic bounded enqueue with GLOBALLY-UNIQUE ticket allocation. In one
/// server-side script: refuse if `LLEN(list) >= max_depth`; else `INCR` the
/// per-queue sequence counter, form `ticket = "{salt}:{seq}"`, `RPUSH` it, and
/// TTL both keys. `KEYS[1]` = list key, `KEYS[2]` = seq key; `ARGV[1]` =
/// max_depth, `ARGV[2]` = instance salt, `ARGV[3]` = ttl secs. Returns
/// `{1, ticket}` enqueued or `{0, ""}` full. Because the salt is per-instance
/// and the `INCR` is atomic across ALL instances, the ticket is globally unique
/// — so ownership-by-value + `LREM` can never match another instance's entry.
pub const ENQUEUE_UNIQUE_LUA: &str = r#"
if redis.call('LLEN', KEYS[1]) >= tonumber(ARGV[1]) then
  return {0, ''}
end
local seq = redis.call('INCR', KEYS[2])
local ticket = ARGV[2] .. ':' .. seq
redis.call('RPUSH', KEYS[1], ticket)
-- EPHEMERAL BY DESIGN (not a durability regression): this is the PROXY
-- ADMISSION queue — each entry is an in-flight HTTP request waiting for a rate
-- token, and those requests die when their gateway process restarts. The TTL is
-- a crash-safety janitor that bounds stale tickets left by a crashed instance;
-- it must self-expire. Durability lives ELSEWHERE and is untouched by this TTL:
-- the rate-limiter COUNTERS, the PREFIX OVERLAY (durable DB), and the durable
-- BLD-06 compiler JOB queue (Namespace::Queue, noeviction) all have their own
-- non-expiring persistence. Do NOT reuse this list as the durable job queue.
redis.call('EXPIRE', KEYS[1], tonumber(ARGV[3]))
redis.call('EXPIRE', KEYS[2], tonumber(ARGV[3]))
return {1, ticket}
"#;

/// TTL (secs) for an active PROXY ADMISSION queue. Intentionally EPHEMERAL /
/// self-cleaning: an admission entry is an in-flight HTTP request waiting for a
/// rate token, which cannot outlive its gateway process, so the TTL exists to
/// reclaim stale tickets from a CRASHED instance — it is a crash-safety janitor,
/// NOT a durability regression. This is NOT the durable compiler job queue: the
/// rate-limiter counters, the prefix overlay, and the durable
/// `crate::redis::Namespace::Queue` job queue (BLD-06, `noeviction`) persist
/// without expiry and are unaffected by this TTL.
const QUEUE_TTL_SECS: i64 = 300;

/// [`Outcome`](Admission) of a bounded queue-admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Admitted — the caller proceeds (a rate-limit token was acquired at the head).
    Admitted,
    /// The queue was already at `max_depth` — shed load (429).
    QueueFull,
    /// The bounded wait elapsed before admission — shed load (429).
    TimedOut,
    /// Redis was unreachable — fail CLOSED (429), never admit unbounded.
    Unavailable,
}

/// The FIFO-queue operations [`run_admission`] needs. Implemented by
/// [`RequestQueue`] over Redis, and by an in-memory fake in the unit tests so
/// the admission/timeout/full/unavailable paths are covered OFFLINE.
#[async_trait]
pub(crate) trait AdmissionQueue: Send + Sync {
    /// Atomically allocate a globally-unique ticket and enqueue it iff depth <
    /// `max_depth`. `Ok(Some(ticket))` enqueued (the caller owns exactly this
    /// value), `Ok(None)` full, `Err(())` unreachable.
    async fn enqueue_unique(&self, max_depth: i64) -> Result<Option<String>, ()>;
    /// Is `ticket` currently at the head of the queue? `Err(())` unreachable.
    async fn at_head(&self, ticket: &str) -> Result<bool, ()>;
    /// Remove `ticket` from the queue (best-effort on give-up). `Err(())` unreachable.
    async fn remove_ticket(&self, ticket: &str) -> Result<(), ()>;
    /// Pop the head element (called once this ticket has been admitted at the head).
    async fn pop_head(&self) -> Result<(), ()>;
}

/// Backend-agnostic bounded-FIFO admission loop (see [`RequestQueue::admit`]).
/// The queue allocates its own globally-unique ticket, so no two instances can
/// hold the same value. Fails CLOSED (`Unavailable`) on any queue error so a
/// Redis outage never admits unbounded.
pub(crate) async fn run_admission<Q, F, Fut>(
    queue: &Q,
    max_depth: i64,
    max_wait: Duration,
    poll: Duration,
    acquire: F,
) -> Admission
where
    Q: AdmissionQueue + ?Sized,
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let ticket = match queue.enqueue_unique(max_depth).await {
        Ok(Some(t)) => t,
        Ok(None) => return Admission::QueueFull,
        Err(()) => return Admission::Unavailable, // fail closed
    };
    let deadline = Instant::now() + max_wait;
    loop {
        match queue.at_head(&ticket).await {
            Ok(true) => {
                if acquire().await {
                    let _ = queue.pop_head().await; // best-effort: drop our head slot
                    return Admission::Admitted;
                }
            }
            Ok(false) => {}
            Err(()) => {
                let _ = queue.remove_ticket(&ticket).await;
                return Admission::Unavailable; // fail closed
            }
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = queue.remove_ticket(&ticket).await;
            return Admission::TimedOut;
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(poll.min(remaining)).await;
    }
}

#[async_trait]
impl AdmissionQueue for RequestQueue {
    async fn enqueue_unique(&self, max_depth: i64) -> Result<Option<String>, ()> {
        let list = self.list_key();
        let seq = self.seq_key();
        let salt = self.instance_salt.clone();
        let script = redis::Script::new(ENQUEUE_UNIQUE_LUA);
        self.backend
            .with_conn(Namespace::Ratelimit, |mut conn| async move {
                // Lua returns {enqueued(0/1), ticket-or-empty}.
                let (ok, ticket): (i64, String) = script
                    .key(list)
                    .key(seq)
                    .arg(max_depth)
                    .arg(salt)
                    .arg(QUEUE_TTL_SECS)
                    .invoke_async(&mut conn)
                    .await?;
                Ok(if ok == 1 { Some(ticket) } else { None })
            })
            .await
    }

    async fn at_head(&self, ticket: &str) -> Result<bool, ()> {
        let key = self.list_key();
        let ticket = ticket.to_string();
        self.backend
            .with_conn(Namespace::Ratelimit, |mut conn| async move {
                let head: Option<String> = redis::cmd("LINDEX")
                    .arg(&key)
                    .arg(0)
                    .query_async(&mut conn)
                    .await?;
                Ok(head.as_deref() == Some(ticket.as_str()))
            })
            .await
    }

    async fn remove_ticket(&self, ticket: &str) -> Result<(), ()> {
        let key = self.list_key();
        let ticket = ticket.to_string();
        self.backend
            .with_conn(Namespace::Ratelimit, |mut conn| async move {
                // count=1: the ticket value is globally unique, so at most one
                // match exists; removing the single first occurrence is exact.
                redis::cmd("LREM")
                    .arg(&key)
                    .arg(1)
                    .arg(&ticket)
                    .query_async::<_, i64>(&mut conn)
                    .await
                    .map(|_| ())
            })
            .await
    }

    async fn pop_head(&self) -> Result<(), ()> {
        let key = self.list_key();
        self.backend
            .with_conn(Namespace::Ratelimit, |mut conn| async move {
                redis::cmd("LPOP")
                    .arg(&key)
                    .query_async::<_, Option<String>>(&mut conn)
                    .await
                    .map(|_| ())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_framework::rate_limit::rate_limit_key;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    // ── In-memory fake queue: exercises the admission loop (`run_admission`)
    // OFFLINE — the SAME control flow the Redis `RequestQueue` uses via the
    // shared `AdmissionQueue` trait. A `salt` + per-instance `seq` model the
    // globally-unique ticket allocation; the `items` list can be SHARED between
    // two fakes to model two gateway instances contending on one Redis key.
    // `fail` forces the unreachable/fail-closed path.
    struct FakeQueue {
        items: Arc<Mutex<std::collections::VecDeque<String>>>,
        salt: String,
        seq: AtomicUsize,
        fail: bool,
    }
    impl FakeQueue {
        fn new(fail: bool) -> Self {
            Self::with_shared(Arc::new(Mutex::new(std::collections::VecDeque::new())), "inst", fail)
        }
        fn with_shared(
            items: Arc<Mutex<std::collections::VecDeque<String>>>,
            salt: &str,
            fail: bool,
        ) -> Self {
            Self { items, salt: salt.to_string(), seq: AtomicUsize::new(0), fail }
        }
    }
    #[async_trait]
    impl AdmissionQueue for FakeQueue {
        async fn enqueue_unique(&self, max_depth: i64) -> Result<Option<String>, ()> {
            if self.fail {
                return Err(());
            }
            let mut q = self.items.lock().unwrap();
            if q.len() as i64 >= max_depth {
                return Ok(None);
            }
            // salt + per-instance monotonic seq (models the Redis-atomic INCR;
            // the salt guarantees uniqueness even if two instances' seqs align).
            let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let ticket = format!("{}:{}", self.salt, n);
            q.push_back(ticket.clone());
            Ok(Some(ticket))
        }
        async fn at_head(&self, ticket: &str) -> Result<bool, ()> {
            if self.fail {
                return Err(());
            }
            Ok(self.items.lock().unwrap().front().map(|s| s.as_str()) == Some(ticket))
        }
        async fn remove_ticket(&self, ticket: &str) -> Result<(), ()> {
            self.items.lock().unwrap().retain(|t| t != ticket);
            Ok(())
        }
        async fn pop_head(&self) -> Result<(), ()> {
            self.items.lock().unwrap().pop_front();
            Ok(())
        }
    }

    #[tokio::test]
    async fn admission_admits_when_a_slot_frees_at_head() {
        let q = FakeQueue::new(false);
        // acquire succeeds on the 2nd poll (simulating a token refilling).
        let calls = AtomicUsize::new(0);
        let acquire = || async {
            calls.fetch_add(1, Ordering::SeqCst) >= 1
        };
        let out = run_admission(
            &q, 128, Duration::from_millis(500), Duration::from_millis(5), acquire,
        )
        .await;
        assert_eq!(out, Admission::Admitted);
        // Our ticket was popped on admit → queue drained.
        assert!(q.items.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn admission_queue_full_sheds_load() {
        let q = FakeQueue::new(false);
        // Pre-fill to the cap so enqueue is refused.
        q.items.lock().unwrap().push_back("existing".into());
        let acquire = || async { true };
        let out = run_admission(
            &q, 1, Duration::from_millis(200), Duration::from_millis(5), acquire,
        )
        .await;
        assert_eq!(out, Admission::QueueFull);
    }

    #[tokio::test]
    async fn admission_times_out_and_removes_ticket() {
        let q = FakeQueue::new(false);
        let acquire = || async { false }; // never acquirable
        let out = run_admission(
            &q, 128, Duration::from_millis(60), Duration::from_millis(10), acquire,
        )
        .await;
        assert_eq!(out, Admission::TimedOut);
        // The abandoned ticket must be removed on give-up (no leak).
        assert!(q.items.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn admission_fails_closed_when_queue_unreachable() {
        let q = FakeQueue::new(true); // every op errors
        let acquire = || async { true };
        let out = run_admission(
            &q, 128, Duration::from_millis(200), Duration::from_millis(5), acquire,
        )
        .await;
        assert_eq!(out, Admission::Unavailable);
    }

    #[tokio::test]
    async fn admission_tickets_are_cross_instance_unique() {
        // Two gateway instances (distinct salts) contending on the SAME queue
        // key (shared list). Their per-instance seqs BOTH start at 1 — modelling
        // the worst case (e.g. just after a Redis reset restarted the counter) —
        // so only the salt keeps the tickets apart.
        let shared = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let inst_a = FakeQueue::with_shared(shared.clone(), "instA", false);
        let inst_b = FakeQueue::with_shared(shared.clone(), "instB", false);

        let ta = inst_a.enqueue_unique(128).await.unwrap().unwrap();
        let tb = inst_b.enqueue_unique(128).await.unwrap().unwrap();

        // Distinct even though both seqs are 1 — the old per-process counter bug
        // would have produced identical `key#0` tickets here.
        assert_ne!(ta, tb, "distinct instances must produce distinct tickets");
        assert!(ta.starts_with("instA:"), "ticket carries its instance salt: {ta}");
        assert!(tb.starts_with("instB:"), "ticket carries its instance salt: {tb}");

        // Ownership-by-value is now safe: A owns the head (ta); B's ticket is NOT
        // the head, so B cannot mistake A's entry for its own.
        assert!(inst_a.at_head(&ta).await.unwrap());
        assert!(!inst_b.at_head(&tb).await.unwrap());

        // A removing ITS OWN ticket leaves B's entry intact (no double-remove /
        // wrong-remove across instances).
        inst_a.remove_ticket(&ta).await.unwrap();
        {
            let q = shared.lock().unwrap();
            assert_eq!(q.len(), 1, "only A's ticket was removed");
            assert_eq!(q.front().unwrap(), &tb, "B's ticket survives and is now head");
        }
        assert!(inst_b.at_head(&tb).await.unwrap());
    }

    // Offline unit tests: no live Redis. They cover the pure/derivable surface
    // (key derivation, script text, fail-closed decision). Behavior against a
    // real Redis (atomicity under concurrency, restart-survival) is covered by
    // the `#[ignore]`d live tests below, which the test-gate runs where a Redis
    // is provisioned.

    #[test]
    fn bucket_key_is_ratelimit_namespaced() {
        let k = rate_limit_key("dev-box", "ledger_accounts");
        assert_eq!(RedisRateLimiter::bucket_key(&k), "ratelimit:dev-box:ledger_accounts");
    }

    #[test]
    fn queue_key_is_ratelimit_namespaced() {
        let backend =
            RedisBackend::build("redis://127.0.0.1:6379", None, 0, 1, Duration::from_millis(200))
                .expect("offline construction");
        let q = RequestQueue::new(backend, "proxy");
        assert_eq!(q.list_key(), "ratelimit:queue:proxy");
    }

    #[test]
    fn lua_script_is_atomic_check_and_consume() {
        // The script must do the whole refill→compare→decrement itself (no
        // client-side read-then-write), which is the entire point of using Lua.
        assert!(TOKEN_BUCKET_LUA.contains("HMGET"), "reads bucket state");
        assert!(TOKEN_BUCKET_LUA.contains("HMSET"), "writes bucket state");
        assert!(TOKEN_BUCKET_LUA.contains("EXPIRE"), "reclaims idle buckets");
        assert!(TOKEN_BUCKET_LUA.contains("return {allowed"), "returns the decision");
    }

    #[test]
    fn now_ms_is_populated() {
        assert!(now_ms() > 0, "epoch millis must be non-zero");
    }

    #[tokio::test]
    async fn always_limited_denies_every_request() {
        // The sentinel used when REDIS_URL is configured-but-unparseable must
        // deny unconditionally (fail closed), never allow.
        let sentinel = AlwaysLimited;
        assert_eq!(sentinel.check("anything").await, RateLimitDecision::Limited);
        assert_eq!(sentinel.check("").await, RateLimitDecision::Limited);
    }

    #[tokio::test]
    async fn unreachable_backend_fails_closed() {
        // A backend pointed at a dead port: `check` must DENY (fail closed for
        // the proxy), never allow-by-default. The short op timeout bounds this.
        let backend = RedisBackend::build(
            "redis://127.0.0.1:6390", // nothing listening
            None,
            0,
            1,
            Duration::from_millis(150),
        )
        .expect("offline construction");
        let limiter = RedisRateLimiter::new(backend, 10, 5.0);
        let key = rate_limit_key("dev-box", "ledger_accounts");
        assert_eq!(
            limiter.check(&key).await,
            RateLimitDecision::Limited,
            "an unreachable limiter must fail CLOSED for the proxy"
        );
    }

    // ── Live tests (require a real Redis; run by the test-gate) ───────────────

    async fn live_backend() -> Option<Arc<RedisBackend>> {
        // Only runs where REDIS_URL is materialized. `from_env` returns None
        // otherwise, so these self-skip off the build farm.
        RedisBackend::from_env()
    }

    #[tokio::test]
    #[ignore = "requires a live REDIS_URL"]
    async fn live_allows_up_to_capacity_then_limits() {
        let Some(backend) = live_backend().await else {
            return;
        };
        let limiter = RedisRateLimiter::new(backend, 3, 0.0001);
        // Unique key per run so reruns don't share a warm bucket.
        let key = format!("test:{}:burst", now_ms());
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Limited);
    }

    #[tokio::test]
    #[ignore = "requires a live REDIS_URL"]
    async fn live_no_oversubscription_under_concurrency() {
        let Some(backend) = live_backend().await else {
            return;
        };
        let limiter = Arc::new(RedisRateLimiter::new(backend, 5, 0.0001));
        let key = format!("test:{}:concurrent", now_ms());
        let mut handles = Vec::new();
        for _ in 0..50 {
            let l = limiter.clone();
            let k = key.clone();
            handles.push(tokio::spawn(async move { l.check(&k).await }));
        }
        let mut allowed = 0;
        for h in handles {
            if h.await.unwrap() == RateLimitDecision::Allowed {
                allowed += 1;
            }
        }
        // Atomic Lua ⇒ exactly the burst capacity is admitted, never more.
        assert_eq!(allowed, 5, "atomic bucket must admit exactly capacity, got {allowed}");
    }

    #[tokio::test]
    #[ignore = "requires a live REDIS_URL"]
    async fn live_queue_is_fifo() {
        let Some(backend) = live_backend().await else {
            return;
        };
        let q = RequestQueue::new(backend, format!("test-{}", now_ms()));
        q.enqueue("a").await.unwrap();
        q.enqueue("b").await.unwrap();
        assert_eq!(q.depth().await.unwrap(), 2);
        assert_eq!(q.dequeue().await.unwrap().as_deref(), Some("a"));
        assert_eq!(q.dequeue().await.unwrap().as_deref(), Some("b"));
        assert_eq!(q.dequeue().await.unwrap(), None);
    }
}
