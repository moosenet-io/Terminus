//! BLD-06 — the durable compiler JOB queue.
//!
//! Multiple agents mark themselves "ready for a compiler run" via
//! `compiler_request`; each readiness lands here as a durable, deduped job. The
//! [`scheduler`](super::scheduler) then dispatches jobs one/few at a time so
//! builds never contend.
//!
//! ## Where it lives
//! On the ONE shared Redis provisioned by BLD-20, under the reserved durable
//! namespace [`Namespace::Queue`] (`queue:*`, logical DB `noeviction`) — so a
//! queued build can never be evicted under memory pressure. This module is that
//! namespace's sole consumer (BLD-20 reserved it; the wiring lives here).
//!
//! ## Atomicity (why Lua)
//! Every state transition that must not race — enqueue+dedupe+coalesce, claim
//! (queued→building under a per-module lock and a per-host cap), and complete
//! (release the module lock + host slot) — is a single **atomic Lua script**,
//! exactly as the BLD-20 rate-limiter does its check-and-consume. Redis runs Lua
//! single-threaded, so an interleaving of two agents/schedulers can never
//! double-enqueue the same `module@ref`, start two conflicting builds of one
//! module, or exceed a host's concurrency cap.
//!
//! ## Fail-safe posture
//! [`RedisQueue`] is built from the shared [`RedisBackend`]; when Redis is not
//! configured there is no queue and the tool surface degrades LOUDLY
//! ([`QueueError::Unavailable`]) rather than silently dropping a build request —
//! a lost "please build" is worse than a surfaced error. Every op is
//! timeout-bounded by the backend; an unreachable Redis surfaces as
//! `Unavailable` and the caller decides.
//!
//! ## Retention model (per job state — bounded growth, durable where it matters)
//! - `queued` / `building` (in-flight): **durable, never expire** — a pending or
//!   running build must survive memory pressure (`noeviction` DB). Their dedupe
//!   pointer is durable too (it is the single-owner claim for `module@ref`).
//! - `held` (an agent's `ready=false` intent that was never promoted): bounded by
//!   a **held-intent TTL** (`BUILD_HELD_INTENT_TTL_SECS`) on BOTH the job hash and
//!   its dedupe pointer, so an abandoned intent (and its dedupe entry) cannot grow
//!   unbounded. Promotion to `queued` (a later `ready=true`) `PERSIST`s both.
//! - `done` / `failed` (terminal): retained for `BUILD_JOB_RETAIN_SECS` (history
//!   for `compiler_status`) then self-expire; the dedupe pointer is cleared on
//!   release (or repointed at a re-run).
//!
//! ## Completion is two individually-atomic idempotent transitions (vs AC2)
//! Completion is realized as TWO transitions rather than one atomic Lua, a
//! deliberate departure from the "single atomic complete" wording: `finalize`
//! writes a durable terminal-outcome marker FIRST (one atomic, token-fenced Lua;
//! does not release), then `release` frees the module lock + host slot (one
//! atomic, token-fenced Lua; keys DERIVED from the job hash, never a caller arg).
//! Each step is individually atomic + idempotent + independently retried. The two
//! steps EXIST precisely to enable the no-rebuild self-heal: the reconcile
//! backstop distinguishes a job that is *finished but not yet released* (marker
//! present ⇒ release only, NO rebuild) from one that *crashed mid-build* (no
//! marker + stale ⇒ requeue). A single atomic complete could not leave that
//! observable "finished-but-unreleased" state, so a completion outage would
//! otherwise force a wasteful rebuild. `finalize`/`release` are LOW-LEVEL; a
//! direct caller should use the retrying [`QueueStore::complete`] (the scheduler
//! drives the two steps directly with its own config-tuned retry).
//!
//! The self-heal is PROMPT, not delayed: reconcile releases a finished-but-
//! unreleased job IMMEDIATELY (the `BUILD_STALE_BUILDING_SECS` age gate applies
//! ONLY to the crashed-requeue path), so if the worker's `release` retries are
//! exhausted, the very next reconcile tick after Redis recovers frees the lock +
//! slot — no waiting out the stale floor. The fence token still prevents any
//! double-free or rebuild.
//!
//! ## Discipline (S1/S7)
//! No infra literals: every key is formed through [`Namespace::Queue`], the
//! endpoint/password come from the vault-materialized env via [`RedisBackend`],
//! and every tunable (retention, held-intent TTL) is a config env var. No secret
//! is read here.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::compiler::host::HostRole;
use crate::redis::{Namespace, RedisBackend};

/// Env var: how long (secs) a COMPLETED job's hash is retained for
/// `compiler_status` before it self-expires. Config-driven; a sane fallback
/// keeps recent history visible without unbounded growth. This bounds retention
/// of ALREADY-FINISHED jobs only — never a queued/in-flight one (those never
/// expire: durable `noeviction`).
const RETAIN_SECS_ENV: &str = "BUILD_JOB_RETAIN_SECS";
const DEFAULT_RETAIN_SECS: i64 = 86_400;

/// Env var: how long (secs) a never-promoted `held` intent (a `ready=false`
/// marker) — and its dedupe pointer — live before self-expiring, so an abandoned
/// intent cannot grow unbounded. A `ready=true` promotion `PERSIST`s them (they
/// become durable queued jobs). Default a day.
const HELD_INTENT_TTL_ENV: &str = "BUILD_HELD_INTENT_TTL_SECS";
const DEFAULT_HELD_INTENT_TTL_SECS: i64 = 86_400;

fn retain_secs() -> i64 {
    std::env::var(RETAIN_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_RETAIN_SECS)
}

/// Bounded-backoff schedule for the queue-layer [`QueueStore::complete`] retry
/// (the sanctioned entry for DIRECT callers — the scheduler uses its own
/// config-tuned retry). Kept modest so an ad-hoc caller isn't a footgun on a
/// brief Redis blip; reconcile is the ultimate backstop for a long outage.
const COMPLETE_RETRY_MAX: u32 = 8;
const COMPLETE_RETRY_BASE_MS: u64 = 25;

/// Exponential backoff (capped) for attempt `n` of a `complete` retry.
fn complete_backoff(n: u32) -> std::time::Duration {
    std::time::Duration::from_millis(COMPLETE_RETRY_BASE_MS.saturating_mul(1u64 << n.min(5)))
}

fn held_intent_ttl_secs() -> i64 {
    std::env::var(HELD_INTENT_TTL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_HELD_INTENT_TTL_SECS)
}

/// Milliseconds since the Unix epoch (a wall clock for request/dispatch
/// timestamps; queue ORDERING uses the durable server-side sequence, not this).
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build-request priority. Higher priority is dispatched first; ties break FIFO
/// by the durable enqueue sequence. Priority NEVER preempts a *running* build
/// (no mid-build cancellation — see the scheduler), only queue order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    /// Parse a priority label; unknown/empty ⇒ `Normal`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Priority::Low,
            "high" => Priority::High,
            _ => Priority::Normal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
        }
    }

    /// The numeric rank used in the queue score (higher ⇒ dispatched sooner).
    pub fn rank(self) -> i64 {
        match self {
            Priority::Low => 0,
            Priority::Normal => 1,
            Priority::High => 2,
        }
    }
}

/// Terminal state of a build, recorded on [`QueueStore::complete`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Done,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Done => "done",
            JobState::Failed => "failed",
        }
    }
}

/// A request to build `module@git_ref`. `heavy` records whether this build needs
/// the heavy host (computed by the caller via the same `select_role` heuristic
/// `compiler_build` uses) so the scheduler can window-gate heavy builds without
/// re-deriving it. `ready=false` records the intent as *held* (not yet
/// dispatchable) so several agents can converge; a later `ready=true` for the
/// same `module@ref` promotes it.
#[derive(Debug, Clone)]
pub struct JobRequest {
    pub module: String,
    pub git_ref: String,
    pub priority: Priority,
    pub heavy: bool,
    pub ready: bool,
}

/// Outcome of an [`QueueStore::enqueue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enqueued {
    /// The durable job id (a NEW one, or the EXISTING one when coalesced).
    pub job_id: String,
    /// `true` ⇒ a brand-new job was created; `false` ⇒ an existing `module@ref`
    /// job absorbed this readiness (coalesced) — the caller's readiness still
    /// counts, but no second build will run.
    pub created: bool,
}

/// A queued (dispatchable) job as seen by the scheduler / status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJob {
    pub job_id: String,
    pub module: String,
    pub git_ref: String,
    pub priority: Priority,
    pub heavy: bool,
}

/// Why a [`QueueStore::claim`] did not take the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The job moved queued→building on this host; the caller now OWNS it and
    /// MUST eventually call [`QueueStore::complete`] with the returned `token`.
    /// The `token` is a FENCE: it is written into the job on claim and checked by
    /// `complete`/reconcile, so a stale retried completion from a worker whose
    /// job was reconciled + re-claimed can never free the NEW claim's slot/lock
    /// (its token no longer matches).
    Claimed { token: String },
    /// The job was no longer queued (already claimed/coalesced away).
    NotQueued,
    /// Another build of the SAME module holds the per-module lock (graceful
    /// serialization — never two conflicting builds of one module at once).
    ModuleBusy,
    /// The target host is already at its concurrency cap.
    HostFull,
    /// The call was REFUSED as malformed: the caller's `module` arg does not match
    /// the job's own stored module (a buggy call that must never take a foreign
    /// module lock — A2). The job is left untouched.
    Rejected,
}

/// An in-flight (building) job — a "lease" — surfaced by `compiler_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub job_id: String,
    pub module: String,
    pub git_ref: String,
    pub host: HostRole,
    pub started_at_ms: i64,
}

/// A point-in-time view of the whole queue for `compiler_status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueSnapshot {
    /// Queued (dispatchable) jobs, in dispatch order.
    pub queued: Vec<QueuedJob>,
    /// In-flight builds (leases), one entry per host slot in use.
    pub leases: Vec<Lease>,
}

/// A queue op could not be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// Redis is not configured or is unreachable — the durable queue is down.
    /// The tool surface degrades LOUDLY on this (never silently drops a build).
    Unavailable,
    /// A completion was attempted with a token that does NOT own the current
    /// claim (wrong or stale — e.g. the job was reconciled + re-claimed, or was
    /// never owned by this caller). The transition did NOT happen; surfaced so a
    /// direct caller can never observe a FALSE success that masks an unfinished
    /// build. NOT returned for the genuine in-flight retry of the same correct
    /// token (which still owns the build until release clears it).
    StaleToken,
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::Unavailable => write!(
                f,
                "compiler job queue is unavailable (Redis not configured or unreachable)"
            ),
            QueueError::StaleToken => write!(
                f,
                "completion token does not own the current claim (wrong/stale); \
                 the build was not completed by this caller"
            ),
        }
    }
}

/// The result of a [`QueueStore::finalize`]: did THIS token's completion record
/// the terminal outcome, or is the token stale/wrong (not the owner)?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeOutcome {
    /// The terminal-outcome marker was recorded — this token owns the build.
    Finalized,
    /// The token did not match the job's current claim; nothing was recorded.
    StaleToken,
}

/// The durable compiler job queue. Implemented by [`RedisQueue`] over the shared
/// Redis, and by an offline, semantically-identical fake (`fake::InMemoryQueue`,
/// test-only) so the scheduler and the atomic enqueue/claim/complete guarantees
/// are unit-tested with NO Redis.
#[async_trait]
pub trait QueueStore: Send + Sync {
    /// Atomically enqueue `req`, deduping/coalescing by `module@ref`. A held job
    /// is promoted when a later `ready=true` request arrives.
    async fn enqueue(&self, req: &JobRequest) -> Result<Enqueued, QueueError>;

    /// The next `limit` dispatchable jobs in dispatch order (priority, then FIFO).
    async fn peek(&self, limit: usize) -> Result<Vec<QueuedJob>, QueueError>;

    /// Atomically try to claim `job` for a build on `host`: succeeds only if it
    /// is still queued, the module lock is free, and the host is below `cap`. On
    /// success returns [`ClaimOutcome::Claimed`] carrying the fence token the
    /// completion must present.
    async fn claim(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        cap: u32,
    ) -> Result<ClaimOutcome, QueueError>;

    /// LOW-LEVEL step 1 of completion: durably record the terminal outcome (a
    /// marker) in ONE atomic, token-fenced, idempotent Lua. Does NOT release the
    /// lock/slot and does NOT retry. Returns [`FinalizeOutcome::Finalized`] when
    /// THIS token owns the claim (the marker was written — idempotent on retry of
    /// the same token), or [`FinalizeOutcome::StaleToken`] when the token does not
    /// match (nothing recorded) — the caller must NOT treat that as success. This
    /// marker is what lets [`reconcile`](Self::reconcile) release (never rebuild) a
    /// finished-but-unreleased job. **Direct callers should prefer
    /// [`complete`](Self::complete)** (which retries + surfaces a stale token); the
    /// scheduler drives `finalize`/`release` directly with its own config-tuned retry.
    async fn finalize(
        &self,
        job_id: &str,
        state: JobState,
        token: &str,
    ) -> Result<FinalizeOutcome, QueueError>;

    /// LOW-LEVEL step 2 of completion: release the module lock + host slot (keys
    /// DERIVED from the job hash's own module/host — A1), honor a re-run / clear
    /// the dedupe entry, and record the terminal state from the `finalize` marker,
    /// in ONE atomic, token-fenced, idempotent Lua. Does NOT retry. A token
    /// mismatch (already released, or reconciled + re-claimed) is a safe no-op.
    /// **Direct callers should prefer [`complete`](Self::complete).**
    async fn release(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        token: &str,
    ) -> Result<(), QueueError>;

    /// Token-fenced IMMEDIATE requeue of a claimed (`building`) job back to
    /// `queued`, WITHOUT recording any terminal outcome: free the module lock +
    /// host slot (both derived in-Lua from the job's OWN stored fields — A1),
    /// clear the fence token, and re-add the job to the dispatch set so a later
    /// scheduler tick picks it up. Used by the scheduler when a heavy build cannot
    /// acquire its idle-mode lease (insufficient freed RAM) — the build must NOT
    /// run, but the request must NOT be lost (BLD-11). Idempotent + token-fenced:
    /// a wrong/stale token (already released, or reconciled + re-claimed) is a safe
    /// no-op. Mirrors the crash-requeue branch of [`reconcile`](Self::reconcile),
    /// but is immediate and token-gated rather than lease-age gated.
    async fn requeue(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        token: &str,
    ) -> Result<(), QueueError>;

    /// The SANCTIONED completion entry for a direct (non-scheduler) caller:
    /// [`finalize`](Self::finalize) THEN [`release`](Self::release), each an
    /// individually-atomic idempotent transition, EACH RETRIED with bounded
    /// backoff so a Redis outage self-heals instead of surfacing raw. Recording
    /// the outcome first preserves the no-rebuild guarantee: if `release` never
    /// lands, reconcile finds the marker and releases (never rebuilds); the
    /// scheduler uses the same two-step shape with its own tuned retry.
    ///
    /// A WRONG/STALE token surfaces as `Err(`[`QueueError::StaleToken`]`)` — never
    /// a false `Ok(())` — so a direct caller cannot mask a build it does not own.
    /// The genuine in-flight retry of the SAME correct token still succeeds (the
    /// token owns the build until `release` clears it).
    async fn complete(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        state: JobState,
        token: &str,
    ) -> Result<(), QueueError> {
        // STEP 1: record the outcome (retry a transient outage; a stale token is
        // a definitive non-success — do NOT report Ok for a transition that did
        // not happen).
        let mut finalized = false;
        for attempt in 0..COMPLETE_RETRY_MAX.max(1) {
            match self.finalize(job_id, state, token).await {
                Ok(FinalizeOutcome::Finalized) => {
                    finalized = true;
                    break;
                }
                Ok(FinalizeOutcome::StaleToken) => return Err(QueueError::StaleToken),
                Err(QueueError::StaleToken) => return Err(QueueError::StaleToken),
                Err(QueueError::Unavailable) => {
                    tokio::time::sleep(complete_backoff(attempt)).await;
                }
            }
        }
        if !finalized {
            return Err(QueueError::Unavailable);
        }
        // STEP 2: free the lock/slot (retry until it lands; on the correct token
        // it succeeds — release is a token-fenced no-op only for a stale token,
        // which cannot occur here since finalize just confirmed ownership).
        for attempt in 0..COMPLETE_RETRY_MAX.max(1) {
            if self.release(job_id, module, host, token).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(complete_backoff(attempt)).await;
        }
        Err(QueueError::Unavailable)
    }

    /// Crash/restart backstop: sweep every `building` job. TWO paths with DISTINCT
    /// timing:
    ///   - **FINISHED-but-unreleased** (a durable `outcome` marker present, e.g.
    ///     the worker's `release` retries were exhausted): released IMMEDIATELY on
    ///     this sweep, NO rebuild — the `stale_after` age gate is NOT applied, so
    ///     the lock/slot self-heal PROMPTLY on the next tick once Redis recovers
    ///     (including the scheduler's first tick after a restart).
    ///   - **CRASHED mid-build** (NO marker): requeued only once the claim is older
    ///     than `stale_after` (the safe floor), so a genuinely-live long build is
    ///     never wrongly requeued.
    /// The `stale_after` age gate therefore applies ONLY to the crashed-requeue
    /// path. Atomic + token-fenced per job (no double-free/rebuild); a Redis-down
    /// sweep degrades to `Unavailable` (nothing partially changed — retry next tick).
    async fn reconcile(
        &self,
        stale_after: std::time::Duration,
    ) -> Result<ReconcileReport, QueueError>;

    /// A snapshot of queued jobs + in-flight leases for `compiler_status`.
    async fn snapshot(&self) -> Result<QueueSnapshot, QueueError>;
}

/// What a [`QueueStore::reconcile`] sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Crashed-mid-build jobs requeued for a fresh dispatch.
    pub requeued: Vec<String>,
    /// Finished-but-unreleased jobs released WITHOUT a rebuild (self-heal).
    pub released: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Key construction (all through Namespace::Queue — S1)
// ─────────────────────────────────────────────────────────────────────────────

/// The sorted set of DISPATCHABLE job ids (score orders priority then FIFO).
fn zset_key() -> String {
    Namespace::Queue.key("dispatch")
}
/// The monotonic enqueue-sequence counter (FIFO tiebreak within a priority).
fn seq_key() -> String {
    Namespace::Queue.key("seq")
}
/// The namespaced prefix for the per-`(module, ref)` dedupe pointers. Passed to
/// the release/reconcile Lua so it can derive the dedupe key from the job hash's
/// own module/ref without a separate round-trip.
fn dedupe_prefix() -> String {
    Namespace::Queue.key("dedupe:")
}
/// A COLLISION-FREE identity for a `(module, ref)` pair: `"{len(module)}:{module}:{ref}"`.
/// The decimal length prefix marks exactly where `module` ends, so the encoding
/// is injective even when either component contains `:` or `@` — distinct pairs
/// never alias (e.g. `("a","b@c")` ≠ `("a@b","c")`). The IDENTICAL construction
/// is used in the release/reconcile Lua (`#module..':'..module..':'..ref`), so
/// Rust-built and Lua-derived dedupe keys always agree. Encode-only (never decoded).
fn dedupe_id(module: &str, git_ref: &str) -> String {
    format!("{}:{module}:{git_ref}", module.len())
}
/// Per-`(module, ref)` dedupe pointer → the owning job id.
fn dedupe_key(module: &str, git_ref: &str) -> String {
    format!("{}{}", dedupe_prefix(), dedupe_id(module, git_ref))
}
/// The per-job hash prefix (the Lua scripts append the id).
fn job_prefix() -> String {
    Namespace::Queue.key("job:")
}
fn job_key(job_id: &str) -> String {
    format!("{}{job_id}", job_prefix())
}
/// The namespaced prefix for per-module locks. Passed to `RECONCILE_LUA` so it
/// can derive the lock key from the (stale) job hash's own module.
fn module_lock_prefix() -> String {
    Namespace::Queue.key("modulelock:")
}
/// Per-module serialization lock (held for the duration of a build). The Lua now
/// derives this key from the job hash's own module (A1/A2); retained as the
/// canonical key constructor used by tests + the namespace assertion.
#[allow(dead_code)]
fn module_lock_key(module: &str) -> String {
    format!("{}{module}", module_lock_prefix())
}
/// The namespaced prefix for per-host in-flight sets. Passed to the Lua so
/// `release`/`reconcile` can derive the host-slot key from the job hash's OWN
/// stored `host` field (not a caller arg).
fn host_set_prefix() -> String {
    Namespace::Queue.key("inflight:")
}
/// Per-host set of in-flight job ids (its cardinality is the host's live load).
fn host_set_key(host: HostRole) -> String {
    format!("{}{}", host_set_prefix(), host.as_str())
}

// ─────────────────────────────────────────────────────────────────────────────
// Atomic Lua scripts
// ─────────────────────────────────────────────────────────────────────────────

/// Enqueue with dedupe/coalesce/promote. Returns `{job_id, created(0/1)}`.
/// KEYS: 1=dedupe 2=zset 3=seq
/// ARGV: 1=candidate_id 2=prank 3=now 4=job_prefix 5=module 6=ref 7=prio_label
///       8=heavy(0/1) 9=ready(0/1) 10=held_ttl_secs
const ENQUEUE_LUA: &str = r#"
local dedupe=KEYS[1]
local zset=KEYS[2]
local seqk=KEYS[3]
local id=ARGV[1]
local prank=tonumber(ARGV[2])
local now=ARGV[3]
local jp=ARGV[4]
local heavy=ARGV[8]
local ready=ARGV[9]
local held_ttl=tonumber(ARGV[10])
local existing=redis.call('GET', dedupe)
if existing then
  local jk=jp..existing
  local st=redis.call('HGET', jk, 'state')
  if st=='queued' or st=='held' then
    redis.call('HINCRBY', jk, 'coalesced', 1)
    local cur=tonumber(redis.call('HGET', jk, 'prank') or '0')
    if prank>cur then
      redis.call('HSET', jk, 'prank', prank, 'priority', ARGV[7])
    end
    -- Monotonic host-class upgrade: a later heavy/fast request promotes the job
    -- to heavy so it respects the heavy-build window; never downgrade heavy→small.
    if heavy=='1' then
      redis.call('HSET', jk, 'heavy', '1')
    end
    local newst=st
    if ready=='1' and st=='held' then
      redis.call('HSET', jk, 'state', 'queued')
      newst='queued'
      -- Promoted to a durable dispatchable job: drop the held-intent TTL on the
      -- job hash AND its dedupe pointer so they can no longer expire.
      redis.call('PERSIST', jk)
      redis.call('PERSIST', dedupe)
    end
    if newst=='queued' then
      local seq=tonumber(redis.call('HGET', jk, 'seq') or '0')
      local eff=tonumber(redis.call('HGET', jk, 'prank') or '0')
      local score=seq-(eff*1000000000000)
      redis.call('ZADD', zset, score, existing)
    end
    return {existing, 0}
  end
  if st=='building' then
    -- The build is ALREADY happening. Never create a second job for the same
    -- module@ref. Only a READY=true request schedules a single idempotent pending
    -- re-run (a held ready=false arrival records intent but must NOT become a
    -- dispatchable re-run); a third ready request does not stack another. Still
    -- apply the monotonic heavy upgrade + priority bump so the re-run respects them.
    if ready=='1' then
      redis.call('HSET', jk, 'rerun', '1')
    end
    redis.call('HINCRBY', jk, 'coalesced', 1)
    local cur=tonumber(redis.call('HGET', jk, 'prank') or '0')
    if prank>cur then
      redis.call('HSET', jk, 'prank', prank, 'priority', ARGV[7])
    end
    if heavy=='1' then
      redis.call('HSET', jk, 'heavy', '1')
    end
    return {existing, 0}
  end
end
local seq=redis.call('INCR', seqk)
local jk=jp..id
local state='held'
if ready=='1' then state='queued' end
redis.call('HSET', jk, 'module', ARGV[5], 'ref', ARGV[6], 'prank', prank,
  'priority', ARGV[7], 'heavy', heavy, 'seq', seq, 'requested_at', now,
  'coalesced', 1, 'state', state)
redis.call('SET', dedupe, id)
if ready=='1' then
  local score=seq-(prank*1000000000000)
  redis.call('ZADD', zset, score, id)
else
  -- Held (never-dispatched) intent: bound its lifetime so an abandoned
  -- ready=false marker (and its dedupe entry) cannot grow unbounded.
  if held_ttl>0 then
    redis.call('EXPIRE', jk, held_ttl)
    redis.call('EXPIRE', dedupe, held_ttl)
  end
end
return {id, 1}
"#;

/// Claim queued→building under the module lock + host cap. The module-lock key is
/// DERIVED from the job hash's OWN stored module (A2), and the caller's `module`
/// arg is VERIFIED against it — a mismatch is refused so a buggy call can never
/// take a foreign module lock and break per-module serialization. On success
/// writes the claim FENCE token + `started_at` (the reconcile lease clock).
/// Returns `{ok(0/1), token_or_reason}`.
/// KEYS: 1=zset 2=jobhash 3=hostset
/// ARGV: 1=id 2=cap 3=now 4=host 5=claim_token 6=modulelock_prefix 7=expected_module
const CLAIM_LUA: &str = r#"
local st=redis.call('HGET', KEYS[2], 'state')
if st~='queued' then return {0, 'not_queued'} end
local module=redis.call('HGET', KEYS[2], 'module')
if not module then return {0, 'rejected'} end
if ARGV[7] ~= '' and ARGV[7] ~= module then return {0, 'rejected'} end
local lockkey=ARGV[6]..module
if redis.call('EXISTS', lockkey)==1 then return {0, 'module_busy'} end
if redis.call('SCARD', KEYS[3])>=tonumber(ARGV[2]) then return {0, 'host_full'} end
redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('HSET', KEYS[2], 'state', 'building', 'host', ARGV[4],
  'started_at', ARGV[3], 'claim_token', ARGV[5])
redis.call('SET', lockkey, ARGV[1])
redis.call('SADD', KEYS[3], ARGV[1])
return {1, ARGV[5]}
"#;

/// FINALIZE: durable terminal-outcome marker, the FIRST step a worker takes when
/// a build finishes — token-fenced, and it does NOT release the lock/slot. This
/// is what lets the reconcile backstop tell "finished, just needs release" apart
/// from "crashed mid-build", so a job whose worker finished successfully is never
/// rebuilt. Idempotent (re-finalizing with the same token is a no-op-ish HSET).
/// Returns `1` if applied, `0` on a token mismatch (stale/duplicate).
/// KEYS: 1=jobhash   ARGV: 1=outcome(done/failed) 2=now 3=claim_token
const FINALIZE_LUA: &str = r#"
if redis.call('HGET', KEYS[1], 'claim_token') ~= ARGV[3] then return 0 end
redis.call('HSET', KEYS[1], 'outcome', ARGV[1], 'finished_at', ARGV[2])
return 1
"#;

/// Shared release body (Lua fragment). Frees the module lock + host slot, clears
/// the fence token, re-enqueues exactly one re-run if flagged (else clears the
/// dedupe pointer), and finalizes the hash state with the retain TTL. Requires
/// these locals: `jobkey zsetkey seqkey jobid nowv retainv rerun_id jobprefix
/// dedupeprefix lockprefix hostprefix final_state`. Sets `rr_flag`(0/1) + `rr_id`.
///
/// The module-lock key, host-slot key, AND dedupe key are ALL derived from the
/// job hash's OWN stored `module`/`host`/`ref` fields (never a caller arg), so a
/// release/reconcile called with a wrong or stale module/host still frees the
/// CORRECT lock/slot and can never wedge the real ones (A1). The module lock is
/// only deleted if it still points to THIS job (fence).
const RELEASE_BODY: &str = r#"
local module=redis.call('HGET', jobkey, 'module')
local ref=redis.call('HGET', jobkey, 'ref')
local host=redis.call('HGET', jobkey, 'host')
if module then
  local lockkey=lockprefix..module
  if redis.call('GET', lockkey)==jobid then redis.call('DEL', lockkey) end
end
if host then
  redis.call('SREM', hostprefix..host, jobid)
end
redis.call('HDEL', jobkey, 'claim_token')
local dedupe=false
if module and ref then dedupe=dedupeprefix..string.len(module)..':'..module..':'..ref end
local rerun=redis.call('HGET', jobkey, 'rerun')
local rr_flag=0
local rr_id=''
if rerun=='1' then
  local prank=tonumber(redis.call('HGET', jobkey, 'prank') or '1')
  local prio=redis.call('HGET', jobkey, 'priority') or 'normal'
  local heavy=redis.call('HGET', jobkey, 'heavy') or '0'
  local seq=redis.call('INCR', seqkey)
  rr_id=rerun_id
  local njk=jobprefix..rr_id
  redis.call('HSET', njk, 'module', module, 'ref', ref, 'prank', prank,
    'priority', prio, 'heavy', heavy, 'seq', seq, 'requested_at', nowv,
    'coalesced', 1, 'state', 'queued')
  if dedupe then redis.call('SET', dedupe, rr_id) end
  local score=seq-(prank*1000000000000)
  redis.call('ZADD', zsetkey, score, rr_id)
  rr_flag=1
else
  if dedupe and redis.call('GET', dedupe)==jobid then redis.call('DEL', dedupe) end
end
redis.call('HSET', jobkey, 'state', final_state, 'finished_at', nowv)
redis.call('EXPIRE', jobkey, retainv)
"#;

/// RELEASE: free the lock/slot in ONE atomic, token-fenced, idempotent script.
/// The module-lock + host-slot keys are DERIVED from the job hash's own stored
/// module/host (A1), so a wrong/stale caller arg cannot free the wrong lock/slot.
/// `final_state` comes from the durable `outcome` marker written by FINALIZE
/// (defaulting to `done` if a caller releases without finalizing). A mismatched
/// token (already released, or reconciled + re-claimed) is a safe no-op — it
/// never double-frees the host slot (a SET SREM is a no-op the 2nd time) and
/// never frees another claim's lock/slot. Returns `{rerun_queued(0/1), new_id}`.
/// KEYS: 1=jobhash 2=zset 3=seq
/// ARGV: 1=id 2=now 3=retain_secs 4=rerun_candidate_id 5=job_prefix
///       6=dedupe_prefix 7=claim_token 8=modulelock_prefix 9=host_set_prefix
fn release_lua() -> String {
    format!(
        r#"
if redis.call('HGET', KEYS[1], 'claim_token') ~= ARGV[7] then return {{0, ''}} end
local jobkey=KEYS[1]
local zsetkey=KEYS[2]
local seqkey=KEYS[3]
local jobid=ARGV[1]
local nowv=ARGV[2]
local retainv=tonumber(ARGV[3])
local rerun_id=ARGV[4]
local jobprefix=ARGV[5]
local dedupeprefix=ARGV[6]
local lockprefix=ARGV[8]
local hostprefix=ARGV[9]
local final_state=redis.call('HGET', jobkey, 'outcome') or 'done'
{RELEASE_BODY}
return {{rr_flag, rr_id}}
"#
    )
}

/// Peek the top-N dispatchable jobs, flattened as 5 fields each:
/// `id, module, ref, prank, heavy`.
/// KEYS: 1=zset  ARGV: 1=limit 2=job_prefix
const PEEK_LUA: &str = r#"
local ids=redis.call('ZRANGE', KEYS[1], 0, tonumber(ARGV[1])-1)
local out={}
for _, id in ipairs(ids) do
  local jk=ARGV[2]..id
  out[#out+1]=id
  out[#out+1]=redis.call('HGET', jk, 'module') or ''
  out[#out+1]=redis.call('HGET', jk, 'ref') or ''
  out[#out+1]=redis.call('HGET', jk, 'prank') or '0'
  out[#out+1]=redis.call('HGET', jk, 'heavy') or '0'
end
return out
"#;

/// List the in-flight leases on one host, flattened as 4 fields each:
/// `id, module, ref, started_at`.
/// KEYS: 1=hostset  ARGV: 1=job_prefix
const LEASES_LUA: &str = r#"
local ids=redis.call('SMEMBERS', KEYS[1])
local out={}
for _, id in ipairs(ids) do
  local jk=ARGV[1]..id
  out[#out+1]=id
  out[#out+1]=redis.call('HGET', jk, 'module') or ''
  out[#out+1]=redis.call('HGET', jk, 'ref') or ''
  out[#out+1]=redis.call('HGET', jk, 'started_at') or '0'
end
return out
"#;

/// Reconcile ONE `building` job atomically, distinguishing two cases via the
/// durable `outcome` marker (written by FINALIZE):
///   - `outcome` present ⇒ the worker FINISHED but never released (its completion
///     retries were exhausted). RELEASE only — free the lock/slot, honor a re-run,
///     record the terminal state — and do NOT rebuild (return `2`). This closes
///     the double-BUILD hole in the self-heal path.
///   - no `outcome` + claim older than the lease ⇒ CRASHED mid-build. Requeue —
///     free the lock/slot, clear the token, re-add to the dispatch ZSET (return `1`).
///   - otherwise (a live, fresh build) ⇒ untouched (return `0`).
/// A crashed/finished worker can never permanently wedge the module lock + host
/// slot, and a finished build is never rebuilt. The module lock, host-slot, and
/// dedupe keys are ALL derived in-Lua from the job hash's own module/host/ref via
/// the prefix ARGVs (A1) — never a caller arg.
/// KEYS: 1=jobhash 2=hostset(enumerated) 3=zset 4=seq
/// ARGV: 1=id 2=now 3=stale_ms 4=modulelock_prefix 5=retain 6=rerun_id
///       7=job_prefix 8=dedupe_prefix 9=host_set_prefix
fn reconcile_lua() -> String {
    format!(
        r#"
local jobkey=KEYS[1]
local hostkey=KEYS[2]
local zsetkey=KEYS[3]
local seqkey=KEYS[4]
local jobid=ARGV[1]
local nowv=ARGV[2]
local lockprefix=ARGV[4]
local hostprefix=ARGV[9]
local st=redis.call('HGET', jobkey, 'state')
if st~='building' then
  -- Not building (already released/gone) → ensure it isn't lingering in the host
  -- set, then no-op.
  redis.call('SREM', hostkey, jobid)
  return 0
end
local outcome=redis.call('HGET', jobkey, 'outcome')
if outcome then
  -- FINISHED but not yet released → release only, NO rebuild. RELEASE_BODY
  -- derives the module-lock + host-slot keys from the hash's own fields.
  local retainv=tonumber(ARGV[5])
  local rerun_id=ARGV[6]
  local jobprefix=ARGV[7]
  local dedupeprefix=ARGV[8]
  local final_state=outcome
{RELEASE_BODY}
  return 2
end
-- No terminal marker: crashed/hung mid-build. Requeue only once stale.
local started=tonumber(redis.call('HGET', jobkey, 'started_at') or '0')
if (tonumber(nowv) - started) < tonumber(ARGV[3]) then
  return 0
end
-- Derive the module lock + host slot from the hash's OWN fields.
local module=redis.call('HGET', jobkey, 'module')
if module then
  local lockkey=lockprefix..module
  if redis.call('GET', lockkey)==jobid then redis.call('DEL', lockkey) end
end
local host=redis.call('HGET', jobkey, 'host')
if host then redis.call('SREM', hostprefix..host, jobid) end
redis.call('SREM', hostkey, jobid)
redis.call('HDEL', jobkey, 'claim_token')
redis.call('HSET', jobkey, 'state', 'queued')
local seq=tonumber(redis.call('HGET', jobkey, 'seq') or '0')
local prank=tonumber(redis.call('HGET', jobkey, 'prank') or '1')
local score=seq-(prank*1000000000000)
redis.call('ZADD', zsetkey, score, jobid)
return 1
"#
    )
}

/// Token-fenced IMMEDIATE requeue of a claimed job back to `queued` (BLD-11): free
/// the module lock + host slot — both DERIVED in-Lua from the job hash's OWN stored
/// module/host (A1, never a caller arg) — clear the fence token, and re-add to the
/// dispatch ZSET at the job's stored priority/seq. Records NO terminal outcome (the
/// build never ran). A token mismatch (already released / reconciled + re-claimed)
/// is a safe no-op (`return 0`). Returns `1` when it requeued, `0` on the fenced
/// no-op.
/// KEYS: 1=jobhash 2=zset
/// ARGV: 1=id 2=token 3=modulelock_prefix 4=host_set_prefix
fn requeue_lua() -> String {
    r#"
if redis.call('HGET', KEYS[1], 'claim_token') ~= ARGV[2] then return 0 end
local jobkey=KEYS[1]
local zsetkey=KEYS[2]
local jobid=ARGV[1]
local lockprefix=ARGV[3]
local hostprefix=ARGV[4]
-- Free the per-module lock iff this job owns it (derived from the hash's module).
local module=redis.call('HGET', jobkey, 'module')
if module then
  local lockkey=lockprefix..module
  if redis.call('GET', lockkey)==jobid then redis.call('DEL', lockkey) end
end
-- Free the host slot (derived from the hash's stored host).
local host=redis.call('HGET', jobkey, 'host')
if host then redis.call('SREM', hostprefix..host, jobid) end
-- Clear the claim so a late completion no-ops, and return to the queued state.
redis.call('HDEL', jobkey, 'claim_token')
redis.call('HDEL', jobkey, 'host')
redis.call('HDEL', jobkey, 'started_at')
redis.call('HSET', jobkey, 'state', 'queued')
-- Re-add to the dispatch ZSET at the stored priority/seq score.
local seq=tonumber(redis.call('HGET', jobkey, 'seq') or '0')
local prank=tonumber(redis.call('HGET', jobkey, 'prank') or '1')
local score=seq-(prank*1000000000000)
redis.call('ZADD', zsetkey, score, jobid)
return 1
"#
    .to_string()
}

fn priority_from_rank(rank: i64) -> Priority {
    match rank {
        r if r >= 2 => Priority::High,
        r if r <= 0 => Priority::Low,
        _ => Priority::Normal,
    }
}

/// The durable Redis-backed queue (production).
pub struct RedisQueue {
    backend: Arc<RedisBackend>,
}

impl RedisQueue {
    pub fn new(backend: Arc<RedisBackend>) -> Self {
        Self { backend }
    }

    /// Build from the shared process-global backend; `None` when Redis is not
    /// configured (the whole compiler-queue feature degrades — the tools then
    /// report [`QueueError::Unavailable`]).
    pub fn from_env() -> Option<Self> {
        RedisBackend::from_env().map(Self::new)
    }
}

#[async_trait]
impl QueueStore for RedisQueue {
    async fn enqueue(&self, req: &JobRequest) -> Result<Enqueued, QueueError> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (dedupe, zset, seq) = (dedupe_key(&req.module, &req.git_ref), zset_key(), seq_key());
        let (prank, now, jp) = (req.priority.rank(), now_ms(), job_prefix());
        let (module, git_ref, label) =
            (req.module.clone(), req.git_ref.clone(), req.priority.as_str());
        let (heavy, ready) = (req.heavy as i64, req.ready as i64);
        let held_ttl = held_intent_ttl_secs();
        let script = redis::Script::new(ENQUEUE_LUA);
        let out: Result<(String, i64), ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(dedupe)
                    .key(zset)
                    .key(seq)
                    .arg(id)
                    .arg(prank)
                    .arg(now)
                    .arg(jp)
                    .arg(module)
                    .arg(git_ref)
                    .arg(label)
                    .arg(heavy)
                    .arg(ready)
                    .arg(held_ttl)
                    .invoke_async::<_, (String, i64)>(&mut conn)
                    .await
            })
            .await;
        match out {
            Ok((job_id, created)) => Ok(Enqueued {
                job_id,
                created: created == 1,
            }),
            Err(()) => Err(QueueError::Unavailable),
        }
    }

    async fn peek(&self, limit: usize) -> Result<Vec<QueuedJob>, QueueError> {
        let (zset, jp, limit) = (zset_key(), job_prefix(), limit.max(1) as i64);
        let script = redis::Script::new(PEEK_LUA);
        let out: Result<Vec<String>, ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(zset)
                    .arg(limit)
                    .arg(jp)
                    .invoke_async::<_, Vec<String>>(&mut conn)
                    .await
            })
            .await;
        match out {
            Ok(flat) => Ok(flat
                .chunks_exact(5)
                .map(|c| QueuedJob {
                    job_id: c[0].clone(),
                    module: c[1].clone(),
                    git_ref: c[2].clone(),
                    priority: priority_from_rank(c[3].parse().unwrap_or(1)),
                    heavy: c[4] == "1",
                })
                .collect()),
            Err(()) => Err(QueueError::Unavailable),
        }
    }

    async fn claim(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        cap: u32,
    ) -> Result<ClaimOutcome, QueueError> {
        let (zset, jk, hset) = (zset_key(), job_key(job_id), host_set_key(host));
        let (id, now, host_s) = (job_id.to_string(), now_ms(), host.as_str().to_string());
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (lock_prefix, expected_module) = (module_lock_prefix(), module.to_string());
        let cap = cap.max(1) as i64;
        let script = redis::Script::new(CLAIM_LUA);
        let out: Result<(i64, String), ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(zset)
                    .key(jk)
                    .key(hset)
                    .arg(id)
                    .arg(cap)
                    .arg(now)
                    .arg(host_s)
                    .arg(token)
                    .arg(lock_prefix)
                    .arg(expected_module)
                    .invoke_async::<_, (i64, String)>(&mut conn)
                    .await
            })
            .await;
        match out {
            Ok((1, token)) => Ok(ClaimOutcome::Claimed { token }),
            Ok((_, reason)) => Ok(match reason.as_str() {
                "module_busy" => ClaimOutcome::ModuleBusy,
                "host_full" => ClaimOutcome::HostFull,
                "rejected" => ClaimOutcome::Rejected,
                _ => ClaimOutcome::NotQueued,
            }),
            Err(()) => Err(QueueError::Unavailable),
        }
    }

    async fn finalize(
        &self,
        job_id: &str,
        state: JobState,
        token: &str,
    ) -> Result<FinalizeOutcome, QueueError> {
        let jk = job_key(job_id);
        let (outcome, now, token) = (state.as_str().to_string(), now_ms(), token.to_string());
        let script = redis::Script::new(FINALIZE_LUA);
        // FINALIZE_LUA returns 1 when the marker was written (this token owns the
        // claim) and 0 on a token MISMATCH — distinguish them so `complete` can
        // surface a stale token instead of a false success.
        let out: Result<i64, ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(jk)
                    .arg(outcome)
                    .arg(now)
                    .arg(token)
                    .invoke_async::<_, i64>(&mut conn)
                    .await
            })
            .await;
        match out {
            Ok(1) => Ok(FinalizeOutcome::Finalized),
            Ok(_) => Ok(FinalizeOutcome::StaleToken),
            Err(()) => Err(QueueError::Unavailable),
        }
    }

    async fn release(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        token: &str,
    ) -> Result<(), QueueError> {
        // The `module`/`host` args are advisory only — the module-lock + host-slot
        // keys are DERIVED inside the Lua from the job hash's OWN stored fields
        // (A1), so a wrong/stale caller arg can never free the wrong lock/slot.
        let _ = (module, host);
        // ONE atomic script — no external pre-read (dedupe/lock/host all derived
        // in-Lua from the hash), so a Redis-down release fails as a whole with the
        // lock/slot unchanged (the caller retries) rather than half-releasing.
        let (jk, zset, seq) = (job_key(job_id), zset_key(), seq_key());
        let (id, now, retain) = (job_id.to_string(), now_ms(), retain_secs());
        let rerun_id = uuid::Uuid::new_v4().simple().to_string();
        let (jp, dedupe_prefix, token) = (job_prefix(), dedupe_prefix(), token.to_string());
        let (lock_prefix, host_prefix) = (module_lock_prefix(), host_set_prefix());
        let script = redis::Script::new(&release_lua());
        let out: Result<(i64, String), ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(jk)
                    .key(zset)
                    .key(seq)
                    .arg(id)
                    .arg(now)
                    .arg(retain)
                    .arg(rerun_id)
                    .arg(jp)
                    .arg(dedupe_prefix)
                    .arg(token)
                    .arg(lock_prefix)
                    .arg(host_prefix)
                    .invoke_async::<_, (i64, String)>(&mut conn)
                    .await
            })
            .await;
        out.map(|_| ()).map_err(|()| QueueError::Unavailable)
    }

    async fn requeue(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        token: &str,
    ) -> Result<(), QueueError> {
        // `module`/`host` args are advisory only — the lock + host-slot keys are
        // DERIVED in-Lua from the job hash's OWN stored fields (A1).
        let _ = (module, host);
        let (jk, zset) = (job_key(job_id), zset_key());
        let (id, token) = (job_id.to_string(), token.to_string());
        let (lock_prefix, host_prefix) = (module_lock_prefix(), host_set_prefix());
        let script = redis::Script::new(&requeue_lua());
        let out: Result<i64, ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(jk)
                    .key(zset)
                    .arg(id)
                    .arg(token)
                    .arg(lock_prefix)
                    .arg(host_prefix)
                    .invoke_async::<_, i64>(&mut conn)
                    .await
            })
            .await;
        out.map(|_| ()).map_err(|()| QueueError::Unavailable)
    }

    async fn reconcile(
        &self,
        stale_after: std::time::Duration,
    ) -> Result<ReconcileReport, QueueError> {
        let now = now_ms();
        let stale_ms = stale_after.as_millis() as i64;
        let (lock_prefix, host_prefix, jp, dedupe_prefix, retain) = (
            module_lock_prefix(),
            host_set_prefix(),
            job_prefix(),
            dedupe_prefix(),
            retain_secs(),
        );
        let mut report = ReconcileReport::default();
        for host in [HostRole::Primary, HostRole::Heavy] {
            // Enumerate the host's in-flight ids (the reconcile candidates). A
            // failed read degrades the whole sweep — nothing was mutated.
            let hset = host_set_key(host);
            let ids: Result<Vec<String>, ()> = self
                .backend
                .with_conn(Namespace::Queue, |mut conn| async move {
                    redis::cmd("SMEMBERS")
                        .arg(hset)
                        .query_async::<_, Vec<String>>(&mut conn)
                        .await
                })
                .await;
            let ids = ids.map_err(|()| QueueError::Unavailable)?;
            for id in ids {
                let (jk, hset, zset, seq) =
                    (job_key(&id), host_set_key(host), zset_key(), seq_key());
                let (id_a, lp, hp, rerun_id) = (
                    id.clone(),
                    lock_prefix.clone(),
                    host_prefix.clone(),
                    uuid::Uuid::new_v4().simple().to_string(),
                );
                let (jp, dp) = (jp.clone(), dedupe_prefix.clone());
                // The module-lock, host-slot, and dedupe keys are all derived
                // in-Lua from the job hash's own fields via the prefix ARGVs (as
                // `release` does), so only the 4 fixed keys are passed.
                let script = redis::Script::new(&reconcile_lua());
                let out: Result<i64, ()> = self
                    .backend
                    .with_conn(Namespace::Queue, |mut conn| async move {
                        script
                            .key(jk)
                            .key(hset)
                            .key(zset)
                            .key(seq)
                            .arg(id_a)
                            .arg(now)
                            .arg(stale_ms)
                            .arg(lp)
                            .arg(retain)
                            .arg(rerun_id)
                            .arg(jp)
                            .arg(dp)
                            .arg(hp)
                            .invoke_async::<_, i64>(&mut conn)
                            .await
                    })
                    .await;
                match out {
                    Ok(1) => report.requeued.push(id),
                    Ok(2) => report.released.push(id),
                    Ok(_) => {}
                    Err(()) => return Err(QueueError::Unavailable),
                }
            }
        }
        Ok(report)
    }

    async fn snapshot(&self) -> Result<QueueSnapshot, QueueError> {
        let queued = self.peek(1024).await?;
        let mut leases = Vec::new();
        for host in [HostRole::Primary, HostRole::Heavy] {
            let (hset, jp) = (host_set_key(host), job_prefix());
            let script = redis::Script::new(LEASES_LUA);
            let out: Result<Vec<String>, ()> = self
                .backend
                .with_conn(Namespace::Queue, |mut conn| async move {
                    script
                        .key(hset)
                        .arg(jp)
                        .invoke_async::<_, Vec<String>>(&mut conn)
                        .await
                })
                .await;
            match out {
                Ok(flat) => {
                    for c in flat.chunks_exact(4) {
                        leases.push(Lease {
                            job_id: c[0].clone(),
                            module: c[1].clone(),
                            git_ref: c[2].clone(),
                            host,
                            started_at_ms: c[3].parse().unwrap_or(0),
                        });
                    }
                }
                Err(()) => return Err(QueueError::Unavailable),
            }
        }
        Ok(QueueSnapshot { queued, leases })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Offline, semantically-identical fake (tests only; pub(crate) so the scheduler
// tests share it). Each op takes the one Mutex, so it is atomic in exactly the
// way the Lua scripts are atomic server-side — which is what makes the
// concurrency tests meaningful without a live Redis.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Job {
        module: String,
        git_ref: String,
        prank: i64,
        heavy: bool,
        seq: i64,
        coalesced: i64,
        state: String, // held | queued | building | done | failed
        host: Option<HostRole>,
        started_at: i64,
        /// The fence token of the current claim (None when not building).
        claim_token: Option<String>,
        /// Durable terminal-outcome marker (set by `finalize`; drives reconcile's
        /// release-vs-rebuild decision).
        outcome: Option<String>,
        /// A single pending re-run requested while this job was building.
        rerun: bool,
    }

    #[derive(Default)]
    struct State {
        jobs: HashMap<String, Job>,
        dedupe: HashMap<String, String>,       // module@ref -> id
        module_lock: HashMap<String, String>,  // module -> id
        host_inflight: HashMap<&'static str, Vec<String>>,
        seq: i64,
        next_id: u64,
        next_token: u64,
        /// When set, every op behaves as an unreachable Redis (degradation test).
        down: bool,
        /// The next N `finalize` calls fail as Unavailable (completion-time outage).
        fail_finalizes: usize,
        /// The next N `release` calls fail as Unavailable (completion-time outage).
        fail_releases: usize,
    }

    /// An offline `QueueStore` mirroring the Lua semantics exactly.
    pub(crate) struct InMemoryQueue {
        state: Mutex<State>,
    }

    impl InMemoryQueue {
        pub(crate) fn new() -> Self {
            Self {
                state: Mutex::new(State::default()),
            }
        }

        /// Simulate Redis going down for the degradation test.
        pub(crate) fn set_down(&self, down: bool) {
            self.state.lock().unwrap().down = down;
        }

        /// Make the next `n` `release` calls fail as Unavailable (a completion-
        /// time outage AFTER finalize) so the retry / reconcile paths can be
        /// exercised.
        pub(crate) fn fail_releases(&self, n: usize) {
            self.state.lock().unwrap().fail_releases = n;
        }

        /// Make the next `n` `finalize` calls fail as Unavailable.
        pub(crate) fn fail_finalizes(&self, n: usize) {
            self.state.lock().unwrap().fail_finalizes = n;
        }

        /// Whether a job carries a durable terminal-outcome marker (test helper).
        pub(crate) fn has_outcome(&self, job_id: &str) -> bool {
            self.state
                .lock()
                .unwrap()
                .jobs
                .get(job_id)
                .map(|j| j.outcome.is_some())
                .unwrap_or(false)
        }

        /// Back-date a building job's claim so a reconcile with a positive lease
        /// treats it as stale (test helper — no need to sleep).
        pub(crate) fn backdate_started(&self, job_id: &str, ms_ago: i64) {
            let mut s = self.state.lock().unwrap();
            if let Some(j) = s.jobs.get_mut(job_id) {
                j.started_at = now_ms() - ms_ago;
            }
        }

        /// Current state of a job (test assertion helper).
        pub(crate) fn state_of(&self, job_id: &str) -> Option<String> {
            self.state.lock().unwrap().jobs.get(job_id).map(|j| j.state.clone())
        }

        /// How many jobs are in-flight on `host` (host-slot count — must never
        /// go negative or double-count on a double release).
        pub(crate) fn inflight_count(&self, host: HostRole) -> usize {
            self.state
                .lock()
                .unwrap()
                .host_inflight
                .get(host.as_str())
                .map(Vec::len)
                .unwrap_or(0)
        }

        /// Total number of jobs ever recorded (test assertion helper — proves
        /// a building-state coalesce creates NO second job).
        pub(crate) fn total_jobs(&self) -> usize {
            self.state.lock().unwrap().jobs.len()
        }

        /// How many times `module@ref` coalesced (test assertion helper).
        pub(crate) fn coalesced(&self, module: &str, git_ref: &str) -> i64 {
            let s = self.state.lock().unwrap();
            let dk = dedupe_id(module, git_ref);
            s.dedupe
                .get(&dk)
                .and_then(|id| s.jobs.get(id))
                .map(|j| j.coalesced)
                .unwrap_or(0)
        }
    }

    fn score(seq: i64, prank: i64) -> i64 {
        seq - prank * 1_000_000_000_000
    }

    /// The shared release body (mirrors `RELEASE_BODY` Lua): free the module lock
    /// + host slot — both derived from the job's OWN stored module/host (A1, never
    /// a caller arg) — clear the fence token, honor a re-run (else clear dedupe),
    /// set the terminal state. Assumes the caller holds the state lock.
    fn release_locked(s: &mut State, job_id: &str, final_state: &str) {
        let done = match s.jobs.get(job_id) {
            Some(j) => (
                j.module.clone(),
                j.git_ref.clone(),
                j.host,
                j.prank,
                j.heavy,
                j.rerun,
            ),
            None => return,
        };
        let (dmod, dref, dhost, dprank, dheavy, rerun) = done;
        // Derive the module lock + host slot from the STORED fields.
        if s.module_lock.get(&dmod).map(String::as_str) == Some(job_id) {
            s.module_lock.remove(&dmod);
        }
        if let Some(h) = dhost {
            if let Some(v) = s.host_inflight.get_mut(h.as_str()) {
                v.retain(|id| id != job_id);
            }
        }
        if let Some(j) = s.jobs.get_mut(job_id) {
            j.claim_token = None;
        }
        let dk = dedupe_id(&dmod, &dref);
        if rerun {
            // Re-enqueue EXACTLY one follow-up job for the same module@ref.
            s.seq += 1;
            let seq = s.seq;
            s.next_id += 1;
            let nid = format!("job-{}", s.next_id);
            s.jobs.insert(
                nid.clone(),
                Job {
                    module: dmod,
                    git_ref: dref,
                    prank: dprank,
                    heavy: dheavy,
                    seq,
                    coalesced: 1,
                    state: "queued".into(),
                    host: None,
                    started_at: 0,
                    claim_token: None,
                    outcome: None,
                    rerun: false,
                },
            );
            s.dedupe.insert(dk, nid);
        } else if s.dedupe.get(&dk).map(String::as_str) == Some(job_id) {
            s.dedupe.remove(&dk);
        }
        if let Some(j) = s.jobs.get_mut(job_id) {
            j.state = final_state.to_string();
        }
    }

    #[async_trait]
    impl QueueStore for InMemoryQueue {
        async fn enqueue(&self, req: &JobRequest) -> Result<Enqueued, QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            let dk = dedupe_id(&req.module, &req.git_ref);
            if let Some(existing) = s.dedupe.get(&dk).cloned() {
                let st = s.jobs.get(&existing).map(|j| j.state.clone());
                match st.as_deref() {
                    Some("queued") | Some("held") => {
                        let j = s.jobs.get_mut(&existing).unwrap();
                        j.coalesced += 1;
                        if req.priority.rank() > j.prank {
                            j.prank = req.priority.rank();
                        }
                        // Monotonic host-class upgrade (never downgrade heavy→small).
                        if req.heavy {
                            j.heavy = true;
                        }
                        if req.ready && j.state == "held" {
                            j.state = "queued".into();
                        }
                        return Ok(Enqueued {
                            job_id: existing,
                            created: false,
                        });
                    }
                    Some("building") => {
                        // Already building: never a second job. Only a READY=true
                        // request schedules a single idempotent re-run; a held
                        // (ready=false) arrival records intent but does NOT.
                        let j = s.jobs.get_mut(&existing).unwrap();
                        if req.ready {
                            j.rerun = true;
                        }
                        j.coalesced += 1;
                        if req.priority.rank() > j.prank {
                            j.prank = req.priority.rank();
                        }
                        if req.heavy {
                            j.heavy = true;
                        }
                        return Ok(Enqueued {
                            job_id: existing,
                            created: false,
                        });
                    }
                    _ => {}
                }
            }
            s.seq += 1;
            let seq = s.seq;
            s.next_id += 1;
            let id = format!("job-{}", s.next_id);
            s.jobs.insert(
                id.clone(),
                Job {
                    module: req.module.clone(),
                    git_ref: req.git_ref.clone(),
                    prank: req.priority.rank(),
                    heavy: req.heavy,
                    seq,
                    coalesced: 1,
                    state: if req.ready { "queued".into() } else { "held".into() },
                    host: None,
                    started_at: 0,
                    claim_token: None,
                    outcome: None,
                    rerun: false,
                },
            );
            s.dedupe.insert(dk, id.clone());
            Ok(Enqueued {
                job_id: id,
                created: true,
            })
        }

        async fn peek(&self, limit: usize) -> Result<Vec<QueuedJob>, QueueError> {
            let s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            let mut queued: Vec<(&String, &Job)> =
                s.jobs.iter().filter(|(_, j)| j.state == "queued").collect();
            queued.sort_by_key(|(_, j)| score(j.seq, j.prank));
            Ok(queued
                .into_iter()
                .take(limit)
                .map(|(id, j)| QueuedJob {
                    job_id: id.clone(),
                    module: j.module.clone(),
                    git_ref: j.git_ref.clone(),
                    priority: priority_from_rank(j.prank),
                    heavy: j.heavy,
                })
                .collect())
        }

        async fn claim(
            &self,
            job_id: &str,
            module: &str,
            host: HostRole,
            cap: u32,
        ) -> Result<ClaimOutcome, QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            // Derive the module from the job's OWN stored field; verify the arg.
            let stored_module = match s.jobs.get(job_id) {
                Some(j) if j.state == "queued" => j.module.clone(),
                Some(_) | None => return Ok(ClaimOutcome::NotQueued),
            };
            if !module.is_empty() && module != stored_module {
                return Ok(ClaimOutcome::Rejected);
            }
            if s.module_lock.contains_key(&stored_module) {
                return Ok(ClaimOutcome::ModuleBusy);
            }
            let live = s.host_inflight.get(host.as_str()).map(Vec::len).unwrap_or(0);
            if live as u32 >= cap.max(1) {
                return Ok(ClaimOutcome::HostFull);
            }
            s.next_token += 1;
            let token = format!("tok-{}", s.next_token);
            {
                let j = s.jobs.get_mut(job_id).unwrap();
                j.state = "building".into();
                j.host = Some(host);
                j.started_at = now_ms();
                j.claim_token = Some(token.clone());
            }
            s.module_lock.insert(stored_module, job_id.to_string());
            s.host_inflight
                .entry(host.as_str())
                .or_default()
                .push(job_id.to_string());
            Ok(ClaimOutcome::Claimed { token })
        }

        async fn finalize(
            &self,
            job_id: &str,
            state: JobState,
            token: &str,
        ) -> Result<FinalizeOutcome, QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            if s.fail_finalizes > 0 {
                s.fail_finalizes -= 1;
                return Err(QueueError::Unavailable);
            }
            // FENCE: only the current claim's worker may mark the outcome. A
            // mismatch is a distinct, surfaced outcome (never a false success).
            match s.jobs.get(job_id).and_then(|j| j.claim_token.clone()) {
                Some(t) if t == token => {}
                _ => return Ok(FinalizeOutcome::StaleToken),
            }
            if let Some(j) = s.jobs.get_mut(job_id) {
                j.outcome = Some(state.as_str().to_string());
            }
            Ok(FinalizeOutcome::Finalized)
        }

        async fn release(
            &self,
            job_id: &str,
            module: &str,
            host: HostRole,
            token: &str,
        ) -> Result<(), QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            // Simulate a completion-time outage: the whole op fails, NOTHING is
            // released (mirrors the atomic Lua failing as a whole).
            if s.fail_releases > 0 {
                s.fail_releases -= 1;
                return Err(QueueError::Unavailable);
            }
            // FENCE: only the current claim token may release; a mismatch (already
            // released, or reconciled + re-claimed) is a safe no-op.
            match s.jobs.get(job_id).and_then(|j| j.claim_token.clone()) {
                Some(t) if t == token => {}
                _ => return Ok(()),
            }
            // `module`/`host` args are advisory — release derives keys from the
            // job's OWN stored fields (A1).
            let _ = (module, host);
            let final_state = s
                .jobs
                .get(job_id)
                .and_then(|j| j.outcome.clone())
                .unwrap_or_else(|| "done".into());
            release_locked(&mut s, job_id, &final_state);
            Ok(())
        }

        async fn requeue(
            &self,
            job_id: &str,
            module: &str,
            host: HostRole,
            token: &str,
        ) -> Result<(), QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            // FENCE: only the current claim token may requeue; a mismatch (already
            // released, or reconciled + re-claimed) is a safe no-op.
            match s.jobs.get(job_id).and_then(|j| j.claim_token.clone()) {
                Some(t) if t == token => {}
                _ => return Ok(()),
            }
            // `module`/`host` args are advisory — derive from the job's OWN fields.
            let _ = (module, host);
            let stored_module = s.jobs.get(job_id).map(|j| j.module.clone());
            if let Some(m) = stored_module {
                if s.module_lock.get(&m).map(String::as_str) == Some(job_id) {
                    s.module_lock.remove(&m);
                }
            }
            for v in s.host_inflight.values_mut() {
                v.retain(|x| x != job_id);
            }
            if let Some(j) = s.jobs.get_mut(job_id) {
                j.state = "queued".into();
                j.host = None;
                j.started_at = 0;
                j.claim_token = None; // fence: a late completion no-ops
            }
            Ok(())
        }

        async fn reconcile(
            &self,
            stale_after: std::time::Duration,
        ) -> Result<ReconcileReport, QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            let now = now_ms();
            let stale_ms = stale_after.as_millis() as i64;
            // Snapshot the building candidates + whether each finished (marker).
            let building: Vec<(String, String, Option<HostRole>, Option<String>, i64)> = s
                .jobs
                .iter()
                .filter(|(_, j)| j.state == "building")
                .map(|(id, j)| {
                    (
                        id.clone(),
                        j.module.clone(),
                        j.host,
                        j.outcome.clone(),
                        j.started_at,
                    )
                })
                .collect();
            let mut report = ReconcileReport::default();
            for (id, module, _host, outcome, started) in building {
                if let Some(outcome) = outcome {
                    // FINISHED but not released → release only, NO rebuild.
                    release_locked(&mut s, &id, &outcome);
                    report.released.push(id);
                } else if (now - started) >= stale_ms {
                    // Crashed mid-build → requeue.
                    if s.module_lock.get(&module).map(String::as_str) == Some(id.as_str()) {
                        s.module_lock.remove(&module);
                    }
                    for v in s.host_inflight.values_mut() {
                        v.retain(|x| x != &id);
                    }
                    if let Some(j) = s.jobs.get_mut(&id) {
                        j.state = "queued".into();
                        j.host = None;
                        j.claim_token = None; // fence: a late completion no-ops
                    }
                    report.requeued.push(id);
                }
            }
            Ok(report)
        }

        async fn snapshot(&self) -> Result<QueueSnapshot, QueueError> {
            let queued = self.peek(1024).await?;
            let s = self.state.lock().unwrap();
            let mut leases = Vec::new();
            for (id, j) in s.jobs.iter().filter(|(_, j)| j.state == "building") {
                leases.push(Lease {
                    job_id: id.clone(),
                    module: j.module.clone(),
                    git_ref: j.git_ref.clone(),
                    host: j.host.unwrap_or(HostRole::Primary),
                    started_at_ms: j.started_at,
                });
            }
            Ok(QueueSnapshot { queued, leases })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::InMemoryQueue;
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn req(module: &str, git_ref: &str, prio: Priority, heavy: bool) -> JobRequest {
        JobRequest {
            module: module.into(),
            git_ref: git_ref.into(),
            priority: prio,
            heavy,
            ready: true,
        }
    }

    /// Assert a claim succeeded and return its fence token.
    async fn claim_ok(q: &InMemoryQueue, id: &str, module: &str, host: HostRole, cap: u32) -> String {
        match q.claim(id, module, host, cap).await.unwrap() {
            ClaimOutcome::Claimed { token } => token,
            other => panic!("expected Claimed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn same_module_ref_coalesces_into_one_job() {
        let q = InMemoryQueue::new();
        let a = q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap();
        let b = q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap();
        assert!(a.created);
        assert!(!b.created, "second readiness must coalesce, not create");
        assert_eq!(a.job_id, b.job_id);
        assert_eq!(q.peek(10).await.unwrap().len(), 1, "one coalesced job queued");
        assert_eq!(q.coalesced("chord", "abc"), 2, "both readiness signals counted");
    }

    #[test]
    fn dedupe_identity_is_collision_free_across_at_signs() {
        // Fix 1: raw `{module}@{ref}` aliased distinct pairs. The length-prefixed
        // identity keeps them distinct.
        assert_ne!(dedupe_id("a", "b@c"), dedupe_id("a@b", "c"));
        assert_ne!(dedupe_key("a", "b@c"), dedupe_key("a@b", "c"));
        // Same pair → identical (coalescing still works).
        assert_eq!(dedupe_id("chord", "abc"), dedupe_id("chord", "abc"));
        // A `:` in a component can't alias either (length prefix disambiguates).
        assert_ne!(dedupe_id("a", "b:c"), dedupe_id("a:b", "c"));
    }

    #[tokio::test]
    async fn colliding_pairs_do_not_falsely_coalesce_but_same_pair_still_does() {
        // Fix 1 (behavioral): (a, b@c) and (a@b, c) — which aliased under the raw
        // `@` concat — must NOT coalesce; a genuine same-pair reuse still does.
        let q = InMemoryQueue::new();
        let x = q.enqueue(&req("a", "b@c", Priority::Normal, false)).await.unwrap();
        let y = q.enqueue(&req("a@b", "c", Priority::Normal, false)).await.unwrap();
        assert!(x.created && y.created, "distinct pairs create distinct jobs");
        assert_ne!(x.job_id, y.job_id);
        assert_eq!(q.peek(10).await.unwrap().len(), 2, "no false coalesce (would drop a build)");
        // A real same-pair reuse coalesces onto the existing job.
        let z = q.enqueue(&req("a", "b@c", Priority::Normal, false)).await.unwrap();
        assert!(!z.created && z.job_id == x.job_id);
        assert_eq!(q.peek(10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn concurrent_enqueue_of_same_ref_yields_one_job() {
        let q = Arc::new(InMemoryQueue::new());
        let mut handles = Vec::new();
        for _ in 0..24 {
            let q = q.clone();
            handles.push(tokio::spawn(async move {
                q.enqueue(&req("terminus", "deadbeef", Priority::Normal, false))
                    .await
                    .unwrap()
                    .job_id
            }));
        }
        let mut ids = std::collections::HashSet::new();
        for h in handles {
            ids.insert(h.await.unwrap());
        }
        assert_eq!(ids.len(), 1, "all concurrent readiness coalesces to one id");
        assert_eq!(q.peek(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn priority_then_fifo_ordering() {
        let q = InMemoryQueue::new();
        q.enqueue(&req("a", "1", Priority::Normal, false)).await.unwrap();
        q.enqueue(&req("b", "1", Priority::Normal, false)).await.unwrap();
        q.enqueue(&req("c", "1", Priority::High, false)).await.unwrap();
        let order: Vec<String> = q
            .peek(10)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.module)
            .collect();
        // High first, then the two normals in FIFO order.
        assert_eq!(order, vec!["c", "a", "b"]);
    }

    #[tokio::test]
    async fn same_module_serializes_via_module_lock() {
        let q = InMemoryQueue::new();
        let j1 = q.enqueue(&req("chord", "r1", Priority::Normal, false)).await.unwrap();
        let j2 = q.enqueue(&req("chord", "r2", Priority::Normal, false)).await.unwrap();
        // First claim of module `chord` succeeds; a second (different ref, same
        // module) is refused while the first builds — graceful serialization.
        let tok1 = claim_ok(&q, &j1.job_id, "chord", HostRole::Primary, 4).await;
        assert_eq!(
            q.claim(&j2.job_id, "chord", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::ModuleBusy
        );
        // Once the first finishes, the module lock frees and the second claims.
        q.complete(&j1.job_id, "chord", HostRole::Primary, JobState::Done, &tok1).await.unwrap();
        claim_ok(&q, &j2.job_id, "chord", HostRole::Primary, 4).await;
    }

    #[tokio::test]
    async fn host_cap_bounds_concurrency() {
        let q = InMemoryQueue::new();
        let j1 = q.enqueue(&req("m1", "r", Priority::Normal, false)).await.unwrap();
        let j2 = q.enqueue(&req("m2", "r", Priority::Normal, false)).await.unwrap();
        // cap=1 on primary: first claim ok, second (different module) host-full.
        claim_ok(&q, &j1.job_id, "m1", HostRole::Primary, 1).await;
        assert_eq!(
            q.claim(&j2.job_id, "m2", HostRole::Primary, 1).await.unwrap(),
            ClaimOutcome::HostFull
        );
    }

    #[tokio::test]
    async fn concurrent_claim_of_one_job_admits_exactly_one() {
        let q = Arc::new(InMemoryQueue::new());
        let j = q.enqueue(&req("m", "r", Priority::Normal, false)).await.unwrap();
        let mut handles = Vec::new();
        for _ in 0..16 {
            let q = q.clone();
            let id = j.job_id.clone();
            handles.push(tokio::spawn(async move {
                matches!(
                    q.claim(&id, "m", HostRole::Primary, 8).await.unwrap(),
                    ClaimOutcome::Claimed { .. }
                )
            }));
        }
        let mut claimed = 0;
        for h in handles {
            if h.await.unwrap() {
                claimed += 1;
            }
        }
        assert_eq!(claimed, 1, "exactly one racer may claim a single job");
    }

    #[tokio::test]
    async fn held_then_ready_promotes() {
        let q = InMemoryQueue::new();
        let held = JobRequest {
            ready: false,
            ..req("m", "r", Priority::Normal, false)
        };
        let a = q.enqueue(&held).await.unwrap();
        assert!(a.created);
        assert_eq!(q.peek(10).await.unwrap().len(), 0, "held job is not dispatchable");
        // A later ready request for the same module@ref promotes it.
        let b = q.enqueue(&req("m", "r", Priority::Normal, false)).await.unwrap();
        assert_eq!(a.job_id, b.job_id);
        assert_eq!(q.peek(10).await.unwrap().len(), 1, "promoted to dispatchable");
    }

    #[tokio::test]
    async fn enqueue_while_building_queues_one_rerun_never_a_second_job() {
        let q = InMemoryQueue::new();
        let a = q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap();
        // Dispatch it (now building).
        let tok = claim_ok(&q, &a.job_id, "chord", HostRole::Primary, 4).await;
        // Two agents mark the SAME module@ref ready WHILE it's building. Neither
        // creates a second job; both coalesce onto the building job as a single
        // pending re-run.
        let b = q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap();
        let c = q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap();
        assert!(!b.created && !c.created);
        assert_eq!(b.job_id, a.job_id);
        assert_eq!(c.job_id, a.job_id);
        assert_eq!(q.total_jobs(), 1, "no second job while one is building");
        assert_eq!(q.peek(10).await.unwrap().len(), 0, "nothing dispatchable yet");
        // On completion, EXACTLY ONE follow-up build is re-enqueued (not two).
        q.complete(&a.job_id, "chord", HostRole::Primary, JobState::Done, &tok).await.unwrap();
        let queued = q.peek(10).await.unwrap();
        assert_eq!(queued.len(), 1, "exactly one coalesced re-run queued");
        assert_ne!(queued[0].job_id, a.job_id, "the re-run is a fresh job");
        assert_eq!((queued[0].module.as_str(), queued[0].git_ref.as_str()), ("chord", "abc"));
        // Completing the re-run (no new readiness) leaves the queue empty — the
        // re-run pile is bounded, never unbounded.
        let rid = queued[0].job_id.clone();
        let rtok = claim_ok(&q, &rid, "chord", HostRole::Primary, 4).await;
        q.complete(&rid, "chord", HostRole::Primary, JobState::Done, &rtok).await.unwrap();
        assert_eq!(q.peek(10).await.unwrap().len(), 0);
        assert_eq!(q.total_jobs(), 2, "one original + exactly one re-run");
    }

    #[tokio::test]
    async fn coalesce_upgrades_host_class_to_heavy_monotonically() {
        let q = InMemoryQueue::new();
        // First recorded as a small/primary build.
        let a = q.enqueue(&req("harmony", "r", Priority::Normal, false)).await.unwrap();
        assert!(!q.peek(10).await.unwrap()[0].heavy);
        // A later heavy request for the same module@ref upgrades it to heavy.
        let b = q.enqueue(&req("harmony", "r", Priority::Normal, true)).await.unwrap();
        assert_eq!(a.job_id, b.job_id);
        assert!(q.peek(10).await.unwrap()[0].heavy, "coalesce upgraded to heavy");
        // A subsequent small request must NOT downgrade it (monotonic).
        q.enqueue(&req("harmony", "r", Priority::Normal, false)).await.unwrap();
        assert!(q.peek(10).await.unwrap()[0].heavy, "heavy is never downgraded");
    }

    #[tokio::test]
    async fn complete_releases_module_lock_and_host_slot_atomically() {
        let q = InMemoryQueue::new();
        // j1 (module m1) claimed on primary cap=1: holds the module lock AND the
        // one primary host slot.
        let j1 = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let j2 = q.enqueue(&req("m2", "r2", Priority::Normal, false)).await.unwrap();
        let j1b = q.enqueue(&req("m1", "rB", Priority::Normal, false)).await.unwrap();
        let tok1 = claim_ok(&q, &j1.job_id, "m1", HostRole::Primary, 1).await;
        // While j1 builds: a different module is host-capped, and same module is locked.
        assert_eq!(
            q.claim(&j2.job_id, "m2", HostRole::Primary, 1).await.unwrap(),
            ClaimOutcome::HostFull
        );
        assert_eq!(
            q.claim(&j1b.job_id, "m1", HostRole::Primary, 1).await.unwrap(),
            ClaimOutcome::ModuleBusy
        );
        // One complete releases BOTH the host slot and the module lock atomically.
        q.complete(&j1.job_id, "m1", HostRole::Primary, JobState::Done, &tok1).await.unwrap();
        assert_eq!(q.inflight_count(HostRole::Primary), 0, "host slot count freed");
        let tok2 = claim_ok(&q, &j2.job_id, "m2", HostRole::Primary, 1).await;
        // Free m2's slot, then the same-module job can claim (module lock was freed).
        q.complete(&j2.job_id, "m2", HostRole::Primary, JobState::Done, &tok2).await.unwrap();
        claim_ok(&q, &j1b.job_id, "m1", HostRole::Primary, 1).await;
    }

    #[tokio::test]
    async fn complete_on_redis_down_does_not_half_release_the_lock() {
        let q = InMemoryQueue::new();
        let j1 = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let j1b = q.enqueue(&req("m1", "rB", Priority::Normal, false)).await.unwrap();
        let tok1 = claim_ok(&q, &j1.job_id, "m1", HostRole::Primary, 1).await;
        // Redis goes down at release time → the LOW-LEVEL release fails as a whole;
        // NOTHING is partially released (the module lock is still held). (Uses the
        // low-level `release` so we test the single-shot atomicity, not the
        // retrying `complete`.)
        q.finalize(&j1.job_id, JobState::Done, &tok1).await.unwrap();
        q.set_down(true);
        assert_eq!(
            q.release(&j1.job_id, "m1", HostRole::Primary, &tok1).await,
            Err(QueueError::Unavailable)
        );
        q.set_down(false);
        assert_eq!(
            q.claim(&j1b.job_id, "m1", HostRole::Primary, 1).await.unwrap(),
            ClaimOutcome::ModuleBusy,
            "a failed (whole) release left the lock intact — no half-release wedge"
        );
        // A later successful release cleanly frees, and the retry claims.
        q.release(&j1.job_id, "m1", HostRole::Primary, &tok1).await.unwrap();
        claim_ok(&q, &j1b.job_id, "m1", HostRole::Primary, 1).await;
    }

    #[tokio::test]
    async fn release_with_wrong_module_and_host_args_still_frees_the_correct_lock_and_slot() {
        // A1: release/complete derive the lock+host keys from the job's OWN stored
        // fields, so a wrong/stale caller arg still frees the CORRECT ones.
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let contender = q.enqueue(&req("m1", "r2", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        assert_eq!(q.inflight_count(HostRole::Primary), 1);
        q.finalize(&j.job_id, JobState::Done, &tok).await.unwrap();
        // Release with a WRONG module ("bogus") and WRONG host (Heavy). Because the
        // keys are derived from the hash, this still frees m1's lock + the primary
        // slot — and does NOT touch the heavy slot.
        q.release(&j.job_id, "bogus", HostRole::Heavy, &tok).await.unwrap();
        assert_eq!(q.inflight_count(HostRole::Primary), 0, "correct (primary) slot freed");
        assert_eq!(q.inflight_count(HostRole::Heavy), 0, "wrong (heavy) slot untouched");
        // The m1 module lock was freed → the same-module contender can now claim.
        assert!(matches!(
            q.claim(&contender.job_id, "m1", HostRole::Primary, 1).await.unwrap(),
            ClaimOutcome::Claimed { .. }
        ));
    }

    #[tokio::test]
    async fn requeue_returns_a_claimed_job_to_queued_and_frees_lock_and_slot() {
        // BLD-11: a token-fenced requeue puts a claimed heavy job back to `queued`,
        // frees its module lock + host slot, records NO terminal outcome, and lets a
        // later claim pick it up.
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("harmony", "big", Priority::Normal, true)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "harmony", HostRole::Heavy, 1).await;
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("building"));
        assert_eq!(q.inflight_count(HostRole::Heavy), 1);
        // Requeue (advisory wrong module/host args still derive the correct keys).
        q.requeue(&j.job_id, "bogus", HostRole::Primary, &tok).await.unwrap();
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("queued"), "back to queued");
        assert!(!q.has_outcome(&j.job_id), "no terminal outcome recorded");
        assert_eq!(q.inflight_count(HostRole::Heavy), 0, "host slot freed");
        // The module lock + host slot were freed and the token cleared → the requeued
        // job is fully re-claimable (a claim re-takes the module lock, which proves it
        // was released), and the OLD token no longer owns anything.
        let tok2 = claim_ok(&q, &j.job_id, "harmony", HostRole::Heavy, 1).await;
        assert_ne!(tok, tok2, "a fresh claim mints a new fence token");
    }

    #[tokio::test]
    async fn requeue_with_a_stale_token_is_a_safe_noop() {
        // A wrong/stale token must not requeue (it would clobber a live re-claim).
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        // A stale token no-ops: the job stays building, the slot stays held.
        q.requeue(&j.job_id, "m1", HostRole::Primary, "wrong-token").await.unwrap();
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("building"));
        assert_eq!(q.inflight_count(HostRole::Primary), 1);
        // The correct token still requeues.
        q.requeue(&j.job_id, "m1", HostRole::Primary, &tok).await.unwrap();
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("queued"));
    }

    #[tokio::test]
    async fn claim_with_mismatched_module_arg_is_rejected_and_takes_no_foreign_lock() {
        // A2: a claim whose module arg disagrees with the job's stored module is
        // REFUSED — it must never take a foreign module lock.
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        assert_eq!(
            q.claim(&j.job_id, "not-m1", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::Rejected,
            "a mismatched module arg is refused"
        );
        // It took no lock under the wrong name, and the job is still queued: a
        // correct claim still succeeds and same-module serialization holds.
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("queued"));
        let j2 = q.enqueue(&req("m1", "r2", Priority::Normal, false)).await.unwrap();
        claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 4).await;
        assert_eq!(
            q.claim(&j2.job_id, "m1", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::ModuleBusy,
            "the correct module lock still serializes same-module builds"
        );
    }

    #[tokio::test]
    async fn direct_caller_complete_self_heals_across_a_release_outage() {
        // A3/B1: the retrying `complete` (sanctioned direct-caller entry) rides out
        // a brief release outage and frees the slot without the caller retrying.
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        q.fail_releases(2); // first two releases fail, then succeed
        q.complete(&j.job_id, "m1", HostRole::Primary, JobState::Done, &tok).await.unwrap();
        assert_eq!(q.inflight_count(HostRole::Primary), 0, "self-healed: slot freed");
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn reconcile_requeues_a_stale_building_job() {
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        assert_eq!(q.inflight_count(HostRole::Primary), 1);
        // Make the claim look old (crashed/hung worker), then reconcile.
        q.backdate_started(&j.job_id, 60_000);
        let report = q.reconcile(Duration::from_secs(1)).await.unwrap();
        assert_eq!(report.requeued, vec![j.job_id.clone()]);
        assert!(report.released.is_empty(), "a crashed build is requeued, not 'released'");
        // Module lock + host slot freed; the job is dispatchable again.
        assert_eq!(q.inflight_count(HostRole::Primary), 0);
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("queued"));
        assert_eq!(q.peek(10).await.unwrap().len(), 1);
        // A fresh claim of the SAME module succeeds (the wedged lock was freed).
        claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
    }

    #[tokio::test]
    async fn reconcile_leaves_a_fresh_build_alone() {
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        // Just claimed → not stale under a 1h lease → untouched.
        let report = q.reconcile(Duration::from_secs(3600)).await.unwrap();
        assert!(report.requeued.is_empty() && report.released.is_empty());
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("building"));
        assert_eq!(q.inflight_count(HostRole::Primary), 1);
    }

    #[tokio::test]
    async fn reconcile_releases_a_finished_but_unreleased_job_without_rebuild() {
        // C: a worker FINISHED (finalize wrote the marker) but its release never
        // landed (Redis outage). Reconcile must RELEASE it — never requeue/rebuild.
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        // The build finished: the worker durably records the terminal outcome...
        q.finalize(&j.job_id, JobState::Done, &tok).await.unwrap();
        assert!(q.has_outcome(&j.job_id));
        // ...but the release is stuck (make it look old, as if retries were spent).
        q.backdate_started(&j.job_id, 60_000);
        let report = q.reconcile(Duration::from_secs(1)).await.unwrap();
        assert_eq!(report.released, vec![j.job_id.clone()], "finished job is released");
        assert!(report.requeued.is_empty(), "a FINISHED job must NEVER be requeued/rebuilt");
        // Lock + slot freed, terminal state recorded, nothing re-dispatchable.
        assert_eq!(q.inflight_count(HostRole::Primary), 0);
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("done"));
        assert_eq!(q.peek(10).await.unwrap().len(), 0, "no rebuild was queued");
    }

    #[tokio::test]
    async fn ready_false_while_building_does_not_schedule_a_rerun() {
        // A: a held (ready=false) arrival during a build must NOT become a
        // dispatchable re-run; only a later ready=true schedules one.
        let q = InMemoryQueue::new();
        let a = q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &a.job_id, "chord", HostRole::Primary, 1).await;
        // ready=false while building → coalesces intent, but NO rerun.
        let held = JobRequest { ready: false, ..req("chord", "abc", Priority::Normal, false) };
        let b = q.enqueue(&held).await.unwrap();
        assert_eq!(b.job_id, a.job_id);
        assert!(!b.created);
        q.complete(&a.job_id, "chord", HostRole::Primary, JobState::Done, &tok).await.unwrap();
        assert_eq!(q.peek(10).await.unwrap().len(), 0, "ready=false must NOT schedule a re-run");

        // Now a ready=true while building DOES schedule exactly one re-run.
        let c = q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap();
        let tok2 = claim_ok(&q, &c.job_id, "chord", HostRole::Primary, 1).await;
        q.enqueue(&JobRequest { ready: false, ..req("chord", "abc", Priority::Normal, false) })
            .await
            .unwrap();
        q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap(); // ready=true
        q.complete(&c.job_id, "chord", HostRole::Primary, JobState::Done, &tok2).await.unwrap();
        assert_eq!(q.peek(10).await.unwrap().len(), 1, "a ready=true arrival schedules one re-run");
    }

    #[tokio::test]
    async fn wrong_token_completion_surfaces_stale_never_false_success() {
        // Fix 1: a complete()/finalize() with a WRONG token must NOT report Ok —
        // it must surface a non-success (StaleToken) so a direct caller cannot
        // mask a build that is neither finished nor released.
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;

        // finalize with a wrong token → StaleToken (no marker written).
        assert_eq!(
            q.finalize(&j.job_id, JobState::Done, "wrong-token").await.unwrap(),
            FinalizeOutcome::StaleToken
        );
        // complete with a wrong token → Err(StaleToken), NOT Ok(()).
        assert_eq!(
            q.complete(&j.job_id, "m1", HostRole::Primary, JobState::Done, "wrong-token").await,
            Err(QueueError::StaleToken)
        );
        // The build is genuinely UNFINISHED — not masked as complete/released.
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("building"));
        assert!(!q.has_outcome(&j.job_id), "no outcome marker for a wrong token");
        assert_eq!(q.inflight_count(HostRole::Primary), 1, "slot still held");

        // The correct token finalizes, and complete() is idempotent across an
        // in-flight release outage (retries the SAME correct token to success).
        assert_eq!(
            q.finalize(&j.job_id, JobState::Done, &tok).await.unwrap(),
            FinalizeOutcome::Finalized
        );
        q.fail_releases(2);
        q.complete(&j.job_id, "m1", HostRole::Primary, JobState::Done, &tok).await.unwrap();
        assert_eq!(q.inflight_count(HostRole::Primary), 0, "correct token self-heals + releases");
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn stale_completion_and_double_release_are_idempotent_no_slot_underflow() {
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let stale_tok = claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        // Reconcile it away (as if the worker crashed) and let a fresh worker reclaim.
        q.backdate_started(&j.job_id, 60_000);
        q.reconcile(Duration::from_secs(1)).await.unwrap();
        let new_tok = claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        assert_ne!(stale_tok, new_tok);
        assert_eq!(q.inflight_count(HostRole::Primary), 1);
        // The crashed worker's LATE completion (old token) surfaces StaleToken —
        // it must NOT free the new claim's host slot or module lock.
        assert_eq!(
            q.complete(&j.job_id, "m1", HostRole::Primary, JobState::Done, &stale_tok).await,
            Err(QueueError::StaleToken)
        );
        assert_eq!(
            q.inflight_count(HostRole::Primary),
            1,
            "stale completion must not free the re-claimed job's slot"
        );
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("building"));
        // The rightful completion releases exactly once...
        q.complete(&j.job_id, "m1", HostRole::Primary, JobState::Done, &new_tok).await.unwrap();
        assert_eq!(q.inflight_count(HostRole::Primary), 0);
        // ...and a DUPLICATE of it (token now cleared) surfaces StaleToken and does
        // NOT underflow / double-free the host-slot count.
        assert_eq!(
            q.complete(&j.job_id, "m1", HostRole::Primary, JobState::Done, &new_tok).await,
            Err(QueueError::StaleToken)
        );
        assert_eq!(
            q.inflight_count(HostRole::Primary),
            0,
            "double release must not underflow the host-slot count"
        );
    }

    #[tokio::test]
    async fn redis_down_degrades_to_unavailable() {
        let q = InMemoryQueue::new();
        q.set_down(true);
        assert_eq!(
            q.enqueue(&req("m", "r", Priority::Normal, false)).await,
            Err(QueueError::Unavailable)
        );
        assert_eq!(q.peek(10).await, Err(QueueError::Unavailable));
        assert_eq!(
            q.claim("x", "m", HostRole::Primary, 1).await,
            Err(QueueError::Unavailable)
        );
    }

    #[test]
    fn priority_parse_and_rank() {
        assert_eq!(Priority::parse("HIGH"), Priority::High);
        assert_eq!(Priority::parse("low"), Priority::Low);
        assert_eq!(Priority::parse("weird"), Priority::Normal);
        assert!(Priority::High.rank() > Priority::Normal.rank());
        assert!(Priority::Normal.rank() > Priority::Low.rank());
    }

    // ── F: real-Redis Lua contract test ────────────────────────────────────
    // Exercises the ACTUAL Lua scripts against an EPHEMERAL redis-server bound to
    // loopback on an unused port (flushed, torn down after). Auto-SKIPS cleanly
    // when `redis-server` is not installed (e.g. a Redis-less CI/dev box) rather
    // than failing. NEVER touches the prod Redis. The loopback literal here is
    // test-only harness wiring, not production infra config.

    struct EphemeralRedis {
        child: std::process::Child,
        port: u16,
    }
    impl EphemeralRedis {
        fn start() -> Option<Self> {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .ok()?
                .local_addr()
                .ok()?
                .port();
            let child = std::process::Command::new("redis-server")
                .args([
                    "--port",
                    &port.to_string(),
                    "--bind",
                    "127.0.0.1",
                    "--save",
                    "",
                    "--appendonly",
                    "no",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?; // binary missing ⇒ Err ⇒ None ⇒ clean skip
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(Self { child, port });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let mut c = child;
            let _ = c.kill();
            None
        }
        fn url(&self) -> String {
            format!("redis://127.0.0.1:{}", self.port)
        }
    }
    impl Drop for EphemeralRedis {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    async fn raw<T: redis::FromRedisValue>(b: &RedisBackend, cmd: &str, args: &[String]) -> T {
        let (cmd, args) = (cmd.to_string(), args.to_vec());
        b.with_conn(Namespace::Queue, |mut conn| async move {
            let mut c = redis::cmd(&cmd);
            for a in &args {
                c.arg(a);
            }
            c.query_async::<_, T>(&mut conn).await
        })
        .await
        .expect("raw redis cmd")
    }

    #[tokio::test]
    async fn redis_lua_contract_against_ephemeral_server() {
        let Some(server) = EphemeralRedis::start() else {
            eprintln!("SKIP redis_lua_contract: redis-server not installed");
            return;
        };
        let backend =
            RedisBackend::build(&server.url(), None, 0, 1, Duration::from_millis(500)).unwrap();
        assert!(backend.ping().await, "ephemeral redis must answer PING");
        let _: () = raw(&backend, "FLUSHALL", &[]).await;
        let q = RedisQueue::new(backend.clone());

        let mk = |m: &str, r: &str, p: Priority, heavy: bool, ready: bool| JobRequest {
            module: m.into(),
            git_ref: r.into(),
            priority: p,
            heavy,
            ready,
        };

        // 1) Dedupe/coalesce + monotonic priority bump (real ENQUEUE_LUA).
        let a = q.enqueue(&mk("chord", "abc", Priority::Normal, false, true)).await.unwrap();
        let b = q.enqueue(&mk("chord", "abc", Priority::High, false, true)).await.unwrap();
        assert!(a.created && !b.created && a.job_id == b.job_id);
        let queued = q.peek(10).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].priority, Priority::High, "priority bumped in-place");

        // 2) Keys live under the Namespace::Queue prefix in the durable DB 0.
        let keys: Vec<String> = raw(&backend, "KEYS", &["queue:*".into()]).await;
        assert!(!keys.is_empty() && keys.iter().all(|k| k.starts_with("queue:")));

        // 2b) DURABILITY (A4): the durable DB the queue relies on must be
        // `noeviction` so a queued/in-flight build is never evicted under memory
        // pressure. Prove it against the live server (the deploy sets this), not
        // just via namespace routing.
        let policy: Vec<String> =
            raw(&backend, "CONFIG", &["GET".into(), "maxmemory-policy".into()]).await;
        assert_eq!(policy.get(1).map(String::as_str), Some("noeviction"),
            "the durable queue DB must run under a noeviction maxmemory-policy");
        // And confirm the queue keeps working under that policy (a queued job
        // survives) — the reliance is real, not incidental.
        let _: () = raw(&backend, "CONFIG", &["SET".into(), "maxmemory-policy".into(), "noeviction".into()]).await;
        assert_eq!(q.peek(10).await.unwrap().len(), 1, "queued job persists under noeviction");

        // 3) Claim writes a fence token + module lock; a 2nd claim is ModuleBusy.
        let tok = match q.claim(&a.job_id, "chord", HostRole::Primary, 1).await.unwrap() {
            ClaimOutcome::Claimed { token } => token,
            o => panic!("{o:?}"),
        };
        let j2 = q.enqueue(&mk("chord", "z", Priority::Normal, false, true)).await.unwrap();
        assert_eq!(
            q.claim(&j2.job_id, "chord", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::ModuleBusy
        );

        // 4) ready=true while building → exactly one re-run after finalize+release.
        q.enqueue(&mk("chord", "abc", Priority::Normal, false, true)).await.unwrap();
        q.finalize(&a.job_id, JobState::Done, &tok).await.unwrap();
        q.release(&a.job_id, "chord", HostRole::Primary, &tok).await.unwrap();
        let after = q.peek(10).await.unwrap();
        let rerun = after.iter().find(|j| j.module == "chord" && j.git_ref == "abc").unwrap();
        assert_ne!(rerun.job_id, a.job_id, "re-run is a fresh job");
        // Rust-built and Lua-derived dedupe keys must AGREE on the collision-free
        // encoding: a fresh enqueue of the same pair coalesces onto the Lua-written
        // rerun pointer (rather than creating a 2nd job).
        let recoalesce = q.enqueue(&mk("chord", "abc", Priority::Normal, false, true)).await.unwrap();
        assert!(!recoalesce.created && recoalesce.job_id == rerun.job_id,
            "Rust dedupe_key must match the release-Lua's derived dedupe key");

        // 5) reconcile: a CRASHED build (no outcome, stale) is requeued.
        let rtok = match q.claim(&rerun.job_id, "chord", HostRole::Primary, 1).await.unwrap() {
            ClaimOutcome::Claimed { token } => token,
            o => panic!("{o:?}"),
        };
        let job_key = Namespace::Queue.key(&format!("job:{}", rerun.job_id));
        let _: () = raw(&backend, "HSET", &[job_key.clone(), "started_at".into(), "1".into()]).await;
        let rep = q.reconcile(Duration::from_secs(1)).await.unwrap();
        assert!(rep.requeued.contains(&rerun.job_id) && rep.released.is_empty());
        // The stale worker's late completion (old token) is fenced off: it
        // surfaces StaleToken (never a false Ok) and touches nothing.
        assert_eq!(
            q.complete(&rerun.job_id, "chord", HostRole::Primary, JobState::Done, &rtok).await,
            Err(QueueError::StaleToken)
        );

        // 6) reconcile: a FINISHED build (outcome marker, stale) is released, NOT rebuilt.
        let ftok = match q.claim(&rerun.job_id, "chord", HostRole::Primary, 1).await.unwrap() {
            ClaimOutcome::Claimed { token } => token,
            o => panic!("{o:?}"),
        };
        q.finalize(&rerun.job_id, JobState::Done, &ftok).await.unwrap();
        let _: () = raw(&backend, "HSET", &[job_key.clone(), "started_at".into(), "1".into()]).await;
        let rep = q.reconcile(Duration::from_secs(1)).await.unwrap();
        assert!(
            rep.released.contains(&rerun.job_id) && rep.requeued.is_empty(),
            "a FINISHED build is released, never requeued/rebuilt"
        );

        // 7) Held-intent TTL: a ready=false intent (+ its dedupe) expires; a later
        //    ready=true promotion PERSISTs both (durable).
        let held = q.enqueue(&mk("harmony", "h1", Priority::Normal, false, false)).await.unwrap();
        let held_key = Namespace::Queue.key(&format!("job:{}", held.job_id));
        let dedupe_key = super::dedupe_key("harmony", "h1");
        let ttl_job: i64 = raw(&backend, "TTL", &[held_key.clone()]).await;
        let ttl_dedupe: i64 = raw(&backend, "TTL", &[dedupe_key.clone()]).await;
        assert!(ttl_job > 0 && ttl_dedupe > 0, "held intent + dedupe must have a TTL");
        q.enqueue(&mk("harmony", "h1", Priority::Normal, false, true)).await.unwrap(); // promote
        let ttl_job: i64 = raw(&backend, "TTL", &[held_key]).await;
        let ttl_dedupe: i64 = raw(&backend, "TTL", &[dedupe_key]).await;
        assert_eq!(ttl_job, -1, "promoted job must be persistent (durable)");
        assert_eq!(ttl_dedupe, -1, "promoted dedupe pointer must be persistent");
    }

    #[tokio::test]
    async fn redis_concurrent_claim_serializes_same_module() {
        // B4: two schedulers race to claim TWO DIFFERENT refs of the SAME module,
        // against the real CLAIM_LUA — exactly one wins, the other is ModuleBusy
        // (per-module serialization holds under real concurrency).
        let Some(server) = EphemeralRedis::start() else {
            eprintln!("SKIP redis_concurrent_claim: redis-server not installed");
            return;
        };
        let backend =
            RedisBackend::build(&server.url(), None, 0, 1, Duration::from_millis(500)).unwrap();
        let _: () = raw(&backend, "FLUSHALL", &[]).await;
        let q = Arc::new(RedisQueue::new(backend.clone()));
        let mk = |r: &str| JobRequest {
            module: "chord".into(),
            git_ref: r.into(),
            priority: Priority::Normal,
            heavy: false,
            ready: true,
        };
        let j1 = q.enqueue(&mk("r1")).await.unwrap();
        let j2 = q.enqueue(&mk("r2")).await.unwrap();
        // Race both claims concurrently.
        let (qa, qb) = (q.clone(), q.clone());
        let (ia, ib) = (j1.job_id.clone(), j2.job_id.clone());
        let ta = tokio::spawn(async move { qa.claim(&ia, "chord", HostRole::Primary, 4).await.unwrap() });
        let tb = tokio::spawn(async move { qb.claim(&ib, "chord", HostRole::Primary, 4).await.unwrap() });
        let (ra, rb) = (ta.await.unwrap(), tb.await.unwrap());
        let claimed = [&ra, &rb]
            .iter()
            .filter(|o| matches!(o, ClaimOutcome::Claimed { .. }))
            .count();
        let busy = [&ra, &rb].iter().filter(|o| matches!(o, ClaimOutcome::ModuleBusy)).count();
        assert_eq!(claimed, 1, "exactly one racer claims the module: {ra:?} / {rb:?}");
        assert_eq!(busy, 1, "the other is serialized out (ModuleBusy): {ra:?} / {rb:?}");
    }

    #[test]
    fn queue_namespace_binds_to_the_durable_noeviction_db() {
        // E: the durable-queue criterion must be enforced in code. Namespace::Queue
        // is a DURABLE namespace, and the shared backend routes it to the durable
        // logical DB (DB0, server-side `noeviction`) — distinct from the volatile
        // DB — so a queued/in-flight build can never be evicted under pressure.
        assert!(
            Namespace::Queue.is_durable(),
            "the compiler job queue namespace MUST be durable (noeviction)"
        );
        // Build an offline backend (durable DB 0, volatile DB 1 — the defaults).
        let backend = RedisBackend::build(
            "redis://127.0.0.1:6379",
            None,
            0,
            1,
            std::time::Duration::from_millis(200),
        )
        .expect("offline construction");
        assert_eq!(
            backend.db_for(Namespace::Queue),
            0,
            "Queue must resolve to the durable DB"
        );
        // And it must NOT share the volatile DB that the LRU-evicted namespaces use.
        assert_ne!(
            backend.db_for(Namespace::Queue),
            backend.db_for(Namespace::Ratelimit),
            "the durable queue must not live in the volatile (evictable) DB"
        );
    }

    #[test]
    fn keys_all_carry_the_queue_namespace() {
        for k in [
            zset_key(),
            seq_key(),
            dedupe_key("chord", "abc"),
            job_key("id"),
            module_lock_key("chord"),
            host_set_key(HostRole::Heavy),
        ] {
            assert!(k.starts_with("queue:"), "{k} must be under the Queue namespace");
        }
    }
}
