# Constellation CI/CD — compiler tool, idle-mode, seamless fleet deploy
plane_project: TERM
module: Terminus
prefix: BLD
spec_id: S117-constellation-cicd-compiler

## Metadata
- **Author:** <operator> (Moose)
- **Session:** S117
- **Date:** 2026-07-12
- **Module version:** Terminus v1.3.x
- **Estimated total:** ~64h autonomous agent work (18 items, 6 phases; multi-repo)
- **Context:** The fleet has no disciplined build/deploy system. Ad-hoc `cargo build` on
  shared hosts caused disk-fill, OOM, toolchain drift (a stray `rustup update` broke a build
  host mid-session), and Plex-contention/swap-thrash on the shared node. A baseline sweep found:
  only one host has real build headroom, deploy containers are 2–4 GB (can't compile), rustc
  versions differ across hosts (`1.95/1.96/1.97`), and there is no shared build cache. This spec
  builds a **Terminus compiler tool** (queue + scheduler + sccache + capped builds + publish/
  promote), an **idle-mode** API on the LLM proxy + test harness that frees the big host's RAM/GPU
  on demand, realigns the **constellation-updater** to *fetch prebuilt artifacts* (build-once →
  publish → consume) for seamless health-gated fleet updates, adds a **fleet/deploy GUI**, and
  standardizes container storage/config. Grounded in the measured baseline (release builds peak
  3–5 GB; the RAM ceiling is the *test-gate* build × parallelism — a knob, not a fixed need).

## Pre-flight
- Repos touched (multi-repo — each item's ## FILES names its repo): `moosenet/Terminus` (compiler
  tool, updater realign), `moosenet/Chord` (idle-mode), `moosenet/harmony` (fleet GUI + API),
  `moosenet/constellation-updater` (fetch-artifact mode). Build/test each on its own build host
  through the NEW compiler path once it exists; until then, per v3.21.
- Vault/config keys required (names only — S7): `SCCACHE_REDIS` (the existing terminus Redis
  endpoint — CONFIRM before Phase 0), `BUILD_DATASET_ROOT` (the appdata-backed shared build dir),
  `BUILD_HOST_PRIMARY` / `BUILD_HOST_HEAVY` (role → host resolution), `RUST_TOOLCHAIN_PINNED`.
- Infrastructure: the shared build dataset exists (pre-created at `${BUILD_DATASET_ROOT}`); the
  primary build host's rootfs is already block-image-over-NFS ext4 (exec-safe, TB-capacity); the
  heavy build host has the pinned toolchain + musl cross-toolchain.
- **HARD OPERATIONAL RULE (applies to every item): NEVER restart or migrate the primary dev/build
  container (`BUILD_HOST_PRIMARY`) while an agent session or build is active on it — a restart kills
  the running agent and its build. Mount/migration items are OPERATOR, scheduled for a quiescent
  window.**

## Architecture (the target system)
Two-tier, build-once → publish → consume:
- **Primary builder** (dev box, ample fast appdata-backed ext4 disk, moderate RAM): default host;
  builds inside a resource-capped cgroup; worktree + cargo target on its local ext4 (already
  appdata-backed → capacity + exec-safety, no tmpfs needed); publishes artifacts to the shared
  dataset.
- **Heavy builder** (big-RAM/GPU host): for big/fast builds; the compiler calls **idle-mode** to
  free ~120 GB RAM + GPU, builds at full parallelism on tmpfs (or an appdata-backed build disk),
  then reactivates services.
- **Deploy hosts** (2–4 GB): never build — they *fetch* the prebuilt artifact.
Storage rule: **block-over-NFS (raw image → ext4) for live cargo targets** (exec/lock/mtime safe);
**file-level NFS shared dir only for source-staging + sccache + artifact publish/consume** (never a
live target). sccache backend = the existing terminus Redis (no new service). One pinned rustc
fleet-wide.

---

## Phase 0 — Foundations (infra + toolchain; mostly operator)

### BLD-01: Shared build dataset — RW export + mounts (operator)
- **Priority:** Critical
- **Labels:** terminus, cicd, infra, storage
- **Agent:** <operator>
- **Estimate:** 1h
- **Type:** human-action
- **Description:** Make the pre-created shared build dataset writable by the dev-box build uid and
  mounted on the build/deploy hosts, so the compiler can publish artifacts and deploy hosts can
  fetch them.
- **Steps:**
  1. On the NAS, export the appdata build dataset (`${BUILD_DATASET_ROOT}`) with the dev-box build
     uid mapped for write (`mapall`/`maproot` to that uid, or an equivalent LXC idmap bind-mount) —
     currently root-only-writable.
  2. Add a file-level mount of `${BUILD_DATASET_ROOT}` into the build/deploy containers:
     dev-box **RW**, deploy hosts **RO**. **Do this in a quiescent window** — a container mount-point
     change may require a container restart; **NEVER restart `BUILD_HOST_PRIMARY` while an agent/build
     is active** (schedule it, or accept the interim relay-publish path in BLD-05 that needs no mount).
  3. Verify: dev-box uid can create+write a file under `${BUILD_DATASET_ROOT}/artifacts/`; deploy
     hosts can read it.
- **Acceptance criteria:**
  - [ ] `${BUILD_DATASET_ROOT}` is writable by the dev-box build uid and readable by deploy hosts
  - [ ] No hardcoded infrastructure values recorded in any tracked file (paths via config vars)
  - [ ] `BUILD_HOST_PRIMARY` was NOT restarted while an agent/build was active

### BLD-02: Pin one rustc toolchain fleet-wide
- **Priority:** High
- **Labels:** terminus, cicd, toolchain
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Eliminate the `1.95/1.96/1.97` drift (which breaks cross-host caching and caused
  a mid-session build break). Add a `rust-toolchain.toml` pinning one version to every buildable
  repo; the compiler tool ensures that exact version is installed before building (no ad-hoc
  `rustup update`).

  ## FILES
  - `rust-toolchain.toml` (new) in each buildable repo: `moosenet/Terminus`, `moosenet/Chord`,
    `moosenet/harmony`, `moosenet/lumina-constellation`, `moosenet/Muse` — pin `channel` +
    `targets = ["x86_64-unknown-linux-musl"]` + `components = ["rust-src"]`.
  - `docs/build.md` (new, Terminus) — the pinned-toolchain + no-ad-hoc-rustup rule.

  ## APPROACH
  1. Choose the pinned channel (the newest known-good across hosts); add `rust-toolchain.toml` to
     each repo (multi-repo → one PR per repo).
  2. Document: the compiler tool runs `rustup toolchain install $(pin)` idempotently before a build;
     humans/agents never `rustup update` on a shared build host (that is what broke a host).
  3. No source-code hostnames/IPs; toolchain channel is a version string only.

  ## TEST PLAN
  - On each build host, `rustup show` after a compiler build reports the pinned version.
  - `cargo build` in each repo resolves the pinned toolchain (auto-installs if missing).
  - Verify no hardcoded infrastructure values in new files.

  ## EDGE CASES
  - A host missing the pinned toolchain → the compiler installs it (idempotent), never silently
    uses a different one.
  - A repo that legitimately needs a different version → its own `rust-toolchain.toml` overrides;
    sccache keys include the compiler version so caches don't cross-contaminate.

- **Acceptance criteria:**
  - [ ] Every buildable repo carries a `rust-toolchain.toml` pinning one channel + musl target
  - [ ] The compiler tool installs the pinned toolchain idempotently before building
  - [ ] `docs/build.md` documents the no-ad-hoc-`rustup`-on-shared-hosts rule (README/docs updated)
  - [ ] No hardcoded infrastructure values in new/modified files
  - [ ] All existing tests still pass

### BLD-03: Build-host provisioning — musl-tools + sccache + toolchain (operator/ops)
- **Priority:** High
- **Labels:** terminus, cicd, infra
- **Agent:** <operator>
- **Estimate:** 1h
- **Type:** human-action
- **Description:** Ensure both build hosts have the prerequisites the compiler assumes.
- **Steps:**
  1. Install the musl cross-toolchain (`musl-tools`) on `BUILD_HOST_PRIMARY` (via the sanctioned
     admin path — no sudo inside the container; reach it from its <host> node) and confirm on
     `BUILD_HOST_HEAVY`.
  2. Install the `sccache` binary on both build hosts.
  3. Install the pinned toolchain (BLD-02) on both.
  4. **No restart of `BUILD_HOST_PRIMARY` while an agent/build runs** — package installs do not need
     a restart; do not trigger one.
- **Acceptance criteria:**
  - [ ] `x86_64-linux-musl-gcc`, `sccache`, and the pinned rustc are present on both build hosts
  - [ ] `BUILD_HOST_PRIMARY` not restarted while active
  - [ ] No secrets/infra values hardcoded (endpoints via config)

### BLD-04: Confirm + wire the sccache→Redis backend (operator + config)
- **Priority:** High
- **Labels:** terminus, cicd, cache
- **Agent:** <operator>
- **Estimate:** 30m
- **Type:** human-action
- **Description:** sccache uses the EXISTING terminus Redis (no new service) — the endpoint was not
  found on the heavy host's loopback, so confirm where `terminus_primary`'s Redis actually lives.
- **Steps:**
  1. Read `terminus_primary`'s runtime config for its Redis endpoint (a `REDIS_URL`-style value);
     record it as the vault/config key `SCCACHE_REDIS` (name only, value in vault).
  2. Confirm reachability from both build hosts.
  3. Set `SCCACHE_REDIS` in the build hosts' environment (materialized from the vault, never
     hardcoded) so the compiler tool (BLD-05) can point sccache at it.
- **Acceptance criteria:**
  - [ ] `SCCACHE_REDIS` is confirmed, reachable from both build hosts, and materialized from the vault
  - [ ] No Redis endpoint literal committed to any tracked file

---

## Phase 1 — The compiler tool (Terminus)

### BLD-05: `compiler_build` — capped, sccache-backed, host-selecting build + publish
- **Priority:** Critical
- **Labels:** terminus, cicd, compiler
- **Agent:** claude
- **Estimate:** 8h
- **Description:** The core Terminus tool. Given a module + git ref, it selects a build host,
  relays source, runs an sccache-backed cargo build inside a resource-capped systemd scope on the
  pinned toolchain, and publishes a checksummed artifact to the shared dataset.

  ## FILES
  - `src/compiler/mod.rs` (new, Terminus) — tool registration + `compiler_build`.
  - `src/compiler/host.rs` — host selection (primary vs heavy) from RAM/size heuristics + config.
  - `src/compiler/scope.rs` — run cargo inside a systemd transient scope with `MemoryMax` +
    `MemorySwapMax=0` + `CPUQuota` + `IOWeight` (Plex protection — an over-budget build OOMs inside
    its own cgroup, never swap-thrashes the node).
  - `src/compiler/sccache.rs` — sccache env wiring to `SCCACHE_REDIS` (vault-sourced).
  - `src/compiler/publish.rs` — artifact layout + sha256 + `current` pointer under `BUILD_DATASET_ROOT`.
  - `README.md` (Terminus) — the `compiler_*` tool surface.

  ## APPROACH
  1. `compiler_build(module, ref, host="auto", profile, fast=false)`: resolve host (auto → primary
     unless the module's known peak or `fast` needs the heavy host); ensure the pinned toolchain;
     relay the worktree to `${BUILD_DATASET_ROOT}/src/<module>/<ref>` (rsync, or build in place on
     the primary's local ext4); set `CARGO_TARGET_DIR` to a LOCAL/tmpfs exec-safe path (NEVER the
     file-level NFS dir); export sccache→`SCCACHE_REDIS`.
  2. Run cargo inside the capped scope (`systemd-run --scope -p MemoryMax=… -p MemorySwapMax=0 -p
     CPUQuota=… -p IOWeight=…`). Parameterize `-j`/caps per host so it fits the host's budget.
  3. On success, sha256 the artifact, copy it to `${BUILD_DATASET_ROOT}/artifacts/<module>/<channel>/
     <sha>/<target>/<bin>` + `.sha256`; do NOT flip `current` here (that is BLD-07 promote/publish).
  4. Interim (before BLD-01 mounts land): the primary may publish by relaying the artifact to a host
     that has the dataset mounted RW (a single ssh/rsync hop) — no primary-host mount required.
  5. All hosts/paths/endpoints via config/vault; no literals. Secrets via SecretManager.

  ## TEST PLAN
  - `compiler_build` on the primary host builds a small module, produces a checksummed artifact
    under the dataset; the sha matches.
  - The build runs inside a scope with `MemorySwapMax=0` (verify `systemctl show <scope>`); an
    artificially tiny `MemoryMax` OOMs the build inside its own cgroup WITHOUT node-wide swap.
  - sccache shows cache activity (`sccache --show-stats`) against `SCCACHE_REDIS`.
  - `CARGO_TARGET_DIR` is never on the file-level NFS dir (assert path).
  - Verify no hardcoded infra values; secrets via SecretManager.

  ## EDGE CASES
  - Heavy host busy/serving → defer to the scheduler (BLD-06); do not build unbounded.
  - Missing pinned toolchain → install idempotently, don't fall back to a different version.
  - Redis unreachable → sccache degrades to local-dir cache (`${BUILD_DATASET_ROOT}/cache/sccache`),
    logged, build still succeeds (never blocks on the cache).
  - Artifact dataset not mounted on the build host yet → relay-publish hop (approach step 4).

- **Acceptance criteria:**
  - [ ] `compiler_build` builds on the selected host, pinned toolchain, sccache→Redis, capped scope with `MemorySwapMax=0`
  - [ ] It publishes a sha256-checksummed artifact to the shared dataset (checksum verified)
  - [ ] The live cargo target dir is always local/tmpfs (exec-safe), never the file-level NFS dir
  - [ ] Redis-down degrades to the local sccache dir without failing the build (negative test)
  - [ ] README documents the `compiler_*` surface (README updated)
  - [ ] No hardcoded infrastructure values; secrets via SecretManager
  - [ ] All existing tests still pass

### BLD-06: Queue + scheduler — multi-agent ready-marking, windows, graceful serialization
- **Priority:** Critical
- **Labels:** terminus, cicd, scheduler
- **Agent:** claude
- **Estimate:** 8h
- **Description:** Multiple agents mark "ready for a compiler run"; the scheduler makes runs happen
  gracefully — small/capped builds now on the primary, heavy builds (needing the heavy host's
  idle-mode) within configured windows / fleet-quiet gates — with bounded concurrency so builds
  never contend.

  ## FILES
  - `src/compiler/queue.rs` (new) — `compiler_request` (module, ref, priority, ready), dedupe by
    (module, ref), persisted queue (Redis or the intake DB).
  - `src/compiler/scheduler.rs` (new) — window/quiet gating, per-host concurrency caps, idle-mode
    coordination, event emission (queued→building→published).
  - `src/compiler/mod.rs` — `compiler_status` (queue, in-flight, leases, sccache hit-rate).
  - `README.md` — request/schedule surface.

  ## APPROACH
  1. `compiler_request(module, ref, priority, ready=true)`: enqueue (dedupe same module@ref;
     coalesce multiple agents' readiness into one run).
  2. Scheduler loop: dispatch small/capped builds immediately on the primary (bounded concurrency);
     hold heavy builds (that require the heavy host + idle-mode) for a configured window
     (`BUILD_WINDOW_HOURS`) or a fleet-quiet signal; one build per host at a time (or a per-host cap).
  3. Coordinate idle-mode: only acquire the heavy host's idle-mode (BLD-11) when a heavy build is
     actually dispatched, and release right after.
  4. Emit events for the GUI (BLD-15) and requesting agents to subscribe to.
  5. Config-driven windows/caps; no literals.

  ## TEST PLAN
  - Two agents `compiler_request` the same module@ref → one coalesced run.
  - A heavy build queued outside the window stays queued; inside the window it dispatches.
  - Per-host concurrency cap holds (a 2nd build for a busy host queues).
  - `compiler_status` reflects queue + in-flight + leases.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Window closes mid-build → the in-flight build finishes; no new heavy build starts.
  - A stuck build → scheduler timeout + surfaced in `compiler_status`; lease released.
  - Priority inversion → high-priority requests preempt queue order but never a running build.

- **Acceptance criteria:**
  - [ ] `compiler_request` enqueues + dedupes; multiple agents' readiness coalesces into one run
  - [ ] Heavy builds respect the configured window / fleet-quiet gate; small builds run immediately
  - [ ] Per-host concurrency is bounded; idle-mode is acquired only for dispatched heavy builds
  - [ ] `compiler_status` exposes queue, in-flight, leases, cache hit-rate
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### BLD-07: Artifact store — publish, `current` pointer, `compiler_release` promote
- **Priority:** High
- **Labels:** terminus, cicd, artifacts
- **Agent:** codex
- **Estimate:** 5h
- **Description:** Formalize the artifact store + channel pointers so deploy is build-once →
  publish → promote (never rebuild for stable).

  ## FILES
  - `src/compiler/publish.rs` — `current` pointer per (module, channel); a `dist-manifest.json`-style
    index per artifact.
  - `src/compiler/mod.rs` — `compiler_release(module, sha, from_channel, to_channel)` (pointer flip,
    no rebuild).
  - `docs/build.md` — the store layout + channel model.

  ## APPROACH
  1. Layout: `${BUILD_DATASET_ROOT}/artifacts/<module>/<channel>/<sha>/<target>/<bin>+.sha256`, plus
     `<module>/<channel>/current` (pointer file = blessed sha) and a small per-sha manifest.
  2. `compiler_build` writes the sha dir; publish flips `experimental/current` to the new sha.
  3. `compiler_release` promotes: copy/point `stable/current` → an already-built experimental sha
     (Rust-train model; no recompile). Retain ≥2 shas per channel; prune older.
  4. All via config paths.

  ## TEST PLAN
  - After a build, `experimental/current` points at the new sha; the manifest is valid.
  - `compiler_release` flips `stable/current` to an existing experimental sha with no rebuild.
  - Retention keeps ≥2 shas; prune removes older.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Promote a sha that was never built → refuse (fail closed).
  - Concurrent publish + promote → pointer writes are atomic (temp + rename).

- **Acceptance criteria:**
  - [ ] Artifacts land at the documented layout with `current` pointers per (module, channel)
  - [ ] `compiler_release` promotes by pointer flip (no rebuild); promoting an unbuilt sha is refused
  - [ ] Retention keeps ≥2 shas and prunes older; pointer writes are atomic
  - [ ] `docs/build.md` documents the store + channel model (docs updated)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### BLD-08: `compiler_status` + fleet version query
- **Priority:** Medium
- **Labels:** terminus, cicd
- **Agent:** codex
- **Estimate:** 3h
- **Description:** A read surface: queue + in-flight builds, artifact `current` per module/channel,
  and each deploy host's `.deployed_sha` (module × host → version), for the GUI (BLD-15) and agents.

  ## FILES
  - `src/compiler/status.rs` (new) — aggregate queue + store pointers + deploy markers.
  - `README.md` — the status surface.

  ## APPROACH
  1. Gather: scheduler queue/in-flight; store `current` per module/channel; deploy `.deployed_sha`
     per host (read the markers the updater writes — via the existing host-reach path, not new creds).
  2. Return a structured module × host matrix with built-at + channel + health hint.

  ## TEST PLAN
  - `compiler_status` returns queue + store pointers + a module×host deployed-sha matrix.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - A host unreachable → its cell is `unknown`, not an error.
  - No marker yet → `undeployed`.

- **Acceptance criteria:**
  - [ ] `compiler_status` returns queue/in-flight + `current` pointers + module×host deployed-sha matrix
  - [ ] Unreachable host / missing marker degrade to `unknown`/`undeployed`, not errors
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### BLD-19: Compiler progress/events API — live build status for clients
- **Priority:** High
- **Labels:** terminus, cicd, compiler, api
- **Agent:** claude
- **Estimate:** 5h
- **Description:** A first-class progress surface so clients (the fleet GUI, requesting agents, the
  Harmony adapter) can query AND subscribe to live build status/progress — queue position,
  stage, percent/step, streamed log tail, and terminal outcome — for every `compiler_request`.

  ## FILES
  - `src/compiler/events.rs` (new, Terminus) — a per-request event bus + ring buffer of the last N
    events/log lines; stages `queued → scheduled → relaying → building(step/total) → publishing →
    deployed | failed | rolled_back`.
  - `src/compiler/mod.rs` — `compiler_progress(request_id)` (snapshot) and a subscribe surface
    (SSE/WebSocket stream, or MCP long-poll) that pushes events as they occur.
  - `src/compiler/scope.rs` / `queue.rs` / `publish.rs` — emit stage transitions + a bounded log tail
    into the bus (cargo's `--message-format=json` compile progress → step/total; publish → sha).
  - `README.md` — the progress/subscribe contract.

  ## APPROACH
  1. Every request carries a stable `request_id`; the scheduler/build/publish stages emit typed
     events (stage, timestamp, optional `{step,total}` from cargo JSON, a bounded log tail, and on
     terminal: outcome + artifact sha or error).
  2. `compiler_progress(request_id)` returns the latest snapshot; a subscribe endpoint streams events
     live (each stage transition + throttled log lines) so a GUI shows a real progress bar, not a spinner.
  3. Keep a bounded history so a late subscriber still gets the current state + recent log tail.
  4. Sanitize the log tail (no secrets/tokens — S6) before it leaves the process.
  5. No hardcoded infra; the stream is auth-gated like the rest of the surface.

  ## TEST PLAN
  - Submit a build; `compiler_progress` transitions queued→…→deployed with `{step,total}` during
    building; a subscriber receives each transition live.
  - A failed build surfaces `failed` + a sanitized error tail; a rollback surfaces `rolled_back`.
  - Log tail is sanitized (inject a fake token → it is redacted, S6).
  - A late subscriber gets the current snapshot + recent tail.
  - Verify no hardcoded infra values; no secrets in the stream.

  ## EDGE CASES
  - Unknown `request_id` → empty/`not_found`, not an error.
  - High log volume → throttle/bound the tail; never unbounded memory.
  - Subscriber disconnect → clean up the subscription; the build is unaffected.

- **Acceptance criteria:**
  - [ ] `compiler_progress(request_id)` returns a live snapshot; a subscribe stream pushes each stage transition + throttled log tail
  - [ ] Building stage reports `{step,total}` (from cargo JSON) so clients render a real progress bar
  - [ ] Terminal states (`deployed`/`failed`/`rolled_back`) are reported with sha or sanitized error
  - [ ] The log tail is secret-sanitized (S6); a late subscriber still gets current state + recent tail
  - [ ] README documents the progress/subscribe contract (README updated)
  - [ ] No hardcoded infrastructure values; no secrets in the event stream
  - [ ] All existing tests still pass

---

## Phase 2 — Idle-mode (Chord + MINT)

### BLD-09: Chord idle-mode API — release providers, GPU locks, models, RAM
- **Priority:** Critical
- **Labels:** chord, idle-mode, gpu
- **Agent:** claude
- **Estimate:** 6h
- **Description:** New Chord admin surface to free the heavy host for the compiler: on `idle`, stop
  all providers, release all GPU locks, demote all resident models to storage (unload VRAM +
  release the system RAM they hold), and enter a low-footprint wait. On `activate` (explicit or
  lazy on next request), restore. These surfaces do not exist today.

  ## FILES
  - `chord-proxy/src/admin/idle.rs` (new, Chord) — `idle`/`activate` handlers + state.
  - `chord-proxy/src/providers/*` — a stop/park hook per provider.
  - `chord-proxy/src/gpu/*` — release GPU locks; unload/demote models to storage.
  - `README.md` (Chord) — the idle/activate contract.

  ## APPROACH
  1. `POST /admin/idle` (auth-gated): stop accepting new work, drain in-flight, stop providers,
     release GPU locks, unload models (demote to storage), free the RAM they held; record an
     `idle` state + a resume manifest (what to restore).
  2. `POST /admin/activate`: reverse — reload from the resume manifest; also support LAZY activate
     (first real request after idle triggers restore).
  3. Report freed-RAM in the idle response so the compiler knows headroom is available.
  4. Fail-safe: a hard timeout re-activates if no compiler lease is held; never leave the proxy
     dead silently.
  5. No hardcoded infra; secrets via SecretManager; do not log model/provider secrets.

  ## TEST PLAN
  - `POST /admin/idle` → providers stopped, GPU locks released, models demoted, RAM freed (measure
    MemAvailable delta); state == idle; freed-RAM reported.
  - `POST /admin/activate` (and lazy-on-request) → providers/models restored; a real completion works.
  - Idle with in-flight work → drains before releasing.
  - Verify no hardcoded infra values; no secrets logged.

  ## EDGE CASES
  - GPU lock held by a non-Chord process → report which; don't force-kill.
  - Activate while already active / idle while already idle → idempotent no-op.
  - Crash during idle → the resume manifest + a watchdog restore on restart.

- **Acceptance criteria:**
  - [ ] `POST /admin/idle` stops providers, releases GPU locks, demotes models to storage, frees RAM, reports freed-RAM
  - [ ] `POST /admin/activate` (explicit + lazy-on-request) restores full service; a real completion succeeds after
  - [ ] In-flight work drains before release; idle/activate are idempotent
  - [ ] A watchdog re-activates on timeout/crash (never silently dead)
  - [ ] README documents the idle/activate contract (README updated)
  - [ ] No hardcoded infrastructure values; secrets via SecretManager, never logged
  - [ ] All existing tests still pass

### BLD-10: MINT test-harness idle-mode
- **Priority:** High
- **Labels:** terminus, mint, idle-mode
- **Agent:** claude
- **Estimate:** 4h
- **Description:** The MINT test harness follows the same idle/activate contract as Chord so it too
  releases its resources for a compiler run on the shared big host.

  ## FILES
  - `src/mint/idle.rs` (new, Terminus) — idle/activate mirroring BLD-09's contract for MINT.
  - `README.md` — MINT idle note.

  ## APPROACH
  1. Implement `idle`/`activate` for MINT: stop its providers/GPU usage, release RAM, resume manifest.
  2. Same watchdog + idempotency + freed-RAM reporting as BLD-09.

  ## TEST PLAN
  - MINT `idle` frees its resources (measured); `activate` restores; a MINT run works after.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Same as BLD-09 (idempotent, watchdog, in-flight drain).

- **Acceptance criteria:**
  - [ ] MINT `idle`/`activate` release + restore resources; a run works after activate
  - [ ] Idempotent + watchdog-protected; freed-RAM reported
  - [ ] README updated for MINT idle
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### BLD-11: Compiler ↔ idle-mode lease wiring
- **Priority:** High
- **Labels:** terminus, cicd, idle-mode
- **Agent:** codex
- **Estimate:** 4h
- **Description:** The scheduler's `compiler_idle_acquire(host)` / `_release(host)` drive Chord +
  MINT idle-mode around a heavy build — freeing ~120 GB before, restoring after.

  ## FILES
  - `src/compiler/idle_lease.rs` (new) — acquire (idle Chord+MINT, wait for freed-RAM), hold a
    lease, release (activate), with a hard max-lease timeout.
  - `src/compiler/scheduler.rs` — call the lease around heavy dispatch.

  ## APPROACH
  1. `idle_acquire(host)`: call Chord `idle` + MINT `idle`; wait until reported freed-RAM ≥ the
     build's budget; record the lease.
  2. `idle_release(host)`: `activate` both; clear the lease.
  3. Enforce a max-lease timeout → auto-release (services never left idle indefinitely).

  ## TEST PLAN
  - A dispatched heavy build acquires the lease → Chord+MINT idle → build runs with the freed RAM →
    release → services active again.
  - Lease timeout auto-releases even if the build hangs.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Idle fails to free enough RAM → don't start the heavy build; requeue + alert.
  - Build crashes → the lease's release still runs (activate); no stuck idle.

- **Acceptance criteria:**
  - [ ] Heavy dispatch acquires the idle-lease, waits for freed-RAM, releases (activates) after — verified end to end
  - [ ] Max-lease timeout auto-activates; a crashed build still triggers release
  - [ ] Insufficient freed-RAM aborts the heavy build (requeue), never builds under budget
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

## Phase 3 — Deploy realignment (constellation-updater) + build discipline

### BLD-12: constellation-updater fetch-artifact mode
- **Priority:** Critical
- **Labels:** constellation-updater, cicd, deploy
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Realign the updater from build-on-dest (impossible on 2–4 GB deploy CTs) to
  fetch-prebuilt-artifact. Keep everything that already works — idle-gate, sha-compare, backup,
  atomic-mv, restart, health-gate, rollback, marker — and swap only the artifact source.

  ## FILES
  - `constellation-update.sh` (constellation-updater) — add `MODE=fetch`: pull the artifact for
    (module, channel, `current`-sha) from `${BUILD_DATASET_ROOT}` (RO mount) or a release, verify
    the sha, then the existing backup→atomic-mv→restart→health→rollback→marker flow.
  - `config/*.conf` — replace `BUILD_CMD` with `MODE=fetch` + `ARTIFACT_SRC=<store path/url>`;
    keep `INSTALL_DEST`, `VERSION_MARKER`, `HEALTH_CMD`, `CHANNEL`.
  - `README.md` — fetch vs build modes.

  ## APPROACH
  1. Add `MODE` (`fetch` default for deploy CTs; `build` retained as a fallback for build-capable
     hosts). In `fetch`, the "new version" signal is the store's `current` sha (not gitea HEAD).
  2. Fetch the artifact matching the host's target; verify `.sha256`; then the UNCHANGED
     backup/atomic-mv/health-gate/rollback/marker path.
  3. No creds/toolchain/RAM needed on the dest.
  4. Config-driven; no literals.

  ## TEST PLAN
  - On a deploy host, `MODE=fetch` pulls the `current` artifact, verifies sha, atomic-swaps,
    health-gates, writes the marker; a bad health check rolls back to the backup.
  - A checksum mismatch aborts before swap (fail closed).
  - `MODE=build` fallback still works on a build-capable host.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - `current` unchanged vs marker → no-op (idempotent).
  - Artifact missing/partial → abort, keep running version, alert.
  - Health fails post-swap → rollback + marker unchanged.

- **Acceptance criteria:**
  - [ ] `MODE=fetch` pulls + sha-verifies the `current` artifact and health-gated atomic-swaps with rollback
  - [ ] Checksum mismatch / missing artifact aborts before swap (running version intact)
  - [ ] `MODE=build` fallback preserved for build-capable hosts; deploy CTs need no toolchain/creds/RAM
  - [ ] README documents fetch vs build modes (docs updated)
  - [ ] No hardcoded infrastructure values in new/modified files
  - [ ] All existing tests still pass

### BLD-13: Trigger-on-publish — `compiler_deploy` fires the updater fleet-wide
- **Priority:** High
- **Labels:** terminus, cicd, deploy
- **Agent:** codex
- **Estimate:** 4h
- **Description:** Close the loop for seamless updates: after a successful publish/promote, the
  compiler triggers the updater on the target hosts so a change lands fleet-wide in seconds (nightly
  timers remain the catch-all).

  ## FILES
  - `src/compiler/deploy.rs` (new, Terminus) — `compiler_deploy(module, channel, hosts="all")`:
    invoke `constellation-update@<module>` on each dest (via the existing sanctioned host-reach
    path), collect per-host outcome.
  - `README.md` — the deploy trigger.

  ## APPROACH
  1. After publish/promote, optionally auto-`compiler_deploy` (config flag) or expose it for the GUI.
  2. Fire the dest updater (fetch-mode); aggregate results (deployed/rolled-back/skipped) per host.
  3. Respect the deploy hosts' health-gate + rollback (BLD-12) — the compiler only triggers; the
     updater owns the swap safety.

  ## TEST PLAN
  - `compiler_deploy` triggers fetch-mode updaters on the dests; each health-gated swaps or rolls back;
    results aggregated.
  - A dest that rolls back is reported, not masked.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - A dest unreachable → reported skipped; others proceed.
  - Partial fleet success → surfaced clearly; nightly timer catches stragglers.

- **Acceptance criteria:**
  - [ ] `compiler_deploy` triggers the fetch-mode updater on the dests and aggregates per-host outcomes
  - [ ] Rollbacks/unreachable hosts are reported, not masked; nightly timers remain the catch-all
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### BLD-14: Build discipline — no ad-hoc `cargo build`; agents route through the compiler
- **Priority:** High
- **Labels:** terminus, cicd, discipline, docs
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Codify the single-door build rule (like `review_run`): no Claude-Code agent or
  Harmony build-space runs ad-hoc `cargo build`/`cargo test` on a shared host — all builds go
  through the compiler queue. Add a thin Harmony adapter so its test-gate/deploy route through the
  compiler.

  ## FILES
  - `harmony-core/src/compiler_client.rs` (new, harmony) — submit `compiler_request` + await result
    for Stage-4 test-gate / Stage-7 deploy.
  - `docs/build.md` (Terminus) + the build skill note — the discipline rule.

  ## APPROACH
  1. Document the rule: shared-host builds only via `compiler_*`; ad-hoc `cargo build` on a shared
     host is a reviewable violation (it caused disk-fill/OOM/toolchain-drift/Plex-contention).
  2. Harmony adapter: its build/test-gate calls `compiler_request` (worktree local, compiler relays)
     rather than compiling inline; consumes the artifact for deploy.
  3. Agents (this pipeline) submit to the compiler for any shared-host build.

  ## TEST PLAN
  - Harmony's test-gate routes a build through the compiler and consumes the artifact.
  - `docs/build.md` states the rule + rationale + the local-worktree/compiler-relay pattern.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - A quick local `cargo check` on the dev-box's own appdata-backed ext4 (not a shared host, capped)
    is allowed; the rule targets SHARED-host contention. State the boundary explicitly.

- **Acceptance criteria:**
  - [ ] The no-ad-hoc-build-on-shared-hosts rule is documented with rationale + the boundary
  - [ ] Harmony's build/test-gate routes through `compiler_request` and consumes the artifact
  - [ ] `docs/build.md` + the build skill note updated (docs updated)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

## Phase 4 — Fleet/deploy GUI (harmony-web)

### BLD-15: Fleet/deploy page — updates, version tracker, host specs
- **Priority:** High
- **Labels:** harmony, web, cicd
- **Agent:** claude
- **Estimate:** 6h
- **Description:** A Deployments page in the web GUI: check-for-updates, a module × host deployed-
  version matrix, per-host system specs + running modules + health, and a (gated) deploy action.

  ## FILES
  - `harmony-web/src/pages/Deployments.tsx` (new, harmony) — the fleet page.
  - `harmony-web/src/components/deploy/*` — version matrix, host-spec cards, update badges.
  - `harmony-web/src/hooks/useFleet.ts` — consume the BLD-16 API.
  - `README.md` — the page.

  ## APPROACH
  1. Render module × host → deployed sha / `current` sha / channel / built-at, with an
     "update available" badge when they differ.
  2. Per host: cores/RAM/disk + running modules + health.
  3. A deploy button → `compiler_deploy` (BLD-13), health-gated with rollback, progress via the
     scheduler events (BLD-06).
  4. No hardcoded infra; all data from the API.

  ## TEST PLAN
  - `npm run typecheck && npm run build` clean.
  - With mock/live API, the matrix + update badges + host specs render; a deploy action calls the API.
  - Verify no hardcoded infra values in the web code.

  ## EDGE CASES
  - Unreachable host → cell shows `unknown`, not a crash.
  - Deploy in progress → live status; disable double-trigger.

- **Acceptance criteria:**
  - [ ] The page shows the module×host version matrix + update badges + per-host specs/health
  - [ ] A gated deploy action triggers `compiler_deploy` with live progress + rollback surfaced
  - [ ] `npm run build` produces the embeddable bundle
  - [ ] README documents the page (README updated for the new user-facing feature)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### BLD-16: harmony-server fleet API
- **Priority:** High
- **Labels:** harmony, server, api, cicd
- **Agent:** codex
- **Estimate:** 5h
- **Description:** The API backing BLD-15: `/api/fleet/status` (version matrix + host specs from
  `compiler_status` + a host-specs probe) and `/api/fleet/deploy` (gated → `compiler_deploy`).

  ## FILES
  - `harmony-server/src/fleet.rs` (new, harmony) — the endpoints (auth-gated).
  - `harmony-server/src/main.rs` — route registration.
  - `README.md` — the API.

  ## APPROACH
  1. `/api/fleet/status`: call the Terminus `compiler_status` tool (single sanctioned door — no new
     direct clients, S9) + a host-specs probe; return the module×host matrix + specs.
  2. `/api/fleet/deploy` (auth-gated): call `compiler_deploy`; stream/emit progress.
  3. Secrets via SecretManager; no literals.

  ## TEST PLAN
  - `/api/fleet/status` returns the matrix + specs (auth required; 401 without).
  - `/api/fleet/deploy` triggers `compiler_deploy` (auth required); progress observable.
  - Verify calls go through the Terminus compiler tool (S9), not a new direct client.

  ## EDGE CASES
  - Compiler/Terminus unreachable → 503 with a clear message, not a crash.
  - Deploy without auth → 401.

- **Acceptance criteria:**
  - [ ] `/api/fleet/status` + `/api/fleet/deploy` back the GUI, auth-gated, via the Terminus compiler tool (S9 single door)
  - [ ] Unreachable compiler → 503 (not crash); unauth deploy → 401
  - [ ] README documents the endpoints (README updated)
  - [ ] No hardcoded infrastructure values; secrets via SecretManager
  - [ ] All existing tests still pass

---

## Phase 5 — Container config + migration (operator)

### BLD-17: Container config optimization — appdata-back + right-size
- **Priority:** Medium
- **Labels:** cicd, infra, containers
- **Agent:** <operator>
- **Estimate:** 2h
- **Type:** human-action
- **Description:** Apply the baseline-driven config: mount the shared build dataset where needed,
  keep local disks small with bulk on appdata, and right-size per the baseline. Deploy CTs become
  fetch-only (BLD-12), reclaiming their build scratch.
- **Steps:**
  1. Deploy hosts: mount `${BUILD_DATASET_ROOT}` RO; after BLD-12 fetch-mode, remove build-on-dest
     scratch (reclaims local disk); right-size disk down if desired.
  2. Move any bulky per-CT service data onto appdata mounts so local disks stay small.
  3. Dev-box: RW build-dataset mount (BLD-01) for artifact publish.
  4. Schedule mount changes for a quiescent window per the HARD RULE.
- **Acceptance criteria:**
  - [ ] Deploy CTs mount the dataset RO and run fetch-only (no build scratch)
  - [ ] Bulky service data relocated to appdata; local disks kept minimal
  - [ ] No `BUILD_HOST_PRIMARY` restart while an agent/build is active
  - [ ] No hardcoded infrastructure values recorded

### BLD-18: Migrate dev-box → <host> behind swap-off caps (operator)
- **Priority:** Low
- **Labels:** cicd, infra, migration
- **Agent:** <operator>
- **Estimate:** 2h
- **Type:** human-action
- **Description:** Once builds are capped + offload-to-heavy-host works, move the dev-box back to the
  <host> node (less RAM). Its rootfs image can stay on appdata (keeps TB fast disk), so this is purely
  a RAM change — the swap-off cgroup caps (BLD-05) are what protect Plex from build contention.
- **Steps:**
  1. Confirm the build cgroup caps (`MemoryMax` + `MemorySwapMax=0` + `CPUQuota`) are sized for the
     post-move RAM and that heavy builds offload to the big host (idle-mode), not the dev-box.
  2. Migrate the container to <host> **only when NO agent session/build is active on it** (a migration
     restarts it — this HARD RULE is why it is a scheduled operator action, never automated mid-session).
  3. Post-move: run a capped build + a Plex-load test concurrently; confirm no swap thrash / no Plex
     interruption.
- **Acceptance criteria:**
  - [ ] Dev-box runs on <host> with build cgroup caps + swap-off; heavy builds offload to the big host
  - [ ] A concurrent build + Plex load shows no swap thrash and no Plex interruption
  - [ ] The migration happened in a quiescent window (no active agent/build); `BUILD_HOST_PRIMARY` never killed under an active session
  - [ ] No hardcoded infrastructure values recorded

---

## Notes for the executing agent
- **HARD RULE, repeated:** never restart/migrate `BUILD_HOST_PRIMARY` (the dev/agent container) while
  an agent session or build is active — it kills the running agent. All mount/migration items (BLD-01,
  17, 18) are operator, scheduled for a quiescent window.
- **Phase order:** 0 → 1 → 2 → 3 → (4 in parallel with 3) → 5. BLD-05 gates 06/07/08; BLD-09/10 gate
  11; 11 gates heavy builds; 12 gates 13; 13 gates the GUI deploy action.
- **Single doors:** the compiler is the only build path on shared hosts (BLD-14); `review_run` remains
  the only review path; the Terminus Plane/Gitea/GitHub tools remain the only forge paths (S9).
- **Secrets/PII:** all hosts/paths/endpoints via config vars materialized from the vault; no literal
  IPs/hostnames/CT-ids/Redis-URLs in any tracked file (S1/S7).
