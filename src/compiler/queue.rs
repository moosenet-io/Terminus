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
    /// Optional cargo `--bin` target to build (BLD/TERM #360). `None` ⇒ the build
    /// defaults `--bin <module>`; set it when the deployable binary's name differs
    /// from the module name (e.g. module `terminus` → bin `terminus_primary`), so
    /// the AUTOMATED queue path can build such modules without an inline override.
    pub bin: Option<String>,
    /// Disrupt-on-demand override (BLD-DISPATCH-01): when `true`, a HEAVY job
    /// dispatches even outside a configured window and without a fleet-quiet
    /// signal — it still goes through the normal module lock / host cap claim and
    /// the idle-mode lease, only the window/quiet GATE is bypassed. Orthogonal to
    /// `priority` (which only orders the queue). Monotonic like `heavy`: a later
    /// coalescing request with `force=true` upgrades the job; it is never
    /// downgraded back to `force=false`.
    pub force: bool,
    /// `"build"` (default) or `"test"` (BLD-ASYNC, TERM #421): which
    /// `compiler_build` mode the scheduler runs this job as. Carried durably so
    /// an async `compiler_build(wait=false, mode=test)` submission — or a
    /// `compiler_request(mode=test)` — dispatches as a test-gate, not a
    /// publish-and-flip build. `""`/anything other than `"test"` behaves as
    /// `"build"` (unchanged default behavior).
    pub mode: String,
    /// PCON-01/04 (S122 root-cause fix): the commit sha `git_ref` was resolved
    /// to at ENQUEUE/request time (before this job is durably recorded), when
    /// SHA-staging is enabled and resolvable — `None` when SHA-staging is off
    /// or a ref genuinely could not be resolved at request time. This is the
    /// job's DURABLE identity: the dedupe key, the module-serialization lock
    /// key, the GC live-set entry, and the on-disk per-sha stage dir all key
    /// off `resolved_sha.as_deref().unwrap_or(&git_ref)` — see
    /// [`job_identity`] — never off the mutable `git_ref` alone once a sha is
    /// known. Resolving ONCE here (not at dispatch time) is what closes the
    /// "ref moved while queued" race: two different refs that happen to
    /// resolve to the same sha now dedupe onto ONE job instead of racing two
    /// independent stagings of the identical commit.
    pub resolved_sha: Option<String>,
}

/// The single identity a job/lease/queued-entry is keyed by everywhere
/// (dedupe, module-serialization lock, GC live-set, on-disk stage dir name):
/// the resolved sha when known, else the raw `git_ref` (SHA-staging off, or a
/// ref that could not be resolved at request time — see [`JobRequest::
/// resolved_sha`]'s doc). Centralizing this one-line rule in a single
/// function is what PCON-04's root-cause fix depends on: every call site
/// below uses THIS, so the queue lock, the GC live-set, and the staged
/// directory name can never drift onto different identities for the same job.
pub fn job_identity<'a>(git_ref: &'a str, resolved_sha: &'a Option<String>) -> &'a str {
    resolved_sha.as_deref().unwrap_or(git_ref)
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
    /// The cargo `--bin` target carried from the enqueue request (BLD/TERM #360).
    /// `None` ⇒ the executor defaults `--bin <module>`.
    pub bin: Option<String>,
    /// Disrupt-on-demand override carried from the enqueue request
    /// (BLD-DISPATCH-01). See [`JobRequest::force`].
    pub force: bool,
    /// `"build"` or `"test"` (BLD-ASYNC, TERM #421). See [`JobRequest::mode`].
    pub mode: String,
    /// See [`JobRequest::resolved_sha`] — carried through so the dispatching
    /// executor builds/stages against the EXACT sha resolved at enqueue time,
    /// never re-resolving (and never re-racing) `git_ref` at dispatch time.
    pub resolved_sha: Option<String>,
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
    /// See [`JobRequest::resolved_sha`] — the GC live-set (PCON-05) MUST use
    /// this (via [`job_identity`]), never `git_ref` alone, so a building
    /// lease protects the SAME on-disk directory name PCON-01 staged.
    pub resolved_sha: Option<String>,
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
    /// but is immediate and token-gated rather than lease-age gated. Also CLEARS any
    /// stale terminal/error metadata so the re-queued job carries no prior-attempt
    /// failure state, and records `reason` as a light `last_requeue_reason`.
    async fn requeue(
        &self,
        job_id: &str,
        module: &str,
        host: HostRole,
        token: &str,
        reason: &str,
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
/// Normalize a raw `mode` arg to its canonical dedupe token: `"test"` iff the
/// input is exactly `"test"`, else `"build"` (covers `"build"`, `""`, and any
/// stale/unknown value). The SAME normalization runs in the release/reconcile
/// Lua (`if mode~='test' then mode='build' end`) so Rust-built and Lua-derived
/// dedupe keys always agree on the mode component.
fn dedupe_mode(mode: &str) -> &'static str {
    if mode == "test" {
        "test"
    } else {
        "build"
    }
}
/// A COLLISION-FREE identity for a `(mode, module, ref)` triple:
/// `"{mode}:{len(module)}:{module}:{ref}"`. `mode` (BLD-ASYNC, TERM #421) is a
/// canonical `build`/`test` token — neither contains `:`, so it is exactly the
/// text before the FIRST `:` and the encoding stays injective; the decimal
/// length prefix then marks where `module` ends, so distinct triples never
/// alias even when a component contains `:`/`@` (e.g. `("a","b@c")` ≠
/// `("a@b","c")`, and `(build,m,r)` ≠ `(test,m,r)`). Keying `mode` in here — not
/// as an adopt-on-coalesce field — is what keeps a `mode=test` submit from
/// coalescing onto (or flipping) a pending `mode=build` job: they are genuinely
/// different work. The IDENTICAL construction is used in the release/reconcile
/// Lua, so Rust-built and Lua-derived dedupe keys always agree. Encode-only.
fn dedupe_id(module: &str, git_ref: &str, mode: &str) -> String {
    format!("{}:{}:{module}:{git_ref}", dedupe_mode(mode), module.len())
}
/// Per-`(mode, module, ref)` dedupe pointer → the owning job id.
fn dedupe_key(module: &str, git_ref: &str, mode: &str) -> String {
    format!("{}{}", dedupe_prefix(), dedupe_id(module, git_ref, mode))
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
/// PCON-04: per-`(module, ref)` serialization lock (held for the duration of a
/// build). Previously keyed by `module` ALONE, which force-serialized every
/// build of a module regardless of which commit — including two builds of
/// DIFFERENT, mutually-isolated SHAs (PCON-01..03 already stage/target them
/// disjointly, so nothing about them actually conflicts). Re-keying to
/// `(module, ref)` means the lock now only ever contends two requests for the
/// SAME ref/sha (which the dedupe pointer above already coalesces to one job
/// in the common case); different-SHA jobs of one module are no longer
/// mutually excluded by this lock. The Lua derives this key from the job
/// hash's own module+ref (A1/A2); retained as the canonical key constructor
/// used by tests + the namespace assertion.
#[allow(dead_code)]
fn module_lock_key(module: &str, git_ref: &str) -> String {
    format!("{}{module}:{git_ref}", module_lock_prefix())
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
///       8=heavy 9=ready 10=held_ttl 11=bin(''=none) 12=force(0/1)
///       8=heavy(0/1) 9=ready(0/1) 10=held_ttl_secs 13=mode 14=resolved_sha(''=none)
///
/// PCON-01/04 (S122 root-cause fix): `KEYS[1]` (the dedupe pointer) is ALREADY
/// computed by the Rust caller from [`job_identity`] — i.e. `resolved_sha` when
/// known, else `ref` — so two requests for DIFFERENT refs that resolve to the
/// SAME sha dedupe onto ONE job here, not two. `resolved_sha` (ARGV[14]) is
/// additionally stored on the job hash so RELEASE/RECONCILE can re-derive that
/// SAME identity later without a caller arg (A1).
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
local bin=ARGV[11]
local force=ARGV[12]
local mode=ARGV[13]
local resolved_sha=ARGV[14]
local existing=redis.call('GET', dedupe)
if existing then
  local jk=jp..existing
  local st=redis.call('HGET', jk, 'state')
  if st=='queued' or st=='held' then
    redis.call('HINCRBY', jk, 'coalesced', 1)
    -- BLD/TERM #360: a later request may supply the cargo bin a binless job lacked
    -- (or correct it). bin is a deterministic property of module@ref, so adopt any
    -- non-empty incoming value; the scheduler then builds the right binary.
    if bin~='' then redis.call('HSET', jk, 'bin', bin) end
    -- BLD-ASYNC (TERM #421): mode is NOT adopted on coalesce — it is part of the
    -- dedupe KEY, so any job we coalesce onto necessarily already has this exact
    -- mode (a differing mode resolves to a different dedupe pointer / job).
    local cur=tonumber(redis.call('HGET', jk, 'prank') or '0')
    if prank>cur then
      redis.call('HSET', jk, 'prank', prank, 'priority', ARGV[7])
    end
    -- Monotonic host-class upgrade: a later heavy/fast request promotes the job
    -- to heavy so it respects the heavy-build window; never downgrade heavy→small.
    if heavy=='1' then
      redis.call('HSET', jk, 'heavy', '1')
    end
    -- BLD-DISPATCH-01: monotonic force upgrade — a later force=true request
    -- disrupts the window/quiet gate for this job; never downgrade force→false.
    if force=='1' then
      redis.call('HSET', jk, 'force', '1')
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
    -- BLD/TERM #360: adopt a non-empty incoming bin on the CURRENT job hash so the
    -- rerun that `release` clones from it carries the correct (possibly newly
    -- supplied) cargo bin, not a stale/absent one.
    if bin~='' then redis.call('HSET', jk, 'bin', bin) end
    -- mode is keyed (see above); never adopted on coalesce.
    local cur=tonumber(redis.call('HGET', jk, 'prank') or '0')
    if prank>cur then
      redis.call('HSET', jk, 'prank', prank, 'priority', ARGV[7])
    end
    if heavy=='1' then
      redis.call('HSET', jk, 'heavy', '1')
    end
    if force=='1' then
      redis.call('HSET', jk, 'force', '1')
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
  'coalesced', 1, 'state', state, 'bin', ARGV[11], 'force', force, 'mode', mode,
  'resolved_sha', resolved_sha)
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
/// DERIVED from the job hash's OWN stored `(module, ref)` (A2; PCON-04 — ref is the
/// resolved SHA once PCON-01's staging is active, so the lock only ever contends
/// same-SHA jobs), and the caller's `module` arg is VERIFIED against it — a
/// mismatch is refused so a buggy call can never take a foreign module lock and
/// break per-`(module, ref)` serialization. On success
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
-- PCON-04 (root-cause fix): the lock is keyed by (module, identity) — see
-- `job_identity`'s doc — where identity is the RESOLVED sha when known, else
-- the raw ref. Two DIFFERENT-sha jobs of one module are never mutually
-- excluded; two jobs that resolved to the SAME sha (even via different
-- original refs) DO still contend this lock.
local ref=redis.call('HGET', KEYS[2], 'ref') or ''
local resolved_sha=redis.call('HGET', KEYS[2], 'resolved_sha') or ''
local identity=ref
if resolved_sha ~= '' then identity=resolved_sha end
local lockkey=ARGV[6]..module..':'..identity
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
local resolved_sha=redis.call('HGET', jobkey, 'resolved_sha') or ''
-- PCON-01/04 (root-cause fix): the ONE identity every key below derives
-- from — the resolved sha when known, else the raw ref. Mirrors Rust's
-- `job_identity` exactly (single rule, both sides).
local identity=ref
if resolved_sha ~= '' then identity=resolved_sha end
local host=redis.call('HGET', jobkey, 'host')
-- BLD-ASYNC (TERM #421): mode is part of the dedupe KEY, so it is re-derived
-- here (canonicalized identically to Rust's dedupe_mode) and folded into the
-- dedupe pointer, so a rerun/clear targets the mode-specific entry.
local mode=redis.call('HGET', jobkey, 'mode') or 'build'
if mode~='test' then mode='build' end
-- PCON-04: lock key is (module, identity) — see `job_identity`'s doc.
if module then
  local lockkey=lockprefix..module..':'..(identity or '')
  if redis.call('GET', lockkey)==jobid then redis.call('DEL', lockkey) end
end
if host then
  redis.call('SREM', hostprefix..host, jobid)
end
redis.call('HDEL', jobkey, 'claim_token')
local dedupe=false
if module and identity then dedupe=dedupeprefix..mode..':'..string.len(module)..':'..module..':'..identity end
local rerun=redis.call('HGET', jobkey, 'rerun')
local rr_flag=0
local rr_id=''
if rerun=='1' then
  local prank=tonumber(redis.call('HGET', jobkey, 'prank') or '1')
  local prio=redis.call('HGET', jobkey, 'priority') or 'normal'
  local heavy=redis.call('HGET', jobkey, 'heavy') or '0'
  local bin=redis.call('HGET', jobkey, 'bin') or ''
  local force=redis.call('HGET', jobkey, 'force') or '0'
  local seq=redis.call('INCR', seqkey)
  rr_id=rerun_id
  local njk=jobprefix..rr_id
  redis.call('HSET', njk, 'module', module, 'ref', ref, 'prank', prank,
    'priority', prio, 'heavy', heavy, 'seq', seq, 'requested_at', nowv,
    'coalesced', 1, 'state', 'queued', 'bin', bin, 'force', force, 'mode', mode,
    'resolved_sha', resolved_sha)
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

/// Peek the top-N dispatchable jobs, flattened as 9 fields each:
/// `id, module, ref, prank, heavy, bin, force, mode, resolved_sha`.
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
  out[#out+1]=redis.call('HGET', jk, 'bin') or ''
  out[#out+1]=redis.call('HGET', jk, 'force') or '0'
  out[#out+1]=redis.call('HGET', jk, 'mode') or ''
  out[#out+1]=redis.call('HGET', jk, 'resolved_sha') or ''
end
return out
"#;

/// List the in-flight leases on one host, flattened as 5 fields each:
/// `id, module, ref, started_at, resolved_sha`.
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
  out[#out+1]=redis.call('HGET', jk, 'resolved_sha') or ''
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
-- Derive the module lock + host slot from the hash's OWN fields. PCON-04
-- (root-cause fix): the lock is keyed by (module, identity) — the resolved
-- sha when known, else the raw ref — see `job_identity`'s doc.
local module=redis.call('HGET', jobkey, 'module')
local crashref=redis.call('HGET', jobkey, 'ref') or ''
local crashsha=redis.call('HGET', jobkey, 'resolved_sha') or ''
local crashidentity=crashref
if crashsha ~= '' then crashidentity=crashsha end
if module then
  local lockkey=lockprefix..module..':'..crashidentity
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
/// build never ran).
///
/// DOUBLE FENCE: requeue proceeds ONLY when ALL of (a) the caller's `claim_token`
/// matches, (b) the job is still `state == 'building'`, AND (c) NO terminal `outcome`
/// marker exists. A terminal job (`done`/`failed`), a non-`building` state, OR a
/// finalized-but-not-yet-released job (a terminal `outcome` written while the token is
/// still present) is NEVER resurrected back to `queued` — even with a matching token
/// (e.g. if an idle-abort requeue races completion/finalization). A token mismatch
/// returns `0`; a non-requeuable (terminal / finalized / non-building) job returns `2`;
/// a genuine requeue returns `1`.
///
/// Also CLEARS stale error metadata (last_error) so a requeued job carries no
/// prior-attempt failure state, and records a light `last_requeue_reason`.
/// KEYS: 1=jobhash 2=zset
/// ARGV: 1=id 2=token 3=modulelock_prefix 4=host_set_prefix 5=requeue_reason
fn requeue_lua() -> String {
    r#"
if redis.call('HGET', KEYS[1], 'claim_token') ~= ARGV[2] then return 0 end
-- STATE FENCE: only a still-`building` job with NO terminal outcome may be requeued. A
-- terminal (done/failed) job, a non-building job, OR a finalized-but-unreleased job (a
-- terminal `outcome` marker present) is NEVER resurrected, even with a matching token.
if redis.call('HGET', KEYS[1], 'state') ~= 'building' then return 2 end
if redis.call('HGET', KEYS[1], 'outcome') then return 2 end
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
-- Clear STALE non-terminal error metadata so a requeued job carries no prior-attempt
-- failure state (the fence above already guarantees NO terminal `outcome` is present).
-- Record a light requeue reason for observability instead.
redis.call('HDEL', jobkey, 'last_error')
redis.call('HSET', jobkey, 'last_requeue_reason', ARGV[5])
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
        // PCON-01/04 (root-cause fix): dedupe on `job_identity` — the resolved
        // sha when known, else the raw ref — NOT `git_ref` alone. This is what
        // makes two DIFFERENT refs that resolve to the SAME sha coalesce onto
        // ONE job instead of racing two independent stagings of the identical
        // commit (closes the shared-remote-source clobber). BLD-ASYNC (TERM
        // #421): the dedupe key also includes the canonical mode, so a
        // mode=test submit gets its OWN queue entry and can never coalesce
        // onto (or flip) a pending mode=build job of the same identity.
        let identity = job_identity(&req.git_ref, &req.resolved_sha).to_string();
        let (dedupe, zset, seq) = (
            dedupe_key(&req.module, &identity, &req.mode),
            zset_key(),
            seq_key(),
        );
        let (prank, now, jp) = (req.priority.rank(), now_ms(), job_prefix());
        let (module, git_ref, label) =
            (req.module.clone(), req.git_ref.clone(), req.priority.as_str());
        let (heavy, ready) = (req.heavy as i64, req.ready as i64);
        let held_ttl = held_intent_ttl_secs();
        // BLD/TERM #360: carry the optional cargo bin (''=none) as ARGV[11].
        let bin = req.bin.clone().unwrap_or_default();
        // BLD-DISPATCH-01: carry the disrupt-on-demand force flag as ARGV[12].
        let force = req.force as i64;
        // BLD-ASYNC (TERM #421): carry the CANONICAL build-vs-test mode as
        // ARGV[13], stored on the job hash so the release/reconcile Lua can
        // re-derive the same mode-keyed dedupe key from the hash's own fields.
        let mode = dedupe_mode(&req.mode);
        // PCON-01/04: carry the resolved sha (''=none) as ARGV[14], stored on
        // the job hash so RELEASE/RECONCILE/CLAIM can re-derive `job_identity`
        // without a caller arg (A1).
        let resolved_sha = req.resolved_sha.clone().unwrap_or_default();
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
                    .arg(bin)
                    .arg(force)
                    .arg(mode)
                    .arg(resolved_sha)
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
                .chunks_exact(9)
                .map(|c| QueuedJob {
                    job_id: c[0].clone(),
                    module: c[1].clone(),
                    git_ref: c[2].clone(),
                    priority: priority_from_rank(c[3].parse().unwrap_or(1)),
                    heavy: c[4] == "1",
                    bin: Some(c[5].clone()).filter(|s| !s.is_empty()),
                    force: c[6] == "1",
                    mode: if c[7] == "test" {
                        "test".to_string()
                    } else {
                        "build".to_string()
                    },
                    resolved_sha: Some(c[8].clone()).filter(|s| !s.is_empty()),
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
        reason: &str,
    ) -> Result<(), QueueError> {
        // `module`/`host` args are advisory only — the lock + host-slot keys are
        // DERIVED in-Lua from the job hash's OWN stored fields (A1).
        let _ = (module, host);
        let (jk, zset) = (job_key(job_id), zset_key());
        let (id, token, reason) = (job_id.to_string(), token.to_string(), reason.to_string());
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
                    .arg(reason)
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
                    for c in flat.chunks_exact(5) {
                        leases.push(Lease {
                            job_id: c[0].clone(),
                            module: c[1].clone(),
                            git_ref: c[2].clone(),
                            host,
                            started_at_ms: c[3].parse().unwrap_or(0),
                            resolved_sha: Some(c[4].clone()).filter(|s| !s.is_empty()),
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
        bin: Option<String>,
        force: bool,
        mode: String,
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
        /// Light observability marker recorded by `requeue` (F4).
        last_requeue_reason: Option<String>,
        /// A NON-terminal transient error recorded while still building (cleared on
        /// requeue so a re-queued job carries no prior-attempt failure state).
        last_error: Option<String>,
        /// See [`JobRequest::resolved_sha`] / [`job_identity`].
        resolved_sha: Option<String>,
    }

    #[derive(Default)]
    struct State {
        jobs: HashMap<String, Job>,
        dedupe: HashMap<String, String>,       // module@ref -> id
        // PCON-04: keyed by `module_lock_key(module, ref)` — the SAME
        // constructor the real Lua's key shape mirrors and tests assert
        // against (single source of truth for the key format) — not module
        // alone.
        module_lock: HashMap<String, String>,
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
        /// The next N `requeue` calls fail as Unavailable (idle-abort requeue outage).
        fail_requeues: usize,
        /// The next N `snapshot` calls fail as Unavailable (PCON-05's
        /// fail-closed-GC test: reconcile/peek stay healthy, only snapshot fails).
        fail_snapshots: usize,
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

        /// Make the next `n` `requeue` calls fail as Unavailable (idle-abort outage).
        pub(crate) fn fail_requeues(&self, n: usize) {
            self.state.lock().unwrap().fail_requeues = n;
        }

        /// Make the next `n` `snapshot` calls fail as Unavailable, WITHOUT
        /// affecting `peek`/`reconcile`/`claim` — isolates the PCON-05
        /// fail-closed-GC path (snapshot fails, dispatch is otherwise healthy)
        /// from the blanket `set_down` degradation.
        pub(crate) fn fail_snapshots(&self, n: usize) {
            self.state.lock().unwrap().fail_snapshots = n;
        }

        /// Record a NON-terminal transient error on a job (test helper), to prove
        /// `requeue` clears stale error state on a still-building job.
        pub(crate) fn set_last_error(&self, job_id: &str, err: &str) {
            if let Some(j) = self.state.lock().unwrap().jobs.get_mut(job_id) {
                j.last_error = Some(err.to_string());
            }
        }

        /// The current `last_error` on a job, if any (test helper).
        pub(crate) fn last_error(&self, job_id: &str) -> Option<String> {
            self.state
                .lock()
                .unwrap()
                .jobs
                .get(job_id)
                .and_then(|j| j.last_error.clone())
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

        /// How many times `(module, ref, mode)` coalesced (test assertion helper).
        pub(crate) fn coalesced(&self, module: &str, git_ref: &str, mode: &str) -> i64 {
            let s = self.state.lock().unwrap();
            let dk = dedupe_id(module, git_ref, mode);
            s.dedupe
                .get(&dk)
                .and_then(|id| s.jobs.get(id))
                .map(|j| j.coalesced)
                .unwrap_or(0)
        }

        /// PCON-04 fidelity test helper: the job id currently holding the
        /// `(module, git_ref)` lock, looked up under the EXACT SAME prefixed key
        /// [`module_lock_key`] constructs — i.e. the same key shape the real
        /// Lua uses (`lockprefix..module..':'..ref`) — so a test can assert the
        /// fake's lock storage is keyed identically to production, not just
        /// internally self-consistent.
        pub(crate) fn module_lock_holder(&self, module: &str, git_ref: &str) -> Option<String> {
            let s = self.state.lock().unwrap();
            s.module_lock.get(&module_lock_key(module, git_ref)).cloned()
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
                j.bin.clone(),
                j.force,
                j.mode.clone(),
                j.resolved_sha.clone(),
            ),
            None => return,
        };
        let (dmod, dref, dhost, dprank, dheavy, rerun, dbin, dforce, dmode, dsha) = done;
        // PCON-01/04 (root-cause fix): derive the module lock + dedupe key from
        // `job_identity` (the resolved sha when known, else the raw ref) — the
        // SAME identity `enqueue` used to build the dedupe pointer and `claim`
        // used to build the lock key.
        let identity = job_identity(&dref, &dsha).to_string();
        let lk = module_lock_key(&dmod, &identity);
        if s.module_lock.get(&lk).map(String::as_str) == Some(job_id) {
            s.module_lock.remove(&lk);
        }
        if let Some(h) = dhost {
            if let Some(v) = s.host_inflight.get_mut(h.as_str()) {
                v.retain(|id| id != job_id);
            }
        }
        if let Some(j) = s.jobs.get_mut(job_id) {
            j.claim_token = None;
        }
        // BLD-ASYNC (TERM #421): the dedupe key is mode-scoped (mirrors RELEASE_BODY).
        let dk = dedupe_id(&dmod, &identity, &dmode);
        if rerun {
            // Re-enqueue EXACTLY one follow-up job for the same identity — the
            // resolved sha is carried forward unchanged (a re-run re-executes the
            // EXACT same resolved commit, never re-resolves the original ref).
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
                    bin: dbin,
                    force: dforce,
                    mode: dmode,
                    seq,
                    coalesced: 1,
                    state: "queued".into(),
                    host: None,
                    started_at: 0,
                    claim_token: None,
                    outcome: None,
                    rerun: false,
                    last_requeue_reason: None,
                    last_error: None,
                    resolved_sha: dsha,
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
            // PCON-01/04 (root-cause fix): dedupe on `job_identity` (resolved sha
            // when known, else the raw ref) — mirrors the real ENQUEUE_LUA/Rust
            // `enqueue()` exactly, so two different refs resolving to the same
            // sha coalesce here too. BLD-ASYNC (TERM #421): mode-scoped — a
            // mode=test submit gets its own entry and never coalesces onto a
            // pending mode=build job.
            let identity = job_identity(&req.git_ref, &req.resolved_sha);
            let dk = dedupe_id(&req.module, identity, &req.mode);
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
                        // BLD/TERM #360: adopt a non-empty incoming bin (mirrors ENQUEUE_LUA).
                        if req.bin.is_some() {
                            j.bin = req.bin.clone();
                        }
                        // BLD-DISPATCH-01: monotonic force upgrade (mirrors ENQUEUE_LUA).
                        if req.force {
                            j.force = true;
                        }
                        // BLD-ASYNC (TERM #421): mode is keyed into the dedupe id,
                        // so a coalescing request always shares this job's mode —
                        // never adopted/flipped here (mirrors ENQUEUE_LUA).
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
                        // BLD/TERM #360: adopt a non-empty incoming bin so the rerun
                        // cloned by release carries it (mirrors ENQUEUE_LUA).
                        if req.bin.is_some() {
                            j.bin = req.bin.clone();
                        }
                        // BLD-DISPATCH-01: monotonic force upgrade (mirrors ENQUEUE_LUA).
                        if req.force {
                            j.force = true;
                        }
                        // BLD-ASYNC (TERM #421): mode is keyed, never adopted here.
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
                    bin: req.bin.clone(),
                    force: req.force,
                    mode: req.mode.clone(),
                    seq,
                    coalesced: 1,
                    state: if req.ready { "queued".into() } else { "held".into() },
                    host: None,
                    started_at: 0,
                    claim_token: None,
                    outcome: None,
                    rerun: false,
                    last_requeue_reason: None,
                    last_error: None,
                    resolved_sha: req.resolved_sha.clone(),
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
                    bin: j.bin.clone(),
                    force: j.force,
                    mode: if j.mode == "test" {
                        "test".to_string()
                    } else {
                        "build".to_string()
                    },
                    resolved_sha: j.resolved_sha.clone(),
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
            // Derive the module (+ identity, for the PCON-04 lock key — PCON-01
            // root-cause fix: resolved sha when known, else the raw ref) from the
            // job's OWN stored fields; verify the caller's `module` arg against it.
            let (stored_module, stored_ref, stored_sha) = match s.jobs.get(job_id) {
                Some(j) if j.state == "queued" => {
                    (j.module.clone(), j.git_ref.clone(), j.resolved_sha.clone())
                }
                Some(_) | None => return Ok(ClaimOutcome::NotQueued),
            };
            if !module.is_empty() && module != stored_module {
                return Ok(ClaimOutcome::Rejected);
            }
            let identity = job_identity(&stored_ref, &stored_sha);
            let lock_key = module_lock_key(&stored_module, identity);
            if s.module_lock.contains_key(&lock_key) {
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
            s.module_lock.insert(lock_key, job_id.to_string());
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
            reason: &str,
        ) -> Result<(), QueueError> {
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(QueueError::Unavailable);
            }
            // Simulate a transient requeue outage (retried by the scheduler).
            if s.fail_requeues > 0 {
                s.fail_requeues -= 1;
                return Err(QueueError::Unavailable);
            }
            // DOUBLE FENCE: requeue proceeds ONLY with a matching claim token AND a
            // still-`building` job that has NO terminal `outcome` marker. A token mismatch
            // (already released / reconciled + re-claimed), a terminal (done/failed) job,
            // a non-building job, OR a finalized-but-unreleased job (outcome present with
            // the token still matching) is a safe no-op — a terminal/finalized job is
            // NEVER resurrected back to queued, even with a matching token.
            match s.jobs.get(job_id) {
                Some(j)
                    if j.claim_token.as_deref() == Some(token)
                        && j.state == "building"
                        && j.outcome.is_none() => {}
                _ => return Ok(()),
            }
            // `module`/`host` args are advisory — derive from the job's OWN fields.
            let _ = (module, host);
            // PCON-01/04 (root-cause fix): the lock key is (module, identity).
            let stored = s
                .jobs
                .get(job_id)
                .map(|j| (j.module.clone(), j.git_ref.clone(), j.resolved_sha.clone()));
            if let Some((m, r, sha)) = stored {
                let identity = job_identity(&r, &sha).to_string();
                let lk = module_lock_key(&m, &identity);
                if s.module_lock.get(&lk).map(String::as_str) == Some(job_id) {
                    s.module_lock.remove(&lk);
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
                // Clear STALE non-terminal error metadata + record the requeue reason.
                // (The fence above guarantees `outcome` is absent, so nothing terminal is
                // being cleared/resurrected here.)
                j.last_error = None;
                j.last_requeue_reason = Some(reason.to_string());
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
            #[allow(clippy::type_complexity)]
            let building: Vec<(
                String,
                String,
                String,
                Option<String>,
                Option<HostRole>,
                Option<String>,
                i64,
            )> = s
                .jobs
                .iter()
                .filter(|(_, j)| j.state == "building")
                .map(|(id, j)| {
                    (
                        id.clone(),
                        j.module.clone(),
                        j.git_ref.clone(),
                        j.resolved_sha.clone(),
                        j.host,
                        j.outcome.clone(),
                        j.started_at,
                    )
                })
                .collect();
            let mut report = ReconcileReport::default();
            for (id, module, git_ref, resolved_sha, _host, outcome, started) in building {
                if let Some(outcome) = outcome {
                    // FINISHED but not released → release only, NO rebuild.
                    release_locked(&mut s, &id, &outcome);
                    report.released.push(id);
                } else if (now - started) >= stale_ms {
                    // Crashed mid-build → requeue. PCON-04 (root-cause fix): lock
                    // keyed by (module, identity) — resolved sha when known, else ref.
                    let identity = job_identity(&git_ref, &resolved_sha);
                    let lk = module_lock_key(&module, identity);
                    if s.module_lock.get(&lk).map(String::as_str) == Some(id.as_str()) {
                        s.module_lock.remove(&lk);
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
            let mut s = self.state.lock().unwrap();
            if s.fail_snapshots > 0 {
                s.fail_snapshots -= 1;
                return Err(QueueError::Unavailable);
            }
            let mut leases = Vec::new();
            for (id, j) in s.jobs.iter().filter(|(_, j)| j.state == "building") {
                leases.push(Lease {
                    job_id: id.clone(),
                    module: j.module.clone(),
                    git_ref: j.git_ref.clone(),
                    host: j.host.unwrap_or(HostRole::Primary),
                    started_at_ms: j.started_at,
                    resolved_sha: j.resolved_sha.clone(),
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
            bin: None,
            module: module.into(),
            git_ref: git_ref.into(),
            priority: prio,
            heavy,
            ready: true,
            force: false,
            mode: "build".to_string(),
            resolved_sha: None,
        }
    }

    /// A request carrying an explicit `resolved_sha` (PCON-01/04 root-cause
    /// fix), for tests exercising identity-based dedupe/lock/GC directly.
    fn req_with_sha(module: &str, git_ref: &str, resolved_sha: &str) -> JobRequest {
        JobRequest {
            resolved_sha: Some(resolved_sha.to_string()),
            ..req(module, git_ref, Priority::Normal, false)
        }
    }

    /// A `mode="test"` request for the same `(module, ref)` — used to obtain a
    /// SECOND, distinct job id sharing the exact same PCON-04 `(module, ref)`
    /// lock identity (the lock key is mode-independent, unlike the dedupe key),
    /// since a same-`(module, ref, mode)` enqueue always coalesces onto any
    /// existing active job rather than creating a new one.
    fn test_req(module: &str, git_ref: &str) -> JobRequest {
        JobRequest {
            mode: "test".to_string(),
            ..req(module, git_ref, Priority::Normal, false)
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
        assert_eq!(q.coalesced("chord", "abc", "build"), 2, "both readiness signals counted");
    }

    #[test]
    fn dedupe_identity_is_collision_free_across_at_signs() {
        // Fix 1: raw `{module}@{ref}` aliased distinct pairs. The length-prefixed
        // identity keeps them distinct.
        assert_ne!(dedupe_id("a", "b@c", "build"), dedupe_id("a@b", "c", "build"));
        assert_ne!(dedupe_key("a", "b@c", "build"), dedupe_key("a@b", "c", "build"));
        // Same triple → identical (coalescing still works).
        assert_eq!(dedupe_id("chord", "abc", "build"), dedupe_id("chord", "abc", "build"));
        // A `:` in a component can't alias either (length prefix disambiguates).
        assert_ne!(dedupe_id("a", "b:c", "build"), dedupe_id("a:b", "c", "build"));
        // BLD-ASYNC (TERM #421): mode is part of the identity — build vs test of
        // the SAME module@ref are DISTINCT (and canonicalized: "", "build", and
        // any unknown value all fold to the build token).
        assert_ne!(dedupe_id("m", "r", "build"), dedupe_id("m", "r", "test"));
        assert_eq!(dedupe_id("m", "r", "build"), dedupe_id("m", "r", ""));
        assert_eq!(dedupe_id("m", "r", "build"), dedupe_id("m", "r", "bogus"));
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
    async fn build_and_test_of_the_same_module_ref_are_distinct_never_coalesce() {
        // BLD-ASYNC (TERM #421): mode is part of the dedupe identity. A mode=test
        // submit must get its OWN queue entry, never coalescing onto (or flipping)
        // a pending mode=build job of the same module@ref — otherwise an async
        // test-gate would silently run the wrong work.
        let test_req = |m: &str, r: &str| JobRequest {
            mode: "test".to_string(),
            ..req(m, r, Priority::Normal, false)
        };
        let q = InMemoryQueue::new();

        // A build is pending; a test submit of the SAME module@ref is a NEW job.
        let b = q.enqueue(&req("terminus", "abc", Priority::Normal, false)).await.unwrap();
        let t = q.enqueue(&test_req("terminus", "abc")).await.unwrap();
        assert!(b.created && t.created, "build and test are distinct jobs");
        assert_ne!(b.job_id, t.job_id);
        assert_eq!(q.peek(10).await.unwrap().len(), 2, "both jobs queued (test did not coalesce)");

        // Each mode coalesces only onto its OWN entry.
        let b2 = q.enqueue(&req("terminus", "abc", Priority::Normal, false)).await.unwrap();
        let t2 = q.enqueue(&test_req("terminus", "abc")).await.unwrap();
        assert!(!b2.created && b2.job_id == b.job_id, "a build coalesces onto the build job");
        assert!(!t2.created && t2.job_id == t.job_id, "a test coalesces onto the test job");
        assert_eq!(q.peek(10).await.unwrap().len(), 2, "still exactly the two mode-distinct jobs");

        // The pending build job's mode was never flipped to test.
        let jobs = q.peek(10).await.unwrap();
        let build_job = jobs.iter().find(|j| j.job_id == b.job_id).unwrap();
        let test_job = jobs.iter().find(|j| j.job_id == t.job_id).unwrap();
        assert_eq!(build_job.mode, "build", "build job's mode is intact");
        assert_eq!(test_job.mode, "test", "test job's mode is intact");
        assert_eq!(q.coalesced("terminus", "abc", "build"), 2);
        assert_eq!(q.coalesced("terminus", "abc", "test"), 2);
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

    #[test]
    fn module_lock_key_has_the_exact_prefixed_shape_the_real_lua_builds() {
        // FINDING 1 (review): pin the EXACT key string `module_lock_key`
        // constructs — `"queue:modulelock:chord:abc"` — so a future edit to
        // either the Rust constructor or the real Lua's
        // `lockprefix..module..':'..ref` concatenation can't silently drift
        // apart without a test catching it.
        assert_eq!(module_lock_key("chord", "abc"), "queue:modulelock:chord:abc");
        assert_eq!(module_lock_prefix(), "queue:modulelock:");
    }

    #[tokio::test]
    async fn fake_module_lock_is_stored_under_the_exact_production_key_shape() {
        // FINDING 1 (review): the fake previously stored its module lock under
        // a BARE `"{module}:{ref}"` string — internally consistent, but NOT the
        // same key shape the real Lua uses (`lockprefix..module..':'..ref`), so
        // the PCON-04 re-key tests weren't actually validating the production
        // key format. The fake must now build the SAME prefixed key
        // (`module_lock_key`, the single source of truth both sides call) —
        // proven here by looking the held lock up through that exact
        // constructor via `module_lock_holder`.
        let q = InMemoryQueue::new();
        let j1 = q.enqueue(&req("chord", "abc", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &j1.job_id, "chord", HostRole::Primary, 4).await;
        assert_eq!(
            q.module_lock_holder("chord", "abc"),
            Some(j1.job_id.clone()),
            "the lock must be discoverable under module_lock_key's exact prefixed shape"
        );
        // A lookup under the OLD, un-prefixed shape must NOT find it (proves
        // the fake isn't accidentally ALSO writing the bare form).
        assert_ne!(module_lock_key("chord", "abc"), "chord:abc");
        q.complete(&j1.job_id, "chord", HostRole::Primary, JobState::Done, &tok).await.unwrap();
        assert_eq!(q.module_lock_holder("chord", "abc"), None, "released after complete");
    }

    #[tokio::test]
    async fn same_module_same_ref_serializes_via_module_lock() {
        // PCON-04: the lock is keyed by (module, ref) now, not module alone — two
        // REQUESTS for the exact same ref (a real re-request, e.g. the dedupe
        // pointer already expired/was cleared) still serialize gracefully.
        let q = InMemoryQueue::new();
        let j1 = q.enqueue(&req("chord", "r1", Priority::Normal, false)).await.unwrap();
        // A second, distinct job of the SAME (module, ref) — bypass dedupe by
        // enqueuing under a different mode so it gets its own job id but shares
        // the (module, ref) lock identity.
        let j2 = q.enqueue(&test_req("chord", "r1")).await.unwrap();
        assert_ne!(j1.job_id, j2.job_id, "distinct jobs (different mode) for test setup");

        let tok1 = claim_ok(&q, &j1.job_id, "chord", HostRole::Primary, 4).await;
        assert_eq!(
            q.claim(&j2.job_id, "chord", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::ModuleBusy,
            "same (module, ref) still serializes"
        );
        // Once the first finishes, the (module, ref) lock frees and the second claims.
        q.complete(&j1.job_id, "chord", HostRole::Primary, JobState::Done, &tok1).await.unwrap();
        claim_ok(&q, &j2.job_id, "chord", HostRole::Primary, 4).await;
    }

    #[tokio::test]
    async fn different_ref_same_module_no_longer_serializes_pcon04() {
        // PCON-04: with staging/targets isolated per (module, sha) by PCON-01..03,
        // the module lock is re-keyed to (module, ref) — so two DIFFERENT refs
        // (in practice, two different resolved SHAs) of the SAME module are no
        // longer force-serialized by a blanket module-wide lock; both may build
        // concurrently, bounded only by the host cap.
        let q = InMemoryQueue::new();
        let j1 = q.enqueue(&req("chord", "sha-aaaa", Priority::Normal, false)).await.unwrap();
        let j2 = q.enqueue(&req("chord", "sha-bbbb", Priority::Normal, false)).await.unwrap();
        claim_ok(&q, &j1.job_id, "chord", HostRole::Primary, 4).await;
        // The second, DIFFERENT-ref job of the same module claims successfully
        // too — it is NOT ModuleBusy.
        let outcome2 = q.claim(&j2.job_id, "chord", HostRole::Primary, 4).await.unwrap();
        assert!(
            matches!(outcome2, ClaimOutcome::Claimed { .. }),
            "different-ref job of the same module must not be ModuleBusy: {outcome2:?}"
        );
    }

    #[tokio::test]
    async fn different_refs_resolving_to_the_same_sha_dedupe_onto_one_job_root_cause_fix() {
        // ROOT-CAUSE FIX (review, FINDING 3's queue-level half): two DIFFERENT
        // original refs ("release/v1" and "main") that RESOLVED to the SAME
        // sha at enqueue time must coalesce onto ONE job — not race two
        // independent stagings/builds of the identical commit. This is what
        // makes the FINDING 3 remote-relay dedup meaningful in the first
        // place: without this, two "different" jobs would both try to relay
        // to (and `--delete` into) the same content-addressed remote_source.
        let q = InMemoryQueue::new();
        let sha = "9".repeat(40);
        let a = q.enqueue(&req_with_sha("chord", "release/v1", &sha)).await.unwrap();
        let b = q.enqueue(&req_with_sha("chord", "main", &sha)).await.unwrap();
        assert!(a.created, "first request creates the job");
        assert!(!b.created, "second request (different ref, SAME resolved sha) coalesces");
        assert_eq!(a.job_id, b.job_id, "both land on the identical job");
        assert_eq!(q.peek(10).await.unwrap().len(), 1, "exactly one queued job, not two");

        // And the module lock these two would contend on is the SAME
        // (module, identity) — proven by claiming once and confirming the
        // held lock is discoverable under `module_lock_key(module, sha)`.
        let tok = claim_ok(&q, &a.job_id, "chord", HostRole::Primary, 4).await;
        assert_eq!(q.module_lock_holder("chord", &sha), Some(a.job_id.clone()));
        q.complete(&a.job_id, "chord", HostRole::Primary, JobState::Done, &tok).await.unwrap();
    }

    #[tokio::test]
    async fn different_refs_same_resolved_sha_yield_identical_stage_key_root_cause_fix() {
        // The staging/addressing half of the SAME fix: `queue::job_identity`
        // (what `compiler::mod`'s `stage_key`/`remote_source` derive from) is
        // IDENTICAL for two jobs that share a resolved sha, regardless of
        // their distinct original refs — this is what makes FINDING 3's
        // remote-relay dedup (same directory, safe reuse) actually correct.
        let sha = "9".repeat(40);
        let a = req_with_sha("chord", "release/v1", &sha);
        let b = req_with_sha("chord", "main", &sha);
        assert_eq!(job_identity(&a.git_ref, &a.resolved_sha), job_identity(&b.git_ref, &b.resolved_sha));
        assert_eq!(job_identity(&a.git_ref, &a.resolved_sha), sha);
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

    // BLD/TERM #360: a fresh request carries its cargo bin onto the queued job, and
    // `peek` surfaces it so the scheduler builds the right binary.
    #[tokio::test]
    async fn enqueue_carries_bin_to_peek() {
        let q = InMemoryQueue::new();
        let jr = JobRequest {
            bin: Some("terminus_primary".into()),
            ..req("terminus", "main", Priority::Normal, false)
        };
        q.enqueue(&jr).await.unwrap();
        let queued = q.peek(10).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].bin.as_deref(), Some("terminus_primary"));
    }

    // BLD/TERM #360 (the review-caught coalesce gap): a binless job that already
    // exists (queued OR held) must ADOPT a bin supplied by a later coalescing
    // request — otherwise the scheduler builds `--bin <module>` and fails.
    #[tokio::test]
    async fn coalesce_adopts_later_supplied_bin() {
        let q = InMemoryQueue::new();
        // 1) queued-state coalesce.
        let a = q.enqueue(&req("terminus", "main", Priority::Normal, false)).await.unwrap();
        assert_eq!(q.peek(10).await.unwrap()[0].bin, None, "no bin yet");
        let with_bin = JobRequest {
            bin: Some("terminus_primary".into()),
            ..req("terminus", "main", Priority::Normal, false)
        };
        let b = q.enqueue(&with_bin).await.unwrap();
        assert_eq!(a.job_id, b.job_id, "coalesced onto the same job");
        assert_eq!(
            q.peek(10).await.unwrap()[0].bin.as_deref(),
            Some("terminus_primary"),
            "queued coalesce must adopt the later-supplied bin"
        );

        // 2) held→ready coalesce also adopts a bin arriving with the promotion.
        let q2 = InMemoryQueue::new();
        let held = JobRequest { ready: false, ..req("terminus", "dev", Priority::Normal, false) };
        q2.enqueue(&held).await.unwrap();
        let promote = JobRequest {
            bin: Some("terminus_primary".into()),
            ..req("terminus", "dev", Priority::Normal, false)
        };
        q2.enqueue(&promote).await.unwrap();
        let queued = q2.peek(10).await.unwrap();
        assert_eq!(queued.len(), 1, "promoted to dispatchable");
        assert_eq!(queued[0].bin.as_deref(), Some("terminus_primary"));
    }

    // BLD/TERM #360: a bin supplied while the job is BUILDING must reach the
    // coalesced re-run (release clones the current hash's bin).
    #[tokio::test]
    async fn rerun_carries_bin_supplied_while_building() {
        let q = InMemoryQueue::new();
        let a = q.enqueue(&req("terminus", "main", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &a.job_id, "terminus", HostRole::Primary, 4).await;
        // A later ready request WITH a bin arrives while building → schedules a rerun
        // AND records the bin on the building job.
        let with_bin = JobRequest {
            bin: Some("terminus_primary".into()),
            ..req("terminus", "main", Priority::Normal, false)
        };
        q.enqueue(&with_bin).await.unwrap();
        q.complete(&a.job_id, "terminus", HostRole::Primary, JobState::Done, &tok).await.unwrap();
        let queued = q.peek(10).await.unwrap();
        assert_eq!(queued.len(), 1, "one coalesced re-run");
        assert_eq!(
            queued[0].bin.as_deref(),
            Some("terminus_primary"),
            "the re-run must carry the bin supplied while building"
        );
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
        // one primary host slot. PCON-04: the lock is (module, ref)-keyed, so
        // j1b must share j1's exact ref to still contend the SAME lock — it uses
        // mode=test to still land as a distinct job id.
        let j1 = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let j2 = q.enqueue(&req("m2", "r2", Priority::Normal, false)).await.unwrap();
        let j1b = q.enqueue(&test_req("m1", "r1")).await.unwrap();
        let tok1 = claim_ok(&q, &j1.job_id, "m1", HostRole::Primary, 1).await;
        // While j1 builds: a different module is host-capped, and same (module,
        // ref) is locked.
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
        // PCON-04: j1b shares j1's exact ref (same lock identity), distinguished
        // by mode so it still lands as a separate job id.
        let j1 = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let j1b = q.enqueue(&test_req("m1", "r1")).await.unwrap();
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
        q.requeue(&j.job_id, "bogus", HostRole::Primary, &tok, "idle-lease-unavailable").await.unwrap();
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
    async fn requeue_clears_stale_error_metadata_on_a_building_job() {
        // A requeue of a still-BUILDING job (the intended path) clears any NON-terminal
        // stale error recorded during the prior attempt — the re-queued job carries no
        // prior failure state. (It does NOT finalize-then-requeue: a terminal job must
        // never be requeued — see `requeue_never_resurrects_a_finalized_job`.)
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("harmony", "big", Priority::Normal, true)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "harmony", HostRole::Heavy, 1).await;
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("building"));
        // A transient error was recorded WHILE the job was still building (non-terminal).
        q.set_last_error(&j.job_id, "transient network blip");
        assert_eq!(q.last_error(&j.job_id).as_deref(), Some("transient network blip"));
        // Requeue (job is building ⇒ the double fence passes) clears the stale error.
        q.requeue(&j.job_id, "harmony", HostRole::Heavy, &tok, "idle-lease-unavailable").await.unwrap();
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("queued"));
        assert!(q.last_error(&j.job_id).is_none(), "requeue cleared the stale last_error");
        assert!(!q.has_outcome(&j.job_id), "no terminal outcome recorded");
    }

    #[tokio::test]
    async fn requeue_never_resurrects_a_finalized_job_even_with_a_matching_token() {
        // STATE FENCE: a FINALIZED job must NOT be requeued back to `queued`, even with a
        // MATCHING token. This is the exact race the finding calls out — finalize records
        // a terminal `outcome` while the claim token is STILL present (release hasn't run
        // yet); a stale idle-abort requeue with that matching token must be a NO-OP.
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("harmony", "big", Priority::Normal, true)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "harmony", HostRole::Heavy, 1).await;
        // Finalize (terminal outcome recorded) but do NOT release yet → the token still
        // matches AND a terminal marker exists.
        q.finalize(&j.job_id, JobState::Failed, &tok).await.unwrap();
        assert!(q.has_outcome(&j.job_id), "terminal outcome present, token still matches");
        // Requeue with the SAME (matching) token must be a NO-OP — the fence rejects it.
        q.requeue(&j.job_id, "harmony", HostRole::Heavy, &tok, "idle-lease-unavailable").await.unwrap();
        assert_ne!(
            q.state_of(&j.job_id).as_deref(),
            Some("queued"),
            "finalized job NOT resurrected to queued despite a matching token"
        );
        assert!(q.has_outcome(&j.job_id), "terminal outcome marker preserved (not cleared)");

        // Also verify a fully-RELEASED terminal job (state failed, token cleared) is a
        // no-op too (both fences reject).
        q.release(&j.job_id, "harmony", HostRole::Heavy, &tok).await.unwrap();
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("failed"));
        q.requeue(&j.job_id, "harmony", HostRole::Heavy, &tok, "idle-lease-unavailable").await.unwrap();
        assert_eq!(
            q.state_of(&j.job_id).as_deref(),
            Some("failed"),
            "released terminal job stays failed"
        );
    }

    #[tokio::test]
    async fn requeue_with_a_stale_token_is_a_safe_noop() {
        // A wrong/stale token must not requeue (it would clobber a live re-claim).
        let q = InMemoryQueue::new();
        let j = q.enqueue(&req("m1", "r1", Priority::Normal, false)).await.unwrap();
        let tok = claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 1).await;
        // A stale token no-ops: the job stays building, the slot stays held.
        q.requeue(&j.job_id, "m1", HostRole::Primary, "wrong-token", "x").await.unwrap();
        assert_eq!(q.state_of(&j.job_id).as_deref(), Some("building"));
        assert_eq!(q.inflight_count(HostRole::Primary), 1);
        // The correct token still requeues.
        q.requeue(&j.job_id, "m1", HostRole::Primary, &tok, "x").await.unwrap();
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
        // PCON-04: j2 shares j's exact ref (same lock identity), distinguished by
        // mode so it still lands as a separate job id.
        let j2 = q.enqueue(&test_req("m1", "r1")).await.unwrap();
        claim_ok(&q, &j.job_id, "m1", HostRole::Primary, 4).await;
        assert_eq!(
            q.claim(&j2.job_id, "m1", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::ModuleBusy,
            "the correct (module, ref) lock still serializes same-ref builds"
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
            bin: None,
            force: false,
            mode: "build".to_string(),
            resolved_sha: None,
        };

        // 1) Dedupe/coalesce + monotonic priority bump (real ENQUEUE_LUA).
        let a = q.enqueue(&mk("chord", "abc", Priority::Normal, false, true)).await.unwrap();
        let b = q.enqueue(&mk("chord", "abc", Priority::High, false, true)).await.unwrap();
        assert!(a.created && !b.created && a.job_id == b.job_id);
        let queued = q.peek(10).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].priority, Priority::High, "priority bumped in-place");
        assert_eq!(queued[0].bin, None, "no bin supplied yet");

        // 1b) BLD/TERM #360 against the REAL ENQUEUE_LUA: a later coalescing request
        // that supplies a cargo bin updates the binless job's hash field (HSET in the
        // queued/held coalesce arm), and PEEK_LUA surfaces it (6th field).
        let with_bin = JobRequest {
            bin: Some("chord".into()),
            ..mk("chord", "abc", Priority::High, false, true)
        };
        q.enqueue(&with_bin).await.unwrap();
        assert_eq!(
            q.peek(10).await.unwrap()[0].bin.as_deref(),
            Some("chord"),
            "real ENQUEUE_LUA coalesce must adopt the later-supplied bin"
        );

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

        // 3) Claim writes a fence token + (module, ref) lock (PCON-04); a 2nd
        // claim of the SAME ref (distinguished by mode, so it's a separate job)
        // is ModuleBusy against the real CLAIM_LUA.
        let tok = match q.claim(&a.job_id, "chord", HostRole::Primary, 1).await.unwrap() {
            ClaimOutcome::Claimed { token } => token,
            o => panic!("{o:?}"),
        };
        let j2 = JobRequest {
            mode: "test".to_string(),
            ..mk("chord", "abc", Priority::Normal, false, true)
        };
        let j2 = q.enqueue(&j2).await.unwrap();
        assert_eq!(
            q.claim(&j2.job_id, "chord", HostRole::Primary, 4).await.unwrap(),
            ClaimOutcome::ModuleBusy
        );

        // 4) ready=true while building → exactly one re-run after finalize+release.
        q.enqueue(&mk("chord", "abc", Priority::Normal, false, true)).await.unwrap();
        q.finalize(&a.job_id, JobState::Done, &tok).await.unwrap();
        q.release(&a.job_id, "chord", HostRole::Primary, &tok).await.unwrap();
        let after = q.peek(10).await.unwrap();
        // Disambiguate from the still-queued mode=test `j2` (same module/ref,
        // different mode/lock-sharing but a distinct dedupe entry) via `mode`.
        let rerun = after
            .iter()
            .find(|j| j.module == "chord" && j.git_ref == "abc" && j.mode == "build")
            .unwrap();
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
        let dedupe_key = super::dedupe_key("harmony", "h1", "build");
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
    async fn redis_concurrent_claim_serializes_same_module_ref() {
        // B4 / PCON-04: two schedulers race to claim the SAME (module, ref)
        // (distinguished only by mode, so each gets its own job id), against the
        // real CLAIM_LUA — exactly one wins, the other is ModuleBusy (the
        // (module, ref) lock serializes under real concurrency).
        let Some(server) = EphemeralRedis::start() else {
            eprintln!("SKIP redis_concurrent_claim: redis-server not installed");
            return;
        };
        let backend =
            RedisBackend::build(&server.url(), None, 0, 1, Duration::from_millis(500)).unwrap();
        let _: () = raw(&backend, "FLUSHALL", &[]).await;
        let q = Arc::new(RedisQueue::new(backend.clone()));
        let mk = |mode: &str| JobRequest {
            module: "chord".into(),
            git_ref: "r1".into(),
            priority: Priority::Normal,
            heavy: false,
            ready: true,
            bin: None,
            force: false,
            mode: mode.to_string(),
            resolved_sha: None,
        };
        let j1 = q.enqueue(&mk("build")).await.unwrap();
        let j2 = q.enqueue(&mk("test")).await.unwrap();
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
        assert_eq!(claimed, 1, "exactly one racer claims the (module, ref) lock: {ra:?} / {rb:?}");
        assert_eq!(busy, 1, "the other is serialized out (ModuleBusy): {ra:?} / {rb:?}");
    }

    #[tokio::test]
    async fn redis_different_ref_same_module_claims_concurrently_pcon04() {
        // PCON-04, against the real CLAIM_LUA: two DIFFERENT refs (in practice,
        // two different resolved SHAs — PCON-01..03 isolate their stage/target
        // dirs) of the SAME module both claim successfully — the (module, ref)
        // lock no longer force-serializes them.
        let Some(server) = EphemeralRedis::start() else {
            eprintln!("SKIP redis_different_ref_same_module: redis-server not installed");
            return;
        };
        let backend =
            RedisBackend::build(&server.url(), None, 0, 1, Duration::from_millis(500)).unwrap();
        let _: () = raw(&backend, "FLUSHALL", &[]).await;
        let q = RedisQueue::new(backend.clone());
        let mk = |r: &str| JobRequest {
            module: "chord".into(),
            git_ref: r.into(),
            priority: Priority::Normal,
            heavy: false,
            ready: true,
            bin: None,
            force: false,
            mode: "build".to_string(),
            resolved_sha: None,
        };
        let j1 = q.enqueue(&mk("sha-aaaa")).await.unwrap();
        let j2 = q.enqueue(&mk("sha-bbbb")).await.unwrap();
        let r1 = q.claim(&j1.job_id, "chord", HostRole::Primary, 4).await.unwrap();
        assert!(matches!(r1, ClaimOutcome::Claimed { .. }), "{r1:?}");
        let r2 = q.claim(&j2.job_id, "chord", HostRole::Primary, 4).await.unwrap();
        assert!(
            matches!(r2, ClaimOutcome::Claimed { .. }),
            "different-ref job of the same module must not be ModuleBusy: {r2:?}"
        );
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
            dedupe_key("chord", "abc", "build"),
            job_key("id"),
            module_lock_key("chord", "abc"),
            host_set_key(HostRole::Heavy),
        ] {
            assert!(k.starts_with("queue:"), "{k} must be under the Queue namespace");
        }
    }
}
