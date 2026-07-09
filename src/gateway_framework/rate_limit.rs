//! Interim, in-process rate-limit hook (TGW-04 — Terminus Primary Gateway
//! sprint, S108).
//!
//! This is explicitly the INTERIM mechanism the S108 spec calls for — a
//! simple per-`(identity, action)` token bucket, single-process, no shared
//! state across replicas. The out-of-scope shared-egress Redis-backed
//! limiter (design doc Phase P4 / S100 relocation) is a LATER, separate
//! migration; this module exists to (a) actually throttle a runaway burst
//! today, and (b) present a trait boundary (`RateLimiter`) narrow enough
//! that swapping in a Redis-backed implementation later is a drop-in
//! replacement of [`InProcessRateLimiter`], not a rewrite of
//! `crate::gateway_framework`'s call sites.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

/// Outcome of a rate-limit check for one `(identity, action)` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitDecision {
    Allowed,
    Limited,
}

/// Seam a later Redis-backed limiter (Phase P4) implements as a drop-in
/// replacement for [`InProcessRateLimiter`] — every call site in
/// `crate::gateway_framework` goes through this trait, never a concrete
/// type, so swapping the backing implementation touches only wiring
/// (`Arc<dyn RateLimiter>` construction), not the pipeline logic.
#[async_trait]
pub trait RateLimiter: Send + Sync {
    /// Check and consume one unit of budget for `key` (conventionally
    /// `"{identity}:{action}"`, see [`rate_limit_key`]). Returns
    /// [`RateLimitDecision::Limited`] when `key` has exhausted its budget for
    /// the current window, `Allowed` otherwise (and, on `Allowed`, budget
    /// is decremented as a side effect — this is a check-and-consume call,
    /// not a peek).
    async fn check(&self, key: &str) -> RateLimitDecision;
}

/// Build the canonical rate-limit / audit key for an `(identity, action)`
/// pair. Shared by the rate limiter and the allowlist/audit stages so all
/// three agree on the same key shape.
pub fn rate_limit_key(identity: &str, action: &str) -> String {
    format!("{identity}:{action}")
}

#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// A minimal per-key token bucket, held in a `Mutex<HashMap<..>>` — adequate
/// for a single terminus-primary process (no cross-replica coordination,
/// which is precisely what the later Redis-backed limiter adds).
pub struct InProcessRateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl InProcessRateLimiter {
    /// Build a limiter with an explicit burst capacity and refill rate
    /// (tokens/sec). Each new key starts with a FULL bucket (`capacity`
    /// tokens) so a burst up to `capacity` succeeds immediately, then
    /// throttles.
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            capacity: capacity.max(1) as f64,
            refill_per_sec: refill_per_sec.max(0.001),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Build a limiter from `crate::config::gateway_rate_limit_burst` /
    /// `crate::config::gateway_rate_limit_refill_per_sec` — what
    /// `terminus_primary`'s `main()` calls.
    pub fn from_env() -> Self {
        Self::new(
            crate::config::gateway_rate_limit_burst(),
            crate::config::gateway_rate_limit_refill_per_sec(),
        )
    }

    fn check_sync(&self, key: &str) -> RateLimitDecision {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let bucket = buckets.entry(key.to_string()).or_insert_with(|| Bucket {
            tokens: self.capacity,
            last_refill: now,
        });

        let elapsed = now.saturating_duration_since(bucket.last_refill);
        if elapsed > Duration::ZERO {
            let refill = elapsed.as_secs_f64() * self.refill_per_sec;
            bucket.tokens = (bucket.tokens + refill).min(self.capacity);
            bucket.last_refill = now;
        }

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            RateLimitDecision::Allowed
        } else {
            RateLimitDecision::Limited
        }
    }
}

#[async_trait]
impl RateLimiter for InProcessRateLimiter {
    async fn check(&self, key: &str) -> RateLimitDecision {
        // The bucket update itself is cheap, synchronous, non-blocking work
        // guarded by a std `Mutex` — no `.await` is held across the lock, so
        // this is safe to call directly from an async context without a
        // `spawn_blocking` hop.
        self.check_sync(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allows_up_to_capacity_then_limits() {
        let limiter = InProcessRateLimiter::new(3, 0.0001); // negligible refill within the test
        let key = rate_limit_key("dev-box", "ledger_accounts");

        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        // 4th call within the same instant exhausts the burst of 3.
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Limited);
    }

    #[tokio::test]
    async fn refills_over_time() {
        let limiter = InProcessRateLimiter::new(1, 1000.0); // fast refill for a deterministic test
        let key = rate_limit_key("dev-box", "ledger_accounts");

        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Limited);

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            limiter.check(&key).await,
            RateLimitDecision::Allowed,
            "bucket should have refilled after waiting"
        );
    }

    #[tokio::test]
    async fn separate_keys_have_independent_budgets() {
        let limiter = InProcessRateLimiter::new(1, 0.0001);
        let key_a = rate_limit_key("dev-box", "ledger_accounts");
        let key_b = rate_limit_key("harmony-primary", "ledger_accounts");

        assert_eq!(limiter.check(&key_a).await, RateLimitDecision::Allowed);
        assert_eq!(limiter.check(&key_a).await, RateLimitDecision::Limited);
        // A different identity on the same action has its own budget.
        assert_eq!(limiter.check(&key_b).await, RateLimitDecision::Allowed);
    }

    #[test]
    fn rate_limit_key_shape() {
        assert_eq!(rate_limit_key("dev-box", "ledger_accounts"), "dev-box:ledger_accounts");
    }
}
