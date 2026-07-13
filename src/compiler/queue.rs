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
//! ## Discipline (S1/S7)
//! No infra literals: every key is formed through [`Namespace::Queue`], the
//! endpoint/password come from the vault-materialized env via [`RedisBackend`],
//! and the one tunable (completed-job retention) is a config env var. No secret
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

fn retain_secs() -> i64 {
    std::env::var(RETAIN_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_RETAIN_SECS)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The job moved queued→building on this host; the caller now OWNS it and
    /// MUST eventually call [`QueueStore::complete`].
    Claimed,
    /// The job was no longer queued (already claimed/coalesced away).
    NotQueued,
    /// Another build of the SAME module holds the per-module lock (graceful
    /// serialization — never two conflicting builds of one module at once).
    ModuleBusy,
    /// The target host is already at its concurrency cap.
    HostFull,
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
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::Unavailable => write!(
                f,
                "compiler job queue is unavailable (Redis not configured or unreachable)"
            ),
        }
    }
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
    /// is still queued, the module lock is free, and the host is below `cap`.
    async fn claim(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        cap: u32,
    ) -> Result<ClaimOutcome, QueueError>;

    /// Atomically finish a claimed build: release the module lock and the host
    /// slot, clear the dedupe entry, and record the terminal `state`.
    async fn complete(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        state: JobState,
    ) -> Result<(), QueueError>;

    /// A snapshot of queued jobs + in-flight leases for `compiler_status`.
    async fn snapshot(&self) -> Result<QueueSnapshot, QueueError>;
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
/// Per-`module@ref` dedupe pointer → the owning job id.
fn dedupe_key(module: &str, git_ref: &str) -> String {
    Namespace::Queue.key(&format!("dedupe:{module}@{git_ref}"))
}
/// The per-job hash prefix (the Lua scripts append the id).
fn job_prefix() -> String {
    Namespace::Queue.key("job:")
}
fn job_key(job_id: &str) -> String {
    format!("{}{job_id}", job_prefix())
}
/// Per-module serialization lock (held for the duration of a build).
fn module_lock_key(module: &str) -> String {
    Namespace::Queue.key(&format!("modulelock:{module}"))
}
/// Per-host set of in-flight job ids (its cardinality is the host's live load).
fn host_set_key(host: HostRole) -> String {
    Namespace::Queue.key(&format!("inflight:{}", host.as_str()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Atomic Lua scripts
// ─────────────────────────────────────────────────────────────────────────────

/// Enqueue with dedupe/coalesce/promote. Returns `{job_id, created(0/1)}`.
/// KEYS: 1=dedupe 2=zset 3=seq
/// ARGV: 1=candidate_id 2=prank 3=now 4=job_prefix 5=module 6=ref 7=prio_label
///       8=heavy(0/1) 9=ready(0/1)
const ENQUEUE_LUA: &str = r#"
local dedupe=KEYS[1]
local zset=KEYS[2]
local seqk=KEYS[3]
local id=ARGV[1]
local prank=tonumber(ARGV[2])
local now=ARGV[3]
local jp=ARGV[4]
local ready=ARGV[9]
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
    local newst=st
    if ready=='1' and st=='held' then
      redis.call('HSET', jk, 'state', 'queued')
      newst='queued'
    end
    if newst=='queued' then
      local seq=tonumber(redis.call('HGET', jk, 'seq') or '0')
      local eff=tonumber(redis.call('HGET', jk, 'prank') or '0')
      local score=seq-(eff*1000000000000)
      redis.call('ZADD', zset, score, existing)
    end
    return {existing, 0}
  end
end
local seq=redis.call('INCR', seqk)
local jk=jp..id
local state='held'
if ready=='1' then state='queued' end
redis.call('HSET', jk, 'module', ARGV[5], 'ref', ARGV[6], 'prank', prank,
  'priority', ARGV[7], 'heavy', ARGV[8], 'seq', seq, 'requested_at', now,
  'coalesced', 1, 'state', state)
redis.call('SET', dedupe, id)
if ready=='1' then
  local score=seq-(prank*1000000000000)
  redis.call('ZADD', zset, score, id)
end
return {id, 1}
"#;

/// Claim queued→building under the module lock + host cap. Returns
/// `{ok(0/1), reason}`.
/// KEYS: 1=zset 2=jobhash 3=modulelock 4=hostset
/// ARGV: 1=id 2=cap 3=now 4=host
const CLAIM_LUA: &str = r#"
local st=redis.call('HGET', KEYS[2], 'state')
if st~='queued' then return {0, 'not_queued'} end
if redis.call('EXISTS', KEYS[3])==1 then return {0, 'module_busy'} end
if redis.call('SCARD', KEYS[4])>=tonumber(ARGV[2]) then return {0, 'host_full'} end
redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('HSET', KEYS[2], 'state', 'building', 'host', ARGV[4], 'started_at', ARGV[3])
redis.call('SET', KEYS[3], ARGV[1])
redis.call('SADD', KEYS[4], ARGV[1])
return {1, 'claimed'}
"#;

/// Release a finished build. Returns `1`.
/// KEYS: 1=jobhash 2=modulelock 3=hostset 4=dedupe
/// ARGV: 1=id 2=state 3=now 4=retain_secs
const COMPLETE_LUA: &str = r#"
if redis.call('GET', KEYS[2])==ARGV[1] then redis.call('DEL', KEYS[2]) end
redis.call('SREM', KEYS[3], ARGV[1])
if redis.call('GET', KEYS[4])==ARGV[1] then redis.call('DEL', KEYS[4]) end
redis.call('HSET', KEYS[1], 'state', ARGV[2], 'finished_at', ARGV[3])
redis.call('EXPIRE', KEYS[1], tonumber(ARGV[4]))
return 1
"#;

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
        let (zset, jk, lock, hset) = (
            zset_key(),
            job_key(job_id),
            module_lock_key(module),
            host_set_key(host),
        );
        let (id, now, host_s) = (job_id.to_string(), now_ms(), host.as_str().to_string());
        let cap = cap.max(1) as i64;
        let script = redis::Script::new(CLAIM_LUA);
        let out: Result<(i64, String), ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(zset)
                    .key(jk)
                    .key(lock)
                    .key(hset)
                    .arg(id)
                    .arg(cap)
                    .arg(now)
                    .arg(host_s)
                    .invoke_async::<_, (i64, String)>(&mut conn)
                    .await
            })
            .await;
        match out {
            Ok((1, _)) => Ok(ClaimOutcome::Claimed),
            Ok((_, reason)) => Ok(match reason.as_str() {
                "module_busy" => ClaimOutcome::ModuleBusy,
                "host_full" => ClaimOutcome::HostFull,
                _ => ClaimOutcome::NotQueued,
            }),
            Err(()) => Err(QueueError::Unavailable),
        }
    }

    async fn complete(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        state: JobState,
    ) -> Result<(), QueueError> {
        // The dedupe key is `module@ref`; complete() is given the id + module,
        // so resolve the ref from the job hash (one bounded read; still degrades
        // on Unavailable) to clear the right dedupe pointer.
        let git_ref = self.job_ref(job_id).await?;
        let (jk, lock, hset, dedupe) = (
            job_key(job_id),
            module_lock_key(module),
            host_set_key(host),
            dedupe_key(module, &git_ref),
        );
        let (id, st, now, retain) =
            (job_id.to_string(), state.as_str().to_string(), now_ms(), retain_secs());
        let script = redis::Script::new(COMPLETE_LUA);
        let out: Result<i64, ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                script
                    .key(jk)
                    .key(lock)
                    .key(hset)
                    .key(dedupe)
                    .arg(id)
                    .arg(st)
                    .arg(now)
                    .arg(retain)
                    .invoke_async::<_, i64>(&mut conn)
                    .await
            })
            .await;
        out.map(|_| ()).map_err(|()| QueueError::Unavailable)
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

impl RedisQueue {
    /// Read a job's git ref from its hash (needed to reconstruct the dedupe key
    /// on `complete`). `Unavailable` on a down Redis; an empty string when the
    /// hash is gone (already expired) — harmless, the dedupe DEL then no-ops.
    async fn job_ref(&self, job_id: &str) -> Result<String, QueueError> {
        let jk = job_key(job_id);
        let out: Result<Option<String>, ()> = self
            .backend
            .with_conn(Namespace::Queue, |mut conn| async move {
                redis::cmd("HGET")
                    .arg(jk)
                    .arg("ref")
                    .query_async::<_, Option<String>>(&mut conn)
                    .await
            })
            .await;
        out.map(|o| o.unwrap_or_default())
            .map_err(|()| QueueError::Unavailable)
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
    }

    #[derive(Default)]
    struct State {
        jobs: HashMap<String, Job>,
        dedupe: HashMap<String, String>,       // module@ref -> id
        module_lock: HashMap<String, String>,  // module -> id
        host_inflight: HashMap<&'static str, Vec<String>>,
        seq: i64,
        next_id: u64,
        /// When set, every op behaves as an unreachable Redis (degradation test).
        down: bool,
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

        /// How many times `module@ref` coalesced (test assertion helper).
        pub(crate) fn coalesced(&self, module: &str, git_ref: &str) -> i64 {
            let s = self.state.lock().unwrap();
            let dk = format!("{module}@{git_ref}");
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

    #[async_trait]
    impl QueueStore for InMemoryQueue {
        async fn enqueue(&self, req: &JobRequest) -> Result<Enqueued, QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            let dk = format!("{}@{}", req.module, req.git_ref);
            if let Some(existing) = s.dedupe.get(&dk).cloned() {
                let promote = matches!(
                    s.jobs.get(&existing).map(|j| j.state.as_str()),
                    Some("queued") | Some("held")
                );
                if promote {
                    let j = s.jobs.get_mut(&existing).unwrap();
                    j.coalesced += 1;
                    if req.priority.rank() > j.prank {
                        j.prank = req.priority.rank();
                    }
                    if req.ready && j.state == "held" {
                        j.state = "queued".into();
                    }
                    return Ok(Enqueued {
                        job_id: existing,
                        created: false,
                    });
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
            match s.jobs.get(job_id).map(|j| j.state.clone()) {
                Some(st) if st == "queued" => {}
                _ => return Ok(ClaimOutcome::NotQueued),
            }
            if s.module_lock.contains_key(module) {
                return Ok(ClaimOutcome::ModuleBusy);
            }
            let live = s.host_inflight.get(host.as_str()).map(Vec::len).unwrap_or(0);
            if live as u32 >= cap.max(1) {
                return Ok(ClaimOutcome::HostFull);
            }
            {
                let j = s.jobs.get_mut(job_id).unwrap();
                j.state = "building".into();
                j.host = Some(host);
                j.started_at = now_ms();
            }
            s.module_lock.insert(module.to_string(), job_id.to_string());
            s.host_inflight
                .entry(host.as_str())
                .or_default()
                .push(job_id.to_string());
            Ok(ClaimOutcome::Claimed)
        }

        async fn complete(
            &self,
            job_id: &str,
            module: &str,
            host: HostRole,
            state: JobState,
        ) -> Result<(), QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            if s.module_lock.get(module).map(String::as_str) == Some(job_id) {
                s.module_lock.remove(module);
            }
            if let Some(v) = s.host_inflight.get_mut(host.as_str()) {
                v.retain(|id| id != job_id);
            }
            if let Some(j) = s.jobs.get(job_id) {
                let dk = format!("{}@{}", j.module, j.git_ref);
                if s.dedupe.get(&dk).map(String::as_str) == Some(job_id) {
                    s.dedupe.remove(&dk);
                }
            }
            if let Some(j) = s.jobs.get_mut(job_id) {
                j.state = state.as_str().into();
            }
            Ok(())
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

    fn req(module: &str, git_ref: &str, prio: Priority, heavy: bool) -> JobRequest {
        JobRequest {
            module: module.into(),
            git_ref: git_ref.into(),
            priority: prio,
            heavy,
            ready: true,
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
        assert_eq!(
            q.claim(&j1.job_id, "chord", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::Claimed
        );
        assert_eq!(
            q.claim(&j2.job_id, "chord", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::ModuleBusy
        );
        // Once the first finishes, the module lock frees and the second claims.
        q.complete(&j1.job_id, "chord", HostRole::Primary, JobState::Done).await.unwrap();
        assert_eq!(
            q.claim(&j2.job_id, "chord", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::Claimed
        );
    }

    #[tokio::test]
    async fn host_cap_bounds_concurrency() {
        let q = InMemoryQueue::new();
        let j1 = q.enqueue(&req("m1", "r", Priority::Normal, false)).await.unwrap();
        let j2 = q.enqueue(&req("m2", "r", Priority::Normal, false)).await.unwrap();
        // cap=1 on primary: first claim ok, second (different module) host-full.
        assert_eq!(
            q.claim(&j1.job_id, "m1", HostRole::Primary, 1).await.unwrap(),
            ClaimOutcome::Claimed
        );
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
                    ClaimOutcome::Claimed
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
