//! GMQ-02 — Redis-backed per-base merge lock + FIFO ordering (the queue core).
//!
//! When several build agents merge PRs into the SAME base branch concurrently,
//! their merges race: agent A merges, `main` advances, and agent B's
//! already-open PR is now stale-based (see `docs/specs/S120-gitea-merge-queue.md`
//! §1). [`MergeQueue`] adds a per-base **critical section** so exactly one merge
//! per base is in flight at a time, and waiters are served in enqueue order
//! (priority-weighted).
//!
//! ## Mechanism
//! For a merge key (conventionally `{owner}/{repo}/{base}`):
//! 1. **Ticket** — `INCR queue:merge:seq:{key}` gives a monotonic FIFO ticket.
//!    The ticket is added to a wait ZSET (`queue:merge:wait:{key}`) with score
//!    `ticket - priority * 1e12` (mirrors the compiler queue's
//!    `src/compiler/queue.rs` dispatch-ordering score), so a higher `priority`
//!    sorts earlier without breaking FIFO among equal priorities.
//! 2. **Serialize** — a bounded poll loop attempts to atomically acquire the
//!    critical section: succeeds only when this ticket is at the FRONT of the
//!    wait ZSET AND the lock key (`queue:merge:lock:{key}`) is not currently
//!    held. Acquiring removes the ticket from the ZSET and `SET`s the lock with
//!    a random fence token and a `PX` TTL, all in one Lua script — no other
//!    waiter can ever observe "front + free" and win the race.
//! 3. **Crash backstop** — the lock is TTL-bound. If a holder crashes without
//!    releasing, the lock key simply expires; the next-in-line ticket (already
//!    removed from the wait ZSET when the crashed holder acquired) then sees
//!    the lock as free on its next poll and proceeds. No separate reconcile
//!    sweep is needed — the TTL check IS the reconcile, evaluated on every
//!    acquire attempt.
//! 4. **Release** — a fence-token-guarded compare-and-delete: only the holder
//!    whose token still matches the stored value frees the lock, so a stale
//!    holder (whose TTL already expired and whose slot was reclaimed by the
//!    next ticket) can never free someone else's lock.
//!
//! ## Degrade-open
//! [`MergeQueue::from_env`] returns `None` when Redis is not configured — there
//! is deliberately no `MergeQueue` to construct, so a caller's own fallback path
//! (call the merge directly) is exercised, matching every other degrade-open
//! primitive in this codebase (sccache, the rate limiter's fail-open siblings).
//! Once constructed, a **mid-operation** Redis error (`Err(())`, i.e.
//! unreachable/timed out) is treated the same way: logged once and `f` is run
//! immediately, unqueued — availability over strict ordering for a short merge
//! (see `docs/specs/S120-gitea-merge-queue.md` §1, "Degrade-open").

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::warn;

use crate::redis::{Namespace, RedisBackend};

/// Config knobs for the merge queue. All optional via env, safe defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MergeQueueConfig {
    /// Whether the queue path is enabled at all. `false` makes
    /// [`MergeQueue::with_merge_slot`] run `f` immediately, unqueued — the same
    /// as the Redis-absent degrade-open path, but operator-controlled.
    pub enabled: bool,
    /// Lock TTL (seconds): the crash backstop. **This is a lease, not a hard
    /// mutex** — `lock_ttl_secs` MUST exceed the maximum realistic
    /// critical-section (merge) duration. A holder whose critical section
    /// runs longer than this WILL lose its lease: the lock key expires, the
    /// next waiter's poll sees it as free, and the two holders' critical
    /// sections then overlap. The fence token only guarantees
    /// release-safety (a stale/expired holder can never free a re-taken
    /// lock out from under its new holder) — it does NOT guarantee mutual
    /// exclusion beyond the lease window. This is a known, accepted
    /// tradeoff (see `docs/specs/S120-gitea-merge-queue.md` EDGE CASES); it
    /// is intentionally NOT addressed with heartbeat/lease-renewal here —
    /// callers must simply set a TTL comfortably above a merge's realistic
    /// duration (GMQ-04 does this).
    pub lock_ttl_secs: u64,
    /// Max time (seconds) a waiter polls for its turn before giving up with
    /// [`MergeQueueError::Busy`], never blocking forever.
    pub max_wait_secs: u64,
    /// TTL (seconds) for the wait ZSET's own `EXPIRE` backstop — bounds how
    /// long an abandoned waiter (crashed between enqueue and its first poll)
    /// can wedge a key. Must be bounded by (and is derived from, by default)
    /// `max_wait_secs` — it must never be shorter than the longest a real,
    /// still-polling waiter can legitimately outlive it.
    pub wait_ttl_secs: u64,
}

impl Default for MergeQueueConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lock_ttl_secs: DEFAULT_LOCK_TTL_SECS,
            max_wait_secs: DEFAULT_MAX_WAIT_SECS,
            wait_ttl_secs: DEFAULT_MAX_WAIT_SECS + 60,
        }
    }
}

const DEFAULT_LOCK_TTL_SECS: u64 = 120;
const DEFAULT_MAX_WAIT_SECS: u64 = 300;
const POLL_MIN_MS: u64 = 25;
const POLL_MAX_MS: u64 = 250;

impl MergeQueueConfig {
    /// Read from `GITEA_MERGE_QUEUE_ENABLED` / `GITEA_MERGE_QUEUE_LOCK_TTL_SECS`
    /// / `GITEA_MERGE_QUEUE_MAX_WAIT_SECS` / `GITEA_MERGE_QUEUE_WAIT_TTL_SECS`
    /// via `crate::config`'s accessors.
    pub fn from_env() -> Self {
        Self {
            enabled: crate::config::gitea_merge_queue_enabled(),
            lock_ttl_secs: crate::config::gitea_merge_queue_lock_ttl_secs(),
            max_wait_secs: crate::config::gitea_merge_queue_max_wait_secs(),
            wait_ttl_secs: crate::config::gitea_merge_queue_wait_ttl_secs(),
        }
    }
}

/// Errors [`MergeQueue::with_merge_slot`] can surface itself (distinct from
/// whatever `f`'s own future produces — that is returned unwrapped as `Ok(T)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeQueueError {
    /// `max_wait_secs` elapsed before this waiter reached the front of the
    /// queue AND found the lock free. Callers should surface a clear
    /// "queue busy, retry" rather than hanging.
    Busy,
}

impl std::fmt::Display for MergeQueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeQueueError::Busy => {
                write!(f, "merge queue busy: timed out waiting for the merge slot, retry")
            }
        }
    }
}

impl std::error::Error for MergeQueueError {}

/// Outcome of one acquire attempt.
enum AcquireAttempt {
    /// Acquired the slot; carries the fence token the release must present.
    Acquired(String),
    /// Not yet this ticket's turn, or the lock is currently held.
    NotYet,
    /// The backing store is unreachable right now (degrade-open trigger).
    Unavailable,
}

/// The store abstraction [`MergeQueue`] drives — implemented by
/// [`RedisMergeLockStore`] over the shared Redis, and by an offline
/// semantically-identical fake (`fake::InMemoryMergeLockStore`, test-only) so
/// the ordering/lock/crash-backstop guarantees are unit-tested with NO Redis.
#[async_trait]
trait MergeLockStore: Send + Sync {
    /// Take a FIFO ticket for `key` and register it in the wait ordering with
    /// `priority` weighting. Returns the ticket id (unique per `key`).
    /// `wait_ttl` bounds the wait ordering's own self-healing `EXPIRE`
    /// backstop (config-derived by the caller — see
    /// [`MergeQueueConfig::wait_ttl_secs`] — never a bare hardcoded value).
    async fn enqueue(&self, key: &str, priority: i64, wait_ttl: Duration) -> Result<u64, ()>;

    /// Attempt to acquire the critical section for `key` on behalf of
    /// `ticket`: succeeds only if `ticket` is at the front of the wait
    /// ordering AND the lock is not currently held (including "held" meaning
    /// "TTL not yet expired" — an expired lock reads as free). On success the
    /// ticket is removed from the wait ordering and the lock is set with
    /// `fence` and `ttl`.
    async fn try_acquire(
        &self,
        key: &str,
        ticket: u64,
        fence: &str,
        ttl: Duration,
    ) -> Result<AcquireAttempt, ()>;

    /// Remove `ticket` from the wait ordering without acquiring — used when a
    /// waiter gives up (`max_wait_secs` exceeded) so it never blocks the
    /// ticket behind it.
    async fn cancel(&self, key: &str, ticket: u64) -> Result<(), ()>;

    /// Fence-guarded release: frees the lock only if `fence` still matches the
    /// stored value (a stale/expired holder can never free a re-taken lock).
    /// Best-effort / idempotent — a mismatch or an already-free lock is a
    /// safe no-op.
    async fn release(&self, key: &str, fence: &str) -> Result<(), ()>;
}

/// Redis-backed [`MergeLockStore`]. Keys live under `Namespace::Queue`:
/// `queue:merge:seq:{key}` (ticket counter), `queue:merge:wait:{key}` (FIFO/
/// priority ordering ZSET), `queue:merge:lock:{key}` (the serialization lock).
struct RedisMergeLockStore {
    backend: Arc<RedisBackend>,
}

fn seq_key(key: &str) -> String {
    Namespace::Queue.key(&format!("merge:seq:{key}"))
}
fn wait_key(key: &str) -> String {
    Namespace::Queue.key(&format!("merge:wait:{key}"))
}
fn lock_key(key: &str) -> String {
    Namespace::Queue.key(&format!("merge:lock:{key}"))
}

/// FIFO/priority ordering score, mirroring the compiler queue's dispatch ZSET
/// (`src/compiler/queue.rs`, `score = seq - prank*1e12`): a higher `priority`
/// sorts earlier (more negative score) without disturbing FIFO order among
/// equal priorities (broken only by the monotonic `ticket`).
fn ordering_score(ticket: u64, priority: i64) -> f64 {
    ticket as f64 - (priority as f64) * 1_000_000_000_000.0
}

/// Acquire: succeeds only if `ARGV[1]` (this ticket) is the ZSET front AND the
/// lock key is not held. On success, removes the ticket from the wait ZSET and
/// sets the lock (fence + TTL) atomically — no other poller can observe
/// "front + free" and win the same race.
/// KEYS: 1=wait_zset 2=lock  ARGV: 1=ticket_member 2=fence 3=ttl_ms
const TRY_ACQUIRE_LUA: &str = r#"
local front = redis.call('ZRANGE', KEYS[1], 0, 0)
if #front == 0 or front[1] ~= ARGV[1] then
  return {0, 'not_front'}
end
if redis.call('EXISTS', KEYS[2]) == 1 then
  return {0, 'held'}
end
redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('SET', KEYS[2], ARGV[2], 'PX', ARGV[3])
return {1, ARGV[2]}
"#;

/// Fence-guarded release: only deletes the lock if it still holds `ARGV[1]`.
/// KEYS: 1=lock  ARGV: 1=fence
const RELEASE_LUA: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  redis.call('DEL', KEYS[1])
end
return 1
"#;

impl RedisMergeLockStore {
    fn new(backend: Arc<RedisBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl MergeLockStore for RedisMergeLockStore {
    async fn enqueue(&self, key: &str, priority: i64, wait_ttl: Duration) -> Result<u64, ()> {
        let seqk = seq_key(key);
        let ticket: i64 = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("INCR").arg(&seqk).query_async::<_, i64>(&mut conn).await
            })
            .await?;
        let ticket = ticket.max(0) as u64;
        let waitk = wait_key(key);
        let score = ordering_score(ticket, priority);
        let member = ticket.to_string();
        let wait_ttl_secs = wait_ttl.as_secs().max(1) as i64;
        let _: i64 = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("ZADD")
                    .arg(&waitk)
                    .arg(score)
                    .arg(&member)
                    .query_async::<_, i64>(&mut conn)
                    .await?;
                // Self-healing backstop for an abandoned waiter (e.g. the
                // caller crashed between enqueue and its first poll): bound the
                // whole wait ZSET's lifetime by the caller's config-derived
                // `wait_ttl` (see `MergeQueueConfig::wait_ttl_secs`, default
                // `max_wait_secs + 60`) so it cannot wedge a key forever, and
                // so the backstop is never shorter than a legitimately
                // still-polling waiter. Refreshed on every enqueue.
                ::redis::cmd("EXPIRE")
                    .arg(&waitk)
                    .arg(wait_ttl_secs)
                    .query_async::<_, i64>(&mut conn)
                    .await
            })
            .await?;
        Ok(ticket)
    }

    async fn try_acquire(
        &self,
        key: &str,
        ticket: u64,
        fence: &str,
        ttl: Duration,
    ) -> Result<AcquireAttempt, ()> {
        let (waitk, lockk) = (wait_key(key), lock_key(key));
        let member = ticket.to_string();
        let ttl_ms = ttl.as_millis().max(1) as i64;
        let fence = fence.to_string();
        let script = ::redis::Script::new(TRY_ACQUIRE_LUA);
        let out: (i64, String) = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(waitk)
                    .key(lockk)
                    .arg(member)
                    .arg(fence)
                    .arg(ttl_ms)
                    .invoke_async(&mut conn)
                    .await
            })
            .await?;
        Ok(match out.0 {
            1 => AcquireAttempt::Acquired(out.1),
            _ => AcquireAttempt::NotYet,
        })
    }

    async fn cancel(&self, key: &str, ticket: u64) -> Result<(), ()> {
        let waitk = wait_key(key);
        let member = ticket.to_string();
        let _: i64 = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("ZREM")
                    .arg(&waitk)
                    .arg(&member)
                    .query_async::<_, i64>(&mut conn)
                    .await
            })
            .await?;
        Ok(())
    }

    async fn release(&self, key: &str, fence: &str) -> Result<(), ()> {
        let lockk = lock_key(key);
        let fence = fence.to_string();
        let script = ::redis::Script::new(RELEASE_LUA);
        self.backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script.key(lockk).arg(fence).invoke_async::<_, i64>(&mut conn).await
            })
            .await?;
        Ok(())
    }
}

/// Best-effort "log once" guard so a Redis outage during `with_merge_slot`
/// doesn't spam a warning per merge attempt while it's down.
static LOGGED_DEGRADE: AtomicBool = AtomicBool::new(false);

fn log_degrade_once(context: &str) {
    if !LOGGED_DEGRADE.swap(true, Ordering::Relaxed) {
        warn!(
            "merge queue: {context} — degrading open (running the merge unqueued); this warning \
             logs once per process",
        );
    }
}

/// Redis-backed per-base merge lock + FIFO ordering. See the module doc for
/// the mechanism; construct via [`MergeQueue::from_env`].
pub struct MergeQueue {
    store: Arc<dyn MergeLockStore>,
}

impl MergeQueue {
    /// `None` when Redis is not configured — callers fall back to calling
    /// their merge directly, unqueued (degrade-open).
    pub fn from_env() -> Option<Self> {
        RedisBackend::from_env().map(Self::new)
    }

    /// Build over an already-resolved shared Redis backend.
    pub fn new(backend: Arc<RedisBackend>) -> Self {
        Self {
            store: Arc::new(RedisMergeLockStore::new(backend)),
        }
    }

    #[cfg(test)]
    fn from_store(store: Arc<dyn MergeLockStore>) -> Self {
        Self { store }
    }

    /// Run `f` inside the per-`key` critical section: acquire the FIFO/
    /// priority-ordered slot (or degrade open per the module doc), run `f`,
    /// and release — even on an early return or (via the `Drop` guard) a
    /// panic — so the lock can never leak.
    ///
    /// `key` is the caller-chosen critical-section identity (conventionally
    /// `{owner}/{repo}/{base}`); `priority` biases ordering like the compiler
    /// queue (higher runs earlier among concurrent waiters, FIFO within a
    /// priority); `cfg` supplies the lock TTL and the max-wait ceiling.
    ///
    /// ## Lease invariant (not a hard mutex)
    /// `cfg.lock_ttl_secs` MUST exceed the maximum realistic duration of `f`
    /// (the merge). The lock is a **lease**: if `f` runs longer than the
    /// TTL, the lock key expires mid-flight and the next waiter's poll sees
    /// it as free, so its critical section can start while THIS `f` is
    /// still running (see [`MergeQueueConfig::lock_ttl_secs`]). The fence
    /// token guarantees release-safety only — a stale/expired holder can
    /// never free a lock a later holder has since acquired — it does NOT
    /// extend mutual exclusion past the lease window. This is a documented,
    /// accepted tradeoff (`docs/specs/S120-gitea-merge-queue.md` EDGE
    /// CASES), not a bug; there is no heartbeat/renewal here, so set the TTL
    /// comfortably above a merge's realistic duration.
    ///
    /// ## Release timing
    /// On the normal (non-panic) return path, the lock is released
    /// synchronously (awaited) before this function returns — so by the
    /// time a caller observes the result, the next same-key waiter can
    /// already acquire immediately. A panic unwinding through `f` instead
    /// falls back to the `Drop` guard's best-effort spawned release.
    pub async fn with_merge_slot<F, Fut, T>(
        &self,
        key: &str,
        priority: i64,
        cfg: &MergeQueueConfig,
        f: F,
    ) -> Result<T, MergeQueueError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        if !cfg.enabled {
            return Ok(f().await);
        }

        let wait_ttl = Duration::from_secs(cfg.wait_ttl_secs.max(1));
        let ticket = match self.store.enqueue(key, priority, wait_ttl).await {
            Ok(t) => t,
            Err(()) => {
                log_degrade_once("enqueue failed (Redis unreachable)");
                return Ok(f().await);
            }
        };

        let fence = uuid::Uuid::new_v4().simple().to_string();
        let ttl = Duration::from_secs(cfg.lock_ttl_secs.max(1));
        let max_wait = Duration::from_secs(cfg.max_wait_secs);
        let deadline = tokio::time::Instant::now() + max_wait;
        let mut backoff_ms = POLL_MIN_MS;

        loop {
            match self.store.try_acquire(key, ticket, &fence, ttl).await {
                Ok(AcquireAttempt::Acquired(granted_fence)) => {
                    // Guard is the PANIC/early-unwind backstop only: if `f`
                    // panics, unwinding drops `guard` and its `Drop` spawns a
                    // best-effort release. On the NORMAL path below we release
                    // synchronously (awaited) BEFORE returning, then disarm
                    // the guard so `Drop` does not also (redundantly) spawn a
                    // release — this guarantees the lock is already free by
                    // the time `with_merge_slot` returns to its caller, so the
                    // next same-key waiter never sees it held past that point.
                    let mut guard = ReleaseGuard {
                        store: Arc::clone(&self.store),
                        key: key.to_string(),
                        fence: granted_fence,
                        armed: true,
                    };
                    let out = f().await;
                    guard.armed = false;
                    let _ = self.store.release(key, &guard.fence).await;
                    return Ok(out);
                }
                Ok(AcquireAttempt::NotYet) => {}
                Ok(AcquireAttempt::Unavailable) | Err(()) => {
                    // Best-effort: try to drop our ticket before degrading so
                    // we don't wedge the next waiter; ignore the result — the
                    // wait ZSET's own EXPIRE backstop covers a failure here.
                    let _ = self.store.cancel(key, ticket).await;
                    log_degrade_once("acquire failed (Redis unreachable)");
                    return Ok(f().await);
                }
            }

            if tokio::time::Instant::now() >= deadline {
                let _ = self.store.cancel(key, ticket).await;
                return Err(MergeQueueError::Busy);
            }

            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(POLL_MAX_MS);
        }
    }
}

/// PANIC/early-unwind backstop only: the normal (non-panic) path in
/// `with_merge_slot` releases the lock synchronously and sets `armed = false`
/// before returning, so `Drop` becomes a no-op on that path. If `f` panics,
/// unwinding drops this guard while still armed, and `Drop` spawns a
/// best-effort release so the lock can never leak — fenced so it can only
/// ever free the slot THIS acquire holds.
struct ReleaseGuard {
    store: Arc<dyn MergeLockStore>,
    key: String,
    fence: String,
    armed: bool,
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if !self.armed {
            // Normal path already released synchronously; nothing to do.
            return;
        }
        let store = Arc::clone(&self.store);
        let key = self.key.clone();
        let fence = self.fence.clone();
        // Best-effort: spawn the release so `Drop` (sync) can drive an async
        // release. If there is no reachable runtime (e.g. a fully torn-down
        // process), the lock's own TTL is the backstop.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = store.release(&key, &fence).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant as StdInstant;

    /// An offline `MergeLockStore` mirroring the Lua semantics exactly, so the
    /// ordering/lock/crash-backstop guarantees are unit-tested with NO Redis.
    struct InMemoryMergeLockStore {
        state: StdMutex<State>,
    }

    #[derive(Default)]
    struct State {
        seq: HashMap<String, u64>,
        /// key -> Vec<(ticket, score)> kept sorted by score ascending (front = index 0).
        wait: HashMap<String, Vec<(u64, f64)>>,
        /// key -> (fence, expires_at)
        lock: HashMap<String, (String, StdInstant)>,
        /// When true, every op behaves as an unreachable backend.
        down: bool,
    }

    impl InMemoryMergeLockStore {
        fn new() -> Self {
            Self {
                state: StdMutex::new(State::default()),
            }
        }

        fn set_down(&self, down: bool) {
            self.state.lock().unwrap().down = down;
        }

        /// Force-expire a held lock immediately (simulate a crashed holder
        /// whose TTL has elapsed) without waiting in real time.
        fn expire_lock(&self, key: &str) {
            let mut s = self.state.lock().unwrap();
            if let Some((_, exp)) = s.lock.get_mut(key) {
                *exp = StdInstant::now() - Duration::from_millis(1);
            }
        }

        fn is_locked(&self, key: &str) -> bool {
            let s = self.state.lock().unwrap();
            match s.lock.get(key) {
                Some((_, exp)) => *exp > StdInstant::now(),
                None => false,
            }
        }
    }

    #[async_trait]
    impl MergeLockStore for InMemoryMergeLockStore {
        async fn enqueue(&self, key: &str, priority: i64, _wait_ttl: Duration) -> Result<u64, ()> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(());
            }
            let counter = s.seq.entry(key.to_string()).or_insert(0);
            *counter += 1;
            let ticket = *counter;
            let score = ordering_score(ticket, priority);
            let bucket = s.wait.entry(key.to_string()).or_default();
            bucket.push((ticket, score));
            bucket.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            Ok(ticket)
        }

        async fn try_acquire(
            &self,
            key: &str,
            ticket: u64,
            fence: &str,
            ttl: Duration,
        ) -> Result<AcquireAttempt, ()> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Ok(AcquireAttempt::Unavailable);
            }
            let front = s.wait.get(key).and_then(|b| b.first().copied());
            if front.map(|(t, _)| t) != Some(ticket) {
                return Ok(AcquireAttempt::NotYet);
            }
            let held = match s.lock.get(key) {
                Some((_, exp)) => *exp > StdInstant::now(),
                None => false,
            };
            if held {
                return Ok(AcquireAttempt::NotYet);
            }
            if let Some(bucket) = s.wait.get_mut(key) {
                bucket.retain(|(t, _)| *t != ticket);
            }
            s.lock
                .insert(key.to_string(), (fence.to_string(), StdInstant::now() + ttl));
            Ok(AcquireAttempt::Acquired(fence.to_string()))
        }

        async fn cancel(&self, key: &str, ticket: u64) -> Result<(), ()> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(());
            }
            if let Some(bucket) = s.wait.get_mut(key) {
                bucket.retain(|(t, _)| *t != ticket);
            }
            Ok(())
        }

        async fn release(&self, key: &str, fence: &str) -> Result<(), ()> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(());
            }
            if let Some((held_fence, _)) = s.lock.get(key) {
                if held_fence == fence {
                    s.lock.remove(key);
                }
            }
            Ok(())
        }
    }

    fn fast_cfg() -> MergeQueueConfig {
        MergeQueueConfig {
            enabled: true,
            lock_ttl_secs: 60,
            max_wait_secs: 5,
            wait_ttl_secs: 65,
        }
    }

    fn queue_over(store: Arc<InMemoryMergeLockStore>) -> MergeQueue {
        MergeQueue::from_store(store as Arc<dyn MergeLockStore>)
    }

    #[tokio::test]
    async fn two_concurrent_same_key_run_strictly_serially_in_fifo_order() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q1 = Arc::new(queue_over(Arc::clone(&store)));
        let q2 = Arc::new(queue_over(Arc::clone(&store)));
        let cfg = fast_cfg();

        let order = Arc::new(StdMutex::new(Vec::<&'static str>::new()));
        let inside = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let (o1, i1) = (Arc::clone(&order), Arc::clone(&inside));
        let cfg1 = cfg;
        let h1 = tokio::spawn(async move {
            q1.with_merge_slot("owner/repo/main", 0, &cfg1, || async move {
                let n = i1.fetch_add(1, Ordering::SeqCst) + 1;
                assert_eq!(n, 1, "a second holder entered while the first was still inside");
                tokio::time::sleep(Duration::from_millis(60)).await;
                o1.lock().unwrap().push("first");
                i1.fetch_sub(1, Ordering::SeqCst);
            })
            .await
        });
        // Ensure h1 enqueues (and very likely acquires) before h2 enqueues, so
        // FIFO order is deterministic in this test.
        tokio::time::sleep(Duration::from_millis(15)).await;

        let (o2, i2) = (Arc::clone(&order), Arc::clone(&inside));
        let cfg2 = cfg;
        let h2 = tokio::spawn(async move {
            q2.with_merge_slot("owner/repo/main", 0, &cfg2, || async move {
                let n = i2.fetch_add(1, Ordering::SeqCst) + 1;
                assert_eq!(n, 1, "a second holder entered while another was still inside");
                o2.lock().unwrap().push("second");
                i2.fetch_sub(1, Ordering::SeqCst);
            })
            .await
        });

        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();

        assert_eq!(*order.lock().unwrap(), vec!["first", "second"], "waiters must be served FIFO");
    }

    #[tokio::test]
    async fn a_third_key_runs_concurrently_with_a_held_first_key() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q_a = queue_over(Arc::clone(&store));
        let q_b = queue_over(Arc::clone(&store));
        let cfg = fast_cfg();

        let started_a = Arc::new(tokio::sync::Notify::new());
        let sa = Arc::clone(&started_a);
        let ha = tokio::spawn(async move {
            q_a.with_merge_slot("owner/repo/main", 0, &cfg, || async move {
                sa.notify_one();
                tokio::time::sleep(Duration::from_millis(150)).await;
                "a-done"
            })
            .await
        });
        started_a.notified().await;

        // A different key must NOT wait behind key A's held lock.
        let began = StdInstant::now();
        let out_b = q_b
            .with_merge_slot("owner/repo/other", 0, &cfg, || async { "b-done" })
            .await
            .unwrap();
        assert_eq!(out_b, "b-done");
        assert!(
            began.elapsed() < Duration::from_millis(100),
            "an unrelated key must run concurrently, not wait on a different key's lock"
        );

        assert_eq!(ha.await.unwrap().unwrap(), "a-done");
    }

    #[tokio::test]
    async fn a_crashed_holder_lets_the_next_waiter_proceed_once_the_ttl_expires() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q1 = queue_over(Arc::clone(&store));
        let q2 = queue_over(Arc::clone(&store));
        let cfg = fast_cfg();

        // Ticket 1 acquires and then "crashes" (never releases — we leak the
        // guard by leaking the future via mem::forget so Drop never runs,
        // simulating a hard process crash rather than a clean unwind).
        let store_for_ticket1 = Arc::clone(&store);
        let acquired = Arc::new(tokio::sync::Notify::new());
        let acq = Arc::clone(&acquired);
        let crash_cfg = cfg;
        let crash_handle = tokio::spawn(async move {
            let q = queue_over(Arc::clone(&store_for_ticket1));
            let _ = q
                .with_merge_slot("owner/repo/main", 0, &crash_cfg, || async move {
                    acq.notify_one();
                    // Hold well past its own natural completion so we can force
                    // an expiry from the outside rather than racing real time.
                    std::future::pending::<()>().await;
                })
                .await;
        });
        acquired.notified().await;
        assert!(store.is_locked("owner/repo/main"), "ticket 1 must hold the lock");

        // Ticket 2 starts waiting behind the crashed holder.
        let store_for_ticket2 = Arc::clone(&store);
        let cfg2 = cfg;
        let h2 = tokio::spawn(async move {
            let _ = store_for_ticket2; // keep alive
            q2.with_merge_slot("owner/repo/main", 5, &cfg2, || async move { "recovered" })
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Simulate the crash: force the lock's TTL to have elapsed.
        store.expire_lock("owner/repo/main");

        let result = h2.await.unwrap();
        assert_eq!(result, Ok("recovered"), "the next waiter must proceed once the TTL backstop frees the lock");

        crash_handle.abort();
        let _ = q1
            .with_merge_slot("owner/repo/main", 0, &cfg, || async { "cleanup" })
            .await; // drains any leftover state; result not asserted
    }

    #[tokio::test]
    async fn fence_mismatch_never_frees_another_holders_lock() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        // Directly exercise the store: acquire under ticket A's fence, then
        // simulate ticket A's lock expiring and ticket B re-acquiring with a
        // NEW fence; ticket A's late release (old fence) must be a no-op.
        let t1 = store.enqueue("k", 0, Duration::from_secs(60)).await.unwrap();
        let acquired = store
            .try_acquire("k", t1, "fence-A", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(matches!(acquired, AcquireAttempt::Acquired(_)));
        assert!(store.is_locked("k"));

        // Force expiry (crash) and let a new ticket take over with a new fence.
        store.expire_lock("k");
        let t2 = store.enqueue("k", 0, Duration::from_secs(60)).await.unwrap();
        let acquired2 = store
            .try_acquire("k", t2, "fence-B", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(matches!(acquired2, AcquireAttempt::Acquired(_)));
        assert!(store.is_locked("k"), "ticket B now holds the lock");

        // Ticket A's stale release (wrong fence) must NOT free ticket B's lock.
        store.release("k", "fence-A").await.unwrap();
        assert!(store.is_locked("k"), "a fence mismatch must never free another holder's lock");

        // The correct fence DOES free it.
        store.release("k", "fence-B").await.unwrap();
        assert!(!store.is_locked("k"));
    }

    #[tokio::test]
    async fn degrade_open_runs_f_immediately_when_redis_is_absent_conceptually() {
        // "Redis absent" for `MergeQueue` itself means `from_env()` returns
        // `None` and the caller runs `f` directly — there is no `MergeQueue`
        // to call `with_merge_slot` on. Assert that contract at the
        // `from_env` layer (offline: no `REDIS_URL` set in this test process).
        std::env::remove_var("REDIS_URL");
        std::env::remove_var("PLANE_REDIS_URL");
        // `from_env` is a memoized process-global singleton (see
        // `RedisBackend::from_env`'s doc) — assert the *documented* contract
        // (`from_env().map(Self::new)`) is what `MergeQueue::from_env` computes,
        // rather than re-asserting the singleton's own memoization here.
        let expect_none = RedisBackend::from_env().is_none();
        let got = MergeQueue::from_env();
        assert_eq!(got.is_none(), expect_none);
    }

    #[tokio::test]
    async fn disabled_config_runs_f_immediately_even_with_a_store_present() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(store);
        let cfg = MergeQueueConfig {
            enabled: false,
            ..fast_cfg()
        };
        let out = q
            .with_merge_slot("owner/repo/main", 0, &cfg, || async { "ran" })
            .await
            .unwrap();
        assert_eq!(out, "ran");
    }

    #[tokio::test]
    async fn store_unreachable_mid_op_degrades_open() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        store.set_down(true);
        let q = queue_over(store);
        let out = q
            .with_merge_slot("owner/repo/main", 0, &fast_cfg(), || async { "ran-unqueued" })
            .await
            .unwrap();
        assert_eq!(out, "ran-unqueued");
    }

    #[tokio::test]
    async fn normal_return_releases_the_lock_synchronously_before_returning() {
        // Regression test for the review finding: `with_merge_slot` must NOT
        // rely on the `Drop`-spawned release for the normal path — the lock
        // must already be free by the time it returns, so the very next
        // `with_merge_slot` call on the same key acquires immediately without
        // waiting on a scheduled-but-not-yet-run spawned task.
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(Arc::clone(&store));
        let cfg = fast_cfg();

        q.with_merge_slot("owner/repo/main", 0, &cfg, || async { "first" })
            .await
            .unwrap();

        // If release were still only best-effort via `tokio::spawn` in
        // `Drop`, this next acquire could race the not-yet-run spawned task
        // and see the lock as still held (falling through to the poll loop
        // instead of acquiring on the very first attempt). Assert it's
        // immediate by bounding the whole thing tightly in real time.
        let began = StdInstant::now();
        let out = q
            .with_merge_slot("owner/repo/main", 0, &cfg, || async { "second" })
            .await
            .unwrap();
        assert_eq!(out, "second");
        assert!(
            began.elapsed() < Duration::from_millis(20),
            "the lock must already be free immediately after the prior call returned normally, \
             took {:?}",
            began.elapsed()
        );
        assert!(!store.is_locked("owner/repo/main"), "lock must be free after a normal return");
    }

    #[tokio::test]
    async fn max_wait_exceeded_returns_busy() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q1 = queue_over(Arc::clone(&store));
        let q2 = queue_over(Arc::clone(&store));
        let holder_cfg = fast_cfg();

        let started = Arc::new(tokio::sync::Notify::new());
        let s = Arc::clone(&started);
        let holder = tokio::spawn(async move {
            q1.with_merge_slot("owner/repo/main", 0, &holder_cfg, || async move {
                s.notify_one();
                tokio::time::sleep(Duration::from_secs(2)).await;
            })
            .await
        });
        started.notified().await;

        let waiter_cfg = MergeQueueConfig {
            enabled: true,
            lock_ttl_secs: 60,
            max_wait_secs: 0, // any positive wait exceeds this immediately after the first poll
            wait_ttl_secs: 60,
        };
        let result = q2
            .with_merge_slot("owner/repo/main", 0, &waiter_cfg, || async { "should-not-run" })
            .await;
        assert_eq!(result, Err(MergeQueueError::Busy));

        holder.await.unwrap().unwrap();
    }
}
