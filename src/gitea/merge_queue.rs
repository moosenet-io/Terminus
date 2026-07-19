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
    /// "queue busy, retry" rather than hanging. Also returned by
    /// [`MergeQueue::enforce_spacing`] when the min-delay remainder would
    /// exceed `max_wait_secs` (GMQ-03) — the caller should never sleep past
    /// that ceiling either.
    Busy,
    /// GMQ-03 stale-base guard: the PR is not currently mergeable (a stale
    /// base or a real conflict) — do NOT merge. Callers should surface this
    /// as "rebase current base and retry", never race a merge attempt
    /// against Gitea's own `mergeable: Some(false)`.
    NotMergeable,
}

impl std::fmt::Display for MergeQueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeQueueError::Busy => {
                write!(f, "merge queue busy: timed out waiting for the merge slot, retry")
            }
            MergeQueueError::NotMergeable => {
                write!(
                    f,
                    "pull request not mergeable (stale base or conflict) — rebase current base and retry"
                )
            }
        }
    }
}

impl std::error::Error for MergeQueueError {}

/// GMQ-04: map a [`MergeQueueError`] to the [`crate::error::ToolError`] variant
/// `MergePr::execute` surfaces to the caller. `Busy` (queue contention, either
/// from [`MergeQueue::with_merge_slot`]'s own wait ceiling or from
/// [`MergeQueue::enforce_spacing`]'s min-delay remainder exceeding it) is a
/// transient "retry" condition, not a hard failure — mapped to `Execution` so
/// it reads as "the operation didn't complete", distinct from a real conflict.
/// `NotMergeable` (the GMQ-03 stale-base guard) IS a real conflict — mapped to
/// `Conflict` so callers can distinguish "rebase and retry" from "try again
/// shortly".
impl From<MergeQueueError> for crate::error::ToolError {
    fn from(e: MergeQueueError) -> Self {
        match e {
            MergeQueueError::Busy => crate::error::ToolError::Execution(e.to_string()),
            MergeQueueError::NotMergeable => crate::error::ToolError::Conflict(e.to_string()),
        }
    }
}

/// GMQ-03 stale-base guard: the decision [`evaluate_merge_guard`] returns for
/// an already-fetched [`GiteaPullRequest`] — evaluated INSIDE the merge
/// queue's critical section, immediately before the merge POST, so a caller
/// (GMQ-04's `MergePr::execute`) never races a merge against a base that has
/// moved since the PR was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeGuardDecision {
    /// Safe to merge: either `mergeable == Some(true)`, or `mergeable ==
    /// None` (Gitea is still computing it — see the variant's own doc for
    /// why that is treated as Proceed, not blocked).
    Proceed,
    /// The PR is already merged (`merged == true`). Idempotent success: the
    /// caller should report the merge as already done, NOT attempt another
    /// merge POST (Gitea would 405/409 on an already-merged PR).
    AlreadyMerged,
    /// `mergeable == Some(false)` — Gitea has determined this PR cannot be
    /// merged right now (stale base or a real conflict). The caller MUST NOT
    /// POST a merge; surface [`MergeQueueError::NotMergeable`] instead.
    NotMergeable,
}

/// GMQ-03 stale-base guard: decide whether an already-fetched PR is safe to
/// merge right now. Pure/synchronous — the caller is responsible for the
/// `GiteaClient::get` fetch itself (inside the critical section, so the read
/// is current); this function only interprets the result.
///
/// Rules (checked in this order):
/// 1. `merged == true` ⇒ [`MergeGuardDecision::AlreadyMerged`] — idempotent,
///    not an error: some earlier attempt (or another process) already merged
///    this PR, so there's nothing left to do.
/// 2. `mergeable == Some(false)` ⇒ [`MergeGuardDecision::NotMergeable`] — a
///    real, Gitea-confirmed reason the merge cannot proceed (stale base,
///    unresolved conflict). Do NOT merge; the caller should return a clear
///    "rebase current base and retry" error.
/// 3. `mergeable == None` (Gitea has not finished computing mergeability
///    yet — a known Gitea async-compute quirk) ⇒
///    [`MergeGuardDecision::Proceed`]. This is deliberately NOT treated as
///    "unknown, block" — the per-base [`MergeQueue`] critical section has
///    already serialized every other merge to this base, so the fetched PR
///    reflects the current, uncontested state of `base`; there is no
///    concurrent merge that could still land underneath this one while we
///    hold the slot. Blocking here would only punish every merge whose PR
///    happens to have an unset `mergeable` at fetch time, for no safety gain.
/// 4. Otherwise (`mergeable == Some(true)`) ⇒
///    [`MergeGuardDecision::Proceed`].
pub fn evaluate_merge_guard(pr: &crate::gitea::types::GiteaPullRequest) -> MergeGuardDecision {
    if pr.merged {
        return MergeGuardDecision::AlreadyMerged;
    }
    match pr.mergeable {
        Some(false) => MergeGuardDecision::NotMergeable,
        Some(true) | None => MergeGuardDecision::Proceed,
    }
}

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
pub(crate) trait MergeLockStore: Send + Sync {
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

    /// GMQ-03 spacing: the epoch-ms timestamp of the last successful merge to
    /// `key`, or `None` if `key` has never recorded one. Used by
    /// [`MergeQueue::enforce_spacing`] to compute how long (if at all) the
    /// next merge to the same base must wait.
    async fn last_merge_ms(&self, key: &str) -> Result<Option<i64>, ()>;

    /// GMQ-03 spacing: stamp `key`'s last-merge time as `now_ms` (called by
    /// [`MergeQueue::record_merge`] after a successful merge), so the NEXT
    /// same-key merge's [`MergeQueue::enforce_spacing`] call measures the gap
    /// from this point. `ttl_secs` is the marker's own expiry, DERIVED from
    /// the configured spacing window (see [`derive_last_merge_ttl_secs`]) —
    /// never a bare hardcoded value — so the marker can never expire while a
    /// same-key spacing wait could still legitimately depend on it.
    async fn record_merge_ms(&self, key: &str, now_ms: i64, ttl_secs: u64) -> Result<(), ()>;
}

/// GMQ-03 review finding: derive the `queue:merge:last:{key}` marker's TTL
/// from the configured spacing window instead of a bare hardcoded literal.
/// If `GITEA_MERGE_QUEUE_MIN_DELAY_SECS` (or a `max_wait_secs`-bounded wait)
/// exceeded a fixed 24h TTL, the marker would expire before the spacing
/// window ends and [`MergeQueue::enforce_spacing`] would then treat the base
/// as never-merged and proceed immediately — silently violating the "same-base
/// merges are spaced ≥ min_delay" contract. Taking `max(min_delay_secs,
/// LAST_MERGE_TTL_FLOOR_SECS) + LAST_MERGE_TTL_MARGIN_SECS` keeps the normal
/// small-delay case at the same generous 24h-plus retention floor as before,
/// while guaranteeing the marker always outlives any legitimately-configured
/// spacing window.
fn derive_last_merge_ttl_secs(min_delay_secs: u64) -> u64 {
    min_delay_secs
        .max(LAST_MERGE_TTL_FLOOR_SECS)
        .saturating_add(LAST_MERGE_TTL_MARGIN_SECS)
}

/// Retention floor for the last-merge marker when `min_delay_secs` is small:
/// self-cleans an abandoned base after ~24h rather than accumulating forever.
const LAST_MERGE_TTL_FLOOR_SECS: u64 = 86_400;
/// Safety margin added atop `min_delay_secs`/the floor so the marker never
/// expires exactly as the spacing window ends.
const LAST_MERGE_TTL_MARGIN_SECS: u64 = 60;

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
/// Companion ZSET to `wait_key`: member = ticket, score = the ticket's own
/// enqueue-time deadline (epoch ms). This is the per-waiter age-prune
/// mechanism (Finding A / GMQ-02 r2) — distinct from (and stronger than) the
/// whole-ZSET `EXPIRE` backstop on `wait_key`, which only bounds the ENTIRE
/// key's lifetime and gets pushed out on every unrelated enqueue. A ticket's
/// individual deadline here is fixed at enqueue time and is never refreshed,
/// so a caller that crashes while holding the front (or any) ticket ages out
/// on its own, bounded by `wait_ttl`, regardless of how many later callers
/// enqueue behind it.
fn deadline_key(key: &str) -> String {
    Namespace::Queue.key(&format!("merge:deadline:{key}"))
}
/// GMQ-03 spacing: `queue:merge:last:{key}` holds the epoch-ms timestamp of
/// the last successful merge to `key` — a plain `GET`/`SET`, not a ZSET (only
/// ever one value per key).
fn last_key(key: &str) -> String {
    Namespace::Queue.key(&format!("merge:last:{key}"))
}

/// Current epoch time in milliseconds, for the deadline ZSET's score.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// FIFO/priority ordering score, mirroring the compiler queue's dispatch ZSET
/// (`src/compiler/queue.rs`, `score = seq - prank*1e12`): a higher `priority`
/// sorts earlier (more negative score) without disturbing FIFO order among
/// equal priorities (broken only by the monotonic `ticket`).
fn ordering_score(ticket: u64, priority: i64) -> f64 {
    ticket as f64 - (priority as f64) * 1_000_000_000_000.0
}

/// Acquire: first PRUNES any waiter whose own enqueue-time deadline (recorded
/// in the companion `deadline_zset`, see [`deadline_key`]) has passed — this
/// is the age-based self-heal for an abandoned (e.g. hard-crashed) waiter, so
/// a stale ticket can never wedge the front forever regardless of how many
/// later callers enqueue behind it (Finding A / GMQ-02 r2). After pruning,
/// succeeds only if `ARGV[1]` (this ticket) is the ZSET front AND the lock key
/// is not held. On success, removes the ticket from both ZSETs and sets the
/// lock (fence + TTL) atomically — no other poller can observe "front + free"
/// and win the same race.
/// KEYS: 1=wait_zset 2=lock 3=deadline_zset  ARGV: 1=ticket_member 2=fence 3=ttl_ms 4=now_ms
const TRY_ACQUIRE_LUA: &str = r#"
local expired = redis.call('ZRANGEBYSCORE', KEYS[3], '-inf', ARGV[4])
for i = 1, #expired do
  redis.call('ZREM', KEYS[1], expired[i])
  redis.call('ZREM', KEYS[3], expired[i])
end
local front = redis.call('ZRANGE', KEYS[1], 0, 0)
if #front == 0 or front[1] ~= ARGV[1] then
  return {0, 'not_front'}
end
if redis.call('EXISTS', KEYS[2]) == 1 then
  return {0, 'held'}
end
redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('ZREM', KEYS[3], ARGV[1])
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

/// Atomic enqueue (Finding A / GMQ-02 r3): allocates the ticket, adds it to
/// BOTH the wait ZSET and the deadline ZSET, and sets both keys' `EXPIRE`
/// backstop — all in ONE Lua script, so a mid-enqueue connection error can
/// never leave a partial ticket (present in the wait ZSET but missing from
/// the deadline ZSET, and therefore un-age-prunable — see the module doc's
/// "orphan-ticket wedge" description). Either the ticket exists in both
/// ZSETs, or nothing was written at all.
/// KEYS: 1=seq 2=wait_zset 3=deadline_zset
/// ARGV: 1=priority 2=wait_ttl_secs 3=now_ms 4=wait_ttl_ms
const ENQUEUE_LUA: &str = r#"
local ticket = redis.call('INCR', KEYS[1])
local priority = tonumber(ARGV[1])
local score = ticket - (priority * 1000000000000.0)
local member = tostring(ticket)
local deadline_ms = tonumber(ARGV[3]) + tonumber(ARGV[4])
redis.call('ZADD', KEYS[2], score, member)
redis.call('ZADD', KEYS[3], deadline_ms, member)
redis.call('EXPIRE', KEYS[2], ARGV[2])
redis.call('EXPIRE', KEYS[3], ARGV[2])
return ticket
"#;

impl RedisMergeLockStore {
    fn new(backend: Arc<RedisBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl MergeLockStore for RedisMergeLockStore {
    async fn enqueue(&self, key: &str, priority: i64, wait_ttl: Duration) -> Result<u64, ()> {
        let (seqk, waitk, deadlinek) = (seq_key(key), wait_key(key), deadline_key(key));
        let wait_ttl_secs = wait_ttl.as_secs().max(1) as i64;
        let wait_ttl_ms = wait_ttl.as_millis() as i64;
        let now = now_ms();
        let script = ::redis::Script::new(ENQUEUE_LUA);
        let ticket: i64 = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(seqk)
                    .key(waitk)
                    .key(deadlinek)
                    .arg(priority)
                    .arg(wait_ttl_secs)
                    .arg(now)
                    .arg(wait_ttl_ms)
                    .invoke_async(&mut conn)
                    .await
            })
            .await?;
        Ok(ticket.max(0) as u64)
    }

    async fn try_acquire(
        &self,
        key: &str,
        ticket: u64,
        fence: &str,
        ttl: Duration,
    ) -> Result<AcquireAttempt, ()> {
        let (waitk, lockk, deadlinek) = (wait_key(key), lock_key(key), deadline_key(key));
        let member = ticket.to_string();
        let ttl_ms = ttl.as_millis().max(1) as i64;
        let fence = fence.to_string();
        let now = now_ms();
        let script = ::redis::Script::new(TRY_ACQUIRE_LUA);
        let out: (i64, String) = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(waitk)
                    .key(lockk)
                    .key(deadlinek)
                    .arg(member)
                    .arg(fence)
                    .arg(ttl_ms)
                    .arg(now)
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
        let deadlinek = deadline_key(key);
        let member = ticket.to_string();
        let _: i64 = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("ZREM")
                    .arg(&waitk)
                    .arg(&member)
                    .query_async::<_, i64>(&mut conn)
                    .await?;
                ::redis::cmd("ZREM")
                    .arg(&deadlinek)
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

    async fn last_merge_ms(&self, key: &str) -> Result<Option<i64>, ()> {
        let lastk = last_key(key);
        let val: Option<i64> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("GET").arg(&lastk).query_async::<_, Option<i64>>(&mut conn).await
            })
            .await?;
        Ok(val)
    }

    async fn record_merge_ms(&self, key: &str, now_ms: i64, ttl_secs: u64) -> Result<(), ()> {
        let lastk = last_key(key);
        // TTL is caller-derived from the configured spacing window (see
        // `derive_last_merge_ttl_secs`) — NOT a bare hardcoded value — so the
        // marker always outlives any legitimately-configured min-delay,
        // while still self-cleaning if this base is never merged to again.
        let ttl_secs = ttl_secs as i64;
        let _: () = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("SET")
                    .arg(&lastk)
                    .arg(now_ms)
                    .arg("EX")
                    .arg(ttl_secs)
                    .query_async::<_, ()>(&mut conn)
                    .await
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

    /// Construct over an arbitrary [`MergeLockStore`] — `pub(crate)` (not just
    /// module-private) so tests OUTSIDE this module (GMQ-04's `crate::gitea`
    /// tool tests) can build a real `MergeQueue` over the `fake` module's
    /// in-memory store without a live Redis.
    #[cfg(test)]
    pub(crate) fn from_store(store: Arc<dyn MergeLockStore>) -> Self {
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
                    // Guard is the PANIC/early-unwind/cancellation backstop:
                    // if `f` panics, OR this `.await` is itself dropped
                    // (cancelled) before completing, unwinding drops `guard`
                    // and — as long as it is still armed — its `Drop` spawns
                    // a best-effort release. On the NORMAL path below we must
                    // NOT disarm the guard until AFTER the synchronous release
                    // has actually completed (Finding B / GMQ-02 r3): disarming
                    // first and awaiting the release second leaves a window
                    // where a cancellation of THIS future (e.g. the caller's
                    // own future being dropped mid-`release().await`) sees
                    // `armed == false` and skips the fallback entirely, so the
                    // lock would linger until its TTL. Awaiting the release
                    // first, and disarming only once it has returned, ensures
                    // a cancellation during the release attempt still leaves
                    // `armed == true` and so still triggers the `Drop`
                    // fallback.
                    let mut guard = ReleaseGuard {
                        store: Arc::clone(&self.store),
                        key: key.to_string(),
                        fence: granted_fence,
                        armed: true,
                    };
                    let out = f().await;
                    let _ = self.store.release(key, &guard.fence).await;
                    guard.armed = false;
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

    /// GMQ-03 spacing: block (bounded) until at least `min_delay_secs` have
    /// elapsed since the last recorded merge to `key` ([`Self::record_merge`]),
    /// then return. Must be called AFTER acquiring the `key` critical section
    /// (i.e. from inside the closure passed to [`Self::with_merge_slot`]) —
    /// spacing is a per-base property of the merge sequence, not a
    /// stand-alone gate a caller could correctly enforce outside the lock.
    ///
    /// - `min_delay_secs == 0` ⇒ returns immediately, no Redis round-trip at
    ///   all — the explicit "spacing disabled" fast path.
    /// - No prior recorded merge for `key` ⇒ returns immediately (nothing to
    ///   space against).
    /// - The store being unreachable degrades open (logs once, proceeds
    ///   immediately) — same posture as the rest of the queue (availability
    ///   over strictness for a short merge).
    /// - If the remaining wait would exceed `max_wait_secs`, returns
    ///   [`MergeQueueError::Busy`] rather than sleeping past the caller's own
    ///   ceiling (the edge case the spec calls out explicitly).
    pub async fn enforce_spacing(
        &self,
        key: &str,
        min_delay_secs: u64,
        max_wait_secs: u64,
    ) -> Result<(), MergeQueueError> {
        if min_delay_secs == 0 {
            return Ok(());
        }
        let last_ms = match self.store.last_merge_ms(key).await {
            Ok(v) => v,
            Err(()) => {
                log_degrade_once("spacing lookup failed (Redis unreachable)");
                return Ok(());
            }
        };
        let Some(last_ms) = last_ms else {
            // Never merged before under this key — nothing to space against.
            return Ok(());
        };

        let now = now_ms();
        let min_delay_ms = (min_delay_secs as i64).saturating_mul(1000);
        let elapsed_ms = now.saturating_sub(last_ms).max(0);
        if elapsed_ms >= min_delay_ms {
            return Ok(());
        }
        let remainder_ms = (min_delay_ms - elapsed_ms) as u64;
        let max_wait_ms = max_wait_secs.saturating_mul(1000);
        if remainder_ms > max_wait_ms {
            return Err(MergeQueueError::Busy);
        }
        tokio::time::sleep(Duration::from_millis(remainder_ms)).await;
        Ok(())
    }

    /// GMQ-03 spacing: stamp `key`'s last-merge time as "now" — call this
    /// AFTER a successful merge (still inside the critical section) so the
    /// NEXT same-key merge's [`Self::enforce_spacing`] call measures the gap
    /// from this point. `min_delay_secs` is the same value the caller passes
    /// to [`Self::enforce_spacing`] (from `cfg`/`GITEA_MERGE_QUEUE_MIN_DELAY_SECS`)
    /// — it is used ONLY to derive the marker's own TTL
    /// ([`derive_last_merge_ttl_secs`]), so the marker can never expire
    /// before a same-key spacing wait bounded by that delay could still
    /// legitimately depend on it. Best-effort: a store failure here only
    /// means the NEXT merge's spacing check degrades open (treats it as
    /// "never merged before"), which is the same safe direction every other
    /// degrade-open path in this module takes — it never blocks a merge that
    /// already succeeded.
    pub async fn record_merge(&self, key: &str, min_delay_secs: u64) {
        let ttl_secs = derive_last_merge_ttl_secs(min_delay_secs);
        if self.store.record_merge_ms(key, now_ms(), ttl_secs).await.is_err() {
            log_degrade_once("recording last-merge time failed (Redis unreachable)");
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

/// Offline `MergeLockStore` fake, exposed `pub(crate)` (mirrors
/// `crate::compiler::queue`'s own `#[cfg(test)] pub(crate) mod fake` pattern)
/// so tests OUTSIDE this module — specifically GMQ-04's `crate::gitea` tool
/// tests — can build a real, fully-functional `MergeQueue` (ordering, the
/// crash-backstop lock, spacing, everything) with NO live Redis, the exact
/// same way this module's own `tests` below do.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant as StdInstant;

    /// An offline `MergeLockStore` mirroring the Lua semantics exactly, so the
    /// ordering/lock/crash-backstop guarantees are unit-tested with NO Redis.
    pub(crate) struct InMemoryMergeLockStore {
        state: StdMutex<State>,
    }

    #[derive(Default)]
    struct State {
        seq: HashMap<String, u64>,
        /// key -> Vec<(ticket, score, deadline)> kept sorted by score ascending
        /// (front = index 0). `deadline` mirrors the Redis companion
        /// `deadline_zset`: fixed at enqueue time, never refreshed by later
        /// enqueues — a ticket past its own deadline is pruned in
        /// `try_acquire`, exactly like `TRY_ACQUIRE_LUA`.
        wait: HashMap<String, Vec<(u64, f64, StdInstant)>>,
        /// key -> (fence, expires_at)
        lock: HashMap<String, (String, StdInstant)>,
        /// key -> last-merge epoch-ms (GMQ-03 spacing), mirroring
        /// `queue:merge:last:{key}`.
        last: HashMap<String, i64>,
        /// key -> the TTL (seconds) `record_merge_ms` was last called with,
        /// so tests can assert the derived TTL without needing a real
        /// expiring store.
        last_ttl_secs: HashMap<String, u64>,
        /// When true, every op behaves as an unreachable backend.
        down: bool,
    }

    impl InMemoryMergeLockStore {
        pub(crate) fn new() -> Self {
            Self {
                state: StdMutex::new(State::default()),
            }
        }

        pub(crate) fn set_down(&self, down: bool) {
            self.state.lock().unwrap().down = down;
        }

        /// Force-expire a held lock immediately (simulate a crashed holder
        /// whose TTL has elapsed) without waiting in real time.
        pub(crate) fn expire_lock(&self, key: &str) {
            let mut s = self.state.lock().unwrap();
            if let Some((_, exp)) = s.lock.get_mut(key) {
                *exp = StdInstant::now() - Duration::from_millis(1);
            }
        }

        /// Force a still-waiting ticket's own age-prune deadline into the past
        /// (simulate a hard-crashed waiter whose `wait_ttl_secs` has elapsed)
        /// without waiting in real time — the in-memory analog of forcing an
        /// entry in Redis's `deadline_zset` below `now_ms()`.
        pub(crate) fn expire_ticket(&self, key: &str, ticket: u64) {
            let mut s = self.state.lock().unwrap();
            if let Some(bucket) = s.wait.get_mut(key) {
                for entry in bucket.iter_mut() {
                    if entry.0 == ticket {
                        entry.2 = StdInstant::now() - Duration::from_millis(1);
                    }
                }
            }
        }

        /// Tickets currently still waiting (front-to-back order) for `key`,
        /// for assertions that a ticket was (or was not) removed.
        pub(crate) fn waiting_tickets(&self, key: &str) -> Vec<u64> {
            let s = self.state.lock().unwrap();
            s.wait
                .get(key)
                .map(|b| b.iter().map(|(t, _, _)| *t).collect())
                .unwrap_or_default()
        }

        pub(crate) fn is_locked(&self, key: &str) -> bool {
            let s = self.state.lock().unwrap();
            match s.lock.get(key) {
                Some((_, exp)) => *exp > StdInstant::now(),
                None => false,
            }
        }

        /// The TTL (seconds) `record_merge_ms` was last called with for
        /// `key`, or `None` if it was never recorded.
        pub(crate) fn last_ttl_secs(&self, key: &str) -> Option<u64> {
            let s = self.state.lock().unwrap();
            s.last_ttl_secs.get(key).copied()
        }
    }

    #[async_trait]
    impl MergeLockStore for InMemoryMergeLockStore {
        async fn enqueue(&self, key: &str, priority: i64, wait_ttl: Duration) -> Result<u64, ()> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(());
            }
            let counter = s.seq.entry(key.to_string()).or_insert(0);
            *counter += 1;
            let ticket = *counter;
            let score = ordering_score(ticket, priority);
            let deadline = StdInstant::now() + wait_ttl;
            let bucket = s.wait.entry(key.to_string()).or_default();
            bucket.push((ticket, score, deadline));
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
            // Age-prune (Finding A / GMQ-02 r2): drop any ticket whose own
            // enqueue-time deadline has passed, mirroring
            // `TRY_ACQUIRE_LUA`'s `ZRANGEBYSCORE`+`ZREM` on `deadline_zset`.
            // This runs BEFORE the front check so an abandoned front ticket
            // cannot wedge a later, still-live ticket behind it.
            let now = StdInstant::now();
            if let Some(bucket) = s.wait.get_mut(key) {
                bucket.retain(|(_, _, deadline)| *deadline > now);
            }
            let front = s.wait.get(key).and_then(|b| b.first().copied());
            if front.map(|(t, _, _)| t) != Some(ticket) {
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
                bucket.retain(|(t, _, _)| *t != ticket);
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
                bucket.retain(|(t, _, _)| *t != ticket);
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

        async fn last_merge_ms(&self, key: &str) -> Result<Option<i64>, ()> {
            let s = self.state.lock().unwrap();
            if s.down {
                return Err(());
            }
            Ok(s.last.get(key).copied())
        }

        async fn record_merge_ms(&self, key: &str, now_ms: i64, ttl_secs: u64) -> Result<(), ()> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(());
            }
            s.last.insert(key.to_string(), now_ms);
            s.last_ttl_secs.insert(key.to_string(), ttl_secs);
            Ok(())
        }
    }

    /// Build a real `MergeQueue` over a fresh in-memory fake store — the
    /// crate-wide entry point other modules' tests use (GMQ-04's
    /// `crate::gitea` tool tests included) to exercise `with_merge_slot`/
    /// `enforce_spacing`/`record_merge` end-to-end with no live Redis.
    pub(crate) fn queue_over(store: Arc<InMemoryMergeLockStore>) -> MergeQueue {
        MergeQueue::from_store(store as Arc<dyn MergeLockStore>)
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{queue_over, InMemoryMergeLockStore};
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant as StdInstant;

    fn fast_cfg() -> MergeQueueConfig {
        MergeQueueConfig {
            enabled: true,
            lock_ttl_secs: 60,
            max_wait_secs: 5,
            wait_ttl_secs: 65,
        }
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

    // ── GMQ-02 r2: Finding A — per-waiter self-heal ─────────────────────

    #[tokio::test]
    async fn a_crashed_front_waiters_ticket_is_age_pruned_and_does_not_wedge_the_queue() {
        // Simulate a caller that enqueued and then hard-crashed (never polled,
        // never called `cancel`) while its ticket sat at the FRONT. Unlike the
        // whole-ZSET EXPIRE (which a later enqueue would keep refreshing
        // forever), the per-ticket deadline is fixed at enqueue time, so once
        // it's forced into the past (standing in for `wait_ttl_secs`
        // elapsing), `try_acquire` prunes it and a later, live waiter is
        // unwedged — even though a second ticket was enqueued AFTER the first
        // one's deadline was set, which would have refreshed a whole-ZSET TTL.
        let store = Arc::new(InMemoryMergeLockStore::new());
        let cfg = fast_cfg();

        let crashed_ticket = store.enqueue("owner/repo/main", 0, Duration::from_secs(60)).await.unwrap();
        assert_eq!(store.waiting_tickets("owner/repo/main"), vec![crashed_ticket]);

        // A later, live waiter enqueues behind the crashed ticket (this is the
        // "refreshes the whole-ZSET TTL under retry/load" scenario from the
        // finding) and would normally be wedged behind it forever.
        let q2 = queue_over(Arc::clone(&store));
        let live_cfg = cfg;
        let waiter = tokio::spawn(async move {
            q2.with_merge_slot("owner/repo/main", 0, &live_cfg, || async { "recovered" }).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Force the crashed ticket's own deadline into the past — the
        // in-memory analog of `wait_ttl_secs` having elapsed for JUST that
        // ticket, independent of any later enqueue.
        store.expire_ticket("owner/repo/main", crashed_ticket);

        let result = waiter.await.unwrap();
        assert_eq!(
            result,
            Ok("recovered"),
            "a later waiter must proceed once the abandoned front ticket ages out, not wedge forever"
        );
        assert!(
            !store.waiting_tickets("owner/repo/main").contains(&crashed_ticket),
            "the pruned, abandoned ticket must be gone from the wait ordering"
        );
    }

    #[tokio::test]
    async fn busy_waiter_removes_its_own_ticket_so_it_does_not_block_the_next_waiter() {
        // Ticket 1 is enqueued directly (bypassing `with_merge_slot`) and never
        // resolved — it sits at the front forever, standing in for some other
        // in-flight (not crashed, just slow/stuck) waiter. Ticket 2 goes
        // through `with_merge_slot`, never reaches the front, hits
        // `max_wait_secs`, and must return `Busy` — the fix under test is that
        // ticket 2 removes ITS OWN ticket on that exit path so it can never
        // block whoever enqueues after it.
        let store = Arc::new(InMemoryMergeLockStore::new());
        let stuck_ticket = store.enqueue("owner/repo/main", 0, Duration::from_secs(600)).await.unwrap();

        let q2 = queue_over(Arc::clone(&store));
        let waiter_cfg = MergeQueueConfig {
            enabled: true,
            lock_ttl_secs: 60,
            max_wait_secs: 0, // times out on the very first poll
            wait_ttl_secs: 600,
        };
        let result = q2
            .with_merge_slot("owner/repo/main", 0, &waiter_cfg, || async { "should-not-run" })
            .await;
        assert_eq!(result, Err(MergeQueueError::Busy));

        // Ticket 2 must be gone; only the stuck front ticket remains.
        assert_eq!(
            store.waiting_tickets("owner/repo/main"),
            vec![stuck_ticket],
            "a Busy (max_wait-exceeded) waiter must remove its own ticket, not leave it behind"
        );

        // A third, subsequent waiter enqueues cleanly behind the (still
        // legitimately stuck) front ticket, proving ticket 2 left no trace
        // that would otherwise double up / corrupt the ordering.
        let q3 = queue_over(Arc::clone(&store));
        let third_ticket = q3
            .store
            .enqueue("owner/repo/main", 0, Duration::from_secs(600))
            .await
            .unwrap();
        assert_eq!(store.waiting_tickets("owner/repo/main"), vec![stuck_ticket, third_ticket]);
    }

    // ── GMQ-02 r3: Finding A — atomic enqueue ───────────────────────────

    #[tokio::test]
    async fn a_simulated_enqueue_failure_leaves_no_partial_ticket() {
        // The in-memory fake mirrors the Redis Lua script's all-or-nothing
        // semantics: the `down` check happens BEFORE any mutation (ticket
        // allocation, wait-ZSET insert, or deadline-ZSET insert), under the
        // same mutex-held critical section, so a "failed" enqueue can never
        // leave a ticket in one ZSET but not the other (the orphan-ticket
        // wedge from Finding A). Assert directly: after a failed enqueue,
        // there is NO ticket at all in the wait ordering for this key.
        let store = Arc::new(InMemoryMergeLockStore::new());
        store.set_down(true);

        let err = store.enqueue("owner/repo/main", 0, Duration::from_secs(60)).await;
        assert_eq!(err, Err(()), "a down backend must fail the enqueue outright");
        assert!(
            store.waiting_tickets("owner/repo/main").is_empty(),
            "a failed enqueue must leave NO partial ticket in the wait ordering"
        );

        // Recovery: once the backend is back up, enqueue must start clean —
        // no residue from the failed attempt (e.g. no ticket-number gap that
        // would itself indicate a partial write was rolled back rather than
        // never having happened).
        store.set_down(false);
        let ticket = store.enqueue("owner/repo/main", 0, Duration::from_secs(60)).await.unwrap();
        assert_eq!(ticket, 1, "the failed attempt must not have consumed a ticket number");
        assert_eq!(store.waiting_tickets("owner/repo/main"), vec![ticket]);
    }

    // ── GMQ-02 r3: Finding B — disarm only after release completes ─────

    /// Wraps an [`InMemoryMergeLockStore`] so the FIRST call to `release`
    /// never completes (stands in for that `.await` being cancelled
    /// mid-flight — e.g. the caller's own future dropped while awaiting
    /// `with_merge_slot`), while every subsequent call (the `Drop` guard's
    /// spawned fallback) behaves normally. Lets the test observe whether the
    /// `Drop` fallback still fires when the *synchronous* release attempt in
    /// the normal path never returns.
    struct CancelDuringReleaseStore {
        inner: Arc<InMemoryMergeLockStore>,
        release_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl MergeLockStore for CancelDuringReleaseStore {
        async fn enqueue(&self, key: &str, priority: i64, wait_ttl: Duration) -> Result<u64, ()> {
            self.inner.enqueue(key, priority, wait_ttl).await
        }
        async fn try_acquire(
            &self,
            key: &str,
            ticket: u64,
            fence: &str,
            ttl: Duration,
        ) -> Result<AcquireAttempt, ()> {
            self.inner.try_acquire(key, ticket, fence, ttl).await
        }
        async fn cancel(&self, key: &str, ticket: u64) -> Result<(), ()> {
            self.inner.cancel(key, ticket).await
        }
        async fn release(&self, key: &str, fence: &str) -> Result<(), ()> {
            let call_no = self.release_calls.fetch_add(1, Ordering::SeqCst);
            if call_no == 0 {
                // Simulate the normal path's `release(...).await` being
                // cancelled mid-flight by never resolving. Whatever polls
                // this future is expected to be dropped (aborted) rather than
                // ever observe this return.
                std::future::pending::<()>().await;
                unreachable!("this future must be dropped (cancelled), never polled to completion");
            }
            self.inner.release(key, fence).await
        }
        async fn last_merge_ms(&self, key: &str) -> Result<Option<i64>, ()> {
            self.inner.last_merge_ms(key).await
        }
        async fn record_merge_ms(&self, key: &str, now_ms: i64, ttl_secs: u64) -> Result<(), ()> {
            self.inner.record_merge_ms(key, now_ms, ttl_secs).await
        }
    }

    #[tokio::test]
    async fn cancellation_during_the_normal_paths_release_await_still_triggers_the_drop_fallback() {
        // Regression test for Finding B: if `guard.armed` were set to `false`
        // BEFORE awaiting `store.release(...)` (the pre-fix ordering), a
        // cancellation of the `with_merge_slot` future while it's stuck
        // inside that `release().await` would drop `guard` with
        // `armed == false`, so `Drop` would skip the fallback release and the
        // lock would linger until its TTL. With the fix, `armed` stays `true`
        // until the release call actually returns, so `Drop` still spawns a
        // fallback release when this in-flight call is aborted — and that
        // fallback (the store's SECOND `release` call) is what actually frees
        // the lock.
        let inner = Arc::new(InMemoryMergeLockStore::new());
        let store = Arc::new(CancelDuringReleaseStore {
            inner: Arc::clone(&inner),
            release_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let q = MergeQueue::from_store(Arc::clone(&store) as Arc<dyn MergeLockStore>);
        let cfg = fast_cfg();

        let task = tokio::spawn(async move {
            q.with_merge_slot("owner/repo/main", 0, &cfg, || async { "done" }).await
        });

        // Give the task time to: enqueue, acquire, run `f`, and get stuck
        // inside the first (never-resolving) `release(...).await`.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            inner.is_locked("owner/repo/main"),
            "the lock must still be held while stuck inside the stuck release call"
        );

        // Simulate cancellation: abort the task while it is parked inside
        // `release(...).await`, dropping `with_merge_slot`'s future (and, in
        // it, the `ReleaseGuard`) without ever reaching the normal return.
        task.abort();
        let _ = task.await; // JoinError::is_cancelled(); result not needed

        // Give the `Drop`-spawned fallback release a moment to run.
        tokio::time::sleep(Duration::from_millis(60)).await;

        assert!(
            !inner.is_locked("owner/repo/main"),
            "the Drop guard's fallback release must still free the lock when the normal path's \
             own release call is cancelled mid-flight"
        );
        assert_eq!(
            store.release_calls.load(Ordering::SeqCst),
            2,
            "expected the stuck normal-path release call plus exactly one Drop-fallback release call"
        );
    }

    // ── GMQ-03: min-delay spacing ────────────────────────────────────────

    #[tokio::test]
    async fn same_key_merges_are_spaced_at_least_min_delay_apart() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(Arc::clone(&store));

        // First merge: nothing recorded yet, so spacing must not wait at all.
        let began = StdInstant::now();
        q.enforce_spacing("owner/repo/main", 5, 300).await.unwrap();
        assert!(
            began.elapsed() < Duration::from_millis(50),
            "the first merge to a key must never wait on spacing (nothing recorded yet)"
        );
        q.record_merge("owner/repo/main", 5).await;

        // Second merge to the SAME key, immediately after: must wait roughly
        // the configured min_delay (use a small delay so the test stays fast
        // — the mechanism is what's under test, not the exact magnitude).
        let began2 = StdInstant::now();
        q.enforce_spacing("owner/repo/main", 1, 300).await.unwrap();
        let waited = began2.elapsed();
        assert!(
            waited >= Duration::from_millis(900),
            "a second same-key merge must wait close to the full min_delay_secs, waited {waited:?}"
        );
    }

    #[tokio::test]
    async fn min_delay_zero_never_waits() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(Arc::clone(&store));

        q.record_merge("owner/repo/main", 0).await;

        let began = StdInstant::now();
        q.enforce_spacing("owner/repo/main", 0, 300).await.unwrap();
        assert!(
            began.elapsed() < Duration::from_millis(20),
            "min_delay_secs == 0 must disable spacing entirely, no artificial delay"
        );
    }

    #[tokio::test]
    async fn spacing_wait_exceeding_max_wait_returns_busy_instead_of_sleeping() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(Arc::clone(&store));

        // Record a merge "now"; a subsequent enforce_spacing with a large
        // min_delay but a tiny max_wait ceiling must return Busy immediately
        // rather than sleeping past that ceiling.
        q.record_merge("owner/repo/main", 600).await;
        let began = StdInstant::now();
        let result = q.enforce_spacing("owner/repo/main", 600, 1).await;
        assert_eq!(result, Err(MergeQueueError::Busy));
        assert!(
            began.elapsed() < Duration::from_millis(50),
            "a spacing wait that would exceed max_wait_secs must return Busy immediately, not sleep"
        );
    }

    #[tokio::test]
    async fn spacing_degrades_open_when_store_is_unreachable() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(Arc::clone(&store));
        store.set_down(true);

        let began = StdInstant::now();
        let result = q.enforce_spacing("owner/repo/main", 30, 300).await;
        assert_eq!(result, Ok(()));
        assert!(
            began.elapsed() < Duration::from_millis(50),
            "an unreachable store must degrade open (proceed immediately), not block on spacing"
        );
    }

    // ── GMQ-03 review: last-merge marker TTL must be derived, never a bare
    // hardcoded 86_400 that could expire before a large min_delay's spacing
    // window ends ─────────────────────────────────────────────────────────

    #[test]
    fn last_merge_ttl_is_derived_from_min_delay_not_a_bare_constant() {
        // A tiny min_delay stays at the generous 24h-plus-margin retention
        // floor (unchanged behavior for the common case).
        assert_eq!(derive_last_merge_ttl_secs(5), 86_400 + 60);
        assert_eq!(derive_last_merge_ttl_secs(0), 86_400 + 60);

        // A min_delay LARGER than the old bare 86_400 constant must push the
        // derived TTL past it (with margin) — this is exactly the finding:
        // the marker must always outlive the configured spacing window.
        let large_min_delay = 200_000_u64; // > 86_400
        let ttl = derive_last_merge_ttl_secs(large_min_delay);
        assert!(
            ttl >= large_min_delay + 60,
            "derived TTL {ttl} must be >= min_delay_secs ({large_min_delay}) + margin, \
             otherwise the marker expires before the spacing window it's supposed to protect"
        );
        assert_eq!(ttl, large_min_delay + 60);
    }

    #[tokio::test]
    async fn record_merge_passes_a_ttl_derived_from_min_delay_to_the_store() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(Arc::clone(&store));

        // A min_delay far exceeding the old hardcoded 86_400s literal: if the
        // TTL were still a bare 86_400, it would be LESS than min_delay_secs
        // here, reproducing the finding (marker expires mid-spacing-window).
        let large_min_delay = 200_000_u64;
        q.record_merge("owner/repo/main", large_min_delay).await;

        let ttl = store
            .last_ttl_secs("owner/repo/main")
            .expect("record_merge must record a TTL");
        assert!(
            ttl >= large_min_delay + 60,
            "the marker TTL ({ttl}) must be derived to outlive the configured min_delay \
             ({large_min_delay}) plus a safety margin, never a bare hardcoded 86_400"
        );
    }

    // ── GMQ-03: stale-base mergeability guard ───────────────────────────

    fn pr_fixture(merged: bool, mergeable: Option<bool>) -> crate::gitea::types::GiteaPullRequest {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "number": 42,
            "state": "open",
            "title": "test pr",
            "body": null,
            "html_url": "https://gitea.example/owner/repo/pulls/42",
            "user": {"login": "agent", "full_name": null},
            "head": {"label": "agent:feature", "ref": "feature", "sha": "abc123", "repo": null},
            "base": {"label": "owner:main", "ref": "main", "sha": "def456", "repo": null},
            "mergeable": mergeable,
            "merged": merged,
            "created_at": "2026-07-18T00:00:00Z",
            "updated_at": "2026-07-18T00:00:00Z",
        }))
        .expect("valid GiteaPullRequest fixture")
    }

    #[test]
    fn not_mergeable_pr_is_rejected() {
        let pr = pr_fixture(false, Some(false));
        assert_eq!(evaluate_merge_guard(&pr), MergeGuardDecision::NotMergeable);
    }

    #[test]
    fn already_merged_pr_is_idempotent() {
        let pr = pr_fixture(true, Some(true));
        assert_eq!(evaluate_merge_guard(&pr), MergeGuardDecision::AlreadyMerged);
    }

    #[test]
    fn merged_takes_priority_over_mergeable_field() {
        // Even if `mergeable` happens to read false on an already-merged PR
        // (a real Gitea quirk once `merged` flips), `merged` wins.
        let pr = pr_fixture(true, Some(false));
        assert_eq!(evaluate_merge_guard(&pr), MergeGuardDecision::AlreadyMerged);
    }

    #[test]
    fn unknown_mergeability_proceeds() {
        let pr = pr_fixture(false, None);
        assert_eq!(evaluate_merge_guard(&pr), MergeGuardDecision::Proceed);
    }

    #[test]
    fn mergeable_true_proceeds() {
        let pr = pr_fixture(false, Some(true));
        assert_eq!(evaluate_merge_guard(&pr), MergeGuardDecision::Proceed);
    }
}
