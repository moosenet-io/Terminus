## `compiler_request` — queue + scheduler (BLD-06)

`compiler_build` is the single build *door*; `compiler_request` is how agents *ask* for a
build without racing each other. Multiple agents mark a `module@ref` "ready for a compiler
run"; the request is enqueued durably and the **scheduler** turns queued readiness into
gracefully-serialized builds.

```
compiler_request(module, ref, priority="normal", host="auto", fast=false, ready=true)
  → { job_id, created, coalesced, module, ref, priority, heavy, ready }
```

- **Durable queue** — the shared Redis provisioned by BLD-20, under the reserved
  `Namespace::Queue` (`queue:*`, logical DB `noeviction`) so a queued build is never evicted
  under memory pressure. When Redis is not configured, `compiler_request` reports
  `NotConfigured` and the scheduler does not run — it never silently drops a build request.
- **Dedupe / coalesce** — requests are deduped by `module@ref`: several agents marking the
  same `module@ref` ready coalesce into ONE run (the coalesce count is tracked). `ready=false`
  records the intent as *held* until a later `ready=true` for the same `module@ref` promotes it.
- **Atomicity** — enqueue+dedupe+coalesce, claim (queued→building under a per-module lock and
  a per-host cap), and complete (release lock + slot) are each a single atomic Lua script, so
  concurrent agents/schedulers can never double-enqueue, start two conflicting builds of one
  module, or exceed a host's cap.
- **Graceful serialization + windows** — the scheduler dispatches small/primary builds
  immediately (bounded by `BUILD_HOST_CAP_PRIMARY`), and holds **heavy** builds (the ones the
  `select_role` heuristic routes to the heavy host) for a configured window
  (`BUILD_WINDOW_HOURS`, e.g. `22-24,0-6`, wrap-aware; `0-24` is all-day) or a fleet-quiet
  signal (`BUILD_FLEET_QUIET`). A malformed window token — including a never-active `start=24`
  (e.g. `24-6`; `24` is valid only as an end) — is dropped and logged loudly rather than
  silently stranding heavy builds. One build per host at a time unless the per-host cap is
  raised. A window closing mid-build never cancels the in-flight build — it only stops new heavy
  dispatch. Priority (`low|normal|high`) orders the queue but never preempts a running build.
- **Idle-mode seam (BLD-11)** — a heavy build acquires/releases the heavy host's idle-mode
  lease around the build; that coordination is a clean trait seam (`IdleCoordinator`), a
  no-op by default, wired for real by BLD-11 — and touched only for a heavy build actually
  being dispatched.
- **Lock/host keys derived from the job, module verified** — `claim` derives the per-module
  lock key from the job hash's OWN stored module and VERIFIES the caller's module arg against
  it (a mismatch is refused), so a buggy call can never take a foreign lock and break
  serialization. `release`/`reconcile` likewise derive the module-lock + host-slot keys from
  the job's own stored fields, so a wrong/stale caller arg still frees the correct lock+slot
  and can never wedge the real ones.
- **Heavy classification is safety-authoritative** — an explicit `host=primary` is only a
  preference: a known-heavy module (peak over threshold), an ambiguous/unreadable one, or a
  `fast=true` request is still gated through the heavy (window+cap) path even under
  `host=primary`; only a positively-known-small module fast-paths on primary. Explicit `heavy`
  and `fast` are always heavy.
- **Collision-free dedupe identity** — the per-`(module, ref)` dedupe key is a length-prefixed
  encoding (`{len(module)}:{module}:{ref}`), injective even when a component contains `@`/`:`,
  so distinct pairs never alias and coalesce into one (which would silently drop a build); the
  identical construction is used in Rust and in the release/reconcile Lua.
- **One scheduler loop per process, no pre-Redis wedge** — `register()` spawns the scheduler
  behind a process once-guard that is consumed ONLY on an actual spawn: a `register()` that
  runs before Redis is materialized does not burn the slot, so a later `register()` (once
  config arrives) can still spawn exactly one loop; further calls never double-spawn.
- **No permanent wedge, no double-build on a completion outage** — the claim writes a fence
  token; completion is two individually-atomic, token-fenced, idempotent transitions:
  `finalize` (record the terminal outcome FIRST) then `release` (free the lock/slot) — a
  deliberate two-step design (vs one atomic Lua) that is what lets reconcile tell a finished
  job from a crashed one. `finalize` returns a typed outcome (`Finalized` vs `StaleToken`), so
  a completion attempted with a WRONG/stale token surfaces `Err(StaleToken)` — never a false
  `Ok` that would mask an unfinished build; the genuine in-flight retry of the same correct
  token still succeeds. The queue-layer `complete()` is the sanctioned retrying entry for
  direct callers; the scheduler drives the two steps with its own bounded backoff
  (`BUILD_COMPLETE_RETRY_BASE_MS`/`BUILD_COMPLETE_RETRY_MAX`) and yields (no release) if its
  token has gone stale. As a crash/restart backstop, each tick RECONCILES `building` jobs via
  two paths with **distinct timing**: a job that FINISHED (terminal-outcome marker present)
  but was never released is released **immediately** on the next tick — **never rebuilt**, and
  NOT gated on the stale age — so once Redis recovers a finished job's lock/slot self-heal
  promptly (including on the scheduler's first tick after a restart); a job that CRASHED
  mid-build (no marker) is requeued only once its claim is older than `BUILD_STALE_BUILDING_SECS`
  (clamped UP to a safe floor of max-build-timeout + retry window, so a genuinely-live build is
  never wrongly requeued). The `BUILD_STALE_BUILDING_SECS` age gate applies ONLY to the
  crashed-requeue path. The fence token guarantees a crashed worker's late completion can never
  free a reconciled + re-claimed job's slot, and a double release never underflows the
  host-slot count.
- **Safe heavy classification** — an `auto` request whose heaviness is unreadable/ambiguous
  (unparsable module peak or threshold) is treated as HEAVY (window+cap gated), never run
  immediately on the primary; only a positively-small build (no known peak, or a known peak
  at/under a known threshold) dispatches immediately.
- **Bounded retention** — durable states (`queued`/`building`) never expire; a never-promoted
  `held` intent (a `ready=false` marker) and its dedupe pointer expire after
  `BUILD_HELD_INTENT_TTL_SECS` (a `ready=true` promotion `PERSIST`s them); terminal
  (`done`/`failed`) jobs are retained `BUILD_JOB_RETAIN_SECS` then self-expire. A `ready=false`
  arrival while a build is in flight records intent but does NOT schedule a re-run — only a
  `ready=true` does.
- The durable-queue binding is enforced in code (a test asserts `Namespace::Queue` routes to
  the durable DB), and the actual Lua scripts have a real-Redis contract test that spins an
  ephemeral `redis-server` (auto-skips where none is installed) — it also asserts the durable
  DB runs under a `noeviction` `maxmemory-policy` and that concurrent claims of the same
  module serialize to exactly one winner.
- **`compiler_status`** — the status tool is a separate item (BLD-08); it renders
  `compiler::render_queue_status` over the queue snapshot (queue depth, queued jobs,
  in-flight leases per host).

Scheduler knobs are all config env with safe serialize-everything defaults (cap 1, no heavy
window ⇒ heavy builds wait for a window/quiet signal): `BUILD_HOST_CAP_PRIMARY`,
`BUILD_HOST_CAP_HEAVY`, `BUILD_WINDOW_HOURS`, `BUILD_FLEET_QUIET`, `BUILD_SCHED_INTERVAL_SECS`,
`BUILD_SCHED_PEEK`, `BUILD_JOB_RETAIN_SECS`, `BUILD_HELD_INTENT_TTL_SECS`,
`BUILD_STALE_BUILDING_SECS` (clamped to a safe floor), `BUILD_COMPLETE_RETRY_BASE_MS`,
`BUILD_COMPLETE_RETRY_MAX` — no infrastructure literals (S1).
Redis endpoint + password come from the vault-materialized env via the shared `RedisBackend` (S7).
