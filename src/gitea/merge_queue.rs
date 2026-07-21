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
use serde::Serialize;
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

/// PCON-06: the author-facing reason an in-slot rebase + re-gate did NOT land a
/// merge. Each variant is a DISTINCT, clearly-labeled bounce so the PR author
/// can tell a *rebase conflict* (their branch genuinely conflicts with `main`
/// and must be resolved by hand) from a *red gate* (the branch rebased cleanly
/// but the fresh test-gate on the rebased head failed) from a *gate timeout*
/// (the gate did not finish within the queue's wait budget — a transient
/// "retry", not a code failure). Distinct from [`MergeQueueError`], which
/// covers the queue's OWN outcomes (slot contention, the pre-PCON-06 bounce);
/// this covers the rebase-and-re-gate flow's outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegateBounce {
    /// `main` could not be merged into the PR branch without a conflict — the
    /// forge's branch-update reported a conflict. The author must resolve it
    /// and re-open; the queue cannot land this merge.
    RebaseConflict(String),
    /// The branch rebased cleanly, but the FRESH test-gate on the rebased head
    /// came back RED (a compile error or failing tests introduced by — or
    /// exposed against — the new landing state). NOT merged; fix and retry.
    RedGate(String),
    /// The fresh re-gate did not finish within the queue's `max_wait_secs`
    /// budget. The slot is released cleanly (never held indefinitely); this is
    /// a transient "gate timed out, retry", not a code failure.
    GateTimeout(String),
    /// PCON-06 (FIX 2): after a successful branch-update, the PR head never
    /// became visibly advanced past its pre-update SHA within the budget — an
    /// async/incoherent forge read. The queue MUST NOT gate (and merge) a
    /// possibly-stale, un-rebased head, so it bounces "retry" rather than risk
    /// merging code that was never tested against its landing state.
    RebaseNotVisible(String),
    /// PCON-06 (FIX 1): the PR head changed between the moment the re-gate ran
    /// and the merge (a push to the branch during/after the gate). The gated
    /// commit is no longer the branch head, so merging would land an UNTESTED
    /// commit — the queue bounces "retry" instead (the merge is also bound to
    /// the gated SHA server-side, so this is the belt to that suspenders).
    HeadMoved(String),
    /// PCON-06 (FIX B): the base advanced between the re-gate and the merge —
    /// the gated head is no longer mergeable against the CURRENT base, so
    /// merging it would land a head that was tested against a now-stale base.
    /// Bounce "retry" rather than merge against an un-gated base.
    BaseAdvanced(String),
    /// PCON-06 (FIX B): the merge-queue lease (`GITEA_MERGE_QUEUE_LOCK_TTL_SECS`)
    /// is too short to cover the whole rebase→visibility→gate→merge span, so the
    /// slot could expire mid-op and another merge advance `main` between this
    /// gate and merge. Refused up front (a config guard) rather than risk
    /// merging a gated head onto an un-gated base — raise the lock TTL.
    LeaseTooShort(String),
    /// PCON-06 (final): the op ran past its lease deadline (`lock_ttl - margin`)
    /// by the time the merge was about to POST — the slot may no longer be held
    /// exclusively, so another merge could have advanced `main`. Refused at the
    /// boundary (a hard pre-merge deadline check) rather than POST a merge whose
    /// slot exclusivity can no longer be guaranteed; retry re-runs it fresh
    /// under a new lease.
    LeaseExpired(String),
}

impl std::fmt::Display for RegateBounce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegateBounce::RebaseConflict(d) => write!(
                f,
                "merge queue: rebase conflict — current base could not be merged into the PR \
                 branch cleanly ({d}); resolve the conflict on the branch and re-open"
            ),
            RegateBounce::RedGate(d) => write!(
                f,
                "merge queue: re-gate FAILED (red) on the rebased head ({d}) — not merged; fix \
                 the failure and retry"
            ),
            RegateBounce::GateTimeout(d) => write!(
                f,
                "merge queue: re-gate timed out ({d}) — the merge slot was released cleanly, retry"
            ),
            RegateBounce::RebaseNotVisible(d) => write!(
                f,
                "merge queue: no confirmed rebased head (advanced past the pre-update head AND \
                 mergeable against current base) became visible ({d}) — not gated or merged; retry"
            ),
            RegateBounce::HeadMoved(d) => write!(
                f,
                "merge queue: the PR head moved after the re-gate ({d}) — the gated commit is no \
                 longer the branch head, so nothing was merged; retry"
            ),
            RegateBounce::BaseAdvanced(d) => write!(
                f,
                "merge queue: the base advanced after the re-gate ({d}) — the gated head is no \
                 longer mergeable against current base, so nothing was merged; retry"
            ),
            RegateBounce::LeaseTooShort(d) => write!(
                f,
                "merge queue: lock lease too short to cover an in-slot re-gate ({d}) — refusing to \
                 rebase+gate without a lease that spans the whole op; raise \
                 GITEA_MERGE_QUEUE_LOCK_TTL_SECS"
            ),
            RegateBounce::LeaseExpired(d) => write!(
                f,
                "merge queue: re-gate ran past its lease deadline ({d}) — refusing to merge with a \
                 lease whose exclusivity can no longer be guaranteed; retry re-runs it fresh"
            ),
        }
    }
}

impl std::error::Error for RegateBounce {}

/// PCON-06: each [`RegateBounce`] maps to a DISTINCT [`crate::error::ToolError`]
/// the author can act on. A rebase conflict is a genuine `Conflict` (resolve
/// and re-open); a red gate and a gate timeout are both `Execution` ("the merge
/// didn't complete") but stay distinguishable by their message prefixes — a red
/// gate is a code failure to fix, a timeout is a transient retry.
impl From<RegateBounce> for crate::error::ToolError {
    fn from(b: RegateBounce) -> Self {
        match &b {
            RegateBounce::RebaseConflict(_) => crate::error::ToolError::Conflict(b.to_string()),
            RegateBounce::RedGate(_)
            | RegateBounce::GateTimeout(_)
            | RegateBounce::RebaseNotVisible(_)
            | RegateBounce::HeadMoved(_)
            | RegateBounce::BaseAdvanced(_)
            | RegateBounce::LeaseTooShort(_)
            | RegateBounce::LeaseExpired(_) => crate::error::ToolError::Execution(b.to_string()),
        }
    }
}

/// PCON-06: the verdict of a fresh test-gate fired on a rebased head SHA inside
/// the merge queue's critical section — the SAME `compiler_build` (`mode=test`)
/// gate the pipeline's Stage 4 runs, on the resolved SHA of the rebased result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateVerdict {
    /// The gate ran and PASSED — safe to merge the rebased head.
    Green,
    /// The gate ran and FAILED (red): a compile error or failing tests. Do NOT
    /// merge; bounce with this reason (a [`RegateBounce::RedGate`]).
    Red(String),
    /// The gate did not finish within the queue's wait budget — release the
    /// slot cleanly and bounce (`RegateBounce::GateTimeout`), never hold the
    /// slot indefinitely.
    TimedOut,
    /// The compiler door was unreachable or misconfigured at re-gate time (a
    /// spawn/config failure, NOT a red verdict) — fall back to today's
    /// `NotMergeable` bounce (fail-safe, clearly labeled). Carries the reason
    /// for logging.
    Unreachable(String),
}

/// PCON-06: fires a fresh test-gate on the resolved SHA of a rebased PR head,
/// INSIDE the merge queue's critical section. Abstracted as a trait so the
/// queue orchestration ([`crate::gitea::MergePr`]) is unit-tested with a fake
/// gate (no cargo spawn, deterministic verdict), while production wires it to
/// the single-door `compiler_build` (`mode=test`) path — the SAME test-gate
/// Stage 4 runs, satisfying S9 (no second, hand-rolled build path).
///
/// The implementation MUST honor `budget` (the queue's `max_wait_secs`): a gate
/// that would run past it returns [`GateVerdict::TimedOut`] rather than blocking
/// the slot forever.
#[async_trait]
pub(crate) trait ReGate: Send + Sync {
    async fn gate(&self, module: &str, sha: &str, budget: Duration) -> GateVerdict;
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

    /// GMQ-05 read-only status: the current lock holder's fence token plus its
    /// remaining TTL in milliseconds (`PTTL queue:merge:lock:{key}`), or `None`
    /// if the key is not currently locked (including "expired" — an expired
    /// lock reads as free, same as [`Self::try_acquire`]). Pure read — NEVER
    /// mutates the lock, the wait ordering, or anything else.
    async fn lock_status(&self, key: &str) -> Result<Option<(String, i64)>, ()>;

    /// GMQ-05 read-only status: the current wait-queue depth (`ZCARD
    /// queue:merge:wait:{key}`) and the ticket currently at the front (`ZRANGE
    /// queue:merge:wait:{key} 0 0`), or `(0, None)` if nothing is waiting.
    /// Pure read — never prunes expired tickets or otherwise mutates the wait
    /// ordering (unlike [`Self::try_acquire`], which prunes as a side effect
    /// of acquiring).
    async fn wait_status(&self, key: &str) -> Result<(u64, Option<u64>), ()>;
}

/// GMQ-05: a point-in-time, read-only snapshot of a merge-queue key's state —
/// never mutates the lock, the wait ordering, or the last-merge marker. See
/// [`MergeQueue::status`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MergeQueueSnapshot {
    /// The queue key this snapshot describes (`{owner}/{repo}/{base}`
    /// convention, or a caller's `queue_key` override).
    pub key: String,
    /// Whether the critical section is currently held (an expired lock reads
    /// as free/`false`, same as [`MergeLockStore::try_acquire`]).
    pub locked: bool,
    /// The current holder's fence token, if locked. This is an opaque
    /// per-acquire identifier (a random UUID), not a caller/agent identity —
    /// the store never records who requested the lock, only the fence used to
    /// guard its release.
    pub lock_fence: Option<String>,
    /// Remaining lease time in milliseconds, if locked (`PTTL` on the lock
    /// key).
    pub lock_ttl_ms: Option<i64>,
    /// Number of waiters currently queued for this key (`ZCARD` on the wait
    /// ZSET).
    pub wait_depth: u64,
    /// The ticket number currently at the front of the wait ordering (next to
    /// be served), if any are waiting.
    pub next_ticket: Option<u64>,
    /// Epoch-ms timestamp of the last successful merge to this key, or `None`
    /// if this key has never recorded one.
    pub last_merge_ms: Option<i64>,
    /// The epoch-ms timestamp at (or after) which the GMQ-03 spacing rule
    /// allows the next merge to this key to proceed without waiting —
    /// `last_merge_ms + min_delay_secs * 1000`. `None` when there is no
    /// recorded last merge (nothing to space against — a merge could proceed
    /// immediately).
    pub next_allowed_merge_ms: Option<i64>,
    /// The `min_delay_secs` this snapshot's `next_allowed_merge_ms` was
    /// derived from (the caller's override, or
    /// `GITEA_MERGE_QUEUE_MIN_DELAY_SECS`'s default).
    pub min_delay_secs: u64,
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

    async fn lock_status(&self, key: &str) -> Result<Option<(String, i64)>, ()> {
        // Plain GET/PTTL — read-only, no Lua script, no mutation. A tiny
        // (GET-then-PTTL) window exists where the lock could be released or
        // expire between the two calls; that's acceptable for a best-effort
        // observability snapshot (not a decision the merge path depends on).
        let lockk = lock_key(key);
        let fence: Option<String> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("GET").arg(&lockk).query_async::<_, Option<String>>(&mut conn).await
            })
            .await?;
        let Some(fence) = fence else {
            return Ok(None);
        };
        let lockk = lock_key(key);
        let ttl_ms: i64 = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("PTTL").arg(&lockk).query_async::<_, i64>(&mut conn).await
            })
            .await?;
        // PTTL returns -2 (key gone, e.g. expired between the GET and here)
        // or -1 (no TTL set, shouldn't happen for this key) — both read as
        // "not meaningfully locked" rather than a negative TTL.
        if ttl_ms < 0 {
            return Ok(None);
        }
        Ok(Some((fence, ttl_ms)))
    }

    async fn wait_status(&self, key: &str) -> Result<(u64, Option<u64>), ()> {
        let waitk = wait_key(key);
        let waitk_front = waitk.clone();
        let depth: i64 = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("ZCARD").arg(&waitk).query_async::<_, i64>(&mut conn).await
            })
            .await?;
        let front: Vec<String> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                ::redis::cmd("ZRANGE")
                    .arg(&waitk_front)
                    .arg(0)
                    .arg(0)
                    .query_async::<_, Vec<String>>(&mut conn)
                    .await
            })
            .await?;
        let next_ticket = front.into_iter().next().and_then(|s| s.parse::<u64>().ok());
        Ok((depth.max(0) as u64, next_ticket))
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

    /// GMQ-05: read-only inspection of `key`'s current merge-queue state — the
    /// lock holder + its remaining TTL, the wait-queue depth + next ticket,
    /// and the last-merge time + the GMQ-03 spacing rule's next-allowed-merge
    /// time. NEVER mutates the lock, the wait ordering, or the last-merge
    /// marker — this is purely observational (unlike [`Self::with_merge_slot`]
    /// / [`Self::enforce_spacing`] / [`Self::record_merge`], which drive the
    /// actual merge path).
    ///
    /// An unknown/never-used `key` returns an empty-but-`Ok` snapshot (no
    /// lock, zero wait depth, no last-merge record) — the same "nothing here
    /// yet" posture every other read in this module takes, never an error.
    /// A mid-op store error (Redis unreachable) degrades the SAME way: logs
    /// once and folds into the empty snapshot for whichever field couldn't be
    /// read, rather than failing the whole call — this is a status view, not
    /// a decision the merge path depends on.
    ///
    /// `min_delay_secs` is the spacing window this snapshot's
    /// `next_allowed_merge_ms` is computed against — pass the same value a
    /// caller would pass to [`Self::enforce_spacing`] (a per-call override, or
    /// `GITEA_MERGE_QUEUE_MIN_DELAY_SECS`'s default).
    pub async fn status(
        &self,
        key: &str,
        min_delay_secs: u64,
    ) -> Result<MergeQueueSnapshot, MergeQueueError> {
        let lock = match self.store.lock_status(key).await {
            Ok(v) => v,
            Err(()) => {
                log_degrade_once("status: lock lookup failed (Redis unreachable)");
                None
            }
        };
        let (wait_depth, next_ticket) = match self.store.wait_status(key).await {
            Ok(v) => v,
            Err(()) => {
                log_degrade_once("status: wait lookup failed (Redis unreachable)");
                (0, None)
            }
        };
        let last_merge_ms = match self.store.last_merge_ms(key).await {
            Ok(v) => v,
            Err(()) => {
                log_degrade_once("status: last-merge lookup failed (Redis unreachable)");
                None
            }
        };
        let next_allowed_merge_ms =
            last_merge_ms.map(|last| last.saturating_add((min_delay_secs as i64).saturating_mul(1000)));

        Ok(MergeQueueSnapshot {
            key: key.to_string(),
            locked: lock.is_some(),
            lock_fence: lock.as_ref().map(|(fence, _)| fence.clone()),
            lock_ttl_ms: lock.map(|(_, ttl)| ttl),
            wait_depth,
            next_ticket,
            last_merge_ms,
            next_allowed_merge_ms,
            min_delay_secs,
        })
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

// ── PCON-07: speculative merge batching (GitHub-merge-queue model) ──────────
//
// A THROUGHPUT layer on top of PCON-06's serialized rebase-and-re-gate. Instead
// of gating one PR at a time, stack the front N same-base PRs of a base's queue
// into ONE speculative rebased batch, gate the batch ONCE, and merge all N if
// green; on a RED batch, BISECT (binary-split, re-gate halves, eject the
// offender, merge the green remainder, requeue the offender with its reason).
//
// See `docs/specs/S122-pcon07-speculative-batching.md` for the full design note
// (batch formation, single-gate, bisect-on-red algorithm, N-cap, and failure
// semantics).
//
// ## Where this composes with PCON-06 (and why it preserves every guarantee)
// The batch still runs INSIDE one `with_merge_slot` acquisition for the base key
// (the slot still serializes; batching only changes what ONE slot processes).
// The algorithm here is a PURE orchestration over an abstract
// [`SpeculativeBatchOps`] — exactly mirroring how PCON-06 abstracts its
// [`ReGate`] + [`MergeLockStore`] so the delicate correctness is unit-tested
// deterministically with fakes (no live Redis / forge). Production wires
// [`SpeculativeBatchOps`] to the SAME sanctioned PCON-06 helpers
// (`update_pull_branch` for the stack rebase, `ReGate`/`compiler_build` for the
// gate, `merge_pull_with_base` bound to each gated SHA for the land) — no raw
// API calls, single door, S9.
//
// ## Correctness invariant (what lands is what was gated)
// The bisection is constructed so the FINAL surviving set it returns was gated
// GREEN as one unit at some step of the search — never an ad-hoc union of PRs
// that were only ever gated apart. Each individual land then goes through the
// PCON-06 SHA-bound merge (`head_commit_id` + base recheck), so a PR that
// drifted between the batch gate and its land is requeued rather than merged
// untested. Every ejected PR bounces with a clear, distinct reason and is
// requeued. `BUILD_MERGE_BATCH_MAX=1` (the default) never enters this layer at
// all — the merge takes the exact PCON-06 single-PR path, byte-for-byte.

/// PCON-07: the reason a PR was EJECTED from a speculative batch (and requeued
/// so it is retried on its own next round). Each variant is a DISTINCT,
/// author-facing bounce — a *rebase conflict* (the PR genuinely conflicts with
/// the current base during the speculative stack, ejected BEFORE the gate) is
/// clearly separable from a *red-gate offender* (bisection isolated this PR as
/// the one that turned the batch red).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchEjectReason {
    /// The PR conflicted while stacking the speculative batch — it could not be
    /// rebased onto the current base cleanly, so it was ejected before any gate
    /// ran and the batch reformed without it.
    RebaseConflict(String),
    /// Bisection isolated this PR as the offender: gating it on top of the
    /// confirmed-green prefix came back RED. The green remainder merged; this
    /// PR is requeued with the gate's failure reason.
    RedGate(String),
    /// A fail-safe eject: the gate was unavailable (timed out / door
    /// unreachable) for the sub-batch this PR was in during bisection, so it
    /// could not be proven green and is requeued rather than merged blind.
    GateUnavailable(String),
}

impl std::fmt::Display for BatchEjectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchEjectReason::RebaseConflict(d) => write!(
                f,
                "batch: rebase conflict while stacking the speculative batch ({d}) — ejected \
                 pre-gate and requeued; resolve the conflict on the branch and re-open"
            ),
            BatchEjectReason::RedGate(d) => write!(
                f,
                "batch: isolated by bisection as the red-gate offender ({d}) — the green \
                 remainder merged; this PR is requeued, fix the failure and retry"
            ),
            BatchEjectReason::GateUnavailable(d) => write!(
                f,
                "batch: gate unavailable for this PR's sub-batch during bisection ({d}) — \
                 requeued (never merged unproven); retry"
            ),
        }
    }
}

/// PCON-07: the abstract forge/gate operations the speculative-batch algorithm
/// drives. Abstracted as a trait so [`run_speculative_batch`] — the delicate
/// formation/single-gate/bisect logic — is unit-tested with a deterministic
/// fake (no cargo spawn, no live forge), exactly as PCON-06's [`ReGate`] is.
/// Production implements it over the sanctioned PCON-06 helpers (single door,
/// S9): `update_pull_branch` (stack rebase), `ReGate` (gate), and
/// `merge_pull_with_base` bound to each gated SHA (land).
#[async_trait]
pub(crate) trait SpeculativeBatchOps: Send + Sync {
    /// Speculatively rebase/stack `prs` (in queue order) onto the current base.
    /// A PR that CONFLICTS during the rebase is reported in
    /// [`BatchStack::conflicted`] (ejected before the gate); the rest are
    /// [`BatchStack::stacked`], ready to gate as one unit.
    async fn stack(&self, prs: &[u64]) -> BatchStack;

    /// Gate the already-stacked `prs` as ONE unit (the speculative batch gate)
    /// — the SAME PCON-06 test-gate, on the stacked result. Honors `budget`
    /// (returns [`GateVerdict::TimedOut`] rather than blocking the slot forever).
    async fn gate(&self, prs: &[u64], budget: Duration) -> GateVerdict;

    /// Land ONE PR of a green batch, bound to its gated state (the PCON-06
    /// per-PR `head_commit_id` + base recheck invariant). `Ok(())` on a landed
    /// merge; `Err(reason)` if the PR drifted (head/base moved) and must be
    /// requeued instead of merged untested.
    async fn merge(&self, pr: u64) -> Result<(), String>;
}

/// PCON-07: the result of [`SpeculativeBatchOps::stack`] — which PRs stacked
/// cleanly (ready to gate) and which conflicted (ejected before the gate).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BatchStack {
    /// PRs that rebased cleanly onto the current base, in queue order — the set
    /// the batch gate runs on.
    pub stacked: Vec<u64>,
    /// PRs that conflicted during the stack rebase, each with the conflict
    /// detail — ejected pre-gate and requeued (the batch reforms without them).
    pub conflicted: Vec<(u64, String)>,
}

/// PCON-07: the outcome of a speculative batch run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BatchOutcome {
    /// PRs that LANDED, in merge order. Every one was part of the exact set that
    /// was gated GREEN as a unit (see the correctness invariant above).
    pub merged: Vec<u64>,
    /// PRs ejected + the distinct reason each was requeued with (rebase
    /// conflict, red-gate offender, or a fail-safe gate-unavailable eject).
    pub ejected: Vec<(u64, BatchEjectReason)>,
    /// `Some(front)` when the batch could not be gated as a unit (top-level gate
    /// timed out or the door was unreachable) and the caller must instead run
    /// the PCON-06 single-PR path for the front PR (spec: "batch gate times out
    /// → fall back to N=1 for the front PR"). `None` when the batch ran.
    pub fell_back_to_single: Option<u64>,
    /// Number of gate invocations — one for an all-green batch; more when
    /// bisection re-gates sub-batches. For observability + test assertions.
    pub gate_calls: usize,
    /// PRs from a GREEN batch whose SHA-bound land failed (drifted head/base):
    /// requeued (not a red offender, not merged untested). The first failure
    /// stops the land phase — every survivor after it is requeued too (it was
    /// stacked on the one that failed).
    pub merge_failures: Vec<(u64, String)>,
}

/// PCON-07: run a speculative merge batch over `prs` (already capped to
/// `BUILD_MERGE_BATCH_MAX` and with the front PR first). Pure orchestration over
/// `ops`; see the module-level design note and
/// `docs/specs/S122-pcon07-speculative-batching.md`.
///
/// Flow:
/// 1. **Stack** the batch; PRs that conflict are ejected pre-gate
///    ([`BatchEjectReason::RebaseConflict`]) and the batch reforms without them.
/// 2. If nothing stacked cleanly, return (everything was ejected).
/// 3. **Gate once** on the stacked set:
///    - **Green** → land all in order (SHA-bound); a land that drifts is
///      requeued into `merge_failures` and stops the phase.
///    - **TimedOut / Unreachable** → `fell_back_to_single = Some(front)`: the
///      caller runs the PCON-06 single-PR path for the front PR (fail-safe).
///    - **Red** → **bisect**: binary-split, re-gate halves, isolate + eject the
///      offender(s), and land the green remainder — which was gated green as one
///      unit at the isolating step (correctness invariant).
pub(crate) async fn run_speculative_batch(
    ops: &dyn SpeculativeBatchOps,
    prs: &[u64],
    budget: Duration,
) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    let Some(&front) = prs.first() else {
        // Empty input — nothing to do (the caller guarantees a non-empty batch
        // in practice; this is a defensive no-op, never a fall-back).
        return outcome;
    };

    // (1) Stack the batch; eject pre-gate conflicts, reform without them.
    let BatchStack { stacked, conflicted } = ops.stack(prs).await;
    for (pr, reason) in conflicted {
        outcome.ejected.push((pr, BatchEjectReason::RebaseConflict(reason)));
    }
    // (2) Nothing stacked cleanly ⇒ the whole batch conflicted; done.
    if stacked.is_empty() {
        return outcome;
    }

    // (3) Gate the stacked set ONCE.
    outcome.gate_calls += 1;
    match ops.gate(&stacked, budget).await {
        GateVerdict::Green => {
            land_in_order(ops, &stacked, &mut outcome).await;
        }
        GateVerdict::TimedOut | GateVerdict::Unreachable(_) => {
            // Fail-safe: the batch could not be gated as a unit. Fall back to
            // N=1 for the FRONT PR — the caller runs the PCON-06 single path,
            // which re-gates that one PR against its exact landing state. The
            // rest of the batch is simply left in the queue for the next round.
            outcome.fell_back_to_single = Some(front);
        }
        GateVerdict::Red(top_reason) => {
            // (4) Bisect to isolate + eject the offender(s); the survivors it
            // returns were gated GREEN as one unit at the isolating step.
            let (survivors, red_ejected, sub_gate_calls) = bisect_red(
                ops,
                Vec::new(),
                stacked,
                budget,
                Some(GateVerdict::Red(top_reason)),
            )
            .await;
            outcome.gate_calls += sub_gate_calls;
            for (pr, reason) in red_ejected {
                outcome.ejected.push((pr, reason));
            }
            land_in_order(ops, &survivors, &mut outcome).await;
        }
    }
    outcome
}

/// PCON-07: land the confirmed-green `survivors` in order (SHA-bound). The first
/// land that drifts (`Err`) is requeued into `merge_failures` AND every survivor
/// after it too — each later PR was stacked on the one that failed, so it can no
/// longer land against the state it was gated in.
async fn land_in_order(
    ops: &dyn SpeculativeBatchOps,
    survivors: &[u64],
    outcome: &mut BatchOutcome,
) {
    let mut failed_from: Option<usize> = None;
    for (i, &pr) in survivors.iter().enumerate() {
        match ops.merge(pr).await {
            Ok(()) => outcome.merged.push(pr),
            Err(reason) => {
                outcome.merge_failures.push((pr, reason));
                failed_from = Some(i + 1);
                break;
            }
        }
    }
    if let Some(from) = failed_from {
        for &pr in &survivors[from..] {
            outcome.merge_failures.push((
                pr,
                "a prior member of this green batch failed to land, so this PR's gated \
                 landing state no longer holds — requeued"
                    .to_string(),
            ));
        }
    }
}

/// PCON-07: the bisection core. Contract: gate `prefix + batch` (skipping the
/// gate when `known` supplies the verdict for exactly that set), then:
/// - **Green** → the whole `prefix + batch` survives (it was just gated green).
/// - **TimedOut / Unreachable** → fail-safe: eject ALL of `batch`
///   ([`BatchEjectReason::GateUnavailable`]); `prefix` (already green) survives.
/// - **Red**, `batch.len() == 1` → the single PR IS the offender; eject it
///   ([`BatchEjectReason::RedGate`]); `prefix` survives.
/// - **Red**, `batch.len() > 1` → split, recurse left (on `prefix`), then right
///   (on the left's survivors) so each half is gated stacked on the confirmed
///   prefix. The returned survivor set was gated green as one unit at the
///   deepest establishing step.
///
/// Returns `(survivors = prefix + kept, ejected, gate_calls)`.
fn bisect_red<'a>(
    ops: &'a dyn SpeculativeBatchOps,
    prefix: Vec<u64>,
    batch: Vec<u64>,
    budget: Duration,
    known: Option<GateVerdict>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = (Vec<u64>, Vec<(u64, BatchEjectReason)>, usize)> + Send + 'a>,
> {
    Box::pin(async move {
        // Gate `prefix + batch` unless the verdict for exactly that set is
        // already known (the top-level red, threaded in to avoid re-gating it).
        let (verdict, mut gate_calls) = match known {
            Some(v) => (v, 0usize),
            None => {
                let set: Vec<u64> = prefix.iter().copied().chain(batch.iter().copied()).collect();
                (ops.gate(&set, budget).await, 1usize)
            }
        };

        match verdict {
            GateVerdict::Green => {
                let survivors: Vec<u64> =
                    prefix.iter().copied().chain(batch.iter().copied()).collect();
                (survivors, Vec::new(), gate_calls)
            }
            GateVerdict::TimedOut | GateVerdict::Unreachable(_) => {
                // Fail-safe: cannot prove this sub-batch green — requeue all of
                // it rather than merge unproven. `prefix` is already green.
                let ejected = batch
                    .into_iter()
                    .map(|pr| {
                        (
                            pr,
                            BatchEjectReason::GateUnavailable(
                                "gate unavailable during bisection".to_string(),
                            ),
                        )
                    })
                    .collect();
                (prefix, ejected, gate_calls)
            }
            GateVerdict::Red(msg) => {
                if batch.len() == 1 {
                    // Isolated: this lone PR turned the (already-green) prefix
                    // red — it IS the offender. Eject it; the prefix survives.
                    let offender = batch[0];
                    (
                        prefix,
                        vec![(offender, BatchEjectReason::RedGate(msg))],
                        gate_calls,
                    )
                } else {
                    let k = batch.len() / 2;
                    let left: Vec<u64> = batch[..k].to_vec();
                    let right: Vec<u64> = batch[k..].to_vec();

                    // Left half stacked on the confirmed prefix.
                    let (merged_left, mut ejected, gl) =
                        bisect_red(ops, prefix, left, budget, None).await;
                    gate_calls += gl;

                    // Right half stacked on the left's survivors, so the final
                    // surviving set is gated as a coherent stack.
                    let (merged_all, ejected_right, gr) =
                        bisect_red(ops, merged_left, right, budget, None).await;
                    gate_calls += gr;
                    ejected.extend(ejected_right);

                    (merged_all, ejected, gate_calls)
                }
            }
        }
    })
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

        async fn lock_status(&self, key: &str) -> Result<Option<(String, i64)>, ()> {
            let s = self.state.lock().unwrap();
            if s.down {
                return Err(());
            }
            match s.lock.get(key) {
                Some((fence, exp)) => {
                    let now = StdInstant::now();
                    if *exp > now {
                        Ok(Some((fence.clone(), exp.duration_since(now).as_millis() as i64)))
                    } else {
                        // Expired lock reads as free — same posture as
                        // `try_acquire`'s own held-check.
                        Ok(None)
                    }
                }
                None => Ok(None),
            }
        }

        async fn wait_status(&self, key: &str) -> Result<(u64, Option<u64>), ()> {
            let s = self.state.lock().unwrap();
            if s.down {
                return Err(());
            }
            match s.wait.get(key) {
                Some(bucket) => Ok((bucket.len() as u64, bucket.first().map(|(t, _, _)| *t))),
                None => Ok((0, None)),
            }
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
        async fn lock_status(&self, key: &str) -> Result<Option<(String, i64)>, ()> {
            self.inner.lock_status(key).await
        }
        async fn wait_status(&self, key: &str) -> Result<(u64, Option<u64>), ()> {
            self.inner.wait_status(key).await
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

    // ── PCON-06: distinct re-gate bounces map to distinct ToolErrors ─────

    #[test]
    fn regate_bounces_map_to_distinct_author_facing_tool_errors() {
        use crate::error::ToolError;
        // A rebase conflict is a genuine Conflict (resolve + re-open).
        let conflict: ToolError = RegateBounce::RebaseConflict("textual".into()).into();
        assert!(matches!(conflict, ToolError::Conflict(_)));
        assert!(format!("{conflict}").to_lowercase().contains("rebase conflict"));

        // A red gate and a timeout are both Execution ("didn't complete") but
        // stay DISTINGUISHABLE by their message prefixes.
        let red: ToolError = RegateBounce::RedGate("abc123".into()).into();
        assert!(matches!(red, ToolError::Execution(_)));
        let red_msg = format!("{red}").to_lowercase();
        assert!(red_msg.contains("re-gate failed"));
        assert!(!red_msg.contains("timed out"), "a red gate must not read as a timeout");

        let timeout: ToolError = RegateBounce::GateTimeout("budget 300s".into()).into();
        assert!(matches!(timeout, ToolError::Execution(_)));
        let to_msg = format!("{timeout}").to_lowercase();
        assert!(to_msg.contains("timed out"));
        assert!(!to_msg.contains("re-gate failed"), "a timeout must not read as a red gate");
    }

    // ── GMQ-05: read-only status ────────────────────────────────────────

    #[tokio::test]
    async fn status_reflects_a_held_lock_with_holder_and_positive_ttl() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(Arc::clone(&store));
        let cfg = fast_cfg();

        let started = Arc::new(tokio::sync::Notify::new());
        let s = Arc::clone(&started);
        let holder = tokio::spawn(async move {
            q.with_merge_slot("owner/repo/main", 0, &cfg, || async move {
                s.notify_one();
                tokio::time::sleep(Duration::from_millis(150)).await;
            })
            .await
        });
        started.notified().await;

        let q_status = queue_over(Arc::clone(&store));
        let snap = q_status.status("owner/repo/main", 0).await.unwrap();
        assert!(snap.locked, "a held lock must report locked == true");
        assert!(snap.lock_fence.is_some(), "a held lock must report a holder fence");
        let ttl = snap.lock_ttl_ms.expect("a held lock must report a TTL");
        assert!(ttl > 0 && ttl <= 60_000, "TTL must be positive and within the configured lease, got {ttl}");

        holder.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn status_reports_no_lock_for_a_free_key() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(store);

        let snap = q.status("owner/repo/never-locked", 0).await.unwrap();
        assert!(!snap.locked);
        assert!(snap.lock_fence.is_none());
        assert!(snap.lock_ttl_ms.is_none());
    }

    #[tokio::test]
    async fn status_reports_wait_depth_and_next_ticket() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let cfg = fast_cfg();

        // Ticket 1 acquires and holds forever (stands in for a slow in-flight
        // merge); tickets 2 and 3 pile up behind it in the wait ordering.
        let t1 = store.enqueue("owner/repo/main", 0, Duration::from_secs(60)).await.unwrap();
        let acquired = store
            .try_acquire("owner/repo/main", t1, "fence-1", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(matches!(acquired, AcquireAttempt::Acquired(_)));

        let t2 = store.enqueue("owner/repo/main", 0, Duration::from_secs(60)).await.unwrap();
        let t3 = store.enqueue("owner/repo/main", 0, Duration::from_secs(60)).await.unwrap();

        let q = queue_over(Arc::clone(&store));
        let snap = q.status("owner/repo/main", 0).await.unwrap();
        assert_eq!(snap.wait_depth, 2, "two waiters (t2, t3) must be reflected in wait_depth");
        assert_eq!(snap.next_ticket, Some(t2), "the front of the wait ordering must be t2");
        let _ = (cfg, t3);
    }

    #[tokio::test]
    async fn status_reports_next_allowed_merge_after_a_recorded_merge() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(Arc::clone(&store));

        let before = now_ms();
        q.record_merge("owner/repo/main", 5).await;

        let snap = q.status("owner/repo/main", 5).await.unwrap();
        let last = snap.last_merge_ms.expect("a recorded merge must be reflected");
        assert!(last >= before, "last_merge_ms must be at/after the recorded time");

        let next_allowed = snap.next_allowed_merge_ms.expect("next_allowed_merge_ms must be set");
        assert_eq!(
            next_allowed,
            last + 5_000,
            "next_allowed_merge_ms must be last_merge_ms + min_delay_secs*1000"
        );
    }

    #[tokio::test]
    async fn status_for_an_unknown_key_is_an_empty_ok_snapshot_not_an_error() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        let q = queue_over(store);

        let snap = q.status("owner/repo/totally-unused-base", 30).await.unwrap();
        assert!(!snap.locked);
        assert!(snap.lock_fence.is_none());
        assert!(snap.lock_ttl_ms.is_none());
        assert_eq!(snap.wait_depth, 0);
        assert!(snap.next_ticket.is_none());
        assert!(snap.last_merge_ms.is_none());
        assert!(snap.next_allowed_merge_ms.is_none());
        assert_eq!(snap.min_delay_secs, 30);
    }

    #[tokio::test]
    async fn status_degrades_to_an_empty_snapshot_when_the_store_is_unreachable() {
        let store = Arc::new(InMemoryMergeLockStore::new());
        store.set_down(true);
        let q = queue_over(store);

        let snap = q.status("owner/repo/main", 10).await.unwrap();
        assert!(!snap.locked);
        assert_eq!(snap.wait_depth, 0);
        assert!(snap.last_merge_ms.is_none());
        assert!(snap.next_allowed_merge_ms.is_none());
    }

    // ── PCON-07: speculative merge batching (formation / single-gate / bisect)

    /// A deterministic [`SpeculativeBatchOps`] fake: no cargo spawn, no forge —
    /// a gate goes RED whenever the stacked set contains ANY PR in `bad`; a PR
    /// in `conflict` is ejected during the stack; `force_gate` overrides EVERY
    /// gate verdict (e.g. to force a top-level `TimedOut`/`Unreachable`); a PR
    /// in `merge_fail` fails its bound land. Records every gated set and every
    /// landed PR so tests can assert the single-gate / bisect / land invariants.
    struct FakeBatchOps {
        bad: std::collections::HashSet<u64>,
        conflict: std::collections::HashSet<u64>,
        merge_fail: std::collections::HashSet<u64>,
        force_gate: Option<GateVerdict>,
        gate_sets: StdMutex<Vec<Vec<u64>>>,
        landed: StdMutex<Vec<u64>>,
    }

    impl FakeBatchOps {
        fn new() -> Self {
            Self {
                bad: Default::default(),
                conflict: Default::default(),
                merge_fail: Default::default(),
                force_gate: None,
                gate_sets: StdMutex::new(Vec::new()),
                landed: StdMutex::new(Vec::new()),
            }
        }
        fn bad(mut self, prs: &[u64]) -> Self {
            self.bad = prs.iter().copied().collect();
            self
        }
        fn conflict(mut self, prs: &[u64]) -> Self {
            self.conflict = prs.iter().copied().collect();
            self
        }
        fn merge_fail(mut self, prs: &[u64]) -> Self {
            self.merge_fail = prs.iter().copied().collect();
            self
        }
        fn force_gate(mut self, v: GateVerdict) -> Self {
            self.force_gate = Some(v);
            self
        }
        fn gated_sets(&self) -> Vec<Vec<u64>> {
            self.gate_sets.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SpeculativeBatchOps for FakeBatchOps {
        async fn stack(&self, prs: &[u64]) -> BatchStack {
            let mut stacked = Vec::new();
            let mut conflicted = Vec::new();
            for &pr in prs {
                if self.conflict.contains(&pr) {
                    conflicted.push((pr, format!("conflict on #{pr}")));
                } else {
                    stacked.push(pr);
                }
            }
            BatchStack { stacked, conflicted }
        }
        async fn gate(&self, prs: &[u64], _budget: Duration) -> GateVerdict {
            self.gate_sets.lock().unwrap().push(prs.to_vec());
            if let Some(v) = &self.force_gate {
                return v.clone();
            }
            let offenders: Vec<u64> = prs.iter().copied().filter(|p| self.bad.contains(p)).collect();
            if offenders.is_empty() {
                GateVerdict::Green
            } else {
                GateVerdict::Red(format!("red on {offenders:?}"))
            }
        }
        async fn merge(&self, pr: u64) -> Result<(), String> {
            if self.merge_fail.contains(&pr) {
                return Err(format!("drift on #{pr}"));
            }
            self.landed.lock().unwrap().push(pr);
            Ok(())
        }
    }

    fn budget() -> Duration {
        Duration::from_secs(30)
    }

    #[tokio::test]
    async fn batch_all_green_gates_once_and_merges_all_in_order() {
        // The happy path: N same-base PRs, all green → exactly ONE gate on the
        // whole stacked set, and all N land in order.
        let ops = FakeBatchOps::new();
        let out = run_speculative_batch(&ops, &[1, 2, 3], budget()).await;

        assert_eq!(out.gate_calls, 1, "an all-green batch must gate exactly ONCE");
        assert_eq!(out.merged, vec![1, 2, 3], "all N must land in order");
        assert!(out.ejected.is_empty(), "no ejections on an all-green batch");
        assert!(out.fell_back_to_single.is_none());
        assert_eq!(ops.gated_sets(), vec![vec![1, 2, 3]], "the single gate ran on the full stack");
    }

    #[tokio::test]
    async fn batch_one_red_bisects_ejects_exactly_the_offender_and_merges_remainder() {
        // One offender (#2) in a batch of three → bisection isolates and ejects
        // EXACTLY #2 with a red-gate reason; the green remainder [1,3] merges;
        // and [1,3] was gated GREEN as one unit (the correctness invariant).
        let ops = FakeBatchOps::new().bad(&[2]);
        let out = run_speculative_batch(&ops, &[1, 2, 3], budget()).await;

        assert_eq!(out.merged, vec![1, 3], "the green remainder must merge, in order");
        assert_eq!(out.ejected.len(), 1, "exactly one PR ejected");
        let (ejected_pr, reason) = &out.ejected[0];
        assert_eq!(*ejected_pr, 2, "the ejected PR must be EXACTLY the offender");
        assert!(
            matches!(reason, BatchEjectReason::RedGate(_)),
            "the offender must be ejected with a red-gate reason, got {reason:?}"
        );
        assert!(out.gate_calls > 1, "a red batch must bisect (more than one gate)");
        assert!(out.fell_back_to_single.is_none());
        // The exact surviving set [1,3] was gated GREEN as one unit.
        assert!(
            ops.gated_sets().iter().any(|s| s == &vec![1, 3]),
            "the survivor set [1,3] must have been gated as a unit: {:?}",
            ops.gated_sets()
        );
    }

    #[tokio::test]
    async fn batch_multiple_offenders_are_all_ejected_and_the_clean_remainder_merges() {
        // Robustness beyond the single-offender case: #2 and #4 bad in [1,2,3,4]
        // → both ejected, [1,3] merges, and [1,3] was gated green as a unit.
        let ops = FakeBatchOps::new().bad(&[2, 4]);
        let out = run_speculative_batch(&ops, &[1, 2, 3, 4], budget()).await;

        assert_eq!(out.merged, vec![1, 3], "only the clean PRs land, in order");
        let mut ejected: Vec<u64> = out.ejected.iter().map(|(p, _)| *p).collect();
        ejected.sort_unstable();
        assert_eq!(ejected, vec![2, 4], "both offenders must be ejected");
        assert!(out
            .ejected
            .iter()
            .all(|(_, r)| matches!(r, BatchEjectReason::RedGate(_))));
        assert!(
            ops.gated_sets().iter().any(|s| s == &vec![1, 3]),
            "the survivor set must have been gated green as a unit: {:?}",
            ops.gated_sets()
        );
    }

    #[tokio::test]
    async fn batch_rebase_conflict_ejects_pre_gate_and_reforms_without_the_pr() {
        // #2 conflicts during the speculative stack → ejected BEFORE the gate;
        // the batch reforms as [1,3], gates ONCE (the gate never sees #2), and
        // merges [1,3].
        let ops = FakeBatchOps::new().conflict(&[2]);
        let out = run_speculative_batch(&ops, &[1, 2, 3], budget()).await;

        assert_eq!(out.merged, vec![1, 3], "the reformed batch merges without the conflicter");
        assert_eq!(out.gate_calls, 1, "a clean reformed batch gates exactly once");
        assert_eq!(out.ejected.len(), 1);
        let (ejected_pr, reason) = &out.ejected[0];
        assert_eq!(*ejected_pr, 2);
        assert!(
            matches!(reason, BatchEjectReason::RebaseConflict(_)),
            "a stack conflict must be a pre-gate rebase-conflict eject, got {reason:?}"
        );
        // The gate must NEVER have seen the conflicting PR.
        assert_eq!(ops.gated_sets(), vec![vec![1, 3]]);
        assert!(
            !ops.gated_sets().iter().flatten().any(|&p| p == 2),
            "the conflicting PR must never have been gated"
        );
    }

    #[tokio::test]
    async fn batch_gate_timeout_falls_back_to_single_front_pr() {
        // The top-level batch gate times out → the whole batch is NOT bisected;
        // instead fall back to N=1 for the FRONT PR (the caller then runs the
        // PCON-06 single-PR path for it). Nothing is merged or ejected here.
        let ops = FakeBatchOps::new().force_gate(GateVerdict::TimedOut);
        let out = run_speculative_batch(&ops, &[7, 8, 9], budget()).await;

        assert_eq!(
            out.fell_back_to_single,
            Some(7),
            "a batch gate timeout must fall back to N=1 for the FRONT PR"
        );
        assert!(out.merged.is_empty(), "nothing lands on a fall-back");
        assert!(out.ejected.is_empty(), "a fall-back ejects nothing (the batch stays queued)");
        assert_eq!(out.gate_calls, 1, "only the single top-level gate ran before falling back");
    }

    #[tokio::test]
    async fn batch_gate_door_unreachable_also_falls_back_to_single_front_pr() {
        // The compiler door is unreachable at batch-gate time (not a red
        // verdict) → same fail-safe fall-back to N=1 for the front PR.
        let ops = FakeBatchOps::new().force_gate(GateVerdict::Unreachable("door down".into()));
        let out = run_speculative_batch(&ops, &[7, 8], budget()).await;
        assert_eq!(out.fell_back_to_single, Some(7));
        assert!(out.merged.is_empty());
    }

    #[tokio::test]
    async fn batch_of_one_green_gates_once_and_merges() {
        // A batch of exactly one (batch_max effectively 1, or only one PR
        // available) still gates once and merges — the degenerate case that
        // matches the PCON-06 single-PR shape.
        let ops = FakeBatchOps::new();
        let out = run_speculative_batch(&ops, &[42], budget()).await;
        assert_eq!(out.merged, vec![42]);
        assert_eq!(out.gate_calls, 1);
        assert!(out.ejected.is_empty());
        assert!(out.fell_back_to_single.is_none());
    }

    #[tokio::test]
    async fn batch_all_conflict_ejects_everything_and_never_gates() {
        // Every PR conflicts during the stack → all ejected pre-gate, nothing
        // stacked, so the gate never runs and nothing merges.
        let ops = FakeBatchOps::new().conflict(&[1, 2]);
        let out = run_speculative_batch(&ops, &[1, 2], budget()).await;
        assert!(out.merged.is_empty());
        assert_eq!(out.ejected.len(), 2);
        assert_eq!(out.gate_calls, 0, "an all-conflict batch must never gate");
        assert!(out.fell_back_to_single.is_none());
    }

    #[tokio::test]
    async fn batch_green_but_a_bound_land_drifts_requeues_it_and_every_later_member() {
        // A green batch whose member #2 drifts (head/base moved between gate and
        // land): #1 lands, #2 is requeued as a merge failure, and #3 — stacked
        // on #2 — is requeued too rather than landed against a state that no
        // longer holds.
        let ops = FakeBatchOps::new().merge_fail(&[2]);
        let out = run_speculative_batch(&ops, &[1, 2, 3], budget()).await;

        assert_eq!(out.merged, vec![1], "only the pre-drift prefix lands");
        let failed: Vec<u64> = out.merge_failures.iter().map(|(p, _)| *p).collect();
        assert_eq!(failed, vec![2, 3], "the drifter and every later member are requeued");
        assert!(out.ejected.is_empty(), "a land drift is a merge_failure, not a red/conflict eject");
    }
}
