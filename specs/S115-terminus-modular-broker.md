# Terminus Modular Broker — Capability-Secure Tool Workers
plane_project: TERM
module: Terminus
prefix: TMOD
spec_id: S115-terminus-modular-broker

## Metadata
- **Author:** <operator> (Moose)
- **Session:** S115
- **Date:** 2026-07-11
- **Lumina version:** (current)
- **Module version:** Terminus (all-Rust, post-Python-retirement)
- **Estimated total:** ~46h autonomous agent work (foundation), excluding the ~28 follow-on per-domain extractions
- **Context:** Terminus is now 100% Rust (the legacy Python `ai-terminus` fleet hub is retired). The
  tool suite currently lives as compiled-in `Box<dyn RustTool>` objects baked into an immutable
  `Arc<McpServerState>` at startup, so adding/updating any tool requires a full process restart —
  a monolith that can't be worked on in parallel and that holds every fleet credential in one
  blast radius. This spec turns terminus-primary from *the thing that contains all tools* into a
  **capability-secure broker** that routes to, authorizes, and supervises independently deployable
  per-domain **tool workers**, over a pluggable in-box transport with a security-tier floor. It
  leans on seams that already exist: `src/mesh/merge.rs`'s route table + `resolve_call_route`, the
  `src/gateway_framework` allowlist/audit/rate-limit, and the `src/federation` proxy pattern that
  already reaches `terminus_personal` tools out-of-process. This is the FOUNDATION epic (broker
  seam + worker SDK + transport tiers + control plane) plus the first two proof extractions
  (`vitals`, `gitea`); the remaining ~28 domains follow via the extraction template in the
  appendix, each as its own TERM sprint.

## Pre-flight
- Repository: `moosenet/Terminus` on Gitea (existing)
- Working directory: the sanctioned dev-box Terminus checkout (repo-relative paths only below)
- Dependencies: `rustup`, `cargo`, `pkg-config`, `libssl-dev` (mTLS transport), an available
  `arc_swap` crate dependency
- Vault secrets required (referenced by NAME only, materialized at runtime per the crate's
  SecretManager/pki env-materialization convention — never literals): the per-worker mTLS key
  material env names introduced in TMOD-02/TMOD-05, plus the existing per-domain tokens the
  extracted workers will each hold in isolation (e.g. a Gitea PAT name for the gitea worker).
- Infrastructure: Gitea reachable, Plane reachable via the Terminus Plane tool, review-daemon
  reachable for the Stage-5 gate.
- Baseline tests: current `cargo test --workspace` green count on main (record at Stage 0).
- Baseline verify: current `harmony verify` score (record at Stage 0).
- **Prefix registration:** before ingest, confirm `TMOD` is free via `plane_prefix_check` and
  claim it (`plane_prefix_register` → `plane_prefix_promote`). If `TMOD` is taken, take the
  suggested next-available and update this spec's `prefix:` field before creating any Plane items.

---

### TMOD-01: Hot-swappable tool registry (ArcSwap) in McpServerState
- **Priority:** High
- **Labels:** terminus, broker, registry
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Make the compiled-in tool registry replaceable at runtime without restarting
  the process. Today `McpServerState.registry: ToolRegistry` is owned inside an immutable
  `Arc<McpServerState>`; nothing can change the tool set while the server runs. Wrap it so the
  active registry snapshot can be atomically swapped, with in-flight calls finishing on their old
  snapshot and new calls seeing the new one. This is the enabling change for every later stage; on
  its own it is behavior-preserving (the swap path is simply never invoked yet).

  ## FILES
  - `src/mcp_server.rs` — change `registry` field to an `ArcSwap<ToolRegistry>` (or
    `Arc<ArcSwap<ToolRegistry>>`); update `handle_mcp` / `tools/list` / `tools/call` dispatch to
    take a cheap `registry.load()` snapshot per request instead of borrowing the owned field.
  - `src/registry.rs` — add a `snapshot`/`Arc`-friendly constructor path; ensure `ToolRegistry`
    is cheaply clonable-by-Arc for swap (wrap the inner maps in `Arc` if needed rather than
    deep-cloning `Box<dyn RustTool>` — a swap installs a new registry value, it does not clone
    tools).
  - `Cargo.toml` — add `arc_swap` dependency.

  ## APPROACH
  1. Add `arc_swap` to `Cargo.toml`.
  2. In `registry.rs`, restructure `ToolRegistry` so the dispatch-relevant state (`tools`,
     `order`) lives behind a shared `Arc` that a new snapshot can point at; `register` /
     `register_or_replace` build a NEW snapshot value rather than mutating a live one in place.
  3. In `mcp_server.rs`, change `McpServerState.registry` to `ArcSwap<ToolRegistry>`. Every
     handler that reads the registry does `let reg = state.registry.load();` at the top and uses
     that snapshot for the whole request, so an atomic swap mid-request never tears a single call.
  4. Add `McpServerState::swap_registry(new: ToolRegistry)` performing `self.registry.store(...)`.
     Do NOT call it from any live path yet — TMOD-04/05 wire it in.
  5. Keep all existing `register_all` boot wiring identical; only the container type changes.

  ## TEST PLAN
  - `cargo test --workspace` — all existing mcp_server / registry tests pass unchanged.
  - New unit test: build a state, `tools/call` a known tool, `swap_registry` to a registry with an
    added tool, assert the new tool is now callable AND the original still is.
  - New unit test: a call that captured a snapshot before a swap still resolves against the old
    snapshot (no panic, no missing-tool error mid-call).
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - Swap while a `tools/call` is mid-flight — the in-flight call uses its captured snapshot; assert no tear.
  - Duplicate tool name across old/new snapshots — `register_or_replace` semantics preserved (new wins).
  - Empty registry snapshot — `tools/list` returns empty, no panic.

- **Acceptance criteria:**
  - [ ] `McpServerState.registry` is an `ArcSwap<ToolRegistry>`; dispatch uses a per-request snapshot
  - [ ] `swap_registry` atomically replaces the active registry; in-flight calls are unaffected
  - [ ] No live code path invokes `swap_registry` yet (behavior-preserving foundation)
  - [ ] README/module docs note the swappable-registry invariant
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### TMOD-02: Pluggable in-box WorkerTransport with three security tiers + minimum-tier floor
- **Priority:** High
- **Labels:** terminus, broker, transport, security
- **Agent:** claude
- **Estimate:** 8h
- **Description:** Define the transport over which the broker reaches a worker, as a trait with
  three concrete tiers selectable per worker by config — never trusting loopback as a trust
  boundary. Tiers: **T2 (default)** Unix-domain socket with mTLS-over-UDS (kernel `SO_PEERCRED`
  peer identity AND cert CN must agree); **T1** UDS with `SO_PEERCRED` only (same-host, low-risk
  read tools); **T0** mTLS over TCP (for a worker that must live off-box). Security is a config
  dial, not a rebuild. A broker-side **minimum-tier floor** keyed on a worker's declared
  capability class prevents a worker holding write-scoped tools from registering below T2.

  ## FILES
  - `src/broker/transport/mod.rs` — new: `WorkerTransport` trait (`connect`, `call(name, args)`,
    `list`, `health`), `TransportTier` enum {T0, T1, T2}, and the `MinTierPolicy` (capability
    class → minimum tier).
  - `src/broker/transport/uds_peercred.rs` — new: T1 UDS + `SO_PEERCRED` client; extract peer
    uid/gid/pid, map to an expected worker identity.
  - `src/broker/transport/uds_mtls.rs` — new: T2 UDS + TLS-over-UDS; verify peercred AND cert CN,
    require agreement, reuse `src/pki/mtls` for cert handling.
  - `src/broker/transport/mtls_tcp.rs` — new: T0 mTLS over TCP, reuse `src/pki/mtls` +
    `src/mesh/identity`.
  - `src/config.rs` — add per-worker transport config (tier + socket dir / addr, cert material
    env-var NAMES); add the `MinTierPolicy` table.
  - `src/pki/mtls.rs` — reuse `ClientIdentity`; add a helper to assert peercred-vs-cert agreement.

  ## APPROACH
  1. Define `WorkerTransport` as an async trait with `call`/`list`/`health`, returning the same
     `ToolOutput`/error shapes the in-proc registry uses, so the broker treats a routed tool
     identically to a compiled-in one.
  2. Implement T1 (`uds_peercred`): connect a UDS, read `SO_PEERCRED`, resolve uid/gid to the
     configured worker identity; reject a mismatch before any call.
  3. Implement T2 (`uds_mtls`): wrap the UDS in TLS using `src/pki/mtls`; on handshake, extract the
     cert CN as `ClientIdentity` AND read `SO_PEERCRED`; require BOTH resolve to the same worker
     identity or fail closed. This is the default tier.
  4. Implement T0 (`mtls_tcp`) for the off-box case, cert CN identity only, reusing mesh identity.
  5. Config selects a tier per worker. All key/cert material is referenced by env-var NAME and read
     through the crate's SecretManager/pki env-materialization convention — never a raw
     `std::env::var` for key/cert-shaped values, never a literal.
  6. Implement `MinTierPolicy`: map each capability class (e.g. `read_only`, `write_scoped`,
     `secret_holding`) to a minimum tier (write/secret ⇒ T2). Expose
     `MinTierPolicy::permits(class, tier) -> bool`. TMOD-05 enforces it at registration.

  ## TEST PLAN
  - `cargo test --workspace` green.
  - Unit: T1 loopback UDS round-trip echo-tool call succeeds; a peercred mismatch is rejected.
  - Unit: T2 UDS+mTLS handshake with matching peercred+CN succeeds; a CN that disagrees with
    peercred fails closed.
  - Unit: `MinTierPolicy::permits(write_scoped, T1) == false`; `permits(write_scoped, T2) == true`;
    `permits(read_only, T1) == true`.
  - Verify secrets accessed via SecretManager/pki convention, not raw `std::env::var` for key material.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - Worker socket absent / connection refused — `WorkerTransport::health` reports unhealthy; `call` returns a clean "worker unavailable", never a panic.
  - T2 handshake where peercred resolves but the cert is expired/wrong-CN — fail closed, log the mismatch (identities only, never key bytes).
  - A worker configured below its capability floor — surfaced as a config error the broker refuses at load (belt to TMOD-05's runtime enforcement).
  - Off-box T0 worker with an unreachable host — clean unhealthy, no retry-storm.

- **Acceptance criteria:**
  - [ ] `WorkerTransport` trait with T0/T1/T2 implementations, tier chosen per worker by config
  - [ ] T2 requires peercred AND cert-CN agreement; a disagreement fails closed
  - [ ] `MinTierPolicy` rejects a write/secret-scoped worker below T2
  - [ ] All key/cert material referenced by env-var NAME via SecretManager/pki convention, never raw env or literals, never logged
  - [ ] Negative test: peercred/CN mismatch and sub-floor config both rejected
  - [ ] README/module docs document the three tiers and the floor
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### TMOD-03: terminus-worker-sdk crate (thin worker authoring surface)
- **Priority:** High
- **Labels:** terminus, worker-sdk, scaffolding
- **Agent:** claude
- **Estimate:** 6h
- **Description:** A shared crate so a new worker is roughly "impl the tool trait + a `main`". It
  provides the server side of the `WorkerTransport` protocol (the same MCP subset the broker
  speaks: `initialize` / `tools/list` / `tools/call`), the `RustTool` trait re-export and error
  types, identity plumbing (present its cert / accept peercred), and a `capabilities()` +
  semver/manifest advertisement the broker reads at register time. Domain crates depend on this,
  keeping workers thin and uniform. Mirrors how `terminus-client` already factors client code.

  ## FILES
  - `terminus-worker-sdk/Cargo.toml` — new crate.
  - `terminus-worker-sdk/src/lib.rs` — re-export `RustTool`, `ToolOutput`, `ToolError`; define
    `Worker` builder (register tools, choose served tier, run the socket/TLS server).
  - `terminus-worker-sdk/src/server.rs` — the transport-server side matching TMOD-02's tiers.
  - `terminus-worker-sdk/src/manifest.rs` — `WorkerManifest { name, semver, capability_class, tools: Vec<ToolInfo> }` emitted on `initialize`.
  - `Cargo.toml` (workspace) — add the new crate as a workspace member.

  ## APPROACH
  1. Scaffold the crate as a workspace member; depend on the shared tool/error types (factor them
     out of the main crate if they aren't already reusable, minimizing churn).
  2. Implement the server side of each transport tier from TMOD-02 (a worker binds a UDS and/or TLS
     listener per its configured tier and answers `initialize`/`tools/list`/`tools/call`).
  3. `initialize` returns the `WorkerManifest` (name, semver, capability_class, tool catalog) so the
     broker can pin compatible ranges and enforce the tier floor.
  4. Provide a `Worker::builder().tool(...).capability_class(...).serve(tier, addr)` ergonomic entry
     so a domain `main.rs` is a few lines.
  5. Secrets/cert material via the same SecretManager/pki convention; no literals.

  ## TEST PLAN
  - `cargo test --workspace` green including the new crate.
  - Unit: build a trivial one-tool `Worker`, serve on a T1 UDS in-test, connect with TMOD-02's T1
    client, assert `initialize` returns the manifest and `tools/call` round-trips.
  - Unit: manifest advertises the declared `capability_class` and semver.
  - Verify no hardcoded infrastructure values in the new crate.

  ## EDGE CASES
  - A worker declaring zero tools — `initialize` returns an empty catalog, broker registers no routes (no panic).
  - Duplicate tool names within one worker — rejected at build time by the builder.
  - Manifest semver malformed — worker refuses to start with a clear error.

- **Acceptance criteria:**
  - [ ] `terminus-worker-sdk` crate exists as a workspace member; a minimal worker is ~a trait impl + `main`
  - [ ] Serves the `initialize`/`tools/list`/`tools/call` subset over the TMOD-02 tiers
  - [ ] `initialize` advertises a `WorkerManifest` (name, semver, capability_class, tools)
  - [ ] README added for the crate documenting how to author a worker
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### TMOD-04: Dynamic route table + broker dispatch fallthrough to workers
- **Priority:** High
- **Labels:** terminus, broker, routing
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Generalize the existing `mesh::merge` route table (`routes: HashMap<String, Route>`
  + `resolve_call_route`) into a broker-owned, atomically-swappable **tool-name → worker route**
  table, and make broker dispatch fall through to it: a `tools/call` for a name not in the
  compiled-in registry snapshot resolves to a worker route and is dispatched over that worker's
  `WorkerTransport`; `tools/list` merges compiled-in tools with all healthy workers' catalogs
  (reusing `mesh/merge.rs` merge semantics, including namespace-collision handling).

  ## FILES
  - `src/broker/routes.rs` — new: `RouteTable { name -> WorkerRoute }` behind `ArcSwap`; a
    `WorkerRoute { worker_id, transport, manifest }`.
  - `src/mesh/merge.rs` — reuse/extend the merge + collision logic for the combined catalog.
  - `src/mcp_server.rs` — dispatch: try registry snapshot first; on miss, resolve a worker route and
    call over its transport; `tools/list` returns the merged catalog.

  ## APPROACH
  1. Define `RouteTable` behind `ArcSwap` (same pattern as TMOD-01), holding worker routes keyed by
     tool name, each pointing at a `WorkerTransport` handle + the worker's manifest.
  2. In `handle_mcp` `tools/call`: look up the registry snapshot; if absent, resolve the route table
     snapshot; if a route exists and the worker is healthy, dispatch over the transport and adapt
     the response to the same wire shape; if neither, return the existing method-not-found error.
  3. `tools/list`: merge compiled-in `ToolInfo` with each healthy worker's advertised catalog via
     `mesh/merge.rs`, preserving its namespace-collision handling.
  4. Route-table mutations happen ONLY through TMOD-05's control plane (atomic `store` of a new
     snapshot) — dispatch is read-only against a snapshot.

  ## TEST PLAN
  - `cargo test --workspace` green.
  - Unit: with a stub worker route installed, a `tools/call` for a name not compiled in dispatches
    to the worker and returns its output.
  - Unit: `tools/list` returns compiled-in ∪ worker-advertised tools, de-duplicated per merge rules.
  - Unit: a call to a name with no registry entry and no route returns method-not-found unchanged.
  - Unit: a route whose worker reports unhealthy is skipped in `tools/list` and returns "unavailable" on `call`.
  - Verify no hardcoded infrastructure values in new/modified files.

  ## EDGE CASES
  - Name present in BOTH the compiled-in registry and a worker route — compiled-in wins (documented precedence), no ambiguity.
  - Worker goes unhealthy between `list` and `call` — `call` returns a clean unavailable, siblings unaffected (fault isolation).
  - Route table swapped mid-request — the request uses its captured snapshot.

- **Acceptance criteria:**
  - [ ] Broker dispatch falls through registry-miss → worker route over the worker's transport
  - [ ] `tools/list` merges compiled-in and healthy-worker catalogs with collision handling
  - [ ] Route table is `ArcSwap`; dispatch is snapshot-read-only; a dead worker only fails its own tools
  - [ ] Documented precedence when a name exists both compiled-in and as a route
  - [ ] Negative test: unknown name → method-not-found; unhealthy worker → unavailable
  - [ ] README/module docs updated for the routing model
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### TMOD-05: Admin control plane — register/deregister/health/list with authN + capability-floor enforcement
- **Priority:** High
- **Labels:** terminus, broker, control-plane, security
- **Agent:** claude
- **Estimate:** 7h
- **Description:** The missing piece that makes hot add/update real: a small, authenticated admin
  surface the broker exposes (on its control port, not the public `/mcp`) to register a worker
  (name, transport tier, socket/addr, capability class), deregister it, health-check it, and list
  current routes. Registration health-gates the worker (must answer `initialize` + `health`),
  enforces the TMOD-02 `MinTierPolicy` floor (reject a write/secret-scoped worker below T2), runs
  every call through the existing `gateway_framework` allowlist + audit + rate-limit, then installs
  a new route-table snapshot atomically (TMOD-04). AuthN reuses the federation service-JWT / mTLS
  scheme already used for privileged Terminus surfaces — the admin surface is never anonymous.

  ## FILES
  - `src/broker/control.rs` — new: `POST /admin/workers/register|deregister|health`,
    `GET /admin/workers` handlers; request/response types.
  - `src/mcp_server.rs` / router build — mount the admin routes on the control port, gated by the
    existing auth extractor (never on the public `/mcp` path).
  - `src/gateway_framework/mod.rs` — reuse `AllowlistPolicy` + audit + rate-limit for admin ops.
  - `src/broker/routes.rs` — snapshot install/remove on register/deregister.
  - `src/config.rs` — control-port + admin-auth config (env-var NAMES only).

  ## APPROACH
  1. Add the admin router, mounted on the control port only; gate every handler with the existing
     federation/mTLS-or-service-JWT auth extractor. Reject unauthenticated calls.
  2. `register`: validate the manifest (semver, capability_class), enforce `MinTierPolicy` for the
     declared class vs the requested tier (reject sub-floor), open the `WorkerTransport`, run a
     health probe + `initialize`; on success build a new `RouteTable` snapshot adding the worker's
     tools and `store` it atomically. Audit-log the event (identities/names only; never secrets).
  3. `deregister`: remove the worker's routes in a new snapshot, `store` it; in-flight calls on the
     old snapshot finish.
  4. `health`: probe a named worker (or all) and report; do NOT mutate routes here.
  5. `GET /admin/workers`: list current routes (worker id, tools, tier, capability class, last
     health) — no secret material in the response.
  6. All admin ops pass through `gateway_framework` audit + rate-limit like any governed surface.

  ## TEST PLAN
  - `cargo test --workspace` green.
  - Integration: register a stub worker → its tools appear in `tools/list` and are callable via
    TMOD-04, with NO process restart.
  - Integration: update = register-or-replace a worker at a new address → routes flip to the new one
    atomically; a call straddling the swap is not torn.
  - Security: an unauthenticated admin call is rejected; a write-scoped worker registering at T1 is
    rejected on the floor; a health-failing worker is refused registration.
  - Deregister removes routes; subsequent `call` to those tools returns method-not-found.
  - Verify admin audit log redacts tokens/identities per S6; verify no raw `std::env::var` for auth secrets.
  - Verify no hardcoded infrastructure values in new/modified files.

  ## EDGE CASES
  - Duplicate register of the same worker name — treated as register-or-replace (atomic flip), not a duplicate route.
  - Register naming a tool that collides with a compiled-in tool — rejected (or namespaced) per the TMOD-04 precedence rule, with a clear error.
  - Worker dies after registration — health surfaces it; its tools return unavailable; a later re-register heals it.
  - Admin auth secret unset/misconfigured — the admin surface fails closed (refuses all), never opens anonymously.

- **Acceptance criteria:**
  - [ ] Authenticated admin control plane on the control port (never on public `/mcp`) with register/deregister/health/list
  - [ ] Registration health-gates the worker and enforces the `MinTierPolicy` floor (write/secret ⇒ ≥T2)
  - [ ] Add and update take effect with NO process restart via atomic route-table swap
  - [ ] Every admin op runs through gateway_framework allowlist + audit + rate-limit; audit redacts secrets (S6)
  - [ ] AuthN reuses the existing federation service-JWT/mTLS scheme; unauthenticated calls rejected
  - [ ] Negative tests: unauthenticated, sub-floor, and health-failing registrations all rejected
  - [ ] README/module docs document the control-plane surface
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] Secrets accessed via SecretManager/pki convention, not raw env vars
  - [ ] All existing tests still pass

---

### TMOD-06: Health-gated blue-green rollout + rollback for a worker
- **Priority:** Medium
- **Labels:** terminus, broker, rollout, resilience
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Make a worker UPDATE safe and reversible: bring a new worker instance up
  alongside the old (blue-green), health-gate it, atomically flip the route to it, and auto-roll
  back to the prior instance if the new one fails its post-flip health window — reusing the
  health-gate + rollback pattern `constellation-updater` already implements for module self-update.
  A bad worker deploy self-heals instead of poisoning the broker.

  ## FILES
  - `src/broker/rollout.rs` — new: blue-green flip + post-flip health window + rollback-to-previous.
  - `src/broker/routes.rs` — retain the previous `WorkerRoute` for a worker to enable rollback.
  - `src/broker/control.rs` — a `register` for an already-present worker uses the rollout path.

  ## APPROACH
  1. On register-or-replace of an existing worker, stand up the new route without removing the old;
     health-gate the new instance (`initialize` + `health` + optional smoke `tools/list`).
  2. On pass, atomically `store` the snapshot pointing at the new instance, retaining the previous
     `WorkerRoute` as `previous`.
  3. Watch a bounded post-flip health window; if the new instance fails it, atomically restore the
     `previous` route and mark the rollout failed (audit-logged), leaving the worker on its
     last-known-good instance.
  4. Model the states (Staging → Live → (RolledBack|Retired-previous)) after
     `constellation-updater`'s gate/rollback so the semantics match the rest of the fleet.

  ## TEST PLAN
  - `cargo test --workspace` green.
  - Unit: a healthy new instance flips live and the previous is retired after the window.
  - Unit: a new instance that fails the post-flip health window is rolled back; the previous route
    serves throughout; no dropped calls.
  - Unit: rollback is atomic (a call straddling it is not torn).
  - Verify no hardcoded infrastructure values in new/modified files.

  ## EDGE CASES
  - Both old and new unhealthy during a flip — keep serving whichever last passed; surface a clear degraded state, never route to a dead instance.
  - Health window elapses with intermittent flaps — treat as fail-closed (roll back) rather than flapping the route.
  - A deregister arriving mid-rollout — cancel the rollout cleanly, no orphaned `previous`.

- **Acceptance criteria:**
  - [ ] Worker update is blue-green: new instance health-gated before the route flips
  - [ ] Auto-rollback to the previous instance on post-flip health failure, atomically
  - [ ] Rollout states mirror the `constellation-updater` gate/rollback pattern
  - [ ] No dropped calls across a flip or a rollback
  - [ ] Negative test: failing new instance is rolled back, previous serves throughout
  - [ ] README/module docs document the rollout/rollback behavior
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### TMOD-07: Extract `vitals` as the first tool worker (pilot — read-only, T1)
- **Priority:** Medium
- **Labels:** terminus, worker, vitals, pilot
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Prove the whole seam end-to-end on a low-risk domain. Move the `vitals_*` tools
  out of the compiled-in registry into a standalone worker built on `terminus-worker-sdk`, served
  at tier **T1** (read-only ⇒ UDS + peercred is within the floor), registered with the broker via
  the control plane. This validates identity, routing, health-gate, rollout, `tools/list` merge,
  and latency on a domain where a regression is cheap. `vitals` tools remain reachable through the
  broker with identical behavior.

  ## FILES
  - `src/vitals/*` → `terminus-vitals/` (new worker crate; move the domain logic, depend on
    `terminus-worker-sdk`).
  - `terminus-vitals/src/main.rs` — new: build the `Worker` with the `vitals_*` tools, capability
    class `read_only`, serve tier T1.
  - `src/registry.rs` / `src/lib.rs` — remove `vitals::register` from the compiled-in `register_all`.
  - `Cargo.toml` (workspace) — add `terminus-vitals` as a member.

  ## APPROACH
  1. Ground in the Atlas KG (`kg_*`) for the `vitals` blast radius before moving anything.
  2. Create `terminus-vitals`, move the domain logic, keep the tool names and I/O contracts byte-identical.
  3. Wire a `main` that registers the `vitals_*` tools into a `Worker`, declares `read_only`, serves T1.
  4. Remove `vitals` from the compiled-in `register_all`; the broker now reaches it as a route.
  5. Provide a systemd unit example (repo-relative, no host literals) for the worker — actual host
     provisioning is the TMOD-10 human-action item.

  ## TEST PLAN
  - `cargo test --workspace` green (moved tests travel with the crate).
  - Integration: with the vitals worker registered, every prior `vitals_*` tool is listed and
    callable through the broker with the same output as before extraction.
  - Latency check: a broker→worker `vitals_*` call over T1 completes within a small bounded overhead
    of the former in-proc call (record the delta; it should be dominated by the tool's own work).
  - Kill the worker → `vitals_*` return "unavailable" while all other tools keep serving (fault isolation).
  - Verify no hardcoded infrastructure values in new/modified files.

  ## EDGE CASES
  - A `vitals` tool that previously shared an in-proc helper with another domain — vendor/duplicate or factor into the SDK rather than reaching back into the broker.
  - Worker restart mid-call — clean unavailable, no broker crash.
  - `tools/list` ordering — vitals tools still appear (order via merge rules), no silent drops.

- **Acceptance criteria:**
  - [ ] `vitals_*` tools served by a standalone `terminus-vitals` worker on `terminus-worker-sdk`, tier T1
  - [ ] `vitals` removed from compiled-in `register_all`; reached only as a broker route
  - [ ] All `vitals_*` tools list and call through the broker with unchanged behavior
  - [ ] Killing the worker isolates the fault to `vitals_*` only
  - [ ] Measured broker→worker overhead recorded and within a small bound
  - [ ] README/module docs updated (vitals now a worker); example systemd unit added (no host literals)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### TMOD-08: Extract `gitea` as the second worker (T2, least-privilege secret split)
- **Priority:** Medium
- **Labels:** terminus, worker, gitea, security
- **Agent:** claude
- **Estimate:** 7h
- **Description:** Prove the security payoff: move the `gitea_*` tools into a standalone worker at
  tier **T2** (write-scoped/secret-holding ⇒ floor requires ≥T2, UDS + mTLS), where the worker
  holds ONLY the Gitea credential (referenced by its PAT env-var NAME) and NO other fleet secret.
  This demonstrates per-domain blast-radius containment and exercises the `MinTierPolicy` floor for
  real (a T1 registration MUST be rejected). Respects S9 — the gitea worker is still the single
  sanctioned door to Gitea; the broker routes to it, it does not add a second access path.

  ## FILES
  - `src/gitea/*` → `terminus-gitea/` (new worker crate on `terminus-worker-sdk`).
  - `terminus-gitea/src/main.rs` — new: `Worker` with `gitea_*` tools, capability class
    `secret_holding`/`write_scoped`, serve tier T2.
  - `src/registry.rs` / `src/lib.rs` — remove `gitea::register` from compiled-in `register_all`.
  - `Cargo.toml` (workspace) — add `terminus-gitea`.

  ## APPROACH
  1. Ground in the Atlas KG for the `gitea` blast radius, especially any shared forge/identity code
     (`src/forge/*`, gitea identity convention) — factor shared pieces into the SDK rather than
     leaving a hidden coupling.
  2. Move `gitea_*` (including `gitea_cargo_publish`) into `terminus-gitea`, keeping the multi-identity
     `GITEA_PAT_<NAME>` resolution and tool contracts unchanged.
  3. The worker reads ONLY its Gitea PAT(s) via the SecretManager/pki convention; it is provisioned
     with no other domain's secret (verified in TMOD-10 provisioning).
  4. Serve T2 (UDS + mTLS); declare the secret-holding capability class so the floor requires T2.
  5. Remove `gitea` from compiled-in `register_all`; broker routes to the worker. Confirm no second
     Gitea access path is introduced (S9): the worker is the only Gitea client.

  ## TEST PLAN
  - `cargo test --workspace` green (gitea tests travel with the crate).
  - Integration: `gitea_*` tools list and call through the broker at T2 with unchanged behavior.
  - Security: attempting to register the gitea worker at T1 is REJECTED by the floor; only T2 is accepted.
  - Security: assert the worker process env carries only the Gitea PAT name(s), not other fleet
    secret names (least-privilege check — a grep/config assertion in the deploy doc + a test that the
    worker fails closed if asked for an unrelated secret).
  - S9: assert no new/direct Gitea HTTP client outside the worker; all Gitea calls route through it.
  - Verify no hardcoded infrastructure values, no raw `std::env::var` for the PAT.

  ## EDGE CASES
  - A tool that needs BOTH gitea and another domain — it stays a broker-level composition (call two workers), never a worker reaching into another worker's secret.
  - mTLS cert rotation for the gitea worker — handled by re-register/rollout (TMOD-06), no downtime.
  - Gitea PAT scope insufficient (e.g. missing `write:package` for cargo publish) — clean 403 surfaced, per existing behavior.

- **Acceptance criteria:**
  - [ ] `gitea_*` served by a standalone `terminus-gitea` worker at tier T2, holding only the Gitea PAT(s)
  - [ ] `gitea` removed from compiled-in `register_all`; reached only as a broker route
  - [ ] `MinTierPolicy` floor enforced: a sub-T2 registration of this worker is rejected
  - [ ] Least-privilege verified: the worker has no non-Gitea fleet secret
  - [ ] S9 preserved: the worker remains the single Gitea access path; no direct/duplicate client added
  - [ ] All `gitea_*` tools behave unchanged through the broker
  - [ ] README/module docs updated; example systemd unit added (no host literals)
  - [ ] No hardcoded infrastructure values; secrets via SecretManager/pki convention, not raw env
  - [ ] All existing tests still pass

---

### TMOD-09: Architecture doc + per-domain extraction runbook/template
- **Priority:** Medium
- **Labels:** terminus, docs
- **Agent:** gemini
- **Type:** documentation
- **Estimate:** 3h
- **Description:** Document the modular broker architecture and provide the repeatable runbook for
  extracting the remaining ~28 domains, so each extraction is a mechanical, low-judgment sprint.

  ## AUDIENCE
  <operator> (operator) and future contributors / the agents that will run the ~28 follow-on extractions.

  ## OUTLINE
  - Overview: broker vs. workers, the "microkernel" framing (~250 words)
  - The registry/route ArcSwap model and dispatch precedence (~250 words)
  - Transport tiers (T0/T1/T2) and the capability-floor policy — when to pick which (~300 words)
  - The control plane surface and health-gated blue-green rollout/rollback (~250 words)
  - Per-domain extraction runbook: KG-ground → new crate on the SDK → move logic (contracts byte-identical) → pick tier by capability class → remove from `register_all` → register via control plane → verify list/call/fault-isolation → systemd unit (~400 words)
  - Secret-isolation checklist per worker (least privilege) (~150 words)

  ## SOURCES
  - `src/broker/*`, `src/mcp_server.rs`, `src/registry.rs`, `src/mesh/merge.rs`
  - `terminus-worker-sdk/*`, `terminus-vitals/*`, `terminus-gitea/*`
  - The appendix "Per-domain extraction template" at the end of this spec.

  ## TONE
  Technical reference / runbook. Direct, checklist-forward. No hardcoded infrastructure values —
  env-var placeholders and repo-relative paths only.

- **Acceptance criteria:**
  - [ ] Architecture doc committed under `docs/` covering broker, transport tiers, floor, control plane, rollout
  - [ ] A copy-pasteable per-domain extraction runbook a follow-on sprint can execute mechanically
  - [ ] No hardcoded infrastructure values in examples

---

### TMOD-10: Provision per-worker transport identity + scoped secrets + bootstrap units
- **Priority:** High
- **Labels:** terminus, infra, human-action
- **Agent:** <operator>
- **Type:** human-action
- **Description:** Operator provisioning that the pipeline agents cannot and must not do
  themselves (secret/cert material, host units): mint the per-worker mTLS cert material for the T2
  workers and the socket directory for T1/T2, materialize each worker's SCOPED secret set into the
  runtime secret store (the vitals worker: none beyond what it reads; the gitea worker: only its
  Gitea PAT name(s)), provision the broker admin-auth secret (same value the broker validates),
  and stand up the per-worker systemd units on the terminus host. This is an ops action (touches no
  code) but is a hard prerequisite for TMOD-07/08 to run live.
- **Steps:**
  1. In the runtime secret store, add/confirm the per-worker mTLS cert+key material env NAMES from
     TMOD-02, and the broker admin-auth secret NAME from TMOD-05 (broker + issuer share the value).
  2. Create the worker socket directory (path from config, `TERMINUS_WORKER_SOCKET_DIR`-style) with
     perms that gate access to the broker + worker uids only.
  3. Materialize each worker's SCOPED secret set — verify the gitea worker gets ONLY its Gitea PAT
     name(s) and the vitals worker gets no fleet secret it doesn't read.
  4. Install the per-worker systemd units (from the example units in TMOD-07/08) on the terminus
     host, `Restart=on-failure`, running as the intended per-worker uid.
  5. Confirm the broker's `GET /admin/workers` lists both workers healthy after start.

---

## Behavior Spec (broker routing + fail-closed floor)

### State: BROKER_SERVING
- entry: broker started, control plane mounted on the control port, ≥0 workers registered
- exit: process stop
- verify:
  - api_health("${TERMINUS_CONTROL_URL}/admin/workers") == true
  - command_output_contains("GET ${TERMINUS_CONTROL_URL}/admin/workers", "worker")

### Transition: worker REGISTER (hot-add, no restart)
- trigger: authenticated `POST ${TERMINUS_CONTROL_URL}/admin/workers/register`
- guard: worker answers initialize+health AND declared tier satisfies the MinTier floor for its capability class
- action: build+store a new route-table snapshot adding the worker's tools
- verify:
  - after register: the worker's tools appear in `tools/list` via `${TERMINUS_MCP_URL}/mcp`
  - after register: a `tools/call` for one of its tools succeeds
  - process_count("terminus-primary") unchanged across the register (no restart)

### Stall/Failure: sub-floor or unauthenticated registration
- condition: a write/secret-scoped worker requests tier below T2, OR an unauthenticated admin call
- recovery: reject fail-closed; route table unchanged
- verify:
  - api_call("POST", "${TERMINUS_CONTROL_URL}/admin/workers/register", <sub-floor body>, 4xx)
  - api_call("POST", "${TERMINUS_CONTROL_URL}/admin/workers/register", <no-auth>, 401)
  - the target tools do NOT appear in `tools/list` after a rejected register

### API: worker dispatch fault isolation
- input: `tools/call` for a tool whose worker is down
- output: a clean "unavailable" error for that tool only
- verify:
  - other workers' and compiled-in tools still list and call successfully
  - response for the dead worker's tool is a structured unavailable error, not a 5xx crash

---

## Appendix: Per-domain extraction template (stamp once per remaining domain)

For each remaining `src/<domain>/` (ledger, crucible, relay, meridian, prometheus, github, plane,
sentinel, synapse, cortex, soma, council, network, ansible, dev, routines, axon, nexus, seer, media,
weather, news, google, <container-mgr>, <media-service>, commute, reminder, sysversion, …), create a TERM sprint
item shaped like TMOD-07 (read-only domains) or TMOD-08 (write/secret-holding domains):

- **Tier by capability class:** read-only ⇒ T1 (UDS+peercred); write-scoped or secret-holding ⇒ T2
  (UDS+mTLS); off-box (rare) ⇒ T0 (mTLS-TCP). The MinTier floor enforces this at registration.
- **Secret scope:** the worker holds ONLY that domain's secret NAMES, nothing else (least privilege).
- **Contracts byte-identical:** tool names and I/O unchanged; the broker routes transparently.
- **S9:** for domains with a single-sanctioned-door rule (plane, github, gitea), the worker IS that
  door — never add a second/direct API client.
- **Steps:** KG-ground the blast radius → new `terminus-<domain>` crate on `terminus-worker-sdk` →
  move logic + tests → declare capability class + serve tier → remove from compiled-in `register_all`
  → register via the control plane → verify list/call parity + fault isolation + latency → add the
  systemd unit (no host literals) → provision scoped secrets (human-action).
- Broker retains only latency-critical in-proc tools (if any); everything else becomes a worker.
