# Gitea merge queue — ordering + delay for concurrent merges
plane_project: TERM
module: Terminus
prefix: GMQ
spec_id: S120-gitea-merge-queue

## Metadata
- **Author:** Claude (orchestrator), for <operator> (Moose)
- **Session:** S120 · **Date:** 2026-07-18 · **Spec version:** v1.0
- **Estimated total:** ~16h across 5 items (GMQ-01..05); implementer = Sonnet, reviews via `review_run`.
- **North-Star layer:** kernel — this is Terminus-internal forge tooling (the single git door);
  no Module Contract needed.
- **Context:** When multiple build agents merge PRs into the same base concurrently (observed
  live this session driving an 18-item harmony-web sprint), their merges RACE: agent A merges,
  `main` advances, and agent B's already-open PR is now stale-based — a `main..branch` diff
  falsely shows unrelated files being "removed", and B must re-merge current `main` into its
  branch and re-review. This is pure coordination waste. Add a **merge queue** to the single
  git door (`gitea_merge_pr`) that (a) SERIALIZES merges per base branch, (b) ORDERS waiters
  FIFO (enqueue order, priority-weighted), (c) enforces a configurable MIN-DELAY between merges
  to the same base (let `main` settle / mirror/CI react), and (d) guards against a STALE-BASE
  merge by re-checking PR mergeability inside the critical section. It reuses the proven Redis
  primitives from the compiler queue and the rate-limiter — no new infra. It DEGRADES OPEN:
  when Redis is unconfigured/unreachable, `gitea_merge_pr` behaves exactly as today (unqueued),
  so nothing regresses on a Redis-less deployment.

## Pre-flight (verified 2026-07-18)
- Repo: `moosenet/Terminus`, branch `main` (gitea remote — `GITEA_URL`; never branch from a
  public mirror). Root crate `terminus-rs` (lib `terminus_rs`) + members `terminus-client`,
  `terminus-worker-sdk`; toolchain pinned `1.97.0` (`rust-toolchain.toml`).
- Redis is SERVER-SIDE only. `RedisBackend::from_env() -> Option<Arc<Self>>` is a process-global
  `OnceLock` singleton (`src/redis/mod.rs:236`); `terminus-client` has NO redis dep. The
  gitea `register()` is wired into `register_all` (the Chord-embedded server registry) AND
  `register_personal` — `gitea_merge_pr` executes in the `terminus-rs` server process where
  Redis IS available. This is why the queue is viable exactly where merge runs.
- Reused primitives (all real, cited): connection/namespacing `RedisBackend::with_conn(
  Namespace::Queue, …)` + `Namespace::Queue.key(...)` (`src/redis/mod.rs:314,105`); ordering
  ZSET score `seq - prank*1e12` + `queue:seq` INCR (`src/compiler/queue.rs:632,709`); atomic
  lock + fence-token + crash-reconcile Lua (`CLAIM_LUA`/`release`/`reconcile`,
  `src/compiler/queue.rs:697,381,464`); spacing via `RedisRateLimiter::new(backend, capacity,
  refill_per_sec)` (`src/ratelimit/mod.rs:109`); config accessors `src/config.rs` (pattern at
  `:748`), env documented in `.env.example`.
- Merge tool: `MergePr` struct, tool name `gitea_merge_pr`, params `{owner?, repo, pr, style?,
  message?, identity?}` (`src/gitea/mod.rs:1560`). It POSTs `/repos/{o}/{r}/pulls/{pr}/merge`
  with `{"Do": style, "MergeMessageField": message?}` inline in `execute` (`:1596-1652`), then
  best-effort fires `forge::mirror::tools::dispatch_mirror_action("sync-source", …)` — the
  single sanctioned "a gated merge just completed" hook (Stage 6). NOTE the existing bug at
  `:1654`: the success string reports `base` using `style`.
- Mergeability data is already available: `GiteaClient::get` → `GiteaPullRequest`
  (`src/gitea/types.rs`) carries `mergeable: Option<bool>`, `merged: bool`, `head`, `base`,
  `updated_at`.
- Guarded-tool layer (`src/approval.rs`): `gate(tool_name, &args, summary)`; `GUARDED_BARE_NAMES`
  (`:60`) — `gitea_merge_pr` is NOT guarded today (leave it unguarded; the queue is not an
  approval gate).
- Tests: inline `#[cfg(test)] mod tests` per module. Gitea tools use httpmock
  (`src/gitea/mod.rs:5031`); Redis/queue logic uses an in-memory fake (`InMemoryQueue`,
  `src/compiler/queue.rs:1428`) so tests need NO live Redis; live-Redis tests use
  `RedisBackend::from_env_uncached()`.
- Build gate: `cargo test -p terminus-rs` on the dev box's OWN capped host (or via the
  `compiler_*` single build door) — NEVER a bare `cargo build`/`cargo test` on a shared build
  host (`docs/build.md`).
- `mirror_ready`: Terminus mirrors (Stage 7d) — no PII in any spec item (all env-var names).

---

## §1 Design (the mechanism)

A merge to base `B` in repo `owner/repo` acquires a **per-base critical section** keyed
`merge:{owner}/{repo}/{B}` before performing the Gitea merge POST. Inside it:
1. **Order** — waiters take a FIFO ticket (`INCR queue:merge:seq:{key}`) and proceed in ticket
   order (priority-weighted like the compiler ZSET: `score = seq - prank*1e12`). A bounded wait
   with backoff; a max-wait ceiling returns a clear "queue busy, retry" rather than hanging.
2. **Serialize** — an atomic Redis lock (`SET queue:merge:lock:{key} <fence-token> NX PX <ttl>`,
   or a 3-line Lua mirroring `CLAIM_LUA`) so exactly one merge per base is in flight; released
   with a fence-token guard (a stale holder can never free a re-taken lock); a TTL + reconcile
   backstop frees a crashed holder.
3. **Space** — enforce a MIN-DELAY between successive merges to the same base: reuse
   `RedisRateLimiter::new(backend, 1, 1.0/min_delay_secs)` keyed on `key` (capacity 1, no burst)
   OR a `queue:merge:last:{key}` last-merge timestamp check; on "too soon" sleep the remainder
   (bounded) then proceed.
4. **Stale-base guard** — re-fetch the PR (`GiteaClient::get`) INSIDE the section and require
   `merged == false` && `mergeable != Some(false)` before POSTing; if not mergeable, release and
   return a clear "PR not mergeable (stale base / conflict) — rebase and retry" error instead of
   a racing merge.
5. **Merge** — the existing POST + `sync-source` hook (factored into a shared helper, GMQ-01).
6. **Release** — free the lock (fence no-op on mismatch), stamp the last-merge time.

**Degrade-open:** every step is gated on `RedisBackend::from_env().is_some()`. `None` (no Redis)
⇒ skip queue/lock/spacing entirely and merge exactly as today. A Redis op returning `Err(())`
(unreachable) ⇒ log once + proceed unqueued (availability over strictness for a short merge).

Config (`src/config.rs` accessors + `.env.example`), all optional with safe defaults:
- `GITEA_MERGE_QUEUE_ENABLED` (default true when Redis present; false disables the queue path).
- `GITEA_MERGE_QUEUE_MIN_DELAY_SECS` (default e.g. `8`) — min spacing between same-base merges.
- `GITEA_MERGE_QUEUE_LOCK_TTL_SECS` (default e.g. `120`) — lock TTL / crash backstop.
- `GITEA_MERGE_QUEUE_MAX_WAIT_SECS` (default e.g. `300`) — waiter ceiling before "queue busy".
Per-call overrides on `gitea_merge_pr` (all optional, additive): `queue_key` (default
`{owner}/{repo}/{base}`), `min_delay_secs`, `priority`, `no_queue` (bypass — e.g. an emergency
merge), so an agent can tune ordering without env changes.

---

## GMQ-01: Factor the merge into a shared `GiteaClient::merge_pull` helper (+ fix base bug)
- **Priority:** High
- **Labels:** terminus, gitea, refactor
- **Agent:** claude
- **Estimate:** 2h
- **Description:** Extract the inline merge body from `MergePr::execute` into a reusable
  `GiteaClient::merge_pull(owner, repo, pr, style, message) -> Result<GiteaMergeOutcome, …>`
  next to `create_pull`, so both the synchronous tool and the future queue worker call ONE
  path. Fix the existing success-string bug (`base` reported as `style`, `src/gitea/mod.rs:1654`).
  Keep the best-effort `sync-source` mirror hook firing after a successful merge, unchanged.
  NO behavior change to `gitea_merge_pr` yet — this is a pure refactor + bugfix that the rest
  of the spec builds on.

  ## FILES
  - `src/gitea/mod.rs` — `MergePr::execute` calls the new helper; remove the inline POST
  - `src/gitea/client.rs` (or wherever `GiteaClient::create_pull` lives) — add `merge_pull`
  - `src/gitea/types.rs` — a small `GiteaMergeOutcome { merged: bool, message: String, sha? }` if useful

  ## APPROACH
  1. Move the `/pulls/{pr}/merge` POST + response handling into `GiteaClient::merge_pull`,
     mirroring `create_pull`'s shape (auth via the resolved identity, error mapping).
  2. `MergePr::execute` resolves identity/owner as today, then calls `merge_pull` and formats
     the result string — correcting `base` to the real base (not `style`).
  3. Keep the `sync-source` dispatch at the tool layer (or move it into the helper — pick one,
     document it) so it still fires exactly once per successful merge.

  ## TEST PLAN
  - `cargo test -p terminus-rs gitea` (dev-box capped host) — the existing
    `merge_pr_succeeds_even_when_sync_source_is_unconfigured` test still passes.
  - Add a unit test asserting the success string reports the correct base branch (regression
    for the `:1654` bug), using the httpmock Gitea pattern.
  - `grep` for hardcoded IPs/secrets in changed files → 0.

  ## EDGE CASES
  - A 409/blocked merge from Gitea maps to a clear ToolError, not a panic.
  - `sync-source` unconfigured still returns a successful merge (unchanged contract).

- **Acceptance criteria:**
  - [ ] `GiteaClient::merge_pull` exists and is the single merge code path
  - [ ] `gitea_merge_pr` behavior is unchanged except the success string now reports the real base
  - [ ] The `:1654` base=style bug is fixed with a regression test
  - [ ] `sync-source` still fires exactly once after a successful merge
  - [ ] All existing tests pass; no hardcoded infrastructure values in new/modified code

## GMQ-02: Redis-backed per-base merge lock + FIFO ordering (the queue core)
- **Priority:** High
- **Labels:** terminus, gitea, redis, concurrency
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** GMQ-01
- **Description:** Add a `MergeQueue` (new module `src/gitea/merge_queue.rs`) providing a
  per-base **critical section**: FIFO ticket ordering + an atomic serialization lock with a
  fence token and a TTL/reconcile crash backstop, reusing the compiler-queue Redis primitives.
  It exposes `async fn with_merge_slot<F>(key, priority, cfg, f) -> Result<T>` that acquires the
  slot (respecting order + lock), runs `f` (the merge), and releases — or degrades open when
  Redis is absent.

  ## FILES
  - `src/gitea/merge_queue.rs` — NEW: `MergeQueue`, `MergeQueueConfig`, the Lua + ticket logic
  - `src/gitea/mod.rs` — module wiring
  - `src/config.rs` — `gitea_merge_queue_enabled()`, `_lock_ttl_secs()`, `_max_wait_secs()` accessors
  - `.env.example` — document `GITEA_MERGE_QUEUE_ENABLED`, `_LOCK_TTL_SECS`, `_MAX_WAIT_SECS`

  ## APPROACH
  1. `MergeQueue::from_env() -> Option<Self>` = `RedisBackend::from_env().map(Self::new)` (mirrors
     `RedisQueue::from_env`). Keys via `Namespace::Queue.key("merge:...")`.
  2. Ticket: `INCR queue:merge:seq:{key}` for FIFO; priority-weighted position like the compiler
     ZSET (`score = seq - priority*1e12`). A bounded poll loop with backoff waits for the lock,
     honoring `max_wait_secs` (return `MergeQueueError::Busy` past the ceiling).
  3. Lock: atomic `SET queue:merge:lock:{key} <fence> NX PX <ttl*1000>` (or a small Lua like
     `CLAIM_LUA`); release only if the stored fence matches (Lua compare-and-del); a
     `reconcile`-style TTL backstop already frees a crashed holder.
  4. `with_merge_slot`: acquire → run `f` → release in a `Drop`/finally so a panic/early-return
     never leaks the lock. Redis `None` ⇒ just run `f` (degrade open); a mid-op Redis error ⇒
     log once + run `f` unqueued.

  ## TEST PLAN
  - Unit tests with an in-memory fake (mirror `InMemoryQueue`, `src/compiler/queue.rs:1428`):
    two concurrent `with_merge_slot` calls on the same key run STRICTLY serially and in ticket
    order; a third key runs concurrently; a simulated holder-crash (TTL expiry) lets the next
    waiter proceed; fence mismatch does not free another's lock.
  - Degrade-open test: with Redis "absent", `with_merge_slot` runs `f` immediately.
  - `cargo test -p terminus-rs` on the capped dev-box host.

  ## EDGE CASES
  - Lock TTL expiry mid-merge (slow Gitea) — the fence token prevents a late release from
    clobbering the next holder; document the TTL must exceed a realistic merge time.
  - `max_wait_secs` exceeded → `Busy` with a retry hint, never an infinite wait.
  - Redis flaps between acquire and release → release is best-effort; the TTL backstop covers it.

- **Acceptance criteria:**
  - [ ] `MergeQueue::with_merge_slot` serializes same-key sections and orders waiters FIFO/priority
  - [ ] Fence-token release + TTL reconcile handle a crashed/slow holder safely
  - [ ] Degrades open (runs immediately) when Redis is absent or errors
  - [ ] Concurrency + ordering + degrade tests pass with the in-memory fake (no live Redis needed)
  - [ ] Config accessors + `.env.example` added; no hardcoded infrastructure values

## GMQ-03: Configurable min-delay spacing + stale-base mergeability guard
- **Priority:** High
- **Labels:** terminus, gitea, redis, safety
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** GMQ-02
- **Description:** Inside the critical section, enforce a configurable MIN-DELAY between
  successive merges to the same base (spacing) and a STALE-BASE guard that re-checks PR
  mergeability before the merge POST — the two behaviors that directly kill the observed
  stale-base re-merge race.

  ## FILES
  - `src/gitea/merge_queue.rs` — spacing (rate-limiter or last-merge timestamp) + the guard hook
  - `src/gitea/mod.rs` — `MergePr::execute` calls `get` for the mergeability re-check
  - `src/config.rs` — `gitea_merge_queue_min_delay_secs()`
  - `.env.example` — `GITEA_MERGE_QUEUE_MIN_DELAY_SECS`

  ## APPROACH
  1. Spacing: reuse `RedisRateLimiter::new(backend, 1, 1.0/min_delay_secs)` keyed on the merge
     key; on `Limited{retry_after_secs}` sleep the remainder (bounded by `max_wait_secs`) then
     proceed. OR a `queue:merge:last:{key}` timestamp compared to `now`. Pick one, document it.
  2. Stale-base guard: after acquiring the slot and before merging, `GiteaClient::get` the PR;
     if `merged == true` → return a clean "already merged" success (idempotent); if `mergeable
     == Some(false)` → release + `MergeQueueError::NotMergeable` with a "rebase current base and
     retry" message; else proceed.
  3. Stamp `queue:merge:last:{key}` after a successful merge for the next waiter's spacing.

  ## TEST PLAN
  - Unit: two merges to the same key are spaced ≥ `min_delay_secs` (fake clock / rate-limiter
    fake); a not-mergeable PR returns `NotMergeable` and does NOT POST; an already-merged PR is
    idempotent.
  - Spacing disabled (`min_delay_secs=0`) → no artificial delay.
  - `cargo test -p terminus-rs`.

  ## EDGE CASES
  - Gitea reports `mergeable: None` (still computing) → treat as "proceed" (don't block on an
    unknown), documented — the lock already serializes so the base is current.
  - Spacing sleep must not exceed `max_wait_secs`; if it would, return `Busy`.
  - Clock skew on the last-merge timestamp — use the Redis server clock via the rate-limiter Lua
    (already server-clock based), not the app clock.

- **Acceptance criteria:**
  - [ ] Successive same-base merges are spaced ≥ `GITEA_MERGE_QUEUE_MIN_DELAY_SECS`
  - [ ] A non-mergeable (stale-base/conflict) PR returns a clear NotMergeable error, no racing merge
  - [ ] An already-merged PR is idempotent (clean success, no double-merge)
  - [ ] `mergeable: None` proceeds (serialized base is current); spacing honors `max_wait_secs`
  - [ ] All tests pass; no hardcoded infrastructure values

## GMQ-04: Wire the queue into `gitea_merge_pr` (additive params) + docs
- **Priority:** High
- **Labels:** terminus, gitea, docs
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** GMQ-03
- **Description:** Wrap `MergePr::execute`'s merge in `MergeQueue::with_merge_slot` and add the
  additive per-call params. Off-path (no Redis) behavior is byte-identical to today. Update the
  tool description, `.env.example`, and the forge/git docs so agents know merges are ordered +
  spaced.

  ## FILES
  - `src/gitea/mod.rs` — `MergePr::parameters` gains `queue_key?`, `min_delay_secs?`, `priority?`,
    `no_queue?`; `execute` derives `key` (default `{owner}/{repo}/{base}`), then
    `queue.with_merge_slot(key, priority, cfg, || client.merge_pull(...))`
  - `.env.example` — full `GITEA_MERGE_QUEUE_*` block with defaults + one-line explanations
  - `docs/build.md` or `docs/forge.md` (README of the git tools) — a "merge ordering + delay" note

  ## APPROACH
  1. Add the optional schema properties (no `required` change → all optional/back-compatible).
  2. Derive the queue key: explicit `queue_key`, else `{owner}/{repo}/{base}` (fetch the PR's
     base via `get` — already needed for the GMQ-03 guard, reuse that fetch).
  3. `no_queue == true` OR queue disabled OR Redis absent → call `merge_pull` directly (today's path).
  4. Document that this is the single git-merge door and how the ordering/delay behaves.

  ## TEST PLAN
  - Unit (httpmock Gitea): `gitea_merge_pr` with Redis absent merges exactly as before (existing
    tests unchanged); with the queue fake, a merge acquires+releases the slot around the POST;
    `no_queue:true` bypasses the queue.
  - README/docs updated (grep the docs file for the new env vars).
  - `cargo test -p terminus-rs`.

  ## EDGE CASES
  - `queue_key` collision across repos is impossible (default includes owner/repo).
  - A caller passing `min_delay_secs` overrides the env default for that call only.
  - Back-compat: an existing caller passing only `{repo, pr}` still works unchanged.

- **Acceptance criteria:**
  - [ ] `gitea_merge_pr` routes through the queue when enabled+Redis present; identical to today otherwise
  - [ ] New params `queue_key`/`min_delay_secs`/`priority`/`no_queue` are optional + documented
  - [ ] `.env.example` documents all `GITEA_MERGE_QUEUE_*` knobs with defaults
  - [ ] Tool description + forge docs explain merge ordering + delay
  - [ ] All existing `gitea_merge_pr` tests pass unchanged; no hardcoded infrastructure values
  - [ ] README/docs updated to document the new behavior (user-facing tool change)

## GMQ-05: `gitea_merge_queue_status` read tool (observability)
- **Priority:** Medium
- **Labels:** terminus, gitea, observability
- **Agent:** claude
- **Estimate:** 2h
- **Blocked by:** GMQ-02
- **Description:** A read-only tool to inspect the merge queue for a base key: current lock
  holder (if any) + TTL, the next FIFO ticket, and the last-merge timestamp / next-allowed
  time under the spacing rule. Lets an agent/operator see why a merge is waiting.

  ## FILES
  - `src/gitea/mod.rs` — new `MergeQueueStatus` tool (`gitea_merge_queue_status`), registered
  - `src/gitea/merge_queue.rs` — a `status(key) -> MergeQueueSnapshot` read method

  ## APPROACH
  1. `status(key)` reads (no mutation): lock key + PTTL, `queue:merge:seq:{key}`, last-merge ts.
  2. Tool params `{owner?, repo, base}` → derives the key; returns structured JSON + a text summary.
  3. Redis absent → a clear "queue not active (no Redis / disabled)" response, not an error.

  ## TEST PLAN
  - Unit (in-memory fake): status reflects a held lock, a free key, and the spacing next-allowed time.
  - `cargo test -p terminus-rs`.

  ## EDGE CASES
  - Unknown/never-used key → empty snapshot (no lock, seq 0), not an error.
  - Redis absent → "not active" response.

- **Acceptance criteria:**
  - [ ] `gitea_merge_queue_status` returns lock holder/TTL, next ticket, next-allowed-merge time
  - [ ] Read-only (no mutation); Redis-absent returns a clear "not active" response
  - [ ] Structured + text output; tests pass; no hardcoded infrastructure values
