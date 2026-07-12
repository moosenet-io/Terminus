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

use std::sync::Arc;

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

/// A fair (FIFO) request queue backed by a Redis list under the `ratelimit:`
/// namespace. Used to admit over-limit proxy requests in order rather than
/// dropping them. Enqueue/dequeue are single atomic list ops (`RPUSH`/`LPOP`),
/// so ordering holds under concurrency.
pub struct RequestQueue {
    backend: Arc<RedisBackend>,
    /// The queue name (becomes `ratelimit:queue:{name}`).
    name: String,
}

impl RequestQueue {
    pub fn new(backend: Arc<RedisBackend>, name: impl Into<String>) -> Self {
        Self {
            backend,
            name: name.into(),
        }
    }

    fn list_key(&self) -> String {
        Namespace::Ratelimit.key(&format!("queue:{}", self.name))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_framework::rate_limit::rate_limit_key;
    use std::time::Duration;

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
