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
///
/// RLQ-01 (queue-with-feedback): `Limited` and `Degraded` both carry
/// `retry_after_secs` — the estimated number of seconds until a caller
/// should retry — so `crate::gateway_framework::guard` can hand every
/// over-limit caller an actionable recovery time instead of a bare 429 wall.
/// They are kept as SEPARATE variants (not one `Limited { degraded: bool,
/// .. }`) specifically so `match`/`matches!` call sites can't accidentally
/// conflate "you are over budget" with "the limiter backend itself is
/// broken" — see the module doc on `crate::ratelimit` for the outage this
/// distinction fixes.
///
/// Both over-budget variants also carry `refill_per_sec` — the refill rate
/// of the ACTUAL limiter instance that produced this decision (RLQ-01 codex
/// fix #2), NOT a re-read of the config global. This matters for an injected
/// / custom-rate limiter (tests, or a future per-tenant rate): the feedback
/// a caller keys off of must reflect the bucket that actually denied it.
///
/// `PartialEq` only (no `Eq`) because the `f64` fields aren't `Eq`-able;
/// every comparison site already only needs `==`/`matches!`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateLimitDecision {
    /// Under budget; the request proceeds (and budget was consumed as a
    /// side effect of `check` — NOT of `peek`).
    Allowed,
    /// A REAL over-limit: the caller has exhausted its budget. Retry after
    /// approximately `retry_after_secs` (derived from the token bucket:
    /// `(1 - tokens) / refill_per_sec`), where `refill_per_sec` is this
    /// limiter instance's own rate.
    Limited { retry_after_secs: f64, refill_per_sec: f64 },
    /// The limiter backend itself is unavailable/erroring (e.g. Redis
    /// unreachable) — NOT a real rate limit. Distinct from `Limited` so a
    /// caller/operator can tell "you're throttled" apart from "the
    /// rate-limiter is broken" (conflating the two cost a multi-hour
    /// misdiagnosed outage — see `crate::ratelimit`). `retry_after_secs` is
    /// a conservative, config-driven backoff (there is no real bucket state
    /// to derive it from); `refill_per_sec` is the limiter's configured rate,
    /// carried for the feedback response's benefit.
    Degraded { retry_after_secs: f64, refill_per_sec: f64 },
}

impl RateLimitDecision {
    /// `true` for `Limited`/`Degraded` — i.e. `guard()` must not admit the
    /// request outright. `false` only for `Allowed`.
    pub fn is_over_budget(&self) -> bool {
        !matches!(self, RateLimitDecision::Allowed)
    }

    /// `true` only for `Degraded` — the limiter backend fault case, as
    /// opposed to a genuine over-limit (`Limited`) or a clean pass
    /// (`Allowed`).
    pub fn is_degraded(&self) -> bool {
        matches!(self, RateLimitDecision::Degraded { .. })
    }

    /// The recovery estimate carried by `Limited`/`Degraded`, `None` for
    /// `Allowed`.
    pub fn retry_after_secs(&self) -> Option<f64> {
        match self {
            RateLimitDecision::Limited { retry_after_secs, .. }
            | RateLimitDecision::Degraded { retry_after_secs, .. } => Some(*retry_after_secs),
            RateLimitDecision::Allowed => None,
        }
    }

    /// The refill rate (tokens/sec) of the limiter instance that produced
    /// this decision, carried by `Limited`/`Degraded`; `None` for `Allowed`.
    pub fn refill_per_sec(&self) -> Option<f64> {
        match self {
            RateLimitDecision::Limited { refill_per_sec, .. }
            | RateLimitDecision::Degraded { refill_per_sec, .. } => Some(*refill_per_sec),
            RateLimitDecision::Allowed => None,
        }
    }
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

    /// Report the CURRENT decision for `key` WITHOUT consuming budget (RLQ-01
    /// codex fix #3). Used to re-derive an accurate recovery window at
    /// admission-queue timeout time — the pre-wait estimate is stale after a
    /// bounded wait, so `guard()` re-peeks the bucket at the moment it sheds.
    /// Refill accrual (time-based) may be applied as a side effect, but no
    /// token is ever consumed, so calling `peek` never itself changes whether
    /// a subsequent `check` is admitted. A `Degraded`/backend-fault limiter
    /// returns the same distinct signal it would from `check`.
    async fn peek(&self, key: &str) -> RateLimitDecision;
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
            RateLimitDecision::Limited {
                retry_after_secs: self.retry_after_for(bucket.tokens),
                refill_per_sec: self.refill_per_sec,
            }
        }
    }

    /// Refill (time-accrual only — NO consumption) and report the current
    /// decision. RLQ-01 codex fix #3: this is the non-consuming re-derivation
    /// `guard()` uses to get a FRESH recovery window at admission-queue
    /// timeout time. It updates `last_refill`/`tokens` from elapsed time
    /// exactly like `check_sync`, but never decrements — so a `peek` can
    /// never itself deny a later legitimate `check`.
    fn peek_sync(&self, key: &str) -> RateLimitDecision {
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
            RateLimitDecision::Allowed
        } else {
            RateLimitDecision::Limited {
                retry_after_secs: self.retry_after_for(bucket.tokens),
                refill_per_sec: self.refill_per_sec,
            }
        }
    }

    /// Seconds until this bucket accrues back to a full token, from its
    /// current `tokens`. The same math the bucket uses to refill, solved for
    /// "time until >= 1.0". `refill_per_sec` is clamped to >= 0.001 in `new`,
    /// so this never divides by zero.
    fn retry_after_for(&self, tokens: f64) -> f64 {
        let deficit = (1.0 - tokens).max(0.0);
        deficit / self.refill_per_sec
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

    async fn peek(&self, key: &str) -> RateLimitDecision {
        self.peek_sync(key)
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
        let decision = limiter.check(&key).await;
        assert!(matches!(decision, RateLimitDecision::Limited { .. }), "{decision:?}");
    }

    /// RLQ-01: `retry_after_secs` must be a real, accurate estimate — not a
    /// zero/placeholder — derived from the bucket's own deficit and refill
    /// rate, so a caller that retries at `recover_at` actually succeeds
    /// (acceptance criterion: recovery estimate accuracy).
    #[tokio::test]
    async fn limited_carries_accurate_retry_after() {
        // capacity 1, refill 2 tokens/sec: after exhausting the single
        // token, the deficit is 1.0 token, so retry_after should be ~0.5s.
        let limiter = InProcessRateLimiter::new(1, 2.0);
        let key = rate_limit_key("dev-box", "ledger_accounts");

        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        let decision = limiter.check(&key).await;
        match decision {
            RateLimitDecision::Limited { retry_after_secs, refill_per_sec } => {
                assert!(
                    (retry_after_secs - 0.5).abs() < 0.05,
                    "expected ~0.5s retry_after, got {retry_after_secs}"
                );
                // RLQ-01 fix #2: the decision carries the ACTUAL limiter's
                // refill rate, not a config global.
                assert_eq!(refill_per_sec, 2.0);
            }
            other => panic!("expected Limited, got {other:?}"),
        }
    }

    /// RLQ-01 fix #3: `peek` reports the current recovery estimate WITHOUT
    /// consuming budget — so re-deriving the window at timeout can't itself
    /// deny a later legitimate call.
    #[tokio::test]
    async fn peek_does_not_consume_budget() {
        let limiter = InProcessRateLimiter::new(1, 0.0001); // negligible refill
        let key = rate_limit_key("dev-box", "ledger_accounts");

        // Peeking a fresh (full) bucket reports Allowed and consumes nothing…
        assert_eq!(limiter.peek(&key).await, RateLimitDecision::Allowed);
        assert_eq!(limiter.peek(&key).await, RateLimitDecision::Allowed);
        // …so the single real token is still there for `check` to consume.
        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        // Now exhausted: both check and peek report Limited.
        let checked = limiter.check(&key).await;
        assert!(matches!(checked, RateLimitDecision::Limited { .. }), "{checked:?}");
        let peeked = limiter.peek(&key).await;
        assert!(matches!(peeked, RateLimitDecision::Limited { .. }), "{peeked:?}");
    }

    #[tokio::test]
    async fn refills_over_time() {
        let limiter = InProcessRateLimiter::new(1, 1000.0); // fast refill for a deterministic test
        let key = rate_limit_key("dev-box", "ledger_accounts");

        assert_eq!(limiter.check(&key).await, RateLimitDecision::Allowed);
        let decision = limiter.check(&key).await;
        assert!(matches!(decision, RateLimitDecision::Limited { .. }), "{decision:?}");

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
        let decision = limiter.check(&key_a).await;
        assert!(matches!(decision, RateLimitDecision::Limited { .. }), "{decision:?}");
        // A different identity on the same action has its own budget.
        assert_eq!(limiter.check(&key_b).await, RateLimitDecision::Allowed);
    }

    #[test]
    fn rate_limit_key_shape() {
        assert_eq!(rate_limit_key("dev-box", "ledger_accounts"), "dev-box:ledger_accounts");
    }
}
